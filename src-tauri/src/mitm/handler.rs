//! hudsucker handler that wires `cache_store` and `ignore_filter` into the
//! MITM pipeline. macOS's sole interception path (NSURLProtocol was retired
//! in v2.0). Non-GET, ignore-list, and CONNECT-passthrough requests still
//! fall through unchanged.
//!
//! Per-request state lives on `&mut self`: hudsucker's `InternalProxy::proxy`
//! clones the handler per request inside one connection (see hudsucker
//! `proxy/internal.rs:405 — self.clone().proxy(req)`), and within a single
//! `proxy()` call `handle_request` and `handle_response` see the same
//! `&mut self.http_handler`. So storing `request_meta` on the instance is
//! sound for the request → response transition.

use chrono::Utc;
use http::{header, HeaderMap, HeaderValue, Response, StatusCode};
use http_body_util::BodyExt;
use hudsucker::{hyper::Request, Body, HttpContext, HttpHandler, RequestOrResponse};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use crate::cache_store::{self, CachePolicy, Metadata};
use crate::hook::ignore_filter;
use crate::hook::protocol_bypass;
use crate::hook::websocket;

use super::is_learned_passthrough;

/// Hop-by-hop and reqwest-already-handled response headers that MUST NOT be
/// forwarded to the webview. Mirrors `hook::policy::FORWARD_HEADER_BLACKLIST`
/// — duplicated here to keep policy.rs's public surface untouched.
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

/// Headers that pass forward filtering but must NOT be persisted in the
/// sidecar (replaying them on HIT would clobber live state). Mirrors
/// `hook::policy::SIDECAR_HEADER_BLACKLIST`.
const SIDECAR_HEADER_BLACKLIST: &[&str] = &["set-cookie"];

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static LOG_HITS: OnceLock<bool> = OnceLock::new();
static SLOW_MS: OnceLock<u128> = OnceLock::new();

/// Request meta captured in `handle_request` and consumed in
/// `handle_response` so the response side can reuse the original URL for
/// cache-key derivation, ignore-filter checks, and logging.
#[derive(Debug, Clone)]
struct RequestMeta {
    id: u64,
    method: http::Method,
    url: String,
    cache_key: String,
    started_at: Instant,
    request_version: http::Version,
    handle_request_ms: u128,
}

#[derive(Clone, Default)]
pub struct PouchHandler {
    /// Set by `handle_request` when a GET passes the ignore filter and
    /// produces a usable cache key; cleared by `handle_response`. `None`
    /// means handle_response should pass through untouched (non-GET, ignored,
    /// or unparseable URL).
    request_meta: Option<RequestMeta>,
}

impl HttpHandler for PouchHandler {
    /// Called once per CONNECT request. Returning `false` makes hudsucker
    /// skip TLS termination and byte-forward the tunnel via
    /// `TcpStream::connect + io::copy_bidirectional` — the only way to
    /// support cert-pinning sites and legacy TLS 1.0/1.1 + RSA-KX/CBC
    /// servers that rustls + aws-lc-rs cannot speak. Hosts are added to
    /// the passthrough set automatically by the self-learning layer (see
    /// [`super::LearnerLayer`]) when a TLS handshake against them fails.
    async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        let started_at = Instant::now();
        let Some(authority) = req.uri().authority() else {
            let elapsed = elapsed_ms(started_at);
            tracing::trace!(
                target: "hook",
                "[mitm] connect decision=intercept host=<missing> elapsed_ms={}",
                elapsed
            );
            if is_slow_ms(elapsed) {
                tracing::warn!(
                    target: "hook",
                    "[mitm] slow_connect decision=intercept host=<missing> elapsed_ms={} threshold_ms={}",
                    elapsed,
                    slow_threshold_ms()
                );
            }
            return true;
        };
        let host_port = authority.as_str();
        let learned_passthrough = is_learned_passthrough(host_port);
        let elapsed = elapsed_ms(started_at);
        tracing::trace!(
            target: "hook",
            "[mitm] connect host={} learned_passthrough={} elapsed_ms={}",
            host_port,
            learned_passthrough,
            elapsed
        );
        if is_slow_ms(elapsed) {
            tracing::warn!(
                target: "hook",
                "[mitm] slow_connect decision={} host={} learned_passthrough={} elapsed_ms={} threshold_ms={}",
                if learned_passthrough {
                    "passthrough"
                } else {
                    "intercept"
                },
                host_port,
                learned_passthrough,
                elapsed,
                slow_threshold_ms()
            );
        }
        if learned_passthrough {
            tracing::debug!(
                target: "hook",
                "[mitm] passthrough reason=learned host={host_port}"
            );
            return false;
        }
        true
    }

    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let started_at = Instant::now();
        // Reset per-request state up front; if any of the early-out branches
        // fires the response side has nothing to do.
        self.request_meta = None;

        // Method gate — only GETs are cacheable.
        if req.method() != http::Method::GET {
            let method = req.method().clone();
            let version = req.version();
            let uri = req.uri().clone();
            tracing::trace!(
                target: "hook",
                "[mitm] request id={} decision=non_get method={} uri={} version={:?} total_ms={}",
                request_id,
                method,
                uri,
                version,
                elapsed_ms(started_at)
            );
            return RequestOrResponse::Request(req);
        }

        let method = req.method().clone();
        let version = req.version();
        let url_str = full_url(&req);

        if websocket::is_websocket_upgrade(req.headers()) {
            let total_ms = elapsed_ms(started_at);
            tracing::debug!(
                target: "hook",
                "[mitm] request id={} decision=websocket/bypass_cache method={} url={} version={:?} total_ms={}",
                request_id,
                method,
                short_url(&url_str),
                version,
                total_ms
            );
            if is_slow_ms(total_ms) {
                tracing::warn!(
                    target: "hook",
                    "[mitm] slow_request id={} decision=websocket/bypass_cache method={} url={} version={:?} total_ms={} threshold_ms={}",
                    request_id,
                    method,
                    url_str,
                    version,
                    total_ms,
                    slow_threshold_ms()
                );
            }
            return RequestOrResponse::Request(req);
        }

        if let Some(reason) = protocol_bypass::request_bypass_reason(req.headers()) {
            let total_ms = elapsed_ms(started_at);
            tracing::debug!(
                target: "hook",
                "[mitm] request id={} decision=protocol/bypass_cache reason={} method={} url={} version={:?} total_ms={}",
                request_id,
                reason.as_str(),
                method,
                short_url(&url_str),
                version,
                total_ms
            );
            if is_slow_ms(total_ms) {
                tracing::warn!(
                    target: "hook",
                    "[mitm] slow_request id={} decision=protocol/bypass_cache reason={} method={} url={} version={:?} total_ms={} threshold_ms={}",
                    request_id,
                    reason.as_str(),
                    method,
                    url_str,
                    version,
                    total_ms,
                    slow_threshold_ms()
                );
            }
            return RequestOrResponse::Request(req);
        }

        // Ignore-list — pouch's design intent is for these URLs to flow
        // straight through without any cache interaction.
        let ignore_started_at = Instant::now();
        if ignore_filter::is_ignored(&url_str) {
            let ignore_ms = elapsed_ms(ignore_started_at);
            tracing::debug!(
                target: "hook",
                "[mitm] request id={} decision=ignore method={} url={} version={:?} ignore_ms={} total_ms={}",
                request_id,
                method,
                short_url(&url_str),
                version,
                ignore_ms,
                elapsed_ms(started_at)
            );
            tracing::info!(
                target: "hook",
                "[mitm] PASSTHROUGH id={} reason=ignore url={}",
                request_id,
                short_url(&url_str)
            );
            return RequestOrResponse::Request(req);
        }
        let ignore_ms = elapsed_ms(ignore_started_at);

        // Derive cache key. URLs without a host (rare on the proxy path —
        // would require a malformed CONNECT) bypass.
        let cache_key_started_at = Instant::now();
        let cache_key = match cache_store::cache_key_from_url(&url_str) {
            Ok(k) => k,
            Err(e) => {
                let cache_key_ms = elapsed_ms(cache_key_started_at);
                tracing::warn!(
                    target: "hook",
                    "[mitm] cache_key_error id={} url={} err={} cache_key_ms={} total_ms={}",
                    request_id,
                    short_url(&url_str),
                    e,
                    cache_key_ms,
                    elapsed_ms(started_at)
                );
                return RequestOrResponse::Request(req);
            }
        };
        let cache_key_ms = elapsed_ms(cache_key_started_at);

        // Cache lookup. On HIT, short-circuit with a synthesised response;
        // hudsucker forwards it straight to the webview without touching
        // upstream. On MISS, record meta for handle_response.
        let cache_read_started_at = Instant::now();
        if let Some((body, meta)) = cache_store::read(&cache_key).await {
            let cache_read_ms = elapsed_ms(cache_read_started_at);
            let body_len = body.len();
            let response_build_started_at = Instant::now();
            let response = build_response_from_cached(body, meta);
            let response_build_ms = elapsed_ms(response_build_started_at);
            let total_ms = elapsed_ms(started_at);
            if log_hits_enabled() {
                tracing::info!(
                    target: "hook",
                    "[mitm] HIT id={} key={} bytes={}",
                    request_id,
                    cache_key,
                    body_len
                );
                tracing::debug!(
                    target: "hook",
                    "[mitm] request id={} decision=hit method={} url={} version={:?} key={} ignore_ms={} cache_key_ms={} cache_read_ms={} response_build_ms={} total_ms={}",
                    request_id,
                    method,
                    short_url(&url_str),
                    version,
                    cache_key,
                    ignore_ms,
                    cache_key_ms,
                    cache_read_ms,
                    response_build_ms,
                    total_ms
                );
            }
            if is_slow_ms(total_ms) {
                tracing::warn!(
                    target: "hook",
                    "[mitm] slow_request id={} decision=hit method={} url={} version={:?} key={} cache_read_ms={} response_build_ms={} total_ms={} bytes={} content_length={} threshold_ms={}",
                    request_id,
                    method,
                    url_str,
                    version,
                    cache_key,
                    cache_read_ms,
                    response_build_ms,
                    total_ms,
                    body_len,
                    body_len,
                    slow_threshold_ms()
                );
            }
            return RequestOrResponse::Response(response);
        }
        let cache_read_ms = elapsed_ms(cache_read_started_at);

        tracing::debug!(
            target: "hook",
            "[mitm] request id={} decision=miss method={} url={} version={:?} key={} ignore_ms={} cache_key_ms={} cache_read_ms={} total_ms={}",
            request_id,
            method,
            short_url(&url_str),
            version,
            cache_key,
            ignore_ms,
            cache_key_ms,
            cache_read_ms,
            elapsed_ms(started_at)
        );
        self.request_meta = Some(RequestMeta {
            id: request_id,
            method,
            url: url_str,
            cache_key,
            started_at,
            request_version: version,
            handle_request_ms: elapsed_ms(started_at),
        });
        RequestOrResponse::Request(req)
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        // No meta = nothing to do (non-GET, ignored, key error, or cache HIT
        // that already short-circuited at handle_request).
        let Some(meta) = self.request_meta.take() else {
            return res;
        };

        let response_started_at = Instant::now();
        let upstream_version = res.version();
        let upstream_status = res.status();

        if let Some(reason) =
            protocol_bypass::response_bypass_reason(upstream_status, res.headers())
        {
            let handle_response_ms = elapsed_ms(response_started_at);
            let total_ms = elapsed_ms(meta.started_at);
            tracing::debug!(
                target: "hook",
                "[mitm] response id={} decision=protocol/bypass_cache reason={} upstream_version={:?} status={} url={} response_ms={} total_ms={}",
                meta.id,
                reason.as_str(),
                upstream_version,
                upstream_status,
                short_url(&meta.url),
                handle_response_ms,
                total_ms
            );
            if is_slow_ms(total_ms) {
                let upstream_wait_ms = total_ms
                    .saturating_sub(meta.handle_request_ms)
                    .saturating_sub(handle_response_ms);
                tracing::warn!(
                    target: "hook",
                    "[mitm] slow_response id={} decision=protocol/bypass_cache reason={} method={} url={} key={} request_version={:?} upstream_version={:?} status={} handle_request_ms={} upstream_wait_ms={} handle_response_ms={} body_collect_ms=skipped cache_write_ms=None total_ms={} threshold_ms={}",
                    meta.id,
                    reason.as_str(),
                    meta.method,
                    meta.url,
                    meta.cache_key,
                    meta.request_version,
                    upstream_version,
                    upstream_status,
                    meta.handle_request_ms,
                    upstream_wait_ms,
                    handle_response_ms,
                    total_ms,
                    slow_threshold_ms()
                );
            }
            return res;
        }

        // Decompress before collect: hudsucker doesn't auto-decode, so without
        // this the cached bytes are gzip/br wire payload while Content-Encoding
        // is stripped by FORWARD_HEADER_BLACKLIST → HIT replay renders garbage.
        let decode_started_at = Instant::now();
        let res = match hudsucker::decode_response(res) {
            Ok(r) => r,
            Err(e) => {
                let decode_response_ms = elapsed_ms(decode_started_at);
                tracing::warn!(
                    target: "hook",
                    "[mitm] decode_response_failed id={} url={} err={} decode_response_ms={} total_ms={} (passthrough, skip cache)",
                    meta.id,
                    short_url(&meta.url),
                    e,
                    decode_response_ms,
                    elapsed_ms(meta.started_at)
                );
                return Response::new(Body::empty());
            }
        };
        let decode_response_ms = elapsed_ms(decode_started_at);

        // Only persist 2xx responses. Anything else just streams through.
        if !res.status().is_success() {
            let handle_response_ms = elapsed_ms(response_started_at);
            let total_ms = elapsed_ms(meta.started_at);
            let upstream_wait_ms = total_ms
                .saturating_sub(meta.handle_request_ms)
                .saturating_sub(handle_response_ms);
            let content_length = res
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            tracing::debug!(
                target: "hook",
                "[mitm] response id={} decision=non_success upstream_version={:?} status={} url={} decode_response_ms={} response_ms={} total_ms={}",
                meta.id,
                upstream_version,
                res.status(),
                short_url(&meta.url),
                decode_response_ms,
                handle_response_ms,
                total_ms
            );
            if is_slow_ms(total_ms) {
                tracing::warn!(
                    target: "hook",
                    "[mitm] slow_response id={} decision=miss method={} url={} key={} request_version={:?} upstream_version={:?} status={} cache_decision=non_success handle_request_ms={} upstream_wait_ms={} handle_response_ms={} decode_response_ms={} body_collect_ms=skipped cache_write_ms=None total_ms={} bytes={:?} threshold_ms={}",
                    meta.id,
                    meta.method,
                    meta.url,
                    meta.cache_key,
                    meta.request_version,
                    upstream_version,
                    upstream_status,
                    meta.handle_request_ms,
                    upstream_wait_ms,
                    handle_response_ms,
                    decode_response_ms,
                    total_ms,
                    content_length,
                    slow_threshold_ms()
                );
            }
            return res;
        }

        // Honour upstream Cache-Control. Skip the disk write but still
        // forward the response so the webview gets a fresh body.
        let cache_control_started_at = Instant::now();
        let cache_control = res
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let cache_policy = cache_store::should_cache(cache_control.as_deref());
        let cache_control_ms = elapsed_ms(cache_control_started_at);

        // Snapshot every header bit we need before consuming the body.
        let (parts, body) = res.into_parts();
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let etag = parts
            .headers
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let last_modified = parts
            .headers
            .get(header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let forwarded = forwardable_headers(&parts.headers);

        // Collect the body. On read failure we still hand back a fresh
        // (now empty) response — corrupting the webview is worse than
        // missing one cache entry.
        let body_collect_started_at = Instant::now();
        let body_bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes().to_vec(),
            Err(e) => {
                let body_collect_ms = elapsed_ms(body_collect_started_at);
                tracing::warn!(
                    target: "hook",
                    "[mitm] body_collect_failed id={} url={} err={} body_collect_ms={} total_ms={}",
                    meta.id,
                    short_url(&meta.url),
                    e,
                    body_collect_ms,
                    elapsed_ms(meta.started_at)
                );
                return Response::from_parts(parts, Body::from(Vec::<u8>::new()));
            }
        };
        let body_collect_ms = elapsed_ms(body_collect_started_at);

        let mut cache_write_ms = None;
        let mut cache_decision;
        match cache_policy {
            CachePolicy::Skip { reason } => {
                cache_decision = format!("skip:{reason}");
                tracing::info!(
                    target: "hook",
                    "[mitm] STORED id={} skip_write=cache-control:{} url={} bytes={}",
                    meta.id,
                    reason,
                    short_url(&meta.url),
                    body_bytes.len()
                );
            }
            CachePolicy::Store => {
                cache_decision = "store".to_string();
                let sidecar_headers = headers_for_sidecar(&forwarded);
                let metadata = Metadata {
                    original_url: meta.url.clone(),
                    content_type: content_type.clone(),
                    etag,
                    last_modified,
                    saved_at: Utc::now().to_rfc3339(),
                    forwarded_headers: sidecar_headers,
                };
                let cache_write_started_at = Instant::now();
                if let Err(e) = cache_store::write(&meta.cache_key, &body_bytes, &metadata).await {
                    cache_write_ms = Some(elapsed_ms(cache_write_started_at));
                    cache_decision = "store_failed".to_string();
                    tracing::warn!(
                        target: "hook",
                        "[mitm] cache write failed id={} key={} err={} cache_write_ms={} (serving fresh response anyway)",
                        meta.id,
                        meta.cache_key,
                        e,
                        cache_write_ms.unwrap_or_default()
                    );
                } else {
                    cache_write_ms = Some(elapsed_ms(cache_write_started_at));
                    tracing::info!(
                        target: "hook",
                        "[mitm] STORED id={} url={} key={} bytes={}",
                        meta.id,
                        short_url(&meta.url),
                        meta.cache_key,
                        body_bytes.len()
                    );
                }
            }
        }

        // Re-assemble the response with the buffered body so hudsucker can
        // forward it to the webview unchanged.
        let body_len = body_bytes.len();
        let response_build_started_at = Instant::now();
        let response = Response::from_parts(parts, Body::from(body_bytes));
        let response_build_ms = elapsed_ms(response_build_started_at);
        let handle_response_ms = elapsed_ms(response_started_at);
        let total_ms = elapsed_ms(meta.started_at);
        let upstream_wait_ms = total_ms
            .saturating_sub(meta.handle_request_ms)
            .saturating_sub(handle_response_ms);
        tracing::debug!(
            target: "hook",
            "[mitm] response id={} upstream_version={:?} status={} key={} cache_decision={} decode_response_ms={} cache_control_ms={} body_collect_ms={} bytes={} cache_write_ms={:?} response_build_ms={} response_ms={} total_ms={}",
            meta.id,
            upstream_version,
            upstream_status,
            meta.cache_key,
            cache_decision,
            decode_response_ms,
            cache_control_ms,
            body_collect_ms,
            body_len,
            cache_write_ms,
            response_build_ms,
            handle_response_ms,
            total_ms
        );
        if is_slow_ms(total_ms) {
            tracing::warn!(
                target: "hook",
                "[mitm] slow_response id={} decision=miss method={} url={} key={} request_version={:?} upstream_version={:?} status={} cache_decision={} handle_request_ms={} upstream_wait_ms={} handle_response_ms={} decode_response_ms={} cache_control_ms={} body_collect_ms={} cache_write_ms={:?} response_build_ms={} total_ms={} bytes={} threshold_ms={}",
                meta.id,
                meta.method,
                meta.url,
                meta.cache_key,
                meta.request_version,
                upstream_version,
                upstream_status,
                cache_decision,
                meta.handle_request_ms,
                upstream_wait_ms,
                handle_response_ms,
                decode_response_ms,
                cache_control_ms,
                body_collect_ms,
                cache_write_ms,
                response_build_ms,
                total_ms,
                body_len,
                slow_threshold_ms()
            );
        }
        response
    }
}

/// Reconstruct the absolute URL for a request. After `process_connect` →
/// `serve_stream` hudsucker rewrites `req.uri()` with full scheme + authority
/// (see hudsucker proxy/internal.rs:393-403), so for the intercepted HTTPS
/// path `req.uri().to_string()` is already a complete URL. For the plain
/// HTTP proxy path the URI also arrives absolute. We only synthesise from the
/// Host header as a defensive fallback.
fn full_url(req: &Request<Body>) -> String {
    let uri = req.uri();
    if uri.scheme().is_some() && uri.authority().is_some() {
        return uri.to_string();
    }
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    // Default to https — every non-learned-passthrough intercepted CONNECT
    // is TLS-terminated, so plain http is the unusual case.
    format!("https://{host}{path_and_query}")
}

/// First 120 chars of a URL for log readability.
fn short_url(url: &str) -> &str {
    if url.len() <= 120 {
        url
    } else {
        &url[..120]
    }
}

fn elapsed_ms(started_at: Instant) -> u128 {
    started_at.elapsed().as_millis()
}

fn slow_threshold_ms() -> u128 {
    *SLOW_MS.get_or_init(|| {
        std::env::var("POUCH_MITM_SLOW_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u128>().ok())
            .unwrap_or(1_000)
    })
}

fn is_slow_ms(elapsed_ms: u128) -> bool {
    elapsed_ms > slow_threshold_ms()
}

fn log_hits_enabled() -> bool {
    *LOG_HITS.get_or_init(|| {
        std::env::var("POUCH_MITM_LOG_HITS")
            .map(|value| {
                let value = value.trim();
                value == "1"
                    || value.eq_ignore_ascii_case("true")
                    || value.eq_ignore_ascii_case("yes")
                    || value.eq_ignore_ascii_case("on")
            })
            .unwrap_or(false)
    })
}

/// Filter response headers down to the subset we forward to the webview /
/// persist in the sidecar. Drops the hop-by-hop blacklist and Content-Type
/// (the latter is captured separately).
fn forwardable_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "content-type" || FORWARD_HEADER_BLACKLIST.contains(&lower.as_str()) {
            continue;
        }
        let Ok(value_str) = value.to_str() else {
            continue;
        };
        out.push((name.as_str().to_string(), value_str.to_string()));
    }
    out
}

/// Strip headers that must never be replayed from disk on cache HIT (live
/// state, currently just Set-Cookie).
fn headers_for_sidecar(extras: &[(String, String)]) -> Vec<(String, String)> {
    extras
        .iter()
        .filter(|(name, _)| {
            let lower = name.to_ascii_lowercase();
            !SIDECAR_HEADER_BLACKLIST.contains(&lower.as_str())
        })
        .cloned()
        .collect()
}

/// Build a hudsucker `Response<Body>` from a cache HIT. Replays the
/// `forwarded_headers` captured at MISS write time (CORS, Cache-Control,
/// X-Frame-Options, CSP, Vary, etc.) so HIT responses carry the same
/// surface as the original MISS.
fn build_response_from_cached(body: Vec<u8>, meta: Metadata) -> Response<Body> {
    let body_len = body.len();
    let mut builder = Response::builder().status(StatusCode::OK);

    if let Some(headers) = builder.headers_mut() {
        if let Some(ct) = meta.content_type.as_deref() {
            if let Ok(v) = HeaderValue::from_str(ct) {
                headers.insert(header::CONTENT_TYPE, v);
            }
        }
        if let Ok(v) = HeaderValue::from_str(&body_len.to_string()) {
            headers.insert(header::CONTENT_LENGTH, v);
        }
        for (name, value) in &meta.forwarded_headers {
            // Skip Content-Length / Content-Type if they sneaked in (can't
            // happen with the current filter chain but be defensive).
            let lower = name.to_ascii_lowercase();
            if lower == "content-length" || lower == "content-type" {
                continue;
            }
            let Ok(name_parsed) = http::HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            let Ok(value_parsed) = HeaderValue::from_str(value) else {
                continue;
            };
            // `append` keeps multi-value headers (rare on HIT replay since
            // Set-Cookie is filtered, but handles e.g. multi-value Vary).
            headers.append(name_parsed, value_parsed);
        }
    }

    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::from(Vec::<u8>::new())))
}
