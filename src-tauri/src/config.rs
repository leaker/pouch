//! Startup configuration loader for Pouch.
//!
//! Resolution order (first hit wins, every step is non-fatal):
//! 1. `argv[1]` if it parses as `http(s)://...` — handy for `cargo run -- https://...`.
//! 2. `TAURI_HOOK_TARGET_URL` environment variable.
//! 3. `hook.config.json` (resolved by [`crate::util::user_data_path`]):
//!    - dev: `<CARGO_MANIFEST_DIR>/../hook.config.json` (i.e. repo root).
//!    - macOS prod: `~/Library/Application Support/Pouch/hook.config.json`.
//!    - Windows / Linux prod: same directory as the running binary.
//! 4. Built-in fallback: `https://www.leelib.com`.
//!
//! Failures at any step are logged at `warn` and we fall through to the next
//! source; Pouch should always boot successfully even with no config present
//! (an earlier internal design called for panic-on-missing, but this was
//! relaxed to "log + fall through" so Pouch runs out of the box after a
//! clone).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::hook::ignore_filter::IgnoreEntry;
use crate::util::{pretty_path, user_data_path, UserDataKind};

/// Default fallback URL when no other source provides one.
pub const DEFAULT_TARGET_URL: &str = "https://www.leelib.com";

/// Runtime configuration injected as a Tauri `State`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub target_url: String,
    /// Resolved window-size mode (never `Option` — defaults to
    /// [`WindowConfig::default`] when missing from the JSON).
    #[serde(default)]
    pub window: WindowConfig,
}

/// On-disk schema for `hook.config.json`. Kept separate from `Config` so
/// future fields can be optional in the file but always-resolved at runtime.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    target_url: Option<String>,
    /// Optional window-size config. Missing → [`WindowConfig::default`].
    #[serde(default)]
    window: Option<WindowConfig>,
    /// Optional ignoreUrls list — see `hook/ignore_filter.rs` for the
    /// per-entry pattern syntax. Compiled into the global matcher set at
    /// startup; missing/null/empty all mean "no filtering".
    #[serde(default)]
    ignore_urls: Option<Vec<IgnoreEntry>>,
}

/// Initial window-size mode.
///
/// JSON shapes:
/// - `"screen"`     → fill the work area (excludes macOS menubar/dock or
///   Windows taskbar). Default when the field is omitted.
/// - `"fullscreen"` → real fullscreen, hides window chrome.
/// - `{ "width": 1280, "height": 800 }` → fixed logical pixel size.
#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
#[serde(untagged)]
pub enum WindowConfig {
    /// String mode: `"screen"` | `"fullscreen"`.
    Mode(WindowMode),
    /// Pixel size: `{ "width": <px>, "height": <px> }`. `u32` deserialisation
    /// already rejects negatives; zero values fall back to default at apply
    /// time (see `lib.rs`).
    Size { width: u32, height: u32 },
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WindowMode {
    Screen,
    Fullscreen,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self::Mode(WindowMode::Screen)
    }
}

/// Load the runtime configuration following the resolution chain documented
/// at the module level. Never panics.
///
/// Side effect: regardless of which source supplies `target_url`, the
/// `hook.config.json` file is still consulted (best-effort) to install
/// `ignore_urls` rules into the global matcher set — `target_url` precedence
/// (CLI > env > json > default) and `ignore_urls` loading are independent.
///
/// Safe to call multiple times: the ignore-rule installation is an atomic
/// replace (see [`crate::hook::ignore_filter::set_matchers`]), so the runtime
/// Reload path simply re-invokes [`load`] to pick up edits to
/// `hook.config.json` without restarting.
pub fn load() -> Config {
    // Always look at the JSON config first so its `ignore_urls` rules get
    // installed even when `target_url` is overridden via CLI / env.
    // We also pull the optional `window` field out here so the same JSON read
    // serves both target-url-fallback and window-config purposes.
    let (json_target_url, window) = from_config_file();

    if let Some(url) = from_cli_args() {
        info!(target: "hook", "[config] target_url from CLI arg: {}", url);
        return Config {
            target_url: url,
            window,
        };
    }

    if let Some(url) = from_env() {
        info!(target: "hook", "[config] target_url from TAURI_HOOK_TARGET_URL: {}", url);
        return Config {
            target_url: url,
            window,
        };
    }

    if let Some((url, path)) = json_target_url {
        info!(
            target: "hook",
            "[config] target_url from {}: {}",
            pretty_path(&path).display(),
            url
        );
        return Config {
            target_url: url,
            window,
        };
    }

    warn!(
        target: "hook",
        "[config] no target_url provided; using default {}",
        DEFAULT_TARGET_URL
    );
    Config {
        target_url: DEFAULT_TARGET_URL.to_string(),
        window,
    }
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn from_cli_args() -> Option<String> {
    let arg = std::env::args().nth(1)?;
    if looks_like_url(&arg) {
        Some(arg)
    } else {
        None
    }
}

fn from_env() -> Option<String> {
    let raw = std::env::var("TAURI_HOOK_TARGET_URL").ok()?;
    if looks_like_url(&raw) {
        Some(raw)
    } else {
        warn!(
            target: "hook",
            "[config] TAURI_HOOK_TARGET_URL set but does not look like an http(s) URL: {:?}",
            raw
        );
        None
    }
}

/// Find and parse `hook.config.json`, returning:
/// - the resolved `target_url` plus the path we read it from (for logging), if
///   any candidate yielded a valid http(s) URL,
/// - the resolved `WindowConfig` (defaulted when missing or when no candidate
///   parsed successfully).
///
/// Side-effect: when a candidate parses, also installs any `ignore_urls` rules
/// into the global matcher set (see `hook::ignore_filter::set_matchers`),
/// atomically replacing whatever was previously installed (so a Reload
/// re-read picks up additions / removals / edits).
fn from_config_file() -> (Option<(String, PathBuf)>, WindowConfig) {
    let mut window = WindowConfig::default();
    let mut target_url: Option<(String, PathBuf)> = None;

    for candidate in candidate_config_paths() {
        match std::fs::read_to_string(&candidate) {
            Ok(text) => match serde_json::from_str::<ConfigFile>(&text) {
                Ok(parsed) => {
                    // Install ignore_urls regardless of whether target_url is
                    // present/valid — these are independent concerns and we
                    // want filtering active even if target_url falls through
                    // to env/default.
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

                    // Window config follows the same "first-hit wins" pattern
                    // as target_url — we only adopt it from the first candidate
                    // that successfully parsed.
                    if target_url.is_none() {
                        if let Some(w) = parsed.window {
                            window = w;
                        }
                    }

                    match parsed.target_url {
                        Some(url) if looks_like_url(&url) => {
                            target_url = Some((url, candidate));
                            return (target_url, window);
                        }
                        Some(url) => warn!(
                            target: "hook",
                            "[config] {} target_url is not an http(s) URL: {:?}",
                            pretty_path(&candidate).display(),
                            url
                        ),
                        None => warn!(
                            target: "hook",
                            "[config] {} has no target_url field",
                            pretty_path(&candidate).display()
                        ),
                    }
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
    (target_url, window)
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
    fn config_file_parses_target_url() {
        let parsed: ConfigFile =
            serde_json::from_str(r#"{"target_url": "https://example.com/"}"#).unwrap();
        assert_eq!(parsed.target_url.as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn config_file_tolerates_missing_field() {
        let parsed: ConfigFile = serde_json::from_str("{}").unwrap();
        assert!(parsed.target_url.is_none());
    }

    #[test]
    fn window_config_screen_string() {
        let parsed: WindowConfig = serde_json::from_str(r#""screen""#).unwrap();
        assert!(matches!(parsed, WindowConfig::Mode(WindowMode::Screen)));
    }

    #[test]
    fn window_config_fullscreen_string() {
        let parsed: WindowConfig = serde_json::from_str(r#""fullscreen""#).unwrap();
        assert!(matches!(parsed, WindowConfig::Mode(WindowMode::Fullscreen)));
    }

    #[test]
    fn window_config_size_object() {
        let parsed: WindowConfig =
            serde_json::from_str(r#"{"width": 1280, "height": 800}"#).unwrap();
        match parsed {
            WindowConfig::Size { width, height } => {
                assert_eq!(width, 1280);
                assert_eq!(height, 800);
            }
            _ => panic!("expected Size variant"),
        }
    }

    #[test]
    fn window_config_rejects_uppercase_mode() {
        // serde rename_all = "lowercase" + untagged enum: "Screen" matches no
        // variant, so the whole untagged enum fails to deserialise.
        assert!(serde_json::from_str::<WindowConfig>(r#""Screen""#).is_err());
        assert!(serde_json::from_str::<WindowConfig>(r#""FULLSCREEN""#).is_err());
        assert!(serde_json::from_str::<WindowConfig>(r#""max""#).is_err());
    }

    #[test]
    fn config_file_window_defaults_when_missing() {
        let parsed: ConfigFile = serde_json::from_str("{}").unwrap();
        assert!(parsed.window.is_none());
        // The resolved Config (via the load() path) defaults to Screen — we
        // can't easily call load() here because it touches argv/env/fs, but we
        // can confirm the WindowConfig::default() contract directly.
        assert!(matches!(
            WindowConfig::default(),
            WindowConfig::Mode(WindowMode::Screen)
        ));
    }
}
