//! Filesystem-backed cache store for the `cache://` scheme.
//!
//! Layout:
//! - `<cache_root>/<host>/<pathname>` — body bytes
//! - `<cache_root>/<host>/<pathname>.meta.json` — sidecar metadata
//!
//! Writes are atomic (temp file + persist). Reads return `None` if the body
//! or sidecar is missing.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced by [`cache_key_from_url`].
#[derive(Debug, Error)]
pub enum CacheKeyError {
    #[error("invalid URL: {0}")]
    Parse(#[from] url::ParseError),
    #[error("URL is missing a host component")]
    MissingHost,
}

/// Derive the cache key from an arbitrary http/https URL using the same rules
/// the front-end SDK applies in `rewriteUrl` (host + pathname, `/` → `index.html`,
/// query and fragment dropped). Scheme is intentionally ignored — `http://` and
/// `https://` variants of the same host+path share a cache entry.
pub fn cache_key_from_url(url: &str) -> Result<String, CacheKeyError> {
    let parsed = url::Url::parse(url)?;
    let host = parsed.host_str().ok_or(CacheKeyError::MissingHost)?;
    let raw_path = parsed.path();
    let pathname = if raw_path.is_empty() {
        "/index.html".to_string()
    } else if raw_path.ends_with('/') {
        format!("{raw_path}index.html")
    } else {
        raw_path.to_string()
    };
    Ok(format!("{host}{pathname}"))
}

/// Sidecar metadata recorded alongside each cached file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    pub original_url: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// ISO 8601 timestamp of when the entry was written.
    pub saved_at: String,
}

static CACHE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Returns the absolute path of the cache root (`<repo>/overrides`).
///
/// Honours the `CACHE_ROOT_OVERRIDE` env var (used by tests). The directory is
/// created on first call, and its absolute path is logged once at INFO.
pub fn cache_root() -> &'static Path {
    // Tests need to redirect the cache root per-test; honour the override every
    // call rather than caching it (the OnceLock is for the production path).
    if let Ok(override_path) = std::env::var("CACHE_ROOT_OVERRIDE") {
        // Leak to obtain a 'static reference; tests run with a tempdir per test
        // and only set the override inside their own scope, so this is acceptable.
        let path: &'static Path = Box::leak(PathBuf::from(override_path).into_boxed_path());
        let _ = std::fs::create_dir_all(path);
        return path;
    }

    CACHE_ROOT
        .get_or_init(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let candidate = manifest_dir.join("..").join("overrides");
            if let Err(e) = std::fs::create_dir_all(&candidate) {
                tracing::warn!(
                    "[hook] failed to create cache root {}: {}",
                    crate::util::pretty_path(&candidate).display(),
                    e
                );
            }
            let resolved = candidate
                .canonicalize()
                .unwrap_or_else(|_| crate::util::pretty_path(&candidate));
            tracing::info!("[hook] cache root: {}", resolved.display());
            resolved
        })
        .as_path()
}

/// Decode a percent-encoded `cache_key` into a filesystem-safe path segment.
fn decode_key(cache_key: &str) -> String {
    percent_decode_str(cache_key)
        .decode_utf8_lossy()
        .into_owned()
}

fn body_path(cache_key: &str) -> PathBuf {
    cache_root().join(decode_key(cache_key))
}

fn sidecar_path(body: &Path) -> PathBuf {
    let mut p = body.as_os_str().to_owned();
    p.push(".meta.json");
    PathBuf::from(p)
}

/// Read a cached entry. Returns `None` if either the body or sidecar is
/// missing or corrupt.
pub async fn read(cache_key: &str) -> Option<(Vec<u8>, Metadata)> {
    let body = body_path(cache_key);
    let sidecar = sidecar_path(&body);

    let body_bytes = tokio::fs::read(&body).await.ok()?;
    let meta_bytes = tokio::fs::read(&sidecar).await.ok()?;
    let meta: Metadata = serde_json::from_slice(&meta_bytes).ok()?;
    Some((body_bytes, meta))
}

/// Atomically write a cached entry and its sidecar.
pub async fn write(cache_key: &str, body: &[u8], meta: &Metadata) -> std::io::Result<()> {
    let body_target = body_path(cache_key);
    let sidecar_target = sidecar_path(&body_target);

    if let Some(parent) = body_target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let meta_json = serde_json::to_vec_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let body_owned = body.to_vec();

    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let parent = body_target.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache path has no parent")
        })?;

        let mut body_tmp = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(body_tmp.as_file_mut(), &body_owned)?;
        body_tmp.as_file_mut().sync_all()?;
        body_tmp.persist(&body_target).map_err(|e| e.error)?;

        let mut meta_tmp = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(meta_tmp.as_file_mut(), &meta_json)?;
        meta_tmp.as_file_mut().sync_all()?;
        meta_tmp.persist(&sidecar_target).map_err(|e| e.error)?;

        Ok(())
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Delete the cached body and sidecar for a single cache_key. Missing files
/// are not an error.
pub async fn clear_url(cache_key: &str) -> std::io::Result<()> {
    let body = body_path(cache_key);
    let sidecar = sidecar_path(&body);
    ignore_not_found(tokio::fs::remove_file(&body).await)?;
    ignore_not_found(tokio::fs::remove_file(&sidecar).await)?;
    Ok(())
}

/// Recursively delete the directory for a host.
pub async fn clear_host(host: &str) -> std::io::Result<()> {
    let target = cache_root().join(decode_key(host));
    ignore_not_found(tokio::fs::remove_dir_all(&target).await)
}

/// Delete every entry inside the cache root (preserving the root directory
/// and any `.gitkeep` file).
pub async fn clear_all() -> std::io::Result<()> {
    let root = cache_root().to_path_buf();
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(rd) => rd,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name() == ".gitkeep" {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            ignore_not_found(tokio::fs::remove_dir_all(&path).await)?;
        } else {
            ignore_not_found(tokio::fs::remove_file(&path).await)?;
        }
    }
    Ok(())
}

fn ignore_not_found(r: std::io::Result<()>) -> std::io::Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn meta_for(url: &str) -> Metadata {
        Metadata {
            original_url: url.to_string(),
            content_type: Some("text/plain".to_string()),
            etag: Some("\"abc\"".to_string()),
            last_modified: Some("Wed, 01 Jan 2025 00:00:00 GMT".to_string()),
            saved_at: "2026-05-06T12:00:00Z".to_string(),
        }
    }

    /// Run a closure with `CACHE_ROOT_OVERRIDE` pointing at a fresh tempdir.
    async fn with_temp_root<F, Fut, R>(f: F) -> R
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let tmp = TempDir::new().expect("tempdir");
        // Safe in single-threaded tokio test (`flavor = current_thread` default).
        std::env::set_var("CACHE_ROOT_OVERRIDE", tmp.path());
        let result = f(tmp.path().to_path_buf()).await;
        std::env::remove_var("CACHE_ROOT_OVERRIDE");
        drop(tmp);
        result
    }

    /// One large serial test covering all cache_store behaviours so we don't
    /// need to depend on `serial_test` to coordinate the env var override.
    #[tokio::test]
    async fn cache_store_full_suite() {
        // 1) write/read round-trip preserves bytes and metadata exactly.
        with_temp_root(|root| async move {
            let body = b"hello world";
            let meta = meta_for("https://example.com/a.txt");
            write("example.com/a.txt", body, &meta).await.unwrap();

            assert!(root.join("example.com/a.txt").exists());
            assert!(root.join("example.com/a.txt.meta.json").exists());

            let (got_body, got_meta) = read("example.com/a.txt").await.unwrap();
            assert_eq!(got_body, body);
            assert_eq!(got_meta, meta);
        })
        .await;

        // 2) read of a missing key returns None.
        with_temp_root(|_root| async move {
            assert!(read("example.com/missing").await.is_none());
        })
        .await;

        // 2b) read returns None when sidecar is missing even if body exists.
        with_temp_root(|root| async move {
            let body_target = root.join("example.com/orphan.txt");
            tokio::fs::create_dir_all(body_target.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(&body_target, b"x").await.unwrap();
            assert!(read("example.com/orphan.txt").await.is_none());
        })
        .await;

        // 3) clear_url removes both body and sidecar; subsequent read is None.
        with_temp_root(|root| async move {
            let meta = meta_for("https://example.com/b.txt");
            write("example.com/b.txt", b"data", &meta).await.unwrap();
            assert!(root.join("example.com/b.txt").exists());
            clear_url("example.com/b.txt").await.unwrap();
            assert!(!root.join("example.com/b.txt").exists());
            assert!(!root.join("example.com/b.txt.meta.json").exists());
            assert!(read("example.com/b.txt").await.is_none());
            // calling again on a missing entry is a no-op.
            clear_url("example.com/b.txt").await.unwrap();
        })
        .await;

        // 4) clear_host removes the entire host subtree.
        with_temp_root(|root| async move {
            let m = meta_for("https://example.com/x");
            write("example.com/x.txt", b"x", &m).await.unwrap();
            write("example.com/sub/y.txt", b"y", &m).await.unwrap();
            write("other.com/z.txt", b"z", &m).await.unwrap();
            clear_host("example.com").await.unwrap();
            assert!(!root.join("example.com").exists());
            assert!(root.join("other.com/z.txt").exists());
            // missing host is a no-op.
            clear_host("never-existed.com").await.unwrap();
        })
        .await;

        // 5) clear_all removes everything but keeps the root and .gitkeep.
        with_temp_root(|root| async move {
            tokio::fs::write(root.join(".gitkeep"), b"").await.unwrap();
            let m = meta_for("https://example.com/x");
            write("example.com/x.txt", b"x", &m).await.unwrap();
            write("foo.com/y.txt", b"y", &m).await.unwrap();
            clear_all().await.unwrap();
            assert!(root.exists());
            assert!(root.join(".gitkeep").exists());
            assert!(!root.join("example.com").exists());
            assert!(!root.join("foo.com").exists());
        })
        .await;

        // 6) cache_key_from_url maps standard https URL → host + path.
        assert_eq!(
            cache_key_from_url("https://example.com/static/a.png").unwrap(),
            "example.com/static/a.png"
        );

        // 7) http URL behaves identically — scheme does not influence the key.
        assert_eq!(
            cache_key_from_url("http://example.com/static/a.png").unwrap(),
            "example.com/static/a.png"
        );

        // 8) trailing slash → append index.html (matches SDK + url_resolver).
        assert_eq!(
            cache_key_from_url("https://example.com/").unwrap(),
            "example.com/index.html"
        );
        assert_eq!(
            cache_key_from_url("https://example.com/path/").unwrap(),
            "example.com/path/index.html"
        );

        // 9) query and fragment are dropped from the cache key.
        assert_eq!(
            cache_key_from_url("https://api.example.com/v1/data?x=1&y=2#frag").unwrap(),
            "api.example.com/v1/data"
        );

        // 10) malformed URL or missing host → error.
        assert!(matches!(
            cache_key_from_url("not a url"),
            Err(CacheKeyError::Parse(_))
        ));
        assert!(matches!(
            cache_key_from_url("file:///etc/hosts"),
            Err(CacheKeyError::MissingHost)
        ));

        // 11) percent-encoded cache_key is decoded before touching disk.
        with_temp_root(|root| async move {
            let m = meta_for("https://example.com/p%20q.png");
            write("example.com/p%20q.png", b"img", &m).await.unwrap();
            let on_disk = root.join("example.com/p q.png");
            assert!(on_disk.exists(), "decoded path should exist: {:?}", on_disk);
            assert!(!root.join("example.com/p%20q.png").exists());
            let (body, _) = read("example.com/p%20q.png").await.unwrap();
            assert_eq!(body, b"img");
        })
        .await;
    }
}
