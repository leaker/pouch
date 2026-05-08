//! Startup configuration loader for Pouch.
//!
//! Resolution order (first hit wins, every step is non-fatal):
//! 1. `argv[1]` if it parses as `http(s)://...` — handy for `cargo run -- https://...`.
//! 2. `TAURI_HOOK_TARGET_URL` environment variable.
//! 3. `hook.config.json`:
//!    - dev: `<CARGO_MANIFEST_DIR>/../hook.config.json` (i.e. repo root).
//!    - prod: same directory as the running binary, then `./hook.config.json`.
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
use crate::util::pretty_path;

/// Default fallback URL when no other source provides one.
pub const DEFAULT_TARGET_URL: &str = "https://www.leelib.com";

/// Runtime configuration injected as a Tauri `State`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub target_url: String,
}

/// On-disk schema for `hook.config.json`. Kept separate from `Config` so
/// future fields can be optional in the file but always-resolved at runtime.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    target_url: Option<String>,
    /// Optional ignoreUrls list — see `hook/ignore_filter.rs` for the
    /// per-entry pattern syntax. Compiled into the global matcher set at
    /// startup; missing/null/empty all mean "no filtering".
    #[serde(default)]
    ignore_urls: Option<Vec<IgnoreEntry>>,
}

/// Load the runtime configuration following the resolution chain documented
/// at the module level. Never panics.
///
/// Side effect: regardless of which source supplies `target_url`, the
/// `hook.config.json` file is still consulted (best-effort) to install
/// `ignore_urls` rules into the global matcher set — `target_url` precedence
/// (CLI > env > json > default) and `ignore_urls` loading are independent.
pub fn load() -> Config {
    // Always look at the JSON config first so its `ignore_urls` rules get
    // installed even when `target_url` is overridden via CLI / env.
    let json_target_url = from_config_file();

    if let Some(url) = from_cli_args() {
        info!(target: "hook", "[config] target_url from CLI arg: {}", url);
        return Config { target_url: url };
    }

    if let Some(url) = from_env() {
        info!(target: "hook", "[config] target_url from TAURI_HOOK_TARGET_URL: {}", url);
        return Config { target_url: url };
    }

    if let Some((url, path)) = json_target_url {
        info!(
            target: "hook",
            "[config] target_url from {}: {}",
            pretty_path(&path).display(),
            url
        );
        return Config { target_url: url };
    }

    warn!(
        target: "hook",
        "[config] no target_url provided; using default {}",
        DEFAULT_TARGET_URL
    );
    Config {
        target_url: DEFAULT_TARGET_URL.to_string(),
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

/// Find and parse `hook.config.json`, returning the resolved URL plus the path
/// we read it from (for logging). Side-effect: when a candidate parses, also
/// installs any `ignore_urls` rules into the global matcher set (see
/// `hook::ignore_filter::init_from_entries`).
fn from_config_file() -> Option<(String, PathBuf)> {
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
                        crate::hook::ignore_filter::init_from_entries(entries);
                    }

                    match parsed.target_url {
                        Some(url) if looks_like_url(&url) => return Some((url, candidate)),
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
    None
}

/// All locations we will try, in priority order.
fn candidate_config_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();

    // Dev path: <CARGO_MANIFEST_DIR>/../hook.config.json — i.e. repo root.
    // CARGO_MANIFEST_DIR is baked in at compile time via env! and is always
    // available because this crate has a Cargo.toml.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    out.push(manifest_dir.join("..").join("hook.config.json"));

    // Prod path: same directory as the binary.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("hook.config.json"));
        }
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
}
