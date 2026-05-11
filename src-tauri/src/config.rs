//! Startup configuration loader for Pouch.
//!
//! `hook.conf.toml` is read from (resolved by [`crate::util::user_data_path`]):
//!   - dev: `<CARGO_MANIFEST_DIR>/../hook.conf.toml` (i.e. repo root).
//!   - macOS prod: `~/Library/Application Support/Pouch/hook.conf.toml`.
//!   - Windows prod: same directory as the running binary.
//!
//! `cwd / hook.conf.toml` is consulted as a last-ditch fallback.
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
//! upgrading from v1.0.x must migrate their config by hand.
//!
//! Format note: as of v2.0.0, the user-facing config file has moved from
//! `hook.config.json` to `hook.conf.toml`. The schema is unchanged (TOML maps
//! cleanly to the same serde types), but the format switch lets the shipped
//! sample carry rich inline documentation that JSON cannot. Users with an
//! existing `hook.config.json` will see a one-line WARN at startup pointing
//! to the new filename — migration is by hand (no auto-rewrite of user data).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// in `lib.rs::run`). Empty when the TOML omits the field, supplies an
    /// empty value, supplies `[]`, or every entry was dropped as invalid;
    /// the startup path then prompts the user via `NSAlert`.
    #[serde(default)]
    pub startup_urls: Vec<String>,
    /// Resolved window dimensions (never `Option` — defaults to
    /// [`WindowDimensions::default`] when missing from the file).
    #[serde(default)]
    pub window_dimensions: WindowDimensions,
}

/// On-disk schema for `hook.conf.toml`. Kept separate from `Config` so
/// future fields can be optional in the file but always-resolved at runtime.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    /// Optional list of URLs to open at startup. First entry becomes the
    /// main window, subsequent entries become extra windows. Each entry must
    /// be http(s); non-http(s) / unparseable entries are dropped with a warn
    /// at load time. Missing / empty all mean "prompt the user via NSAlert
    /// at startup" — see `lib.rs::run`.
    #[serde(default)]
    startup_urls: Option<Vec<String>>,
    /// Optional window dimensions. Missing → [`WindowDimensions::default`].
    #[serde(default)]
    window_dimensions: Option<WindowDimensions>,
    /// Optional ignoreUrls list — see `hook/ignore_filter.rs` for the
    /// per-entry pattern syntax. Compiled into the global matcher set at
    /// startup; missing/empty all mean "no filtering".
    #[serde(default)]
    ignore_urls: Option<Vec<IgnoreEntry>>,
    /// Optional `[updater]` section. Missing/empty all mean "use defaults"
    /// — see [`UpdaterConfig`]. Independent of `startup_urls` /
    /// `window_dimensions` so users can drop it in piecemeal.
    #[serde(default)]
    updater: Option<UpdaterConfig>,
}

/// `[updater]` section of `hook.conf.toml`. Only one user-facing knob today
/// (auto_check); separated from the runtime [`Config`] because the updater
/// module reads its value via a process-global atomic — see
/// [`is_updater_auto_check_enabled`] — rather than threading the value
/// through every call site.
#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct UpdaterConfig {
    /// When true (default), Pouch fires a silent check ~5s after startup
    /// (see `updater::check_silent`). When false, only the interactive
    /// "Check for Updates…" menu entry triggers a check. The pubkey and
    /// endpoint that govern *whether* an update is trusted live in
    /// `tauri.conf.json -> plugins.updater`; this field controls only the
    /// "do we look at all" question.
    #[serde(default = "default_auto_check")]
    pub auto_check: bool,
}

fn default_auto_check() -> bool {
    true
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            auto_check: default_auto_check(),
        }
    }
}

/// Process-wide cached value of `[updater] auto_check`. Set by [`load`]
/// after parsing the active `hook.conf.toml` and read by
/// [`is_updater_auto_check_enabled`] from the `updater::check_silent`
/// startup task. Defaults to `true` so an unloaded config (load() never
/// called, or the file vanished mid-flight) errs on the side of "check".
///
/// `AtomicBool` rather than `OnceLock<bool>` so a future runtime-Reload
/// path (Cmd+R restarts the process today, but a hot-reload variant is
/// plausible) can update the flag in place without rebuilding the cell.
static UPDATER_AUTO_CHECK: AtomicBool = AtomicBool::new(true);

/// Initial window dimensions. VSCode-style naming: the string variants mirror
/// VSCode's `window.newWindowDimensions` semantics for clarity.
///
/// TOML shapes:
/// - `"default"`    → ordinary floating window at the
///   `DEFAULT_WINDOW_{WIDTH,HEIGHT}` (1280x960) baseline; not maximised, not
///   fullscreen.
/// - `"inherit"`    → restore position / size / mode from the previous
///   session (persisted in `storage.db` — see [`crate::storage`]); falls
///   back to the same default 1280x960 geometry as `"default"` on first
///   launch. Default when the field is omitted (VSCode-style UX —
///   first-launch fallback to default 1280x960).
/// - `"maximized"`  → fill the work area (excludes macOS menubar/dock or
///   Windows taskbar).
/// - `"fullscreen"` → real fullscreen, hides window chrome.
/// - `{ width = 1280, height = 800 }` → fixed logical pixel size.
#[derive(Debug, Deserialize, Serialize, Clone, Copy)]
#[serde(untagged)]
pub enum WindowDimensions {
    /// String mode: `"default"` | `"inherit"` | `"maximized"` | `"fullscreen"`.
    Mode(WindowDimensionsMode),
    /// Pixel size: `{ width = <px>, height = <px> }`. `u32` deserialisation
    /// already rejects negatives; zero values fall back to default at apply
    /// time (see `lib.rs`).
    Size { width: u32, height: u32 },
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WindowDimensionsMode {
    Default,
    /// Restore the last persisted window position / size / mode from
    /// `storage.db` (see [`crate::storage`]). Falls back to the same
    /// default 1280x960 geometry as [`WindowDimensionsMode::Default`] when
    /// `storage.db` is missing or has no `window_state` row recorded yet
    /// (typical on first launch). The row is updated on every resize /
    /// move via a 1-second debounced save.
    Inherit,
    Maximized,
    Fullscreen,
}

impl Default for WindowDimensions {
    fn default() -> Self {
        Self::Mode(WindowDimensionsMode::Inherit)
    }
}

/// Load the runtime configuration from `hook.conf.toml`. Never panics.
///
/// Side effect: regardless of whether `startup_urls` is present, the
/// `hook.conf.toml` file's `ignore_urls` rules are installed into the
/// global matcher set — these concerns are independent.
///
/// Safe to call multiple times: the ignore-rule installation is an atomic
/// replace (see [`crate::hook::ignore_filter::set_matchers`]), so the runtime
/// Reload path simply re-invokes [`load`] to pick up edits to
/// `hook.conf.toml` without restarting.
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

/// Find and parse `hook.conf.toml`, returning:
/// - the resolved list of startup URLs (filtered to valid http(s) URLs only,
///   empty when the field is missing/empty or every candidate failed to
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

    warn_if_legacy_json_present();

    for candidate in candidate_config_paths() {
        match std::fs::read_to_string(&candidate) {
            Ok(text) => match toml::from_str::<ConfigFile>(&text) {
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

                    // Install the `[updater]` section's `auto_check`
                    // flag into the process-global atomic so
                    // `updater::check_silent` can read it without taking
                    // a reference to the resolved Config. Missing whole
                    // section → leave at default (true).
                    let updater_cfg = parsed.updater.unwrap_or_default();
                    UPDATER_AUTO_CHECK.store(updater_cfg.auto_check, Ordering::Relaxed);

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
/// In dev builds this resolves to `<CARGO_MANIFEST_DIR>/../hook.conf.toml`
/// (the repo root). In macOS release builds it resolves to
/// `~/Library/Application Support/Pouch/hook.conf.toml`. In Windows release
/// builds it resolves to `<exe parent>/hook.conf.toml`.
///
/// `cwd / hook.conf.toml` is appended unconditionally as a last-ditch
/// fallback for users running pouch from a directory that happens to contain
/// a config file (rare but cheap to support).
fn candidate_config_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();

    // Primary path — dev: repo root; macOS prod: ~/Library/.../Pouch/;
    // Windows prod: <exe parent>/.
    if let Some(p) = user_data_path(UserDataKind::Config) {
        out.push(p);
    }

    // Fallback: relative to current working directory.
    out.push(PathBuf::from("hook.conf.toml"));

    out
}

/// Best-effort one-line WARN at startup if a legacy `hook.config.json` is
/// found alongside (or instead of) the new `hook.conf.toml`. We deliberately
/// do NOT auto-rewrite the file — TOML serialisation would lose any inline
/// comments the user may have added, and the schema is small enough that
/// hand-migration is the right call. The user's old JSON is left untouched
/// so they can copy values across at their own pace.
fn warn_if_legacy_json_present() {
    let mut legacy_candidates: Vec<PathBuf> = Vec::new();
    if let Some(primary) = user_data_path(UserDataKind::Config) {
        if let Some(parent) = primary.parent() {
            legacy_candidates.push(parent.join("hook.config.json"));
        }
    }
    legacy_candidates.push(PathBuf::from("hook.config.json"));

    for legacy in legacy_candidates {
        if legacy.is_file() {
            warn!(
                target: "hook",
                "[config] legacy {} found; the user-config format moved to hook.conf.toml in v2.0.0. \
                 Please port your settings to hook.conf.toml — the old JSON is no longer read.",
                pretty_path(&legacy).display()
            );
            return; // one warn is enough
        }
    }
}

/// Returns the cached value of `[updater] auto_check`. Reads the
/// process-global atomic set by [`load`]; safe to call from any thread.
/// If `load` has not yet run (i.e. called before `setup` for some reason),
/// returns `true` so the updater errs on the side of checking.
pub fn is_updater_auto_check_enabled() -> bool {
    UPDATER_AUTO_CHECK.load(Ordering::Relaxed)
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
        let parsed: ConfigFile = toml::from_str(
            r#"startup_urls = ["https://a.example/", "http://b.example/"]"#,
        )
        .unwrap();
        assert_eq!(
            parsed.startup_urls.as_deref(),
            Some(&["https://a.example/".to_string(), "http://b.example/".to_string()][..])
        );
    }

    #[test]
    fn config_file_tolerates_missing_startup_urls() {
        let parsed: ConfigFile = toml::from_str("").unwrap();
        assert!(parsed.startup_urls.is_none());
    }

    /// Helper: TOML can't deserialise a bare scalar at the top level (every
    /// document is a table), so each test wraps the value in `x = ...` and
    /// extracts back through a one-shot wrapper struct. Mirrors what
    /// `serde_json::from_str("\"default\"")` did directly in JSON.
    fn parse_window_dimensions(value_toml: &str) -> Result<WindowDimensions, toml::de::Error> {
        #[derive(Deserialize)]
        struct W {
            x: WindowDimensions,
        }
        toml::from_str::<W>(&format!("x = {value_toml}")).map(|w| w.x)
    }

    #[test]
    fn window_dimensions_default_string() {
        let parsed = parse_window_dimensions(r#""default""#).unwrap();
        assert!(matches!(
            parsed,
            WindowDimensions::Mode(WindowDimensionsMode::Default)
        ));
    }

    #[test]
    fn window_dimensions_inherit_string() {
        let parsed = parse_window_dimensions(r#""inherit""#).unwrap();
        assert!(matches!(
            parsed,
            WindowDimensions::Mode(WindowDimensionsMode::Inherit)
        ));
    }

    #[test]
    fn window_dimensions_maximized_string() {
        let parsed = parse_window_dimensions(r#""maximized""#).unwrap();
        assert!(matches!(
            parsed,
            WindowDimensions::Mode(WindowDimensionsMode::Maximized)
        ));
    }

    #[test]
    fn window_dimensions_fullscreen_string() {
        let parsed = parse_window_dimensions(r#""fullscreen""#).unwrap();
        assert!(matches!(
            parsed,
            WindowDimensions::Mode(WindowDimensionsMode::Fullscreen)
        ));
    }

    #[test]
    fn window_dimensions_size_object() {
        let parsed = parse_window_dimensions(r#"{ width = 1280, height = 800 }"#).unwrap();
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
        assert!(parse_window_dimensions(r#""Maximized""#).is_err());
        assert!(parse_window_dimensions(r#""FULLSCREEN""#).is_err());
        assert!(parse_window_dimensions(r#""max""#).is_err());
        // The legacy v1.0.x value `"screen"` is a hard schema break (no alias).
        assert!(parse_window_dimensions(r#""screen""#).is_err());
    }

    #[test]
    fn config_file_window_dimensions_defaults_when_missing() {
        let parsed: ConfigFile = toml::from_str("").unwrap();
        assert!(parsed.window_dimensions.is_none());
        // The resolved Config (via the load() path) defaults to Inherit —
        // we can't easily call load() here because it touches argv/env/fs,
        // but we can confirm the WindowDimensions::default() contract
        // directly.
        assert!(matches!(
            WindowDimensions::default(),
            WindowDimensions::Mode(WindowDimensionsMode::Inherit)
        ));
    }

    #[test]
    fn updater_config_defaults_auto_check_true() {
        // Bare `[updater]` table with no fields → auto_check defaults to true.
        let parsed: ConfigFile = toml::from_str("[updater]\n").unwrap();
        let u = parsed.updater.expect("updater section present");
        assert!(u.auto_check, "default for auto_check should be true");
    }

    #[test]
    fn updater_config_explicit_false_disables() {
        let parsed: ConfigFile =
            toml::from_str("[updater]\nauto_check = false\n").unwrap();
        let u = parsed.updater.expect("updater section present");
        assert!(!u.auto_check);
    }

    #[test]
    fn updater_config_missing_section_is_none() {
        // No [updater] table at all → parsed.updater is None; the caller
        // falls back to UpdaterConfig::default(), which has auto_check = true.
        let parsed: ConfigFile = toml::from_str("").unwrap();
        assert!(parsed.updater.is_none());
        assert!(UpdaterConfig::default().auto_check);
    }

    #[test]
    fn updater_config_rejects_unknown_keys() {
        // `deny_unknown_fields` keeps the schema honest — typos in field
        // names should error at load time instead of silently ignoring
        // user intent (e.g. `auto-check` with a hyphen).
        let err =
            toml::from_str::<ConfigFile>("[updater]\nauto-check = false\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("auto-check") || msg.contains("unknown"),
            "expected unknown-field error mentioning the bad key, got: {msg}"
        );
    }

    #[test]
    fn config_file_parses_full_toml_sample() {
        // Exercise the full schema (startup_urls + window_dimensions +
        // ignore_urls with all four entry shapes) in one shot, mirroring the
        // shipped `hook.conf.toml` sample. Keeps a single source of truth
        // for "the user's file shape really does deserialise".
        let parsed: ConfigFile = toml::from_str(
            r#"
            startup_urls = ["https://www.leelib.com"]
            window_dimensions = "inherit"
            ignore_urls = [
                { suffix = "gstatic.com" },
                { wildcard = "*.google.com" },
                { url_wildcard = "https://ipecho.io/*" },
                { url_regex = "^https://example\\.com/track/.*" },
            ]
            "#,
        )
        .unwrap();
        assert_eq!(parsed.startup_urls.as_deref().map(|s| s.len()), Some(1));
        assert!(matches!(
            parsed.window_dimensions,
            Some(WindowDimensions::Mode(WindowDimensionsMode::Inherit))
        ));
        assert_eq!(parsed.ignore_urls.as_deref().map(|s| s.len()), Some(4));
    }
}
