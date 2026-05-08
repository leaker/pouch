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

use crate::cache_store::{self, Metadata};
use crate::hook::ignore_filter;
use crate::http_fetcher;

/// Maximum time we wait for an upstream GET before falling back to bypass.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// The outcome of evaluating a single intercepted GET request.
#[derive(Debug)]
pub enum Decision {
    /// The platform layer should reply with `body` and `content_type` (if any).
    /// Covers cache hits, cache misses (after a successful fetch + write), and
    /// ignore-list URLs (after a successful fetch with no write).
    Respond {
        body: Vec<u8>,
        content_type: Option<String>,
    },
    /// Could not produce a useful response (upstream error, non-2xx, timeout,
    /// body read error, missing host on the URL). The platform layer should
    /// surface a network error to the webview — policy is not in the business
    /// of synthesising 5xx responses.
    Bypass,
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

    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

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
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let etag = headers
        .get(http::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = headers
        .get(http::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let body_bytes = match response.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => {
            warn!(target: "hook", "BYPASS reason=body_read_error url={} err={}", url, e);
            return Decision::Bypass;
        }
    };

    // Write through. Persistence failure is logged but not fatal — we still
    // hand the freshly-fetched body to the webview so the page renders.
    let meta = Metadata {
        original_url: url.to_string(),
        content_type: content_type.clone(),
        etag,
        last_modified,
        saved_at: Utc::now().to_rfc3339(),
    };
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

    Decision::Respond {
        body: body_bytes,
        content_type,
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
