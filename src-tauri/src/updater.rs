//! Tauri v2 updater integration: silent post-startup check + interactive
//! menu-driven check.
//!
//! Behaviour on macOS: works for the `.app` bundle installed under
//! `/Applications/` or `~/Applications/`. `cargo dev` and any other
//! non-`.app` invocation bypass the silent path (the interactive path
//! shows a release-page notice). Goes through `tauri-plugin-updater`:
//! fetch latest.json -> verify signature -> download `.app.tar.gz` ->
//! replace bundle -> restart. Unchanged in this revision.
//!
//! Behaviour on Windows: "soft" auto-update notice — Pouch checks but
//! never downloads or installs. It deliberately does NOT use
//! `tauri-plugin-updater::check()` because the manifest at
//! `https://github.com/leaker/pouch/releases/latest/download/latest.json`
//! has no `windows-x86_64` entry today (the macOS pubkey-signed
//! `.app.tar.gz` is the only payload), so the plugin would reject every
//! response. Instead we:
//!
//!   1. Fetch `latest.json` directly with `reqwest` (10s timeout).
//!   2. Parse the `version` field and compare against
//!      `CARGO_PKG_VERSION` via `semver::Version`.
//!   3. If a newer version is available, detect the install style from
//!      `current_exe()` (Scoop vs portable) and show a dialog with the
//!      matching follow-up:
//!        - Scoop: "Copy command" copies `scoop update pouch` to the
//!          clipboard.
//!        - Portable: "Open release page" launches the system browser
//!          at the releases page.
//!
//! Any fetch / parse / compare failure logs at warn and silently bails —
//! the user is never bothered by a transient network blip. The
//! interactive ("Check for Updates…" menu) path additionally surfaces a
//! "could not check" dialog on failure so the user gets feedback when
//! they explicitly asked.
//!
//! The endpoint + signing pubkey live in `tauri.conf.json -> plugins.updater`;
//! the user-facing on/off switch lives in `hook.conf.toml -> [updater]`
//! (read via [`crate::config::is_updater_auto_check_enabled`]) — the
//! switch gates BOTH platforms: macOS skips its plugin check, Windows
//! skips the latest.json fetch, both honoured every 24h tick.

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
#[cfg(target_os = "macos")]
use tauri_plugin_updater::UpdaterExt;
use tracing::{info, warn};

/// URL shown in the "auto-update unavailable" dialog so users on portable
/// `.exe` / scoop / dev builds know where to grab the next release manually.
const RELEASES_URL: &str = "https://github.com/leaker/pouch/releases/latest";

/// GitHub-hosted update manifest. Pouch's macOS updater plugin reads this
/// same URL via the `endpoints` array in `tauri.conf.json`; the Windows
/// path fetches it directly because the plugin would reject the manifest
/// (no `windows-x86_64` entry). The release upload step always overwrites
/// this file in the `releases/latest` virtual tag, so a single URL keeps
/// working across releases.
#[cfg(target_os = "windows")]
const LATEST_JSON_URL: &str =
    "https://github.com/leaker/pouch/releases/latest/download/latest.json";

/// Silent post-startup check. Spawned from the `lib.rs` setup closure
/// behind a 5-second sleep so it never competes with first-window
/// rendering / MITM warm-up, then re-armed every 24h. Bails on
/// non-installed layouts (dev, portable .app) and on
/// `hook.conf.toml -> [updater] auto_check = false`.
///
/// Platform split inside:
///   - macOS: full `tauri-plugin-updater` download-and-install flow.
///   - Windows: lightweight "soft" notice — fetches latest.json directly,
///     never downloads the binary itself.
pub async fn check_silent(app: AppHandle) {
    if !is_installed_layout() {
        info!(target: "hook", "[updater] non-installed layout; skipping silent check");
        return;
    }
    if !crate::config::is_updater_auto_check_enabled() {
        info!(
            target: "hook",
            "[updater] hook.conf.toml [updater] auto_check = false; skipping silent check"
        );
        return;
    }

    #[cfg(target_os = "macos")]
    {
        check_silent_macos(app).await;
    }
    #[cfg(target_os = "windows")]
    {
        check_silent_windows(app).await;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = app;
    }
}

/// macOS silent check — full plugin-driven download/install. Unchanged
/// from the previous revision; extracted into its own function only so
/// the dispatcher in [`check_silent`] reads cleanly.
#[cfg(target_os = "macos")]
async fn check_silent_macos(app: AppHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            warn!(target: "hook", "[updater] updater plugin not available: {e}");
            return;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => {
            info!(
                target: "hook",
                "[updater] silent check: new version available v{}",
                update.version
            );
            prompt_and_install(&app, update).await;
        }
        Ok(None) => {
            info!(target: "hook", "[updater] silent check: already on latest");
        }
        Err(e) => {
            warn!(target: "hook", "[updater] silent check failed: {e}");
        }
    }
}

/// User-invoked check (from the "Check for Updates…" menu entry). Always
/// returns a dialog — "no update", "update available", or an error — so the
/// user gets feedback regardless of outcome. Bypasses the `auto_check`
/// preference because the user explicitly asked.
pub async fn check_interactive(app: AppHandle) {
    if !is_installed_layout() {
        // Portable .app / cargo dev — no auto-update wired up for these
        // layouts. (Windows now always reports installed-layout = true
        // because the soft-notice path works for both scoop and portable
        // exes.) Point users at the releases page.
        app.dialog()
            .message(format!(
                "This build does not support auto-update.\n\n\
                 Download the latest release from:\n{RELEASES_URL}"
            ))
            .title("Auto-update unavailable")
            .kind(MessageDialogKind::Info)
            .show(|_| {});
        return;
    }

    #[cfg(target_os = "macos")]
    {
        check_interactive_macos(app).await;
    }
    #[cfg(target_os = "windows")]
    {
        check_interactive_windows(app).await;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = app;
    }
}

/// macOS interactive check — full plugin-driven flow. Unchanged behaviour
/// from the previous revision; extracted only so the dispatcher in
/// [`check_interactive`] reads cleanly.
#[cfg(target_os = "macos")]
async fn check_interactive_macos(app: AppHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            warn!(target: "hook", "[updater] updater plugin not available: {e}");
            app.dialog()
                .message(format!("Updater plugin not available:\n{e}"))
                .title("Update check failed")
                .kind(MessageDialogKind::Error)
                .show(|_| {});
            return;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => {
            prompt_and_install(&app, update).await;
        }
        Ok(None) => {
            app.dialog()
                .message("You are running the latest version of Pouch.")
                .title("No update available")
                .kind(MessageDialogKind::Info)
                .show(|_| {});
        }
        Err(e) => {
            warn!(target: "hook", "[updater] interactive check failed: {e}");
            app.dialog()
                .message(format!("Could not check for updates:\n{e}"))
                .title("Update check failed")
                .kind(MessageDialogKind::Error)
                .show(|_| {});
        }
    }
}

/// Windows silent check. Fetches latest.json directly, compares, and
/// silently shows the soft-notice dialog when a newer version is
/// available. Failures (network, parse, semver) log at warn and bail —
/// the user is never bothered.
#[cfg(target_os = "windows")]
async fn check_silent_windows(app: AppHandle) {
    let Some(latest_version) = fetch_latest_version().await else {
        return;
    };
    let Some(current) = parse_version(env!("CARGO_PKG_VERSION"), "current") else {
        return;
    };
    let Some(latest) = parse_version(&latest_version, "latest") else {
        return;
    };
    if latest <= current {
        info!(
            target: "hook",
            "[updater] silent check: already on latest (v{current} >= v{latest})"
        );
        return;
    }
    info!(
        target: "hook",
        "[updater] silent check: new version v{latest} available (current v{current})"
    );
    prompt_windows_update(app, latest_version).await;
}

/// Windows interactive check. Always shows a dialog so the user has
/// feedback even when there's no update or the check failed.
#[cfg(target_os = "windows")]
async fn check_interactive_windows(app: AppHandle) {
    let Some(latest_version) = fetch_latest_version().await else {
        app.dialog()
            .message(format!(
                "Could not reach GitHub to check for updates.\n\nVisit {RELEASES_URL} manually."
            ))
            .title("Update check failed")
            .kind(MessageDialogKind::Error)
            .show(|_| {});
        return;
    };
    let current = parse_version(env!("CARGO_PKG_VERSION"), "current");
    let latest = parse_version(&latest_version, "latest");
    match (current, latest) {
        (Some(c), Some(l)) if l > c => prompt_windows_update(app, latest_version).await,
        (Some(c), Some(l)) => {
            info!(
                target: "hook",
                "[updater] interactive check: already on latest (v{c} >= v{l})"
            );
            app.dialog()
                .message("You are running the latest version of Pouch.")
                .title("No update available")
                .kind(MessageDialogKind::Info)
                .show(|_| {});
        }
        _ => {
            // semver parse failed on one or both sides — already logged
            // by `parse_version`. Surface a generic failure dialog so the
            // user knows the click registered.
            app.dialog()
                .message(format!(
                    "Could not parse version info from GitHub.\n\nVisit {RELEASES_URL} manually."
                ))
                .title("Update check failed")
                .kind(MessageDialogKind::Error)
                .show(|_| {});
        }
    }
}

/// Helper: parse a string to `semver::Version`, log at warn on failure
/// (with a short `which` tag so the log line says whether the current or
/// latest version failed), return `None` on failure so the caller can
/// `let-else` bail. Used only by the Windows path.
#[cfg(target_os = "windows")]
fn parse_version(s: &str, which: &str) -> Option<semver::Version> {
    match semver::Version::parse(s) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(
                target: "hook",
                "[updater] failed to parse {which} version {:?}: {}",
                s,
                e
            );
            None
        }
    }
}

/// Fetch `latest.json` from the GitHub releases mirror and return its
/// `version` field as a `String`. All errors (network, non-2xx HTTP,
/// JSON parse, missing field) log at warn and resolve to `None` so the
/// caller can `let-else` bail without further matching.
///
/// `reqwest` is already a direct dep (used by the cache_store fetcher);
/// we don't share the existing client because that one is wired with
/// MITM-specific defaults. A fresh, plain client with rustls is fine for
/// a single one-shot GET to GitHub.
#[cfg(target_os = "windows")]
async fn fetch_latest_version() -> Option<String> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("pouch-updater/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "hook", "[updater] reqwest client build failed: {e}");
            return None;
        }
    };
    let response = match client.get(LATEST_JSON_URL).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "hook", "[updater] fetch latest.json failed: {e}");
            return None;
        }
    };
    if !response.status().is_success() {
        warn!(
            target: "hook",
            "[updater] latest.json HTTP {}",
            response.status()
        );
        return None;
    }
    let json: serde_json::Value = match response.json().await {
        Ok(j) => j,
        Err(e) => {
            warn!(target: "hook", "[updater] latest.json parse failed: {e}");
            return None;
        }
    };
    let version = json["version"].as_str().map(str::to_string);
    if version.is_none() {
        warn!(
            target: "hook",
            "[updater] latest.json has no `version` field; payload = {json}"
        );
    }
    version
}

/// Show the "soft update" dialog on Windows. Branches on
/// [`is_scoop_install`]:
///   - Scoop: dialog says "Update with `scoop update pouch`" and offers
///     a "Copy command" button that writes the command to the clipboard.
///     We don't auto-launch PowerShell — that would either need an
///     elevated prompt or pollute the user's shell history, and either
///     way is more surprising than helpful.
///   - Portable: dialog says "Download the latest release from GitHub"
///     and offers "Open release page" which launches the default
///     browser at `RELEASES_URL`.
///
/// Both flows END at the dialog. Pouch never downloads or installs the
/// new build itself on Windows.
#[cfg(target_os = "windows")]
async fn prompt_windows_update(app: AppHandle, version: String) {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    use tauri_plugin_shell::ShellExt;

    let title = "Update available";

    if is_scoop_install() {
        let message = format!(
            "Pouch v{version} is available. Update with:\n\nscoop update pouch"
        );
        if ask_dialog(&app, title, &message, "Copy command", "Later").await {
            if let Err(e) = app.clipboard().write_text("scoop update pouch".to_string()) {
                warn!(
                    target: "hook",
                    "[updater] clipboard write failed: {e}; falling back to release-page open"
                );
                let _ = app.shell().open(RELEASES_URL, None);
            } else {
                info!(target: "hook", "[updater] copied scoop update command to clipboard");
            }
        } else {
            info!(target: "hook", "[updater] user deferred v{version} (scoop)");
        }
    } else {
        let message = format!(
            "Pouch v{version} is available. Download the latest release from GitHub."
        );
        if ask_dialog(&app, title, &message, "Open release page", "Later").await {
            if let Err(e) = app.shell().open(RELEASES_URL, None) {
                warn!(target: "hook", "[updater] shell open {RELEASES_URL} failed: {e}");
            } else {
                info!(target: "hook", "[updater] opened release page for v{version} (portable)");
            }
        } else {
            info!(target: "hook", "[updater] user deferred v{version} (portable)");
        }
    }
}

/// Async-await wrapper around the dialog plugin's callback-style
/// `MessageDialogBuilder::show` for an `OkCancelCustom` two-button
/// prompt. Returns `true` when the user picks the first ("ok") button.
/// We use the non-blocking variant + a `oneshot` because the silent
/// path runs on the `tauri::async_runtime` (`tokio` underneath) and
/// awaiting a oneshot keeps the runtime worker free during the (often
/// minutes-long) gap before the user clicks. Used only by the Windows
/// path; macOS sticks with `blocking_show` inside `prompt_and_install`
/// because that flow continues straight into the synchronous
/// `download_and_install` regardless.
#[cfg(target_os = "windows")]
async fn ask_dialog(
    app: &AppHandle,
    title: &str,
    message: &str,
    ok_label: &str,
    cancel_label: &str,
) -> bool {
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let mut tx_slot = Some(tx);
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            ok_label.to_string(),
            cancel_label.to_string(),
        ))
        .show(move |ok| {
            if let Some(tx) = tx_slot.take() {
                let _ = tx.send(ok);
            }
        });
    rx.await.unwrap_or(false)
}

/// Detect Scoop installs by looking for `\scoop\apps\pouch\` in the
/// (case-insensitive) full path of `current_exe()`. Scoop installs land
/// under `%USERPROFILE%\scoop\apps\pouch\current\pouch.exe` by default;
/// users can move the Scoop root via `SCOOP` env, but the
/// `\apps\<name>\` segment is fixed by Scoop's layout, so the substring
/// check is robust. Anything else (incl. the portable .exe / .zip
/// extraction) is treated as portable.
#[cfg(target_os = "windows")]
fn is_scoop_install() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let p = exe.to_string_lossy().to_lowercase();
    p.contains("\\scoop\\apps\\pouch\\")
}

/// Shared "we found a new version" prompt → download → install → restart
/// pipeline. Called by both [`check_silent`] and [`check_interactive`].
///
/// Uses `MessageDialogBuilder::blocking_show` because both callers run on
/// the `tauri::async_runtime` (i.e. NOT the main UI thread), which is
/// exactly the safe context the plugin's docs spell out for blocking
/// dialogs. The blocking call returns `true` if the user picked the first
/// ("Update") button.
#[cfg(target_os = "macos")]
async fn prompt_and_install(app: &AppHandle, update: tauri_plugin_updater::Update) {
    let version = update.version.clone();
    let notes = update.body.clone().unwrap_or_default();
    let message = if notes.trim().is_empty() {
        format!("Pouch v{version} is available.\n\nUpdate now?")
    } else {
        format!("Pouch v{version} is available.\n\n{notes}\n\nUpdate now?")
    };

    // `blocking_show` is documented as safe off the main thread — we're
    // inside a `tauri::async_runtime::spawn` task here, so this is OK.
    // For `OkCancelCustom`, the bool result is `true` when the user picks
    // the FIRST button ("Update"), `false` for the second ("Later").
    let proceed = app
        .dialog()
        .message(message)
        .title(format!("Update to v{version}"))
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Update".to_string(),
            "Later".to_string(),
        ))
        .blocking_show();

    if !proceed {
        // No skipped-version persistence yet (would need a kv helper on
        // top of `storage.rs`); for now the user gets prompted again on
        // the next launch / next interactive check. See the TODO at the
        // bottom of this module for the storage hook point.
        info!(target: "hook", "[updater] user deferred v{version}");
        return;
    }

    info!(target: "hook", "[updater] downloading + installing v{version}");

    if let Err(e) = update
        .download_and_install(
            |chunk_len, content_len| match content_len {
                Some(total) => {
                    info!(
                        target: "hook",
                        "[updater] progress: chunk {chunk_len} of {total} total"
                    );
                }
                None => {
                    info!(
                        target: "hook",
                        "[updater] progress: chunk {chunk_len} (total size unknown)"
                    );
                }
            },
            || {
                info!(target: "hook", "[updater] download complete; preparing restart");
            },
        )
        .await
    {
        warn!(target: "hook", "[updater] download/install failed: {e}");
        app.dialog()
            .message(format!("Update failed:\n{e}"))
            .title("Update failed")
            .kind(MessageDialogKind::Error)
            .show(|_| {});
        return;
    }

    // The updater plugin auto-restarts after a successful install on every
    // platform we support, but `app.restart()` is documented as `-> !` and
    // safe to call as a fallback in case the plugin chose not to restart
    // (e.g. install completed without re-launch).
    info!(target: "hook", "[updater] restarting to complete update");
    app.restart();
}

#[cfg(target_os = "macos")]
fn is_installed_layout() -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // Both /Applications/Pouch.app/Contents/MacOS/pouch and
    // ~/Applications/Pouch.app/Contents/MacOS/pouch match.
    exe.to_string_lossy().contains("/Pouch.app/Contents/MacOS/")
}

#[cfg(target_os = "windows")]
fn is_installed_layout() -> bool {
    // Windows uses the "soft notice" path (no auto-download), so every
    // non-dev build qualifies — both `scoop install pouch` and the
    // portable .exe / .zip extraction go through the same code path,
    // they just see a differently-worded dialog (see
    // `prompt_windows_update`). Dev builds (`cargo dev` / debug
    // assertions on) still skip so the check doesn't run while you're
    // iterating locally.
    !cfg!(debug_assertions)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_installed_layout() -> bool {
    false
}

// TODO(skipped_version): persist "user said Later on vX" so we don't
// re-prompt every launch / every 24h tick. Needs a generic kv helper on
// top of `storage.rs` (today storage only exposes window_state +
// recent_urls). Until that lands, the silent path will re-prompt on
// every 24h tick when a new version is available — the interactive
// menu entry is always available as the user's explicit escape hatch.
