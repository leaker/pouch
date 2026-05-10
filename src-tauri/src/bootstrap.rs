//! First-run bootstrap for the macOS user-data directory.
//!
//! When the user double-clicks `Pouch.app` for the first time, the per-user
//! data root at `~/Library/Application Support/Pouch/` does not yet exist —
//! the .app bundle is read-only and ships a sample copy of `hook.conf.toml`
//! and `inject/*.js` inside `Contents/Resources/sample/` (configured via the
//! `bundle.resources` map in `tauri.conf.json`).
//!
//! [`bootstrap_macos_user_dir`] copies that sample tree out to
//! `~/Library/Application Support/Pouch/` exactly once: the *very first* time
//! the user runs the app. On every subsequent launch the directory already
//! exists and we leave the user's edits alone — bootstrap is idempotent on
//! non-empty existence (no merge, no overwrite, no resurrection).
//!
//! Failures are logged at WARN and never abort startup: pouch's resolver
//! falls through to an empty `startup_urls` (which the launch path then
//! handles by prompting the user via NSAlert) and an empty `inject/` result
//! if the user-data directory ends up empty, so a missing or unreadable
//! sample bundle just looks like "ran with no config" — the same fallback
//! behaviour we already use for power-loss / disk-full corner cases.
//!
//! This module is **macOS-only**. Windows release builds use a "portable"
//! layout with `hook.conf.toml` + `inject/` next to the binary, and dev
//! builds (`debug_assertions`) read straight from the repo root — neither
//! needs bootstrapping.

#[cfg(all(target_os = "macos", not(debug_assertions)))]
pub use macos::bootstrap_macos_user_dir;

/// No-op shim for the dev / non-macOS build configurations. Lets `lib.rs`
/// call `bootstrap::bootstrap_macos_user_dir(...)` unconditionally without a
/// matching cfg gate at the call site.
#[cfg(not(all(target_os = "macos", not(debug_assertions))))]
pub fn bootstrap_macos_user_dir(_app: &tauri::AppHandle) {
    // Dev / Windows: nothing to do.
}

#[cfg(all(target_os = "macos", not(debug_assertions)))]
mod macos {
    use std::fs;
    use std::path::Path;

    use tauri::{AppHandle, Manager};
    use tracing::{info, warn};

    use crate::util::{macos_app_support_dir, pretty_path};

    /// Sub-directory inside `$RESOURCE` (i.e. `Pouch.app/Contents/Resources/`)
    /// that holds the shipped defaults for `hook.conf.toml` and `inject/`.
    /// Must match the destination paths in `tauri.conf.json` →
    /// `bundle.resources`.
    const SAMPLE_DIR: &str = "sample";

    /// Seed the bundled `sample/` tree into
    /// `~/Library/Application Support/Pouch/` with **file-level**
    /// idempotency: each shipped artifact (`hook.conf.toml`, `inject/`)
    /// is copied only when its destination is missing. Existing user files
    /// are never overwritten; deleted files are restored on next launch.
    ///
    /// File-level (rather than directory-level) idempotency matters because
    /// other early-startup code paths — e.g. `cache_store::cache_root()` —
    /// may have already created `user_dir` (and `user_dir/overrides/`) by
    /// the time this runs, so checking "does `user_dir` exist?" is not a
    /// reliable "have we bootstrapped?" signal.
    ///
    /// Logs everything at INFO/WARN and never panics. Errors are warned and
    /// swallowed because pouch can still boot with no config — the launch
    /// path then prompts the user via NSAlert when `startup_urls` resolves
    /// empty.
    pub fn bootstrap_macos_user_dir(app: &AppHandle) {
        let Some(user_dir) = macos_app_support_dir() else {
            warn!(
                target: "hook",
                "[bootstrap] HOME is not set; skipping macOS user-dir bootstrap"
            );
            return;
        };

        // `create_dir_all` is idempotent: a no-op if `user_dir` (or any
        // ancestor) already exists, which is the common case once
        // `cache_store` has run.
        if let Err(e) = fs::create_dir_all(&user_dir) {
            warn!(
                target: "hook",
                "[bootstrap] failed to create {}: {}",
                pretty_path(&user_dir).display(),
                e
            );
            return;
        }

        // Resolve $RESOURCE/<SAMPLE_DIR>. `resource_dir()` is the canonical
        // Tauri v2 way to get `Pouch.app/Contents/Resources/`; it lines up
        // with the destinations declared in `bundle.resources`.
        let resource_dir = match app.path().resource_dir() {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    target: "hook",
                    "[bootstrap] resource_dir() unavailable, skipping sample copy: {}",
                    e
                );
                return;
            }
        };
        let sample_root = resource_dir.join(SAMPLE_DIR);
        if !sample_root.is_dir() {
            warn!(
                target: "hook",
                "[bootstrap] sample dir not found inside .app: {} (expected from bundle.resources)",
                pretty_path(&sample_root).display()
            );
            return;
        }

        // 1. hook.conf.toml — copy only when the destination is missing.
        //    If the user has edited or kept this file we leave it alone.
        //    Note: a stale `hook.config.json` from a pre-v2.0.0 install may
        //    coexist in the same directory; we deliberately do NOT delete or
        //    rewrite it (config.rs surfaces a one-line WARN pointing the user
        //    at the new filename).
        let dst_config = user_dir.join("hook.conf.toml");
        if !dst_config.exists() {
            let src_config = sample_root.join("hook.conf.toml");
            if src_config.is_file() {
                match fs::copy(&src_config, &dst_config) {
                    Ok(_) => info!(
                        target: "hook",
                        "[bootstrap] seeded {}",
                        pretty_path(&dst_config).display()
                    ),
                    Err(e) => warn!(
                        target: "hook",
                        "[bootstrap] failed to copy {} -> {}: {}",
                        pretty_path(&src_config).display(),
                        pretty_path(&dst_config).display(),
                        e
                    ),
                }
            } else {
                warn!(
                    target: "hook",
                    "[bootstrap] sample hook.conf.toml not found at {}",
                    pretty_path(&src_config).display()
                );
            }
        }

        // 2. inject/ — copy only when the destination directory is missing.
        //    A user who deleted individual `inject/*.js` files but kept the
        //    directory keeps that state (no partial repopulation).
        let dst_inject = user_dir.join("inject");
        if !dst_inject.exists() {
            let src_inject = sample_root.join("inject");
            if src_inject.is_dir() {
                match copy_dir_recursive(&src_inject, &dst_inject) {
                    Ok(n) => info!(
                        target: "hook",
                        "[bootstrap] seeded {} ({} file(s))",
                        pretty_path(&dst_inject).display(),
                        n
                    ),
                    Err(e) => warn!(
                        target: "hook",
                        "[bootstrap] failed to seed {}: {}",
                        pretty_path(&dst_inject).display(),
                        e
                    ),
                }
            } else {
                warn!(
                    target: "hook",
                    "[bootstrap] sample inject/ not found at {}",
                    pretty_path(&src_inject).display()
                );
            }
        }
    }

    /// Copy `src` recursively into `dst`. Returns the number of files copied.
    ///
    /// Hand-rolled with `read_dir` rather than pulling in `walkdir` — the
    /// sample tree is one shallow directory of `*.js` files, so the
    /// generality (and dependency) of walkdir is not worth the build-time
    /// cost.
    fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<u64> {
        fs::create_dir_all(dst)?;
        let mut count: u64 = 0;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if file_type.is_dir() {
                count += copy_dir_recursive(&from, &to)?;
            } else if file_type.is_file() {
                fs::copy(&from, &to)?;
                count += 1;
            }
            // Ignore symlinks / sockets / fifos in the sample tree —
            // shipping any of those would already be a bundling mistake.
        }
        Ok(count)
    }

    #[cfg(test)]
    mod tests {
        use super::copy_dir_recursive;
        use std::fs;
        use tempfile::tempdir;

        #[test]
        fn copy_dir_recursive_copies_nested_files() {
            let src = tempdir().expect("src tempdir");
            let dst = tempdir().expect("dst tempdir");
            fs::create_dir_all(src.path().join("inject")).unwrap();
            fs::write(src.path().join("hook.conf.toml"), b"").unwrap();
            fs::write(src.path().join("inject/global.js"), b"// global").unwrap();
            fs::write(src.path().join("inject/leelib.js"), b"// leelib").unwrap();

            let n = copy_dir_recursive(src.path(), &dst.path().join("out")).unwrap();
            assert_eq!(n, 3, "expected three files copied");
            assert!(dst.path().join("out/hook.conf.toml").is_file());
            assert!(dst.path().join("out/inject/global.js").is_file());
            assert!(dst.path().join("out/inject/leelib.js").is_file());
        }

        #[test]
        fn copy_dir_recursive_creates_destination() {
            let src = tempdir().unwrap();
            fs::write(src.path().join("a.txt"), b"a").unwrap();
            let dst = tempdir().unwrap();
            // Destination doesn't exist yet — copy_dir_recursive should mkdir.
            let target = dst.path().join("nested/missing/leaf");
            let n = copy_dir_recursive(src.path(), &target).unwrap();
            assert_eq!(n, 1);
            assert!(target.join("a.txt").is_file());
        }
    }
}
