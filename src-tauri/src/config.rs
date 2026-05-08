//! Startup configuration loader for Pouch.
//!
//! `hook.config.json` is read from (resolved by [`crate::util::user_data_path`]):
//!   - dev: `<CARGO_MANIFEST_DIR>/../hook.config.json` (i.e. repo root).
//!   - macOS prod: `~/Library/Application Support/Pouch/hook.config.json`.
//!   - Windows / Linux prod: same directory as the running binary.
//!
//! `cwd / hook.config.json` is consulted as a last-ditch fallback.
//!
//! Failures at any step are logged at `warn` and we fall through to the next
//! candidate; Pouch should always boot successfully even with no config
//! present (an earlier internal design called for panic-on-missing, but this
//! was relaxed to "log + fall through" so Pouch runs out of the box after a
//! clone — and an empty resolved `startup_urls` list is handled at startup
//! by prompting the user for a URL via `NSAlert`; see `lib.rs::run`).
//!
//! Schema note: as of v1.1, the per-window URL fields `target_url` (single)
//! and `windows` (array) have been **unified** into a single `startup_urls`
//! array — first entry becomes the main window, the rest become extra
//! windows. This is a hard schema break with no deprecation alias; users
//! upgrading from v1.0.x must migrate their `hook.config.json` by hand.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::hook::ignore_filter::IgnoreEntry;
use crate::util::{pretty_path, user_data_path, UserDataKind};

/// Runtime configuration injected as a Tauri `State`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Resolved list of URLs to open at startup. The first entry becomes the
    /// main `"main"`-labelled window; subsequent entries become extra windows
    /// labelled by [`crate::dialog::next_window_label`]. All entries are
    /// guaranteed to be valid http(s) URL **strings** at this point —
    /// non-http(s) entries are dropped at load time with a `warn` log
    /// (per-entry URL parsing for window creation still happens at use site
    /// in `lib.rs::run`). Empty when the JSON omits the field, supplies
    /// `null`, supplies `[]`, or every entry was dropped as invalid; the
    /// startup path then prompts the user via `NSAlert`.
    #[serde(default)]
    pub startup_urls: Vec<String>,
    /// Resolved window dimensions (never `Option` — defaults to
    /// [`WindowDimensions::default`] when missing from the JSON).
    #[serde(default)]
    pub window_dimensions: WindowDimensions,
}

/// On-disk schema for `hook.config.json`. Kept separate from `Config` so
/// future fields can be optional in the file but always-resolved at runtime.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    /// Optional list of URLs to open at startup. First entry becomes the
    /// main window, subsequent entries become extra windows. Each entry must
    /// be http(s); non-http(s) / unparseable entries are dropped with a warn
    /// at load time. Missing / null / empty all mean "prompt the user via
    /// NSAlert at startup" — see `lib.rs::run`.
    #[serde(default)]
    startup_urls: Option<Vec<String>>,
    /// Optional window dimensions. Missing → [`WindowDimensions::default`].
    #[serde(default)]
    window_dimensions: Option<WindowDimensions>,
    /// Optional ignoreUrls list — see `hook/ignore_filter.rs` for the
    /// per-entry pattern syntax. Compiled into the global matcher set at
    /// startup; missing/null/empty all mean "no filtering".
    #[serde(default)]
    ignore_urls: Option<Vec<IgnoreEntry>>,
}

/// Initial window dimensions. VSCode-style naming: the string variants mirror
/// VSCode's `window.newWindowDimensions` semantics for clarity.
///
/// JSON shapes:
/// - `"default"`    → ordinary floating window at the
///   `DEFAULT_WINDOW_{WIDTH,HEIGHT}` (1280x960) baseline; not maximised, not
///   fullscreen.
/// - `"maximized"`  → fill the work area (excludes macOS menubar/dock or
///   Windows taskbar). Default when the field is omitted.
/// - `"fullscreen"` → real fullscreen, hides window chrome.
/// - `{ "width": 1280, "height": 800 }` → fixed logical pixel size.
#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
#[serde(untagged)]
pub enum WindowDimensions {
    /// String mode: `"default"` | `"maximized"` | `"fullscreen"`.
    Mode(WindowDimensionsMode),
    /// Pixel size: `{ "width": <px>, "height": <px> }`. `u32` deserialisation
    /// already rejects negatives; zero values fall back to default at apply
    /// time (see `lib.rs`).
    Size { width: u32, height: u32 },
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WindowDimensionsMode {
    Default,
    Maximized,
    Fullscreen,
}

impl Default for WindowDimensions {
    fn default() -> Self {
        Self::Mode(WindowDimensionsMode::Maximized)
    }
}

/// Load the runtime configuration from `hook.config.json`. Never panics.
///
/// Side effect: regardless of whether `startup_urls` is present, the
/// `hook.config.json` file's `ignore_urls` rules are installed into the
/// global matcher set — these concerns are independent.
///
/// Safe to call multiple times: the ignore-rule installation is an atomic
/// replace (see [`crate::hook::ignore_filter::set_matchers`]), so the runtime
/// Reload path simply re-invokes [`load`] to pick up edits to
/// `hook.config.json` without restarting.
pub fn load() -> Config {
    let (startup_urls, window_dimensions) = from_config_file();
    Config {
        startup_urls,
        window_dimensions,
    }
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Find and parse `hook.config.json`, returning:
/// - the resolved list of startup URLs (filtered to valid http(s) URLs only,
///   empty when the field is missing/null/empty or every candidate failed to
///   parse),
/// - the resolved `WindowDimensions` (defaulted when missing or when no
///   candidate parsed successfully).
///
/// Side-effect: when a candidate parses, also installs any `ignore_urls` rules
/// into the global matcher set (see `hook::ignore_filter::set_matchers`),
/// atomically replacing whatever was previously installed (so a Reload
/// re-read picks up additions / removals / edits).
fn from_config_file() -> (Vec<String>, WindowDimensions) {
    let mut window_dimensions = WindowDimensions::default();
    let mut startup_urls: Vec<String> = Vec::new();

    for candidate in candidate_config_paths() {
        match std::fs::read_to_string(&candidate) {
            Ok(text) => match serde_json::from_str::<ConfigFile>(&text) {
                Ok(parsed) => {
                    // Install ignore_urls regardless of whether startup_urls
                    // is present/non-empty — these are independent concerns
                    // and we want filtering active even with no windows.
                    if let Some(entries) = parsed.ignore_urls.as_deref() {
                        if !entries.is_empty() {
                            info!(
                                target: "hook",
                                "[config] {} ignore_urls = {} entrie(s)",
                                pretty_path(&candidate).display(),
                                entries.len()
                            );
                        }
                        crate::hook::ignore_filter::set_matchers(entries);
                    } else {
                        // No ignore_urls in the freshly-read file — wipe any
                        // previous rule set so a Reload that *removes* the
                        // field actually clears matchers (not just shadows
                        // them).
                        crate::hook::ignore_filter::set_matchers(&[]);
                    }

                    // First parse-success wins for window_dimensions /
                    // startup_urls — matches the old "first hit wins"
                    // behaviour when multiple candidate paths exist. Adopt
                    // + return on this candidate so secondary paths don't
                    // clobber the already-installed values.
                    if let Some(w) = parsed.window_dimensions {
                        window_dimensions = w;
                    }
                    if let Some(entries) = parsed.startup_urls {
                        for entry in entries {
                            if looks_like_url(&entry) {
                                startup_urls.push(entry);
                            } else {
                                warn!(
                                    target: "hook",
                                    "[config] {} startup_urls entry is not an http(s) URL; skipping: {:?}",
                                    pretty_path(&candidate).display(),
                                    entry
                                );
                            }
                        }
                        if !startup_urls.is_empty() {
                            info!(
                                target: "hook",
                                "[config] {} startup_urls = {} entrie(s)",
                                pretty_path(&candidate).display(),
                                startup_urls.len()
                            );
                        }
                    }
                    return (startup_urls, window_dimensions);
                }
                Err(e) => warn!(
                    target: "hook",
                    "[config] failed to parse {}: {}",
                    pretty_path(&candidate).display(),
                    e
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Silent — most candidates won't exist; only the resolved one matters.
            }
            Err(e) => warn!(
                target: "hook",
                "[config] failed to read {}: {}",
                pretty_path(&candidate).display(),
                e
            ),
        }
    }
    (startup_urls, window_dimensions)
}

/// All locations we will try, in priority order.
///
/// In dev builds this resolves to `<CARGO_MANIFEST_DIR>/../hook.config.json`
/// (the repo root). In macOS release builds it resolves to
/// `~/Library/Application Support/Pouch/hook.config.json`. In Windows / Linux
/// release builds it resolves to `<exe parent>/hook.config.json`.
///
/// `cwd / hook.config.json` is appended unconditionally as a last-ditch
/// fallback for users running pouch from a directory that happens to contain
/// a config file (rare but cheap to support).
fn candidate_config_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();

    // Primary path — dev: repo root; macOS prod: ~/Library/.../Pouch/;
    // Windows/Linux prod: <exe parent>/.
    if let Some(p) = user_data_path(UserDataKind::Config) {
        out.push(p);
    }

    // Fallback: relative to current working directory.
    out.push(PathBuf::from("hook.config.json"));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_url_accepts_http_and_https() {
        assert!(looks_like_url("http://example.com/"));
        assert!(looks_like_url("https://example.com/"));
        assert!(!looks_like_url("ftp://example.com/"));
        assert!(!looks_like_url("example.com"));
        assert!(!looks_like_url(""));
    }

    #[test]
    fn config_file_parses_startup_urls() {
        let parsed: ConfigFile = serde_json::from_str(
            r#"{"startup_urls": ["https://a.example/", "http://b.example/"]}"#,
        )
        .unwrap();
        assert_eq!(
            parsed.startup_urls.as_deref(),
            Some(&["https://a.example/".to_string(), "http://b.example/".to_string()][..])
        );
    }

    #[test]
    fn config_file_tolerates_missing_startup_urls() {
        let parsed: ConfigFile = serde_json::from_str("{}").unwrap();
        assert!(parsed.startup_urls.is_none());
    }

    #[test]
    fn window_dimensions_default_string() {
        let parsed: WindowDimensions = serde_json::from_str(r#""default""#).unwrap();
        assert!(matches!(
            parsed,
            WindowDimensions::Mode(WindowDimensionsMode::Default)
        ));
    }

    #[test]
    fn window_dimensions_maximized_string() {
        let parsed: WindowDimensions = serde_json::from_str(r#""maximized""#).unwrap();
        assert!(matches!(
            parsed,
            WindowDimensions::Mode(WindowDimensionsMode::Maximized)
        ));
    }

    #[test]
    fn window_dimensions_fullscreen_string() {
        let parsed: WindowDimensions = serde_json::from_str(r#""fullscreen""#).unwrap();
        assert!(matches!(
            parsed,
            WindowDimensions::Mode(WindowDimensionsMode::Fullscreen)
        ));
    }

    #[test]
    fn window_dimensions_size_object() {
        let parsed: WindowDimensions =
            serde_json::from_str(r#"{"width": 1280, "height": 800}"#).unwrap();
        match parsed {
            WindowDimensions::Size { width, height } => {
                assert_eq!(width, 1280);
                assert_eq!(height, 800);
            }
            _ => panic!("expected Size variant"),
        }
    }

    #[test]
    fn window_dimensions_rejects_uppercase_mode() {
        // serde rename_all = "lowercase" + untagged enum: "Maximized" matches
        // no variant, so the whole untagged enum fails to deserialise.
        assert!(serde_json::from_str::<WindowDimensions>(r#""Maximized""#).is_err());
        assert!(serde_json::from_str::<WindowDimensions>(r#""FULLSCREEN""#).is_err());
        assert!(serde_json::from_str::<WindowDimensions>(r#""max""#).is_err());
        // The legacy v1.0.x value `"screen"` is a hard schema break (no alias).
        assert!(serde_json::from_str::<WindowDimensions>(r#""screen""#).is_err());
    }

    #[test]
    fn config_file_window_dimensions_defaults_when_missing() {
        let parsed: ConfigFile = serde_json::from_str("{}").unwrap();
        assert!(parsed.window_dimensions.is_none());
        // The resolved Config (via the load() path) defaults to Maximized —
        // we can't easily call load() here because it touches argv/env/fs,
        // but we can confirm the WindowDimensions::default() contract
        // directly.
        assert!(matches!(
            WindowDimensions::default(),
            WindowDimensions::Mode(WindowDimensionsMode::Maximized)
        ));
    }
}
