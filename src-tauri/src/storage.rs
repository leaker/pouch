//! Unified cross-session state persistence (`storage.db`, SQLite).
//!
//! Where this fits in the user-data layout: a sibling of `hook.config.json`
//! and the `inject/` / `overrides/` directories under the user-data root —
//!
//! - dev: `<repo>/storage.db`
//! - macOS prod: `~/Library/Application Support/Pouch/storage.db`
//! - Windows prod: `<exe parent>/storage.db`
//!
//! The path resolver piggy-backs on [`crate::util::user_data_path`] (with
//! `UserDataKind::Config`) for its **parent directory** — the file we want is
//! a sibling of `hook.config.json`, not the config itself.
//!
//! Why a single SQLite KV table (VSCode-style) instead of one JSON file per
//! kind of state:
//! - Single-row updates are atomic and don't rewrite an entire blob — adding
//!   one recent URL no longer rewrites the whole list (and a crash mid-write
//!   no longer truncates it).
//! - One file is easier to reason about than two; future state slots
//!   (per-host preferences, etc.) drop into the same table for free.
//! - SQLite's WAL + `INSERT … ON CONFLICT … DO UPDATE` give us a correct
//!   read-modify-write story without the read+rewrite race the JSON
//!   implementation had.
//!
//! Schema: a single `storage` table with `(key TEXT PK, value TEXT)`. All
//! values are JSON-serialised — keeps the row shape uniform regardless of
//! slot, and `serde_json` is already in scope everywhere else.
//!
//! Concurrency: a process-wide `OnceLock<Mutex<Connection>>` serialises every
//! read/write. SQLite's own busy-timeout / WAL would be enough for many
//! workloads, but the mutex keeps the API trivial (no `Result` plumbing for
//! lock contention) and the call rate (one write per debounced window-state
//! save, plus one per Cmd+N) is low enough that lock contention is a
//! non-issue.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::util::{user_data_path, UserDataKind};

/// File name (a sibling of `hook.config.json` under the user-data root).
const STORAGE_FILENAME: &str = "storage.db";

/// KV-table key for the persisted [`WindowState`] blob (JSON-serialised).
const KEY_WINDOW_STATE: &str = "window_state";

/// KV-table key for the persisted recent-URLs list (JSON-serialised
/// `Vec<String>`).
const KEY_RECENT_URLS: &str = "recent_urls";

/// Cap on how many recent URLs we persist / surface in the dropdown.
/// Twenty is comfortably more than a typical session's distinct URLs while
/// staying short enough that an `NSComboBox` dropdown remains usable
/// (NSComboBox's default `numberOfVisibleItems` is 5; we lift it to 8 in
/// `dialog.rs` for a slightly roomier picker — anything beyond ~20 starts
/// looking like a list-management problem the user shouldn't have to solve).
const MAX_RECENT: usize = 20;

/// Persisted geometry for a single window. Logical pixels (DPR-independent)
/// for `width` / `height`, screen-coordinate pixels for `x` / `y` (matching
/// what Tauri's `outer_position` / `inner_size` return).
///
/// `maximized` / `fullscreen` are recorded alongside the raw position+size so
/// that on restore we can re-apply the same window-mode the user left in,
/// while still keeping the un-maximize / un-fullscreen geometry available
/// (Tauri / wry restore the inner_size + position when the user toggles out
/// of those modes — without the recorded x/y/width/height the unmaximize
/// gesture would fall back to the platform default).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    pub fullscreen: bool,
}

/// Process-wide singleton SQLite connection, lazily opened on first use.
/// Wrapped in a `Mutex` because `rusqlite::Connection` is `!Sync`; the lock
/// serialises every read/write across the codebase. See module docs for why
/// the contention cost is negligible.
static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

/// Resolve the on-disk path for `storage.db`. Mirrors the resolver used by
/// the previous JSON implementation (and by `cache_store` / `config`):
/// piggy-back on `UserDataKind::Config` to find the parent directory of
/// `hook.config.json`, then attach our own filename. Falls back to a bare
/// relative filename when path resolution fails — every call site in this
/// module then opens `./storage.db` in the cwd, which is no worse than the
/// old JSON behaviour and keeps load/save on a non-panicking path.
fn storage_path() -> PathBuf {
    if let Ok(override_path) = std::env::var("POUCH_STORAGE_DB_PATH") {
        return PathBuf::from(override_path);
    }
    if let Some(config_path) = user_data_path(UserDataKind::Config) {
        if let Some(parent) = config_path.parent() {
            return parent.join(STORAGE_FILENAME);
        }
    }
    PathBuf::from(STORAGE_FILENAME)
}

/// Ensure the `storage` KV table exists on `conn`. Idempotent
/// (`CREATE TABLE IF NOT EXISTS`) — safe to call on every connection open.
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS storage (\n            key TEXT PRIMARY KEY,\n            value TEXT NOT NULL\n        )",
        [],
    )?;
    Ok(())
}

/// Lazily open / return the global connection. On first call, resolves the
/// on-disk path, ensures the parent directory exists, opens (or creates) the
/// database, and runs schema bootstrap. Subsequent calls return the cached
/// connection.
///
/// Returns `None` only when the very first open fails — every subsequent
/// caller then also gets `None` and silently no-ops, matching the
/// failure-mode contract of the previous JSON implementation (state
/// persistence is a UX nicety, never a correctness path; a missing /
/// unwritable user-data dir shouldn't crash the app).
fn db() -> Option<&'static Mutex<Connection>> {
    // OnceLock::get_or_init can't fail, but we want to keep the panic-free
    // contract — open in a closure that returns Option, then map into the
    // OnceLock cell only on success. Subsequent calls that hit the closure
    // again (because the previous attempt returned None and thus didn't
    // populate the cell) will retry — desirable: a transient $HOME
    // unavailability at boot shouldn't permanently disable persistence for
    // the rest of the process.
    if let Some(cell) = DB.get() {
        return Some(cell);
    }
    let path = storage_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(&path).ok()?;
    if init_schema(&conn).is_err() {
        return None;
    }
    // `set` returns Err if another thread won the race — fall back to whatever
    // they installed.
    let _ = DB.set(Mutex::new(conn));
    DB.get()
}

/// Read the raw JSON-encoded value for `key`. Returns `None` if the row
/// doesn't exist, the lock is poisoned, the database can't be opened, or the
/// query itself errors — every failure is folded into "missing", same
/// rationale as the previous JSON loader's "garbage in → empty out" contract.
fn get_raw(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM storage WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Upsert `(key, value)` into the storage table. Errors are swallowed — same
/// reason the JSON writer swallows them: persistence is a nice-to-have, not
/// a correctness path.
fn set_raw(conn: &Connection, key: &str, value: &str) {
    let _ = conn.execute(
        "INSERT INTO storage (key, value) VALUES (?1, ?2)\n         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    );
}

/// Persist `state` as the most-recent window state. All errors (database
/// open, lock poisoning, JSON encoding, SQL execution) are swallowed —
/// window-state persistence is a UX nicety, never a correctness path.
pub fn save_window_state(state: WindowState) {
    let Ok(json) = serde_json::to_string(&state) else { return };
    let Some(mutex) = db() else { return };
    let Ok(conn) = mutex.lock() else { return };
    set_raw(&conn, KEY_WINDOW_STATE, &json);
}

/// Read the persisted [`WindowState`].
///
/// Returns `None` on every error path — missing row, malformed JSON,
/// unreadable database — for the same reason the JSON loader did: there is
/// nothing useful to surface from a corrupted state row (we'd just rebuild
/// it on the next save). Silently starting fresh is the right failure mode.
pub fn load_window_state() -> Option<WindowState> {
    let mutex = db()?;
    let conn = mutex.lock().ok()?;
    let raw = get_raw(&conn, KEY_WINDOW_STATE)?;
    serde_json::from_str(&raw).ok()
}

/// Read the persisted list of recent URLs, ordered most-recent-first.
///
/// Returns an empty `Vec` on every error path — missing row, malformed
/// JSON, unreadable database — because there is nothing useful to surface
/// to the user from a corrupted history (we'd just rebuild it on the next
/// `add_recent_url` call). This is a UI-affordance store, not a database
/// of record; silently starting fresh is the right failure mode.
pub fn load_recent_urls() -> Vec<String> {
    let Some(mutex) = db() else { return Vec::new() };
    let Ok(conn) = mutex.lock() else { return Vec::new() };
    let Some(raw) = get_raw(&conn, KEY_RECENT_URLS) else { return Vec::new() };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Record `url` as the most-recently-used entry. Idempotent on duplicates
/// (an existing identical entry is removed before the new one is prepended,
/// so the dropdown always shows the most-recent occurrence at the top), and
/// the on-disk list is truncated to [`MAX_RECENT`] entries on every write.
///
/// Empty / whitespace-only inputs are ignored. All errors (database open,
/// lock poisoning, JSON encoding, SQL execution) are swallowed for the same
/// reason `load_recent_urls` swallows read errors — this is a "nice to
/// have" affordance, never a critical path.
pub fn add_recent_url(url: &str) {
    let url = url.trim();
    if url.is_empty() {
        return;
    }
    let Some(mutex) = db() else { return };
    let Ok(conn) = mutex.lock() else { return };

    let mut urls: Vec<String> = match get_raw(&conn, KEY_RECENT_URLS) {
        Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        None => Vec::new(),
    };
    // Dedupe: drop any prior occurrence so the new entry sits at index 0
    // without producing duplicate dropdown rows.
    urls.retain(|u| u != url);
    urls.insert(0, url.to_string());
    urls.truncate(MAX_RECENT);

    if let Ok(json) = serde_json::to_string(&urls) {
        set_raw(&conn, KEY_RECENT_URLS, &json);
    }
}

#[cfg(test)]
mod tests {
    //! Tests use `Connection::open_in_memory()` and exercise the pure
    //! SQL/JSON helpers (`init_schema` / `get_raw` / `set_raw`) directly,
    //! re-implementing the `add_recent_url` dedupe+truncate logic against
    //! that connection. We deliberately do **not** touch the global
    //! `OnceLock` / `db()` path: it's a process-wide singleton and tests
    //! sharing it would race + leak a real `storage.db` into the user's
    //! home or the repo root.
    //!
    //! What the tests cover:
    //! - `init_schema` is idempotent (calling twice is a no-op).
    //! - Window-state JSON round-trips through SQLite verbatim.
    //! - `get_raw` returns `None` for a missing key.
    //! - `set_raw` overwrites on duplicate key (UPSERT works).
    //! - Recent-URL list de-dupes existing entries and surfaces the new one
    //!   at index 0.
    //! - Recent-URL list truncates to `MAX_RECENT` after enough inserts.
    //! - Empty / whitespace inputs are ignored by the dedupe path.
    //! - Corrupt JSON in the recent-URLs row falls back to an empty list.

    use super::*;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        init_schema(&conn).expect("init_schema");
        conn
    }

    /// Mirror of `add_recent_url` against a caller-supplied connection so we
    /// can exercise the dedupe / truncate / empty-input contract without
    /// touching the global `OnceLock`.
    fn add_recent_url_with(conn: &Connection, url: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        let mut urls: Vec<String> = match get_raw(conn, KEY_RECENT_URLS) {
            Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            None => Vec::new(),
        };
        urls.retain(|u| u != url);
        urls.insert(0, url.to_string());
        urls.truncate(MAX_RECENT);
        if let Ok(json) = serde_json::to_string(&urls) {
            set_raw(conn, KEY_RECENT_URLS, &json);
        }
    }

    fn load_recent_urls_with(conn: &Connection) -> Vec<String> {
        match get_raw(conn, KEY_RECENT_URLS) {
            Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            None => Vec::new(),
        }
    }

    #[test]
    fn init_schema_is_idempotent() {
        let conn = fresh_conn();
        // Second call must not error — `CREATE TABLE IF NOT EXISTS` semantics.
        init_schema(&conn).expect("second init_schema");
    }

    #[test]
    fn window_state_roundtrip_via_sqlite() {
        let conn = fresh_conn();
        let state = WindowState {
            x: 120,
            y: 80,
            width: 1280,
            height: 960,
            maximized: false,
            fullscreen: true,
        };
        let json = serde_json::to_string(&state).unwrap();
        set_raw(&conn, KEY_WINDOW_STATE, &json);

        let loaded_raw = get_raw(&conn, KEY_WINDOW_STATE).expect("row exists");
        let loaded: WindowState = serde_json::from_str(&loaded_raw).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn missing_key_returns_none() {
        let conn = fresh_conn();
        assert!(get_raw(&conn, KEY_WINDOW_STATE).is_none());
        assert!(get_raw(&conn, KEY_RECENT_URLS).is_none());
    }

    #[test]
    fn set_raw_upserts_on_duplicate_key() {
        let conn = fresh_conn();
        set_raw(&conn, KEY_WINDOW_STATE, "\"first\"");
        set_raw(&conn, KEY_WINDOW_STATE, "\"second\"");
        assert_eq!(
            get_raw(&conn, KEY_WINDOW_STATE).as_deref(),
            Some("\"second\"")
        );
    }

    #[test]
    fn recent_url_dedupe_surfaces_existing_entry() {
        let conn = fresh_conn();
        add_recent_url_with(&conn, "https://a.example/");
        add_recent_url_with(&conn, "https://b.example/");
        // Re-add an earlier URL — must move to index 0, never duplicate.
        add_recent_url_with(&conn, "https://a.example/");
        let urls = load_recent_urls_with(&conn);
        assert_eq!(urls, vec!["https://a.example/", "https://b.example/"]);
    }

    #[test]
    fn recent_url_truncates_to_max() {
        let conn = fresh_conn();
        // Insert MAX_RECENT + 5 distinct URLs in order; the oldest 5 must be
        // dropped and the newest sits at index 0.
        for i in 0..(MAX_RECENT + 5) {
            add_recent_url_with(&conn, &format!("https://host{i}.example/"));
        }
        let urls = load_recent_urls_with(&conn);
        assert_eq!(urls.len(), MAX_RECENT);
        // Newest insert (index MAX_RECENT + 4) is at the front.
        assert_eq!(urls[0], format!("https://host{}.example/", MAX_RECENT + 4));
        // Oldest survivor: index 5 (i=0..4 fell off the end).
        assert_eq!(urls[MAX_RECENT - 1], "https://host5.example/");
    }

    #[test]
    fn recent_url_ignores_empty_and_whitespace() {
        let conn = fresh_conn();
        add_recent_url_with(&conn, "");
        add_recent_url_with(&conn, "   ");
        add_recent_url_with(&conn, "\t\n");
        assert!(load_recent_urls_with(&conn).is_empty());
    }

    #[test]
    fn corrupt_recent_urls_json_falls_back_to_empty() {
        let conn = fresh_conn();
        // Plant garbage at the recent-URLs key, then load — should not
        // panic; should return an empty list (matches the JSON
        // implementation's "garbage in → empty out" contract).
        set_raw(&conn, KEY_RECENT_URLS, "{not valid json");
        assert!(load_recent_urls_with(&conn).is_empty());
    }

    #[test]
    fn recent_url_trims_input_before_dedupe() {
        let conn = fresh_conn();
        add_recent_url_with(&conn, "https://a.example/");
        // Same URL with leading/trailing whitespace must dedupe against the
        // existing entry, not produce a second row.
        add_recent_url_with(&conn, "   https://a.example/  ");
        let urls = load_recent_urls_with(&conn);
        assert_eq!(urls, vec!["https://a.example/"]);
    }
}
