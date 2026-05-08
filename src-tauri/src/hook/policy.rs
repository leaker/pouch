//! Platform-agnostic request policy: decides whether a single intercepted
//! request should be served from the local cache, fetched from upstream and
//! cached, fetched without caching (ignore-list), or surfaced as a network
//! error to the webview.
//!
//! This module contains zero platform-specific code; the Windows and macOS
//! interceptors (phase H/I) call `evaluate` and translate the returned
//! [`Decision`] into platform responses.
//!
//! # Why only two decision variants
//!
//! The macOS path uses `NSURLProtocol`, where once `-startLoading` is invoked
//! the protocol implementation **must** produce a response — there is no way
//! to fall back to the webview's default networking mid-load. To keep the
//! Windows and macOS platform layers symmetrical, the policy layer hides this
//! constraint by always returning either a complete response (`Respond`) or a
//! "give up, surface the error" outcome (`Bypass`). Ignore-list matches and
//! cache misses both end up as `Respond`; only an unrecoverable upstream
//! failure (timeout, network error, non-2xx, body read error, missing host)
//! becomes `Bypass`.
//!
//! Method gating (only GET is cacheable) lives in the platform layer — non-GET
//! requests must short-circuit before reaching here.

use std::time::Duration;

use chrono::Utc;
use tracing::{debug, info, warn};

use crate::cache_store::{self, CachePolicy, Metadata};
use crate::hook::ignore_filter;
use crate::http_fetcher;

/// Maximum time we wait for an upstream GET before falling back to bypass.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// The outcome of evaluating a single intercepted GET request.
#[derive(Debug)]
pub enum Decision {
    /// The platform layer should reply with `body`, `content_type` (if any),
    /// and a curated `extra_headers` list to forward verbatim. Covers cache
    /// hits, cache misses (after a successful fetch + write), and ignore-list
    /// URLs (after a successful fetch with no write).
    ///
    /// `extra_headers` carries upstream response headers minus the hop-by-hop
    /// blacklist (see [`forward_response_headers`]). Multi-value headers like
    /// `Set-Cookie` appear as multiple entries with the same name. The
    /// platform layer is responsible for translating this list into its
    /// native response object (`NSHTTPURLResponse` headerFields dict on
    /// macOS, the `\r\n`-joined HSTRING accepted by
    /// `CreateWebResourceResponse` on Windows).
    Respond {
        body: Vec<u8>,
        content_type: Option<String>,
        extra_headers: Vec<(String, String)>,
    },
    /// Could not produce a useful response (upstream error, non-2xx, timeout,
    /// body read error, missing host on the URL). The platform layer should
    /// surface a network error to the webview — policy is not in the business
    /// of synthesising 5xx responses.
    Bypass,
}

/// Hop-by-hop and reqwest-already-handled response headers that MUST NOT be
/// forwarded to the webview. Matched case-insensitively.
///
/// - **Hop-by-hop** (HTTP/1.1 RFC 7230 §6.1): meaningful only between the
///   reqwest client and the upstream server, never to be repeated to the
///   webview.
/// - `Content-Length`: the platform layer (NSHTTPURLResponse / WebView2)
///   computes this from the body it receives. Forwarding the upstream value
///   would mismatch when reqwest auto-decompresses the body.
/// - `Content-Encoding`: reqwest's gzip/brotli/deflate features
///   auto-decompress the body before we see it, so the bytes we hand back
///   are plain text — keeping the upstream encoding header would tell the
///   webview to decompress a second time and corrupt the response.
const FORWARD_HEADER_BLACKLIST: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
    "trailer",
    "content-length",
    "content-encoding",
];

/// Extract the headers we want to forward from a reqwest response. The
/// upstream `Content-Type` is returned separately (for the platform layer to
/// stamp into its first-class response slot); everything else minus the
/// blacklist becomes `extra_headers`. Multi-value headers (notably
/// `Set-Cookie`) emit one entry per value so platform layers can decide
/// whether to join with `\r\n` (Windows) or special-case at delivery time
/// (macOS NSHTTPURLResponse).
fn forward_response_headers(
    headers: &http::HeaderMap,
) -> (Option<String>, Vec<(String, String)>) {
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "content-type" {
            // Captured separately above.
            continue;
        }
        if FORWARD_HEADER_BLACKLIST.contains(&lower.as_str()) {
            continue;
        }
        let Ok(value_str) = value.to_str() else {
            // Non-ASCII header values can't round-trip through any of the
            // platform header APIs we have; drop with no log spam.
            continue;
        };
        out.push((name.as_str().to_string(), value_str.to_string()));
    }
    (content_type, out)
}

/// Evaluate an intercepted GET request and decide how to serve it.
///
/// `request_headers` are forwarded to the upstream request on cache miss /
/// ignore-list passthrough (e.g. `Range`, `Accept-Language`, `Accept-Encoding`).
/// Callers should NOT pass cookie / auth headers from the webview here — see
/// the "Cookie isolation" caveat in `tasks/todo.md` §14.
pub async fn evaluate(url: &str, request_headers: &http::HeaderMap) -> Decision {
    // 1. Ignore-list: do not consult the cache, do not write the cache, but
    //    still fetch the response upstream so the platform layer can serve it
    //    (NSURLProtocol semantics require us to produce a body once we've
    //    accepted the load — see module-level docs).
    if ignore_filter::is_ignored(url) {
        info!(target: "hook", "PASSTHROUGH reason=ignore url={}", url);
        return fetch_only(url, request_headers).await;
    }

    // 2. Derive a cache key. URLs without a host (file://, data:, etc.) bypass.
    let cache_key = match cache_store::cache_key_from_url(url) {
        Ok(k) => k,
        Err(e) => {
            warn!(target: "hook", "BYPASS reason=cache_key_error url={} err={}", url, e);
            return Decision::Bypass;
        }
    };

    // 3. Cache lookup.
    if let Some((body, meta)) = cache_store::read(&cache_key).await {
        info!(target: "hook", "HIT key={} bytes={}", cache_key, body.len());
        return Decision::Respond {
            body,
            content_type: meta.content_type,
            // Cached entries don't preserve the original response headers
            // beyond `content_type`/`etag`/`last_modified` (sidecar schema)
            // so we have nothing else to forward on a HIT. Set-Cookie /
            // Cache-Control reflowing only happens on MISS / ignore.
            extra_headers: Vec::new(),
        };
    }

    // 4. Cache miss — fetch upstream and write through.
    debug!(target: "hook", "MISS key={} fetching upstream", cache_key);
    fetch_and_cache(url, &cache_key, request_headers).await
}

/// Fetch a URL upstream without consulting or writing the cache.
///
/// Used for ignore-list URLs: we still need a body to hand back to the
/// platform layer (see `Decision` docs), but we never persist it.
async fn fetch_only(url: &str, request_headers: &http::HeaderMap) -> Decision {
    let response = match run_fetch(url, request_headers).await {
        Some(r) => r,
        None => return Decision::Bypass,
    };

    let (content_type, extra_headers) = forward_response_headers(response.headers());

    let body_bytes = match response.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => {
            warn!(target: "hook", "BYPASS reason=body_read_error url={} err={}", url, e);
            return Decision::Bypass;
        }
    };

    Decision::Respond {
        body: body_bytes,
        content_type,
        extra_headers,
    }
}

/// Fetch a URL upstream and write it through to the cache before returning.
async fn fetch_and_cache(
    url: &str,
    cache_key: &str,
    request_headers: &http::HeaderMap,
) -> Decision {
    let response = match run_fetch(url, request_headers).await {
        Some(r) => r,
        None => return Decision::Bypass,
    };

    // Snapshot the response headers we care about before consuming the body.
    let headers = response.headers().clone();
    let (content_type, extra_headers) = forward_response_headers(&headers);
    let etag = headers
        .get(http::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = headers
        .get(http::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let cache_control = headers
        .get(http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok());
    let cache_policy = cache_store::should_cache(cache_control);

    let body_bytes = match response.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => {
            warn!(target: "hook", "BYPASS reason=body_read_error url={} err={}", url, e);
            return Decision::Bypass;
        }
    };

    // Write through unless the upstream Cache-Control says otherwise. A
    // skipped write still serves the freshly-fetched body to the webview —
    // we just don't persist it. Persistence failures (after we decided to
    // store) are logged but not fatal.
    let meta = Metadata {
        original_url: url.to_string(),
        content_type: content_type.clone(),
        etag,
        last_modified,
        saved_at: Utc::now().to_rfc3339(),
    };
    match cache_policy {
        CachePolicy::Skip { reason } => {
            info!(
                target: "hook",
                "MISS key={} bytes={} ct={:?} skip_write=cache-control:{}",
                cache_key,
                body_bytes.len(),
                content_type,
                reason
            );
        }
        CachePolicy::Store => {
            if let Err(e) = cache_store::write(cache_key, &body_bytes, &meta).await {
                warn!(
                    target: "hook",
                    "MISS write_failed key={} err={} (serving fresh response anyway)",
                    cache_key, e
                );
            } else {
                info!(
                    target: "hook",
                    "MISS key={} bytes={} ct={:?}",
                    cache_key,
                    body_bytes.len(),
                    content_type
                );
            }
        }
    }

    Decision::Respond {
        body: body_bytes,
        content_type,
        extra_headers,
    }
}

/// Drive a single upstream GET with timeout + 2xx-only acceptance. Returns
/// `None` (and logs a `BYPASS reason=…` line) on any failure mode that the
/// platform layer should surface as a network error.
async fn run_fetch(url: &str, request_headers: &http::HeaderMap) -> Option<reqwest::Response> {
    let fetch_fut = http_fetcher::fetch(url, request_headers);
    let response = match tokio::time::timeout(UPSTREAM_TIMEOUT, fetch_fut).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            warn!(target: "hook", "BYPASS reason=upstream_error url={} err={}", url, e);
            return None;
        }
        Err(_) => {
            warn!(
                target: "hook",
                "BYPASS reason=timeout url={} after={:?}",
                url, UPSTREAM_TIMEOUT
            );
            return None;
        }
    };

    let status = response.status();
    if !status.is_success() {
        warn!(
            target: "hook",
            "BYPASS reason=non_2xx url={} status={}",
            url, status
        );
        return None;
    }

    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderMap;

    /// `forward_response_headers` keeps semantically meaningful upstream
    /// headers (Set-Cookie, Cache-Control, ETag) and drops the hop-by-hop /
    /// reqwest-already-handled blacklist (Connection, Content-Encoding,
    /// Content-Length). Multi-value Set-Cookie produces multiple entries.
    #[test]
    fn forward_response_headers_filters_blacklist() {
        let mut h = HeaderMap::new();
        h.insert("content-type", "text/html; charset=utf-8".parse().unwrap());
        h.append("set-cookie", "a=1; Path=/".parse().unwrap());
        h.append("set-cookie", "b=2; Secure".parse().unwrap());
        h.insert("cache-control", "no-store".parse().unwrap());
        h.insert("etag", "\"abc\"".parse().unwrap());
        h.insert("connection", "keep-alive".parse().unwrap());
        h.insert("content-encoding", "gzip".parse().unwrap());
        h.insert("content-length", "1234".parse().unwrap());
        h.insert("transfer-encoding", "chunked".parse().unwrap());

        let (ct, extras) = forward_response_headers(&h);
        assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));

        // Easy-to-read shape check: collect names lower-case for assertions.
        let lc_names: Vec<String> = extras
            .iter()
            .map(|(n, _)| n.to_ascii_lowercase())
            .collect();

        // Kept.
        assert!(lc_names.iter().any(|n| n == "cache-control"));
        assert!(lc_names.iter().any(|n| n == "etag"));
        // Multiple Set-Cookie preserved as separate entries.
        let cookie_count = lc_names.iter().filter(|n| n == &"set-cookie").count();
        assert_eq!(cookie_count, 2);

        // Stripped.
        assert!(!lc_names.iter().any(|n| n == "connection"));
        assert!(!lc_names.iter().any(|n| n == "content-encoding"));
        assert!(!lc_names.iter().any(|n| n == "content-length"));
        assert!(!lc_names.iter().any(|n| n == "transfer-encoding"));
        // Content-Type is reported via the dedicated return value, not
        // duplicated in extras.
        assert!(!lc_names.iter().any(|n| n == "content-type"));
    }

    /// Empty header map → no content-type, empty extras (no panics).
    #[test]
    fn forward_response_headers_handles_empty_map() {
        let h = HeaderMap::new();
        let (ct, extras) = forward_response_headers(&h);
        assert!(ct.is_none());
        assert!(extras.is_empty());
    }
}
