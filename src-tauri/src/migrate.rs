//! One-shot migrations for Pouch's user-data layout.
//!
//! v2.1.0 promoted Windows from a portable (exe-sibling) data root to the
//! standard Roaming AppData location (`%APPDATA%\Pouch\`), matching how
//! macOS uses `~/Library/Application Support/Pouch/`. This module copies
//! an existing v2.0.x layout into the new home on the first v2.1.0 launch
//! so users do not lose their config / inject scripts / cached overrides.
//!
//! Design rules:
//! - **Idempotent**: a marker file (`.migrated-from-portable`) at the new
//!   root short-circuits every subsequent launch.
//! - **Non-destructive**: never overwrites a file that already exists at
//!   the destination (the user may have already populated the new root by
//!   hand or via a prior partial migration).
//! - **Best-effort**: all failures are logged at WARN and never abort
//!   startup — pouch can still boot with empty or default data; the
//!   resolver chain in `util::user_data_path` already handles that.
//!
//! On macOS the data path didn't change between v2.0.x and v2.1.0, so the
//! module exposes a no-op shim for non-Windows targets and the call site
//! in `lib.rs::setup` can dispatch unconditionally.

#[cfg(target_os = "windows")]
pub fn migrate_legacy_windows_data() {
    use std::fs;
    use tracing::{info, warn};

    let Some(new_root) = crate::util::windows_appdata_dir() else {
        warn!(
            target: "hook",
            "[migrate] %APPDATA% not set; skipping portable->AppData migration"
        );
        return;
    };
    let Some(old_root) = crate::util::windows_legacy_portable_dir() else {
        // current_exe() failed — extremely unusual; nothing we can migrate.
        return;
    };

    // Belt-and-braces: never copy a directory onto itself. The two helpers
    // resolve to fundamentally different roots (%APPDATA% vs the exe's
    // parent), so this only triggers in pathological reinstall layouts.
    if old_root == new_root {
        return;
    }

    let marker = new_root.join(".migrated-from-portable");
    if marker.exists() {
        return;
    }

    // Probe the legacy root for anything worth migrating. If all three
    // slots are absent we are looking at a fresh v2.1.0 install — write
    // nothing (not even the marker; bootstrap will seed defaults).
    let has_legacy_config = old_root.join("hook.conf.toml").is_file();
    let has_legacy_inject = old_root.join("inject").is_dir();
    let has_legacy_overrides = old_root.join("overrides").is_dir();
    if !has_legacy_config && !has_legacy_inject && !has_legacy_overrides {
        return;
    }

    info!(
        target: "hook",
        "[migrate] migrating Windows portable layout: {} -> {}",
        old_root.display(),
        new_root.display()
    );

    if let Err(e) = fs::create_dir_all(&new_root) {
        warn!(
            target: "hook",
            "[migrate] mkdir {} failed: {}; skipping",
            new_root.display(),
            e
        );
        return;
    }

    // 1. hook.conf.toml — copy only when the destination slot is empty,
    //    so a user who already saved a v2.1.0 config keeps it.
    if has_legacy_config {
        let from = old_root.join("hook.conf.toml");
        let to = new_root.join("hook.conf.toml");
        if to.exists() {
            info!(
                target: "hook",
                "[migrate] {} already exists; keeping new copy",
                to.display()
            );
        } else {
            match fs::copy(&from, &to) {
                Ok(_) => info!(target: "hook", "[migrate] copied hook.conf.toml"),
                Err(e) => warn!(
                    target: "hook",
                    "[migrate] copy hook.conf.toml failed: {}",
                    e
                ),
            }
        }
    }

    // 2. inject/ — recursive copy. The helper never overwrites individual
    //    files that already exist at the destination.
    if has_legacy_inject {
        let from = old_root.join("inject");
        let to = new_root.join("inject");
        copy_dir_recursive(&from, &to);
    }

    // 3. overrides/ — same shape as inject/. May be sizeable (it is the
    //    cached HTTP body store) but the copy stays best-effort.
    if has_legacy_overrides {
        let from = old_root.join("overrides");
        let to = new_root.join("overrides");
        copy_dir_recursive(&from, &to);
    }

    // Marker file: written last so a crash mid-copy lets the next launch
    // resume. We accept a small risk of duplicated copy work in that case
    // — the non-overwrite semantics above keep it safe.
    if let Err(e) = fs::write(&marker, b"v2.1.0 migration done\n") {
        warn!(
            target: "hook",
            "[migrate] write marker {} failed: {}; migration may rerun next launch",
            marker.display(),
            e
        );
    }
}

#[cfg(target_os = "windows")]
fn copy_dir_recursive(from: &std::path::Path, to: &std::path::Path) {
    use std::fs;
    use tracing::{info, warn};

    if let Err(e) = fs::create_dir_all(to) {
        warn!(
            target: "hook",
            "[migrate] mkdir {} failed: {}",
            to.display(),
            e
        );
        return;
    }
    let Ok(entries) = fs::read_dir(from) else {
        warn!(
            target: "hook",
            "[migrate] read_dir {} failed",
            from.display()
        );
        return;
    };
    for entry in entries.flatten() {
        let src_path = entry.path();
        let dst_path = to.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path);
        } else if src_path.is_file() {
            // Never overwrite an existing destination file — the user (or
            // a prior partial migration) may have already populated it.
            if dst_path.exists() {
                continue;
            }
            if let Err(e) = fs::copy(&src_path, &dst_path) {
                warn!(
                    target: "hook",
                    "[migrate] copy {} -> {} failed: {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                );
            }
        }
        // Symlinks / sockets / fifos are silently skipped — the legacy
        // tree never produced any (it was managed entirely by Pouch's
        // own fs::copy / fs::write calls), so encountering one is either
        // tampering or a future-shape we don't want to follow blindly.
    }
    info!(
        target: "hook",
        "[migrate] copied dir {} -> {}",
        from.display(),
        to.display()
    );
}

/// macOS / other targets: nothing to migrate. The user-data root didn't
/// change between v2.0.x and v2.1.0 on macOS.
#[cfg(not(target_os = "windows"))]
pub fn migrate_legacy_windows_data() {}
