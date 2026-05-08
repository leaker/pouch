//! macOS-only history of URLs the user has previously typed into the
//! `File → New Window` (Cmd+N) `NSAlert` prompt — surfaced in the alert as
//! the dropdown of an `NSComboBox` accessory view so the user can pick a
//! prior URL instead of retyping.
//!
//! Storage choice — JSON file, not `NSUserDefaults`:
//! - The data already lives in the user's per-app data dir
//!   (`~/Library/Application Support/Pouch/`) alongside `hook.config.json`
//!   and the `inject/` / `overrides/` directories. Keeping the recent-URL
//!   list in the same directory means `View → Reveal Pouch Folder in Finder`
//!   surfaces it next to everything else; nothing about the app lives in an
//!   opaque preferences plist outside the user-visible dir.
//! - Plain JSON is trivially inspectable / editable by the user (and by
//!   tests), and we already depend on `serde_json`. `NSUserDefaults` would
//!   add a separate persistence channel for one tiny array — not worth it.
//!
//! Module-level cfg-gate: this file is only compiled on macOS. The matching
//! `#[cfg(target_os = "macos")]` `mod recent_urls;` declaration lives in
//! `lib.rs`, mirroring the gating used for `dialog` / `titlebar`.

use std::path::PathBuf;

use crate::util::{user_data_path, UserDataKind};

/// File name (a sibling of `hook.config.json` under the user-data root).
const RECENT_URLS_FILENAME: &str = "recent_urls.json";

/// Cap on how many recent URLs we persist / surface in the dropdown.
/// Twenty is comfortably more than a typical session's distinct URLs while
/// staying short enough that an `NSComboBox` dropdown remains usable
/// (NSComboBox's default `numberOfVisibleItems` is 5; we lift it to 8 in
/// `dialog.rs` for a slightly roomier picker — anything beyond ~20 starts
/// looking like a list-management problem the user shouldn't have to solve).
const MAX_RECENT: usize = 20;

/// Resolve the on-disk path for the JSON file. We piggy-back on
/// `UserDataKind::Config` only to derive the user-data *root* (the parent
/// directory of `hook.config.json`); the file we want is a sibling, not the
/// config itself. Falling back to a bare relative filename matches the
/// behaviour `user_data_path` callers use elsewhere when path resolution
/// fails — load/save then no-op silently rather than panic.
fn recent_urls_path() -> PathBuf {
    if let Some(config_path) = user_data_path(UserDataKind::Config) {
        if let Some(parent) = config_path.parent() {
            return parent.join(RECENT_URLS_FILENAME);
        }
    }
    PathBuf::from(RECENT_URLS_FILENAME)
}

/// Read the persisted list of recent URLs, ordered most-recent-first.
///
/// Returns an empty `Vec` on every error path — missing file, malformed
/// JSON, unreadable directory — because there is nothing useful to surface
/// to the user from a corrupted history (we'd just rebuild it on the next
/// `add_recent_url` call). This is a UI-affordance store, not a database;
/// silently starting fresh is the right failure mode.
pub fn load_recent_urls() -> Vec<String> {
    let path = recent_urls_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Record `url` as the most-recently-used entry. Idempotent on duplicates
/// (an existing identical entry is removed before the new one is prepended,
/// so the dropdown always shows the most-recent occurrence at the top), and
/// the on-disk list is truncated to [`MAX_RECENT`] entries on every write.
///
/// Empty / whitespace-only inputs are ignored. All I/O errors (directory
/// creation, read, write, JSON encoding) are swallowed for the same reason
/// `load_recent_urls` swallows read errors — this is a "nice to have"
/// affordance, never a critical path.
pub fn add_recent_url(url: &str) {
    let url = url.trim();
    if url.is_empty() {
        return;
    }
    let mut urls = load_recent_urls();
    // Dedupe: drop any prior occurrence so the new entry sits at index 0
    // without producing duplicate dropdown rows.
    urls.retain(|u| u != url);
    urls.insert(0, url.to_string());
    urls.truncate(MAX_RECENT);

    let path = recent_urls_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(&urls) {
        let _ = std::fs::write(&path, bytes);
    }
}
