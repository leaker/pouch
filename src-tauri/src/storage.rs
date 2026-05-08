//! Cross-session window-state persistence (`storage.json`).
//!
//! Where this fits in the user-data layout: same parent directory as
//! `hook.config.json` and `recent_urls.json` —
//!
//! - dev: `<repo>/storage.json`
//! - macOS prod: `~/Library/Application Support/Pouch/storage.json`
//! - Windows / Linux prod: `<exe parent>/storage.json`
//!
//! The path resolver piggy-backs on [`crate::util::user_data_path`] (with
//! `UserDataKind::Config`) for its **parent directory** — the file we want is
//! a sibling of `hook.config.json`, not the config itself. This mirrors how
//! [`crate::recent_urls`] resolves `recent_urls.json`.
//!
//! Why JSON, not `NSUserDefaults` / Windows Registry: the user's Pouch data
//! already lives in one user-visible directory; keeping window state alongside
//! `hook.config.json` / `inject/` / `overrides/` means `View → Reveal Pouch
//! Folder in Finder` surfaces it next to everything else, and a curious user
//! can inspect / edit / delete it with plain text tools.
//!
//! Schema: a single `window_state` slot — there's only one persisted state
//! shared across **all** windows (main + extras). The intent is "open the next
//! launch where the last session left off"; multi-window support deliberately
//! converges on the most-recently-mutated window's geometry rather than
//! tracking per-label state, because all startup windows from `startup_urls`
//! are created at the same logical position anyway.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::{user_data_path, UserDataKind};

/// File name (a sibling of `hook.config.json` under the user-data root —
/// matches the resolver pattern used by `recent_urls::recent_urls_path`).
const STORAGE_FILENAME: &str = "storage.json";

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

/// Top-level on-disk schema. `window_state: None` represents "no recorded
/// state yet" — `Inherit` mode then falls back to the same default 1280x960
/// geometry the `Default` mode uses.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Storage {
    #[serde(default)]
    pub window_state: Option<WindowState>,
}

/// Resolve the on-disk path for `storage.json`. Mirrors
/// [`crate::recent_urls::recent_urls_path`] exactly: piggy-back on
/// `UserDataKind::Config` to find the parent directory of `hook.config.json`,
/// then attach our own filename. Falls back to a bare relative filename when
/// path resolution fails — load/save then no-op silently rather than panic
/// (window-state persistence is a UX nicety, never a correctness path).
fn storage_path() -> PathBuf {
    if let Some(config_path) = user_data_path(UserDataKind::Config) {
        if let Some(parent) = config_path.parent() {
            return parent.join(STORAGE_FILENAME);
        }
    }
    PathBuf::from(STORAGE_FILENAME)
}

/// Read the persisted [`Storage`] blob from `storage.json`.
///
/// Returns [`Storage::default`] (i.e. `window_state: None`) on every error
/// path — missing file, malformed JSON, unreadable directory — for the same
/// reason `recent_urls::load_recent_urls` swallows errors: there is nothing
/// useful to surface from a corrupted state file (we'd just rebuild it on the
/// next save). Silently starting fresh is the right failure mode.
pub fn load_storage() -> Storage {
    let path = storage_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Storage::default(),
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Persist `state` as the most-recent window state. All I/O errors
/// (directory creation, JSON encoding, write) are swallowed — same rationale
/// as [`load_storage`].
///
/// We `read → mutate → write` rather than blindly overwriting, so future
/// fields added to `Storage` aren't clobbered when an older binary's
/// save_window_state hits a newer schema. Today there's only one field, but
/// the cost of one extra read is negligible compared to a forwards-compat
/// regression.
pub fn save_window_state(state: WindowState) {
    let mut storage = load_storage();
    storage.window_state = Some(state);
    let path = storage_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&storage) {
        let _ = std::fs::write(&path, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_state_roundtrip_json() {
        let state = WindowState {
            x: 120,
            y: 80,
            width: 1280,
            height: 960,
            maximized: false,
            fullscreen: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: WindowState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn storage_default_is_none() {
        let s = Storage::default();
        assert!(s.window_state.is_none());
    }

    #[test]
    fn storage_tolerates_missing_window_state_field() {
        let parsed: Storage = serde_json::from_str("{}").unwrap();
        assert!(parsed.window_state.is_none());
    }

    #[test]
    fn storage_parses_window_state() {
        let parsed: Storage = serde_json::from_str(
            r#"{"window_state":{"x":10,"y":20,"width":1024,"height":768,"maximized":true,"fullscreen":false}}"#,
        )
        .unwrap();
        let st = parsed.window_state.unwrap();
        assert_eq!(st.x, 10);
        assert_eq!(st.y, 20);
        assert_eq!(st.width, 1024);
        assert_eq!(st.height, 768);
        assert!(st.maximized);
        assert!(!st.fullscreen);
    }

    #[test]
    fn storage_corrupt_json_loads_as_default() {
        // Same failure mode contract as recent_urls: garbage in → empty out.
        let bytes = b"{not valid json";
        let parsed: Storage = serde_json::from_slice(bytes).unwrap_or_default();
        assert!(parsed.window_state.is_none());
    }

    /// File-system roundtrip via a tempdir so we never touch the user's real
    /// `storage.json`. Mirrors the read/write helpers but swaps the path
    /// resolver — keeps the production path resolver under test only via the
    /// `cfg(debug_assertions)` `user_data_path` test in `util.rs`.
    #[test]
    fn save_then_load_via_tempdir_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(STORAGE_FILENAME);

        let state = WindowState {
            x: -50,
            y: 25,
            width: 1600,
            height: 1000,
            maximized: false,
            fullscreen: true,
        };
        let storage = Storage {
            window_state: Some(state),
        };
        let bytes = serde_json::to_vec_pretty(&storage).unwrap();
        std::fs::write(&path, bytes).unwrap();

        let loaded_bytes = std::fs::read(&path).unwrap();
        let loaded: Storage = serde_json::from_slice(&loaded_bytes).unwrap();
        assert_eq!(loaded.window_state, Some(state));
    }
}
