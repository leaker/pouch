//! Small cross-cutting utilities.
//!
//! Currently contains [`pretty_path`] — a *lexical* path normaliser used by
//! every log call site that prints a filesystem path. Logs would otherwise
//! show paths like `/Users/.../pouch/src-tauri/../inject/global.js`, which
//! the user has to mentally collapse to `/Users/.../pouch/inject/global.js`
//! when scanning startup output.
//!
//! Why "lexical" and not [`std::fs::canonicalize`]:
//! - We log paths *before* I/O — sometimes for files that don't exist (e.g.
//!   non-resolved `hook.config.json` candidates).
//! - We don't want symlinks resolved: the user-visible `inject/` directory
//!   stays exactly as the user named it on disk.
//! - Lexical normalisation never touches the filesystem and never fails.

use std::path::{Component, Path, PathBuf};

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

    #[cfg(unix)]
    #[test]
    fn collapses_parent_in_middle() {
        assert_eq!(lexical_normalize(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
    }

    #[cfg(unix)]
    #[test]
    fn collapses_trailing_parent() {
        assert_eq!(lexical_normalize(Path::new("/a/b/c/..")), PathBuf::from("/a/b"));
    }

    #[cfg(unix)]
    #[test]
    fn drops_current_dir_segments() {
        assert_eq!(lexical_normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
    }

    #[cfg(unix)]
    #[test]
    fn preserves_leading_parent_in_relative() {
        // Relative paths whose `..` escapes the start must keep the `..` —
        // we have no anchor to resolve them against at lexical level.
        assert_eq!(lexical_normalize(Path::new("../foo")), PathBuf::from("../foo"));
    }

    #[cfg(unix)]
    #[test]
    fn collapses_real_world_inject_path() {
        // The exact shape the user reported: src-tauri/.. cancels out.
        assert_eq!(
            lexical_normalize(Path::new("/Users/x/pouch/src-tauri/../inject/global.js")),
            PathBuf::from("/Users/x/pouch/inject/global.js")
        );
    }

    #[cfg(unix)]
    #[test]
    fn cannot_pop_past_root() {
        // `..` against `/` stays at `/` (matches `path/filepath.Clean`).
        assert_eq!(lexical_normalize(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(lexical_normalize(Path::new("/../..")), PathBuf::from("/"));
    }

    #[cfg(unix)]
    #[test]
    fn empty_input_becomes_dot() {
        assert_eq!(lexical_normalize(Path::new("")), PathBuf::from("."));
    }

    #[cfg(unix)]
    #[test]
    fn relative_parent_after_segment_pops_segment_only() {
        // `a/b/../../c` should collapse to `c`, NOT to `../c`.
        assert_eq!(lexical_normalize(Path::new("a/b/../../c")), PathBuf::from("c"));
    }

    #[cfg(unix)]
    #[test]
    fn relative_parent_then_segment_then_parent() {
        // `../a/..` keeps the leading `..` (no anchor) and folds the inner pair.
        assert_eq!(lexical_normalize(Path::new("../a/..")), PathBuf::from(".."));
    }

    #[cfg(unix)]
    #[test]
    fn pretty_path_is_absolute_for_relative_input() {
        let cwd = std::env::current_dir().expect("cwd");
        let pretty = pretty_path(Path::new("inject/global.js"));
        // Either the cwd-joined absolute (happy path) or the original input
        // (current_dir() fail path); never an intermediate state.
        let expected = lexical_normalize(&cwd.join("inject/global.js"));
        assert_eq!(pretty, expected);
    }
}
