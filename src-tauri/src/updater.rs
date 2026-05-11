//! Tauri v2 updater integration: silent post-startup check + interactive
//! menu-driven check.
//!
//! Behaviour on macOS: works for the `.app` bundle installed under
//! `/Applications/` or `~/Applications/`. `cargo dev` and any other
//! non-`.app` invocation bypass the silent path (the interactive path
//! shows a release-page notice).
//!
//! Behaviour on Windows: no installer ships, so auto-update is not wired
//! up. The silent post-startup check no-ops; the interactive "Check for
//! Updates…" menu entry shows a notice pointing at the GitHub Releases
//! page. Windows upgrades go through `scoop update pouch` or by replacing
//! the portable `.exe` / `.zip`.
//!
//! The endpoint + signing pubkey live in `tauri.conf.json -> plugins.updater`;
//! the user-facing on/off switch lives in `hook.conf.toml -> [updater]`
//! (read via [`crate::config::is_updater_auto_check_enabled`]).

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;
use tracing::{info, warn};

/// URL shown in the "auto-update unavailable" dialog so users on portable
/// `.exe` / scoop / dev builds know where to grab the next release manually.
const RELEASES_URL: &str = "https://github.com/leaker/pouch/releases/latest";

/// Silent post-startup check. Spawned from the `lib.rs` setup closure
/// behind a 5-second sleep so it never competes with first-window
/// rendering / MITM warm-up. Bails on non-installed layouts (dev,
/// portable, scoop) and on `hook.conf.toml -> [updater] auto_check = false`.
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
        // Portable .exe / scoop / cargo dev — auto-update isn't wired up
        // for these layouts; point users at the releases page instead of
        // silently failing.
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

/// Shared "we found a new version" prompt → download → install → restart
/// pipeline. Called by both [`check_silent`] and [`check_interactive`].
///
/// Uses `MessageDialogBuilder::blocking_show` because both callers run on
/// the `tauri::async_runtime` (i.e. NOT the main UI thread), which is
/// exactly the safe context the plugin's docs spell out for blocking
/// dialogs. The blocking call returns `true` if the user picked the first
/// ("Update") button.
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
    // No Windows installer is shipped; updater is macOS-only. Returning
    // false here makes the silent check no-op and routes the interactive
    // menu entry to the release-page notice.
    false
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_installed_layout() -> bool {
    false
}

// TODO(skipped_version): persist "user said Later on vX" so we don't
// re-prompt every launch. Needs a generic kv helper on top of
// `storage.rs` (today storage only exposes window_state + recent_urls).
// Until that lands, the silent path will re-prompt on every launch when
// a new version is available — the interactive menu entry is always
// available as the user's explicit escape hatch.
