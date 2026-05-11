//! Small cross-cutting utilities.
//!
//! Contents:
//!
//! - [`pretty_path`] — a *lexical* path normaliser used by every log call
//!   site that prints a filesystem path. Logs would otherwise show paths
//!   like `/Users/.../pouch/src-tauri/../inject/global.js`, which the user
//!   has to mentally collapse to `/Users/.../pouch/inject/global.js` when
//!   scanning startup output.
//!
//! - [`user_data_dir`] / [`UserDataKind`] — three-tier path resolver for the
//!   user-visible `hook.conf.toml`, `inject/`, and `overrides/` data.
//!   The resolution rules are documented on each function.
//!
//! Why "lexical" and not [`std::fs::canonicalize`]:
//! - We log paths *before* I/O — sometimes for files that don't exist (e.g.
//!   non-resolved `hook.conf.toml` candidates).
//! - We don't want symlinks resolved: the user-visible `inject/` directory
//!   stays exactly as the user named it on disk.
//! - Lexical normalisation never touches the filesystem and never fails.

use std::path::{Component, Path, PathBuf};

/// Default window inner size used when the user has not pinned an explicit
/// `{ width, height }` in `hook.conf.toml`.
///
/// Used by `Default` / `Maximized` / `Fullscreen` / fallback branches in
/// `lib.rs::setup` and `dialog::open_extra_window` (each entry of
/// `startup_urls` plus the Cmd+N runtime new-window dialog), so an
/// unmaximize / un-fullscreen gesture, and any extra window's first paint,
/// restores the window to a sensible 1280x960. Without this, wry/Tauri falls
/// back to the platform default of 800x600 which is too cramped for the kind
/// of dashboards pouch typically targets.
///
/// Single source of truth: both `lib.rs` (main window) and `dialog.rs`
/// (extra windows) import these constants from here, never duplicated.
pub const DEFAULT_WINDOW_WIDTH: f64 = 1280.0;
pub const DEFAULT_WINDOW_HEIGHT: f64 = 960.0;

/// Identifies which user-data slot a path resolver call is for. Used by
/// [`user_data_dir`] / [`user_data_path`] only to log "for which slot" when
/// helpful — the resolution rules do **not** branch on the variant.
#[derive(Debug, Clone, Copy)]
pub enum UserDataKind {
    /// `inject/` directory containing `*.js` rules.
    Inject,
    /// `overrides/` directory used as the cache root.
    Overrides,
    /// `hook.conf.toml` (a file, not a directory — see
    /// [`user_data_path`]).
    Config,
}

impl UserDataKind {
    /// Filesystem name relative to the parent dev/prod root. Returns
    /// `"hook.conf.toml"` for [`UserDataKind::Config`] and the
    /// directory name for the other two variants.
    pub fn name(self) -> &'static str {
        match self {
            UserDataKind::Inject => "inject",
            UserDataKind::Overrides => "overrides",
            UserDataKind::Config => "hook.conf.toml",
        }
    }
}

/// macOS only: the per-app user-data root.
///
/// Returns `~/Library/Application Support/Pouch` (the canonical macOS
/// per-user data directory for a non-sandboxed app), built by joining
/// `$HOME` + `Library` + `Application Support` + `Pouch`. The two-segment
/// `Library/Application Support` join (rather than a single string) keeps
/// the directory name's literal space character intact and uses the OS's
/// path separator everywhere.
///
/// Returns `None` if `$HOME` is missing — pouch then falls through to the
/// portable `<exe parent>` layout, just like Windows.
#[cfg(target_os = "macos")]
pub fn macos_app_support_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Pouch"),
    )
}

/// Windows only: per-user data root at `%APPDATA%\Pouch\`.
///
/// Matches the canonical Roaming AppData layout (KNOWNFOLDERID
/// `FOLDERID_RoamingAppData`) — the equivalent of macOS's
/// `~/Library/Application Support/Pouch/`. v2.1.0 promoted Windows from a
/// "portable" exe-sibling layout to this location so user data survives
/// scoop / MSI upgrades and lives outside `Program Files` (where the
/// installer's working tree is wiped on uninstall).
///
/// Returns `None` only if `%APPDATA%` is unset — extremely rare on real
/// Windows sessions but possible in stripped-down CI / SYSTEM contexts;
/// the [`user_data_path`] chain falls through to the legacy portable
/// layout when that happens.
///
/// Uses `std::env::var_os` rather than the `dirs` crate so we don't pull
/// in another dependency for what is effectively a single env-var read.
#[cfg(target_os = "windows")]
pub fn windows_appdata_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("Pouch"))
}

/// Windows only: the legacy v2.0.x portable root (the directory holding
/// `pouch.exe`).
///
/// Pre-v2.1.0 release builds stored `hook.conf.toml`, `inject/`, and
/// `overrides/` next to the binary. This helper is kept exclusively for
/// the one-shot AppData migration ([`crate::migrate`]) and for the
/// portable fallback inside [`user_data_path`] when `%APPDATA%` is
/// unset; **new code should not use it**.
#[cfg(target_os = "windows")]
pub fn windows_legacy_portable_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
}

/// macOS only: open `~/Library/Application Support/Pouch/` in Finder.
///
/// Used by the menubar entry (`View → Reveal Pouch Folder in Finder`,
/// Cmd+Shift+O) and the titlebar accessory button — both call this same
/// helper so the behaviour stays in sync. We `open <dir>` rather than
/// `open -R <file>` because the user wants to land *inside* the folder
/// (so they can immediately drop in / inspect `inject/`, `overrides/`,
/// `hook.conf.toml`), not "show the folder selected in its parent".
///
/// Creates the directory first if it doesn't exist yet (e.g. first launch
/// where `bootstrap_macos_user_dir` somehow hasn't populated it) — Finder
/// errors out on missing paths, and we'd rather just handle that case
/// silently than surface a useless modal.
#[cfg(target_os = "macos")]
pub fn reveal_pouch_folder() -> std::io::Result<()> {
    let path = macos_app_support_dir().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "$HOME unavailable")
    })?;
    if !path.exists() {
        std::fs::create_dir_all(&path)?;
    }
    std::process::Command::new("open").arg(&path).spawn()?;
    Ok(())
}

/// Resolve the runtime path for a user-data slot.
///
/// Resolution chain (first hit wins, the same shape used by config / inject /
/// cache_store before this helper landed):
///
/// 1. **Dev mode** (`debug_assertions`): `<CARGO_MANIFEST_DIR>/../<name>` —
///    i.e. the in-repo directory next to `src-tauri/`. Always returned;
///    the file/directory may or may not exist on disk yet. **Unchanged on
///    Windows** so `bun run dev` does not scribble into `%APPDATA%`.
/// 2. **macOS prod** (`cfg(target_os = "macos")`, `not(debug_assertions)`):
///    `~/Library/Application Support/Pouch/<name>`. Falls through to step 4
///    if `$HOME` is unset (very unusual — but pouch should still boot).
/// 3. **Windows prod** (`cfg(target_os = "windows")`, `not(debug_assertions)`):
///    `%APPDATA%\Pouch\<name>` (Roaming AppData). v2.1.0 promoted this
///    from the v2.0.x portable layout — see `migrate::migrate_legacy_windows_data`
///    for the one-shot move. Falls through to step 4 if `%APPDATA%` is
///    unset (stripped-down CI / SYSTEM context).
/// 4. **Portable fallback** (both platforms, last resort):
///    `<current_exe parent>/<name>`. If `current_exe()` itself fails we
///    return `None` and the caller treats it as "not found".
///
/// The returned path is **lexically only** — it is not guaranteed to exist.
/// Callers that care use `Path::is_dir()` / `Path::exists()` themselves.
///
/// `kind` selects the leaf name (`"inject"`, `"overrides"`, or
/// `"hook.conf.toml"`); the resolution rules are otherwise identical
/// across all three.
pub fn user_data_path(kind: UserDataKind) -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        // Step 1: dev. CARGO_MANIFEST_DIR is baked at compile time (always
        // available — every cargo target has a Cargo.toml).
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        return Some(manifest_dir.join("..").join(kind.name()));
    }

    // Release builds. Per-platform canonical user-data root first, then a
    // shared portable fallback (exe sibling) so pouch still boots if the
    // OS-level env var (HOME / APPDATA) is unset.
    #[cfg(target_os = "macos")]
    {
        if let Some(root) = macos_app_support_dir() {
            return Some(root.join(kind.name()));
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(root) = windows_appdata_dir() {
            return Some(root.join(kind.name()));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return Some(parent.join(kind.name()));
        }
    }
    None
}

/// Resolve a *directory* slot only. Convenience wrapper around
/// [`user_data_path`] for callers that already know the slot is a dir.
pub fn user_data_dir(kind: UserDataKind) -> Option<PathBuf> {
    user_data_path(kind)
}

/// Make a path log-friendly: absolutise relative paths against the current
/// working directory (best-effort) and lexically collapse `.` / `..` segments.
///
/// Symlinks are left intact (unlike [`std::fs::canonicalize`]) and the path
/// is *not* required to exist. If `current_dir()` is unavailable (extremely
/// rare — process has been chrooted away from a deleted cwd, etc.) we fall
/// through to lexical normalisation of the original input.
pub fn pretty_path(p: &Path) -> PathBuf {
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    lexical_normalize(&absolute)
}

/// Collapse `.` and `..` segments without touching the filesystem.
///
/// Cross-platform safe: `Component::Prefix` (Windows drive letter / UNC) and
/// `Component::RootDir` are pushed verbatim and never popped past. A `..` at
/// the start of a *relative* path is preserved (no anchor to resolve it
/// against); a `..` against an absolute root is dropped (matching Go's
/// `path/filepath.Clean`).
pub(crate) fn lexical_normalize(p: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    // Tracks whether the last pushed component is a real path segment we are
    // allowed to pop. Prefix/RootDir/leading `..` are anchors and must NOT
    // disappear when a later `..` is processed.
    let mut popped_segments: usize = 0;
    let mut has_root = false;

    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                result.push(comp.as_os_str());
                has_root = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if popped_segments > 0 {
                    // We have a real segment to drop.
                    result.pop();
                    popped_segments -= 1;
                } else if !has_root {
                    // Relative path with no segment to pop — preserve `..`
                    // because we have no anchor to resolve it against.
                    result.push("..");
                }
                // If has_root and no segments to pop: this is `/..`; per
                // POSIX `..` of `/` is `/`, so silently drop the `..`.
            }
            Component::Normal(seg) => {
                result.push(seg);
                popped_segments += 1;
            }
        }
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn collapses_parent_in_middle() {
        assert_eq!(lexical_normalize(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn collapses_trailing_parent() {
        assert_eq!(lexical_normalize(Path::new("/a/b/c/..")), PathBuf::from("/a/b"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn drops_current_dir_segments() {
        assert_eq!(lexical_normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn preserves_leading_parent_in_relative() {
        // Relative paths whose `..` escapes the start must keep the `..` —
        // we have no anchor to resolve them against at lexical level.
        assert_eq!(lexical_normalize(Path::new("../foo")), PathBuf::from("../foo"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn collapses_real_world_inject_path() {
        // The exact shape the user reported: src-tauri/.. cancels out.
        assert_eq!(
            lexical_normalize(Path::new("/Users/x/pouch/src-tauri/../inject/global.js")),
            PathBuf::from("/Users/x/pouch/inject/global.js")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cannot_pop_past_root() {
        // `..` against `/` stays at `/` (matches `path/filepath.Clean`).
        assert_eq!(lexical_normalize(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(lexical_normalize(Path::new("/../..")), PathBuf::from("/"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn empty_input_becomes_dot() {
        assert_eq!(lexical_normalize(Path::new("")), PathBuf::from("."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn relative_parent_after_segment_pops_segment_only() {
        // `a/b/../../c` should collapse to `c`, NOT to `../c`.
        assert_eq!(lexical_normalize(Path::new("a/b/../../c")), PathBuf::from("c"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn relative_parent_then_segment_then_parent() {
        // `../a/..` keeps the leading `..` (no anchor) and folds the inner pair.
        assert_eq!(lexical_normalize(Path::new("../a/..")), PathBuf::from(".."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pretty_path_is_absolute_for_relative_input() {
        let cwd = std::env::current_dir().expect("cwd");
        let pretty = pretty_path(Path::new("inject/global.js"));
        // Either the cwd-joined absolute (happy path) or the original input
        // (current_dir() fail path); never an intermediate state.
        let expected = lexical_normalize(&cwd.join("inject/global.js"));
        assert_eq!(pretty, expected);
    }

    #[test]
    fn user_data_kind_name_matches_slot() {
        assert_eq!(UserDataKind::Inject.name(), "inject");
        assert_eq!(UserDataKind::Overrides.name(), "overrides");
        assert_eq!(UserDataKind::Config.name(), "hook.conf.toml");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn user_data_path_dev_points_into_repo_root() {
        // Dev build: every slot resolves to <CARGO_MANIFEST_DIR>/../<name>.
        // We don't canonicalize — the path is only required to exist
        // lexically, so we compare byte-for-byte against what config.rs /
        // inject.rs / cache_store.rs constructed before the helper landed.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for (kind, name) in [
            (UserDataKind::Inject, "inject"),
            (UserDataKind::Overrides, "overrides"),
            (UserDataKind::Config, "hook.conf.toml"),
        ] {
            let got = user_data_path(kind).expect("dev path is always Some");
            assert_eq!(got, manifest_dir.join("..").join(name));
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn user_data_dir_matches_user_data_path() {
        // user_data_dir is just a convenience alias; the contract is "same
        // result as user_data_path".
        for kind in [UserDataKind::Inject, UserDataKind::Overrides, UserDataKind::Config] {
            assert_eq!(user_data_dir(kind), user_data_path(kind));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_app_support_dir_behaviour() {
        // Run both HOME-mutating macos checks inside one test to avoid the
        // env-var race that two parallel #[test]s would have under cargo's
        // default `--test-threads=N` scheduling. We restore HOME at the end.
        let prev = std::env::var_os("HOME");

        // Case 1: $HOME set → returns a path whose tail is exactly
        // `Library/Application Support/Pouch`, with the space-bearing
        // directory kept as a single OS path component.
        std::env::set_var("HOME", "/Users/testuser");
        let dir = macos_app_support_dir().expect("HOME is set");
        let s = dir.to_string_lossy();
        assert!(s.ends_with("/Library/Application Support/Pouch"), "got {s}");
        let comps: Vec<_> = dir
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => n.to_str(),
                _ => None,
            })
            .collect();
        assert!(
            comps.contains(&"Application Support"),
            "expected 'Application Support' as a single component, got {comps:?}"
        );

        // Case 2: $HOME unset → None (caller falls back to portable layout).
        std::env::remove_var("HOME");
        assert!(macos_app_support_dir().is_none());

        // Case 3: reveal_pouch_folder propagates the missing-HOME case as a
        // NotFound io::Error rather than silently spawning `open` against a
        // bogus path. (The happy path actually invokes `open`, which would
        // launch Finder mid-test — we deliberately do NOT exercise that.)
        let err =
            reveal_pouch_folder().expect_err("reveal_pouch_folder must fail without HOME");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

        // Restore.
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}
