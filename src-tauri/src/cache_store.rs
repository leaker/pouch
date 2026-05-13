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

/// Derive the cache key from an arbitrary http/https URL.
///
/// - **No query**: `<host>/<path>` — `path` is preserved verbatim (with `/`
///   → `index.html` so directory-style URLs stay readable as files on disk).
/// - **With query**: `<host>/<path>.__qs_<hash>__<ext>` — the original path
///   (including extension) is preserved byte-for-byte as a prefix, and a
///   short hex hash of the raw query is appended as a flat filename suffix.
///   The original extension is **repeated** at the very end so editors,
///   IDEs, and Quick Look still recognise the file type. URLs that differ
///   only by query string get distinct cache entries (otherwise dynamic
///   query params like `?time=…&sign=…` collide and a stale signed token
///   gets replayed). We hash rather than embed the raw query because Windows
///   file systems reject `?`, `*`, `:`, `<`, `>`, `|` in path names.
/// - **Fragments** (`#frag`) are intentionally dropped — per HTTP semantics
///   fragments are client-side only and never reach the server, so they MUST
///   NOT influence the cache identity (Chromium, Firefox, and the HTTP cache
///   spec all behave this way).
/// - **Scheme** (`http://` vs `https://`) is intentionally ignored — variants
///   of the same host+path share a cache entry.
/// - **Query order** is **not** normalised — `?a=1&b=2` and `?b=2&a=1` map to
///   different keys. This matches the conservative behaviour of every common
///   HTTP cache (CDNs, browsers): re-ordering query params can change which
///   resource the upstream serves, so treating them as distinct is safer.
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

    // `pathname` is guaranteed to start with '/', so the format strings below
    // produce `host/path` without double slashes.
    match parsed.query() {
        Some(q) if !q.is_empty() => {
            let hash = short_query_hash(q);
            let base = format!("{host}{pathname}");
            Ok(append_qs_marker(&base, &hash))
        }
        _ => Ok(format!("{host}{pathname}")),
    }
}

/// Append the query-string marker `.__qs_<hash>__` to the last path segment
/// of `base`, then **repeat** the original extension so editors, IDEs, and
/// Quick Look still recognise the file type.
///
/// Rules:
/// - If the last segment has a real extension (i.e. `.` exists at byte index
///   `> 0` within the filename — leading-dot files like `.htaccess` are
///   treated as extensionless), the extension is duplicated after the
///   marker: `foo.js` → `foo.js.__qs_<hash>__.js`.
/// - Otherwise (no `.` or leading-dot file), the marker is appended without
///   any trailing extension: `.htaccess` → `.htaccess.__qs_<hash>__`.
/// - For double extensions only the **last** one is repeated:
///   `foo.tar.gz` → `foo.tar.gz.__qs_<hash>__.gz`.
fn append_qs_marker(base: &str, hash: &str) -> String {
    let (dir, filename) = match base.rfind('/') {
        Some(idx) => (&base[..=idx], &base[idx + 1..]),
        None => ("", base),
    };

    if filename.is_empty() {
        // Defensive: trailing-slash paths are normalised to `/index.html`
        // upstream, so we should never land here in practice.
        return format!("{base}.__qs_{hash}__");
    }

    match filename.rfind('.') {
        // `idx > 0` excludes leading-dot files (`.htaccess`) from being
        // treated as having an extension. `rfind('.')` returns a byte index;
        // since `.` is ASCII it is always at a char boundary even when the
        // filename contains multi-byte characters elsewhere.
        Some(idx) if idx > 0 => {
            let ext = &filename[idx..]; // includes the leading '.'
            format!("{dir}{filename}.__qs_{hash}__{ext}")
        }
        _ => format!("{dir}{filename}.__qs_{hash}__"),
    }
}

/// 32-bit hex digest of the raw query string, derived from std's
/// `DefaultHasher` (a SipHash-2-4 variant in current rust). 8 hex chars =
/// 32-bit hash space — collision probability is ~1.2e-5 at 100 distinct keys
/// per (host, path), which is acceptable for our single-user cache (a
/// collision returns a stale body and is no worse than the pre-fix bug).
///
/// We deliberately avoid pulling in a cryptographic crate (`sha2` etc.) —
/// this hash is a cache-namespacing key, not a security boundary.
fn short_query_hash(query: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut h = DefaultHasher::new();
    query.hash(&mut h);
    let full = h.finish();
    // Take the low 32 bits as 8 hex chars; `format!("{:08x}", x as u32)` is
    // explicit about the truncation and guarantees a fixed-width suffix.
    format!("{:08x}", full as u32)
}

/// Decision returned by [`should_cache`] for a given response Cache-Control
/// header. Used by upstream callers (in `policy.rs`) to skip the disk write
/// for opt-out responses while still serving the freshly-fetched bytes to
/// the webview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Persist the body + sidecar to disk. The default for a normal 200
    /// response with no Cache-Control restrictions.
    Store,
    /// Skip the disk write and serve the response straight to the webview.
    /// Logged once per response so the user can see why a particular URL
    /// never appears in `overrides/`.
    Skip { reason: &'static str },
}

/// Inspect a response's Cache-Control header and decide whether to persist
/// the body. We honour the conservative subset of [RFC 7234] that pouch can
/// implement without a freshness clock:
///
/// - `no-store` — never write to disk, never read from disk
/// - `no-cache` — read requires revalidation; we don't support conditional
///   GET (see `http_fetcher::STRIPPED_REQUEST_HEADERS`), so the safe choice
///   is to skip the write entirely
/// - `private` — pouch is single-user but we treat it as opt-out anyway
/// - `max-age=0`, `s-maxage=0` — already-stale on arrival, never persist
///
/// Not yet implemented (cost > benefit for pouch's offline-cache use case):
/// - `Pragma: no-cache` (HTTP/1.0 compat) — modern servers ship Cache-Control
/// - `Expires: 0` / past Expires — needs date parsing + a clock
/// - `max-age` countdown / staleness tracking — pouch is "cache forever
///   until the user wipes overrides/", not a freshness-aware cache
///
/// [RFC 7234]: https://datatracker.ietf.org/doc/html/rfc7234#section-5.2
pub fn should_cache(cache_control: Option<&str>) -> CachePolicy {
    let Some(raw) = cache_control else {
        return CachePolicy::Store;
    };

    // Tokenise on commas (top-level Cache-Control directives are
    // comma-separated; the trailing `=value` is optional).
    for token in raw.split(',') {
        let directive = token.trim();
        if directive.is_empty() {
            continue;
        }
        let lower_directive = directive.to_ascii_lowercase();
        let (name, value) = match lower_directive.split_once('=') {
            Some((n, v)) => (n.trim(), Some(v.trim().trim_matches('"'))),
            None => (lower_directive.as_str(), None),
        };

        match name {
            "no-store" => return CachePolicy::Skip { reason: "no-store" },
            "no-cache" => return CachePolicy::Skip { reason: "no-cache" },
            "private" => return CachePolicy::Skip { reason: "private" },
            "max-age" | "s-maxage" => {
                if matches!(value, Some("0")) {
                    return CachePolicy::Skip {
                        reason: if name == "max-age" {
                            "max-age=0"
                        } else {
                            "s-maxage=0"
                        },
                    };
                }
            }
            _ => {}
        }
    }
    CachePolicy::Store
}

/// Sidecar metadata recorded alongside each cached file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Metadata {
    pub original_url: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// ISO 8601 timestamp of when the entry was written.
    pub saved_at: String,

    /// Headers from the upstream response that were forwarded to the webview
    /// at cache write time. Excludes `Set-Cookie` (replay would leak stale
    /// session cookies) and the hop-by-hop blacklist already filtered by
    /// `forward_response_headers`. These are replayed verbatim on cache HIT
    /// so CORS / Cache-Control / X-Frame-Options / CSP / Vary stay consistent
    /// between the first MISS response and subsequent HITs.
    ///
    /// `#[serde(default)]` keeps old sidecars (written before this field
    /// existed) deserialising cleanly — they fall back to an empty vec, which
    /// preserves the pre-fix HIT behaviour for already-cached entries until
    /// the user clears overrides/.
    #[serde(default)]
    pub forwarded_headers: Vec<(String, String)>,
}

static CACHE_ROOT: OnceLock<PathBuf> = OnceLock::new();

#[cfg(test)]
pub(crate) static CACHE_ROOT_OVERRIDE_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

/// Returns the absolute path of the cache root.
///
/// Resolution chain (see [`crate::util::user_data_dir`]):
/// - dev: `<CARGO_MANIFEST_DIR>/../overrides`.
/// - macOS prod: `~/Library/Application Support/Pouch/overrides`.
/// - Windows prod: `<exe parent>/overrides`.
///
/// Honours the `CACHE_ROOT_OVERRIDE` env var (used by tests). The directory
/// is created on first call, and its absolute path is logged once at INFO.
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
            // Falls back to `./overrides` (cwd-relative) in the extremely
            // unlikely case that none of the resolver tiers can give us a
            // path — keeps cache_root() infallible like the rest of pouch's
            // boot path.
            let candidate = crate::util::user_data_dir(crate::util::UserDataKind::Overrides)
                .unwrap_or_else(|| PathBuf::from("overrides"));
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

    let (body_bytes, meta_bytes) = tokio::join!(
        tokio::fs::read(&body),
        tokio::fs::read(&sidecar),
    );
    let body_bytes = body_bytes.ok()?;
    let meta_bytes = meta_bytes.ok()?;
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
            forwarded_headers: Vec::new(),
        }
    }

    /// Run a closure with `CACHE_ROOT_OVERRIDE` pointing at a fresh tempdir.
    async fn with_temp_root<F, Fut, R>(f: F) -> R
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let tmp = TempDir::new().expect("tempdir");
        let _guard = CACHE_ROOT_OVERRIDE_LOCK.lock().await;
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

        // 9) URLs with no query map to the bare `host/path` (back-compat
        //    with cache files written before query-aware keys existed).
        assert_eq!(
            cache_key_from_url("https://x.com/a").unwrap(),
            "x.com/a"
        );

        // 9a) No-query keys are unaffected (path preserved verbatim, including
        //     extension and nested segments).
        assert_eq!(
            cache_key_from_url("https://x.com/foo.js").unwrap(),
            "x.com/foo.js"
        );
        assert_eq!(
            cache_key_from_url("https://x.com/path/bar.css").unwrap(),
            "x.com/path/bar.css"
        );

        // 9b) With a query, the marker `.__qs_<hash>__` is appended as a
        //     flat filename suffix and the original extension is repeated
        //     so editors / Quick Look still recognise the file type.
        //     Distinct queries → distinct hashes (the H1 fix). The exact
        //     hash bytes come from std DefaultHasher and aren't stable
        //     across rust releases — assert structure, not bytes.
        let k1 = cache_key_from_url("https://x.com/foo.js?v=1").unwrap();
        let k2 = cache_key_from_url("https://x.com/foo.js?v=2").unwrap();
        assert!(k1.starts_with("x.com/foo.js.__qs_"), "k1 was {k1}");
        assert!(k1.ends_with("__.js"), "k1 was {k1}");
        assert!(k2.starts_with("x.com/foo.js.__qs_"), "k2 was {k2}");
        assert!(k2.ends_with("__.js"), "k2 was {k2}");
        assert_ne!(k1, k2, "different queries must yield different keys");

        // 9c) Nested path is preserved verbatim as the prefix, marker is
        //     still appended to the last segment.
        let k_nested = cache_key_from_url("https://x.com/path/bar.css?v=1").unwrap();
        assert!(
            k_nested.starts_with("x.com/path/bar.css.__qs_"),
            "k_nested was {k_nested}"
        );
        assert!(k_nested.ends_with("__.css"), "k_nested was {k_nested}");

        // 9c-i) Leading-dot file (`.htaccess`) has no real extension, so
        //       no extension is repeated after the marker.
        let k_ht = cache_key_from_url("https://x.com/.htaccess?v=1").unwrap();
        assert!(
            k_ht.starts_with("x.com/.htaccess.__qs_"),
            "k_ht was {k_ht}"
        );
        assert!(k_ht.ends_with("__"), "k_ht was {k_ht}");
        assert!(
            !k_ht.ends_with(".htaccess"),
            "leading-dot must not be repeated as ext: {k_ht}"
        );

        // 9c-ii) Double extension: only the **last** segment is repeated.
        let k_tgz = cache_key_from_url("https://x.com/foo.tar.gz?v=1").unwrap();
        assert!(
            k_tgz.starts_with("x.com/foo.tar.gz.__qs_"),
            "k_tgz was {k_tgz}"
        );
        assert!(k_tgz.ends_with("__.gz"), "k_tgz was {k_tgz}");

        // 9d) Fragment is NOT part of the cache key (HTTP semantics).
        assert_eq!(
            cache_key_from_url("https://x.com/foo.js?b=c").unwrap(),
            cache_key_from_url("https://x.com/foo.js?b=c#frag").unwrap(),
        );

        // 9e) Empty query (`?`) is treated as no query.
        assert_eq!(
            cache_key_from_url("https://x.com/a?").unwrap(),
            "x.com/a"
        );

        // 9f) Trailing-slash → index.html upstream rule still applies; the
        //     marker is appended to the synthesised `index.html` filename.
        let k_root = cache_key_from_url("https://x.com/?q=1").unwrap();
        assert!(
            k_root.starts_with("x.com/index.html.__qs_"),
            "k_root was {k_root}"
        );
        assert!(k_root.ends_with("__.html"), "k_root was {k_root}");

        // 9g) Cache-Control decisions cover the subset we honour.
        assert_eq!(should_cache(None), CachePolicy::Store);
        assert_eq!(should_cache(Some("public")), CachePolicy::Store);
        assert!(matches!(
            should_cache(Some("no-store")),
            CachePolicy::Skip { reason: "no-store" }
        ));
        assert!(matches!(
            should_cache(Some("private, max-age=300")),
            CachePolicy::Skip { reason: "private" }
        ));
        assert!(matches!(
            should_cache(Some("no-cache")),
            CachePolicy::Skip { reason: "no-cache" }
        ));
        assert!(matches!(
            should_cache(Some("max-age=0")),
            CachePolicy::Skip { reason: "max-age=0" }
        ));
        assert!(matches!(
            should_cache(Some("s-maxage=0")),
            CachePolicy::Skip { reason: "s-maxage=0" }
        ));
        // Non-zero max-age is still cacheable (we don't track freshness).
        assert_eq!(should_cache(Some("max-age=300")), CachePolicy::Store);
        // Mixed-case directives.
        assert!(matches!(
            should_cache(Some("No-Store")),
            CachePolicy::Skip { reason: "no-store" }
        ));

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

        // 12) forwarded_headers round-trip: write a sidecar containing CORS /
        //     Cache-Control / Vary entries and verify read returns them as a
        //     Vec<(String, String)> with order preserved. This is the cache
        //     HIT replay path's source of truth.
        with_temp_root(|_root| async move {
            let mut m = meta_for("https://api.example.com/data");
            m.forwarded_headers = vec![
                (
                    "Access-Control-Allow-Origin".to_string(),
                    "https://app.example.com".to_string(),
                ),
                ("Cache-Control".to_string(), "max-age=3600".to_string()),
                ("Vary".to_string(), "Accept-Encoding".to_string()),
            ];
            write("api.example.com/data", b"{}", &m).await.unwrap();
            let (_body, got) = read("api.example.com/data").await.unwrap();
            assert_eq!(got.forwarded_headers, m.forwarded_headers);
        })
        .await;

        // 13) Backward-compat: a sidecar written before the
        //     `forwarded_headers` field existed must still deserialise. The
        //     `#[serde(default)]` attribute makes the field optional and
        //     falls back to an empty Vec — old entries on disk continue to
        //     work, just without HIT-path header replay until they're
        //     refreshed.
        with_temp_root(|root| async move {
            let body_target = root.join("legacy.example.com/old.txt");
            tokio::fs::create_dir_all(body_target.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(&body_target, b"legacy").await.unwrap();
            // Hand-crafted JSON missing `forwarded_headers` — mirrors a
            // sidecar produced by an older pouch build.
            let legacy_json = br#"{
                "original_url": "https://legacy.example.com/old.txt",
                "content_type": "text/plain",
                "etag": null,
                "last_modified": null,
                "saved_at": "2026-05-01T00:00:00Z"
            }"#;
            tokio::fs::write(
                root.join("legacy.example.com/old.txt.meta.json"),
                legacy_json,
            )
            .await
            .unwrap();
            let (body, meta) = read("legacy.example.com/old.txt").await.unwrap();
            assert_eq!(body, b"legacy");
            assert!(
                meta.forwarded_headers.is_empty(),
                "legacy sidecar must default to empty forwarded_headers, got {:?}",
                meta.forwarded_headers
            );
            assert_eq!(meta.content_type.as_deref(), Some("text/plain"));
        })
        .await;
    }
}
