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
use http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use http_body_util::BodyExt;
use hudsucker::{Body, HttpContext, HttpHandler, RequestOrResponse, hyper::Request};

use crate::cache_store::{self, CachePolicy, Metadata};
use crate::hook::ignore_filter;

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

/// Request meta captured in `handle_request` and consumed in
/// `handle_response` so the response side can reuse the original URL for
/// cache-key derivation, ignore-filter checks, and logging.
#[derive(Debug, Clone)]
struct RequestMeta {
    url: String,
    cache_key: String,
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
        let Some(authority) = req.uri().authority() else {
            return true;
        };
        let host_port = authority.as_str();
        if is_learned_passthrough(host_port) {
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
        // Reset per-request state up front; if any of the early-out branches
        // fires the response side has nothing to do.
        self.request_meta = None;

        // Method gate — only GETs are cacheable.
        if req.method() != http::Method::GET {
            tracing::trace!(
                target: "hook",
                "[mitm] request method={} uri={} (non-GET, passthrough)",
                req.method(),
                req.uri()
            );
            return RequestOrResponse::Request(req);
        }

        let url_str = full_url(&req);

        // Ignore-list — pouch's design intent is for these URLs to flow
        // straight through without any cache interaction.
        if ignore_filter::is_ignored(&url_str) {
            tracing::info!(
                target: "hook",
                "[mitm] PASSTHROUGH reason=ignore url={}",
                short_url(&url_str)
            );
            return RequestOrResponse::Request(req);
        }

        // Derive cache key. URLs without a host (rare on the proxy path —
        // would require a malformed CONNECT) bypass.
        let cache_key = match cache_store::cache_key_from_url(&url_str) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(
                    target: "hook",
                    "[mitm] cache_key_error url={} err={}",
                    short_url(&url_str),
                    e
                );
                return RequestOrResponse::Request(req);
            }
        };

        // Cache lookup. On HIT, short-circuit with a synthesised response;
        // hudsucker forwards it straight to the webview without touching
        // upstream. On MISS, record meta for handle_response.
        if let Some((body, meta)) = cache_store::read(&cache_key).await {
            tracing::info!(
                target: "hook",
                "[mitm] HIT key={} bytes={}",
                cache_key,
                body.len()
            );
            return RequestOrResponse::Response(build_response_from_cached(body, meta));
        }

        tracing::debug!(
            target: "hook",
            "[mitm] MISS url={} key={}",
            short_url(&url_str),
            cache_key
        );
        self.request_meta = Some(RequestMeta {
            url: url_str,
            cache_key,
        });
        RequestOrResponse::Request(req)
    }

    async fn handle_response(
        &mut self,
        _ctx: &HttpContext,
        res: Response<Body>,
    ) -> Response<Body> {
        // No meta = nothing to do (non-GET, ignored, key error, or cache HIT
        // that already short-circuited at handle_request).
        let Some(meta) = self.request_meta.take() else {
            return res;
        };

        // Decompress before collect: hudsucker doesn't auto-decode, so without
        // this the cached bytes are gzip/br wire payload while Content-Encoding
        // is stripped by FORWARD_HEADER_BLACKLIST → HIT replay renders garbage.
        let res = match hudsucker::decode_response(res) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "hook",
                    "[mitm] decode_response_failed url={} err={} (passthrough, skip cache)",
                    short_url(&meta.url),
                    e
                );
                return Response::new(Body::empty());
            }
        };

        // Only persist 2xx responses. Anything else just streams through.
        if !res.status().is_success() {
            tracing::debug!(
                target: "hook",
                "[mitm] skip_write status={} url={}",
                res.status(),
                short_url(&meta.url)
            );
            return res;
        }

        // Honour upstream Cache-Control. Skip the disk write but still
        // forward the response so the webview gets a fresh body.
        let cache_control = res
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let cache_policy = cache_store::should_cache(cache_control.as_deref());

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
        let body_bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes().to_vec(),
            Err(e) => {
                tracing::warn!(
                    target: "hook",
                    "[mitm] body_collect_failed url={} err={}",
                    short_url(&meta.url),
                    e
                );
                return Response::from_parts(parts, Body::from(Vec::<u8>::new()));
            }
        };

        match cache_policy {
            CachePolicy::Skip { reason } => {
                tracing::info!(
                    target: "hook",
                    "[mitm] STORED skip_write=cache-control:{} url={} bytes={}",
                    reason,
                    short_url(&meta.url),
                    body_bytes.len()
                );
            }
            CachePolicy::Store => {
                let sidecar_headers = headers_for_sidecar(&forwarded);
                let metadata = Metadata {
                    original_url: meta.url.clone(),
                    content_type: content_type.clone(),
                    etag,
                    last_modified,
                    saved_at: Utc::now().to_rfc3339(),
                    forwarded_headers: sidecar_headers,
                };
                if let Err(e) =
                    cache_store::write(&meta.cache_key, &body_bytes, &metadata).await
                {
                    tracing::warn!(
                        target: "hook",
                        "[mitm] cache write failed key={} err={} (serving fresh response anyway)",
                        meta.cache_key, e
                    );
                } else {
                    tracing::info!(
                        target: "hook",
                        "[mitm] STORED url={} key={} bytes={}",
                        short_url(&meta.url),
                        meta.cache_key,
                        body_bytes.len()
                    );
                }
            }
        }

        // Re-assemble the response with the buffered body so hudsucker can
        // forward it to the webview unchanged.
        Response::from_parts(parts, Body::from(body_bytes))
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
