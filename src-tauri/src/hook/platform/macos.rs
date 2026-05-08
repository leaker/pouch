//! macOS native interception via `NSURLProtocol` + a private
//! `WKBrowsingContextController.registerSchemeForCustomProtocol:` selector.
//!
//! # Design
//!
//! 1. `install_global()` is called once at app startup (before any webview is
//!    created). It registers our `HookURLProtocol` subclass with
//!    `NSURLProtocol`, then asks `WKBrowsingContextController` (via the
//!    private selector — see [decisions/2026-05-07-direction-pivot.md]) to
//!    route `https` and `http` requests through the registered protocol
//!    chain.
//!
//! 2. Once a webview issues an HTTPS request, `+canInitWithRequest:` returns
//!    YES (modulo the `X-Hook-Bypass` marker, see below) and our `-startLoading`
//!    is invoked on the main thread.
//!
//! 3. `-startLoading` cannot block (the webview's main runloop is the caller).
//!    We snapshot the request URL + headers, then `tokio::spawn` an async task
//!    on a dedicated multi-thread runtime to drive
//!    [`crate::hook::policy::evaluate`]. When the task finishes, we hop back
//!    onto the main dispatch queue (via `dispatch2`) before invoking any
//!    `NSURLProtocolClient` callbacks — those callbacks are documented to
//!    require the runloop that received the load, which on macOS is always
//!    the main thread for webview-initiated loads.
//!
//! 4. To keep policy's reqwest client from being hooked back through this
//!    same protocol, every request that crosses
//!    [`crate::http_fetcher::fetch`] gets stamped with a benign marker header
//!    (`X-Hook-Bypass: 1`); our `+canInitWithRequest:` returns NO on requests
//!    carrying it, breaking the loop.
//!
//! # 80% blueprint
//!
//! This file is a Rust+objc2 translation of yue/yue's
//! `nativeui/mac/browser/nu_custom_protocol.mm`. The structural choices
//! (`startLoading` snapshots strings, dispatches to main, then `-stopLoading`
//! is a no-op) follow the original C++; the Rust-specific differences are
//! noted inline.
//!
//! # Known simplifications
//!
//! - `-stopLoading` is a no-op + `tracing::debug` line. Cancellation requires
//!   threading a `CancellationToken` through the spawned future and the ivar
//!   storage; we accept that policy keeps running to completion if the
//!   webview cancels mid-flight (matching Electron's behaviour — the renderer
//!   simply ignores late callbacks).
//! - The `client_only` ignore-list passthrough still goes through reqwest
//!   (rather than letting NSURLSession handle it), because once
//!   `-startLoading` accepts the request there is no NSURLProtocol API to
//!   defer to the default loader. Policy hides this in `Decision::Respond`
//!   so we never branch on it here.

use std::sync::OnceLock;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, Bool, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, ClassType, Message};
use objc2_foundation::{
    NSData, NSDictionary, NSError, NSErrorDomain, NSHTTPURLResponse, NSObject, NSString,
    NSURLCacheStoragePolicy, NSURLProtocol, NSURLProtocolClient, NSURLRequest,
};
use tokio::runtime::Runtime;
use tracing::{debug, warn};

use crate::hook::policy::{self, Decision};

/// Marker header set by reqwest-bound requests so our `+canInitWithRequest:`
/// can short-circuit and avoid an infinite recursion if reqwest were ever
/// routed through `NSURLSession` (it isn't today, but defence in depth).
const BYPASS_HEADER: &str = "X-Hook-Bypass";

/// Multi-thread tokio runtime used to drive `policy::evaluate` from inside
/// `-startLoading`. We can't use `tauri::async_runtime` here because that's a
/// single-threaded runtime owned by the Tauri main loop; we need a worker pool
/// that survives independent of the main runloop's draining.
static TOKIO_RT: OnceLock<Runtime> = OnceLock::new();

fn rt() -> &'static Runtime {
    TOKIO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("hook-tokio")
            .build()
            .expect("failed to build tokio runtime for the macOS interceptor")
    })
}

/// Install the global NSURLProtocol subclass + register https/http with the
/// private `WKBrowsingContextController` selector. Idempotent (guarded by
/// `INSTALLED`); subsequent calls are no-ops.
///
/// Called from [`super::install_global`] before the main webview is built;
/// `WKBrowsingContextController.registerSchemeForCustomProtocol:` MUST run
/// before any `WKWebView` issues its first https request, otherwise that
/// request bypasses our `HookURLProtocol` chain.
///
/// Returns [`super::InstallError::Platform`] only if the private selector
/// cannot be located (i.e. WebKit's internal API has shifted). Subclass
/// registration itself cannot fail at runtime — `define_class!` is build-time.
pub fn install_global() -> Result<(), super::InstallError> {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.get().is_some() {
        return Ok(());
    }

    register_url_protocol_class();
    register_https_with_browsing_context_controller()?;

    // Eagerly initialise the runtime so the very first intercepted request
    // doesn't pay a runtime-build cost on the main thread.
    let _ = rt();

    let _ = INSTALLED.set(());
    tracing::info!(
        target: "hook",
        "[hook][mac] NSURLProtocol installed; https/http routed through HookURLProtocol"
    );
    Ok(())
}

/// `[NSURLProtocol registerClass:HookURLProtocol]`.
fn register_url_protocol_class() {
    let cls: &AnyClass = HookURLProtocol::class();
    let _registered: bool = unsafe { msg_send![NSURLProtocol::class(), registerClass: cls] };
    debug!(
        target: "hook",
        "[hook][mac] registerClass -> {}",
        if _registered { "ok" } else { "rejected" }
    );
}

/// Calls the private `+[WKBrowsingContextController registerSchemeForCustomProtocol:]`
/// for both `https` and `http`. Without this, `WKWebView` ignores any custom
/// protocols registered with `NSURLProtocol` for the standard schemes.
///
/// Returns `InstallError::Platform` if the class cannot be found at runtime
/// (Apple has removed it in a future macOS release).
fn register_https_with_browsing_context_controller() -> Result<(), super::InstallError> {
    let cls = AnyClass::get(c"WKBrowsingContextController").ok_or_else(|| {
        super::InstallError::Platform(
            "WKBrowsingContextController class not found — WebKit private API shifted?".into(),
        )
    })?;

    for scheme in ["https", "http"] {
        let scheme_ns = NSString::from_str(scheme);
        let _: () = unsafe { msg_send![cls, registerSchemeForCustomProtocol: &*scheme_ns] };
    }
    debug!(
        target: "hook",
        "[hook][mac] WKBrowsingContextController.registerSchemeForCustomProtocol: https + http"
    );
    Ok(())
}

// =====================================================================
// HookURLProtocol — NSURLProtocol subclass
// =====================================================================

define_class!(
    /// `NSURLProtocol` subclass that funnels every webview HTTP/HTTPS request
    /// through [`crate::hook::policy::evaluate`]. See module docs for the
    /// startup/runtime model.
    #[unsafe(super(NSURLProtocol, NSObject))]
    #[name = "HookURLProtocol"]
    pub struct HookURLProtocol;

    impl HookURLProtocol {
        /// `+canInitWithRequest:` — accept https/http (and reject anything
        /// stamped with our bypass marker, see module docs). Returning YES
        /// commits us to producing a response; non-GET requests are rejected
        /// here so the webview's default loader handles them transparently.
        #[unsafe(method(canInitWithRequest:))]
        fn can_init_with_request(request: &NSURLRequest) -> Bool {
            let url = match request.URL() {
                Some(u) => u,
                None => return Bool::NO,
            };
            let scheme = match url.scheme() {
                Some(s) => s.to_string().to_lowercase(),
                None => return Bool::NO,
            };
            if scheme != "https" && scheme != "http" {
                return Bool::NO;
            }

            // Tauri's dev server (and the production index) is served from a
            // local 127.0.0.1 / localhost origin. Hooking those would intercept
            // the trampoline page itself — we only want to hook the *target*
            // remote URL the trampoline navigates to. Skip any local host.
            let host = url
                .host()
                .map(|h| h.to_string().to_ascii_lowercase())
                .unwrap_or_default();
            if is_local_host(&host) {
                return Bool::NO;
            }

            // Only GET is cacheable / hookable. Defer everything else to the
            // webview's default networking — NSURLProtocol skips this class
            // and walks on to the next registered protocol (which is the
            // default URL loading system).
            let method = request
                .HTTPMethod()
                .map(|m| m.to_string().to_uppercase())
                .unwrap_or_else(|| "GET".to_string());
            if method != "GET" {
                return Bool::NO;
            }

            // Reqwest-bound requests carry our bypass marker; never re-hook
            // them (defence in depth: reqwest uses its own TCP stack today,
            // not NSURLSession, but this guards against future regressions).
            let marker = NSString::from_str(BYPASS_HEADER);
            if request.valueForHTTPHeaderField(&marker).is_some() {
                return Bool::NO;
            }

            Bool::YES
        }

        /// `+canonicalRequestForRequest:` — yue passes the request through
        /// unchanged; we follow suit. The retain dance is handled by
        /// `Retained::from`.
        #[unsafe(method_id(canonicalRequestForRequest:))]
        fn canonical_request_for_request(
            request: &NSURLRequest,
        ) -> Retained<NSURLRequest> {
            request.retain()
        }

        /// `-startLoading` — main entry. Snapshot request data on the calling
        /// thread (must not outlive the autorelease pool of the calling
        /// runloop), spawn the policy future, and hop back to main before
        /// invoking client callbacks.
        #[unsafe(method(startLoading))]
        fn start_loading(&self) {
            let request = self.request();

            let url_str = match request.URL().and_then(|u| u.absoluteString()) {
                Some(s) => s.to_string(),
                None => {
                    self.fail_with(NSURLErrorBadURL);
                    return;
                }
            };

            let header_map = match nsdict_to_header_map(request.allHTTPHeaderFields().as_deref()) {
                Ok(h) => h,
                Err(e) => {
                    warn!(target: "hook", "[hook][mac] header conversion failed url={} err={}", url_str, e);
                    self.fail_with(NSURLErrorUnknown);
                    return;
                }
            };

            // Retain self; the policy task holds onto this until its main-thread
            // callback fires the client callbacks. `Retained` is Send/Sync for
            // immutable Objective-C objects and for our purposes (we only call
            // `client()` from main) this is sound.
            let self_retained: Retained<HookURLProtocol> = self.retain();

            rt().spawn(async move {
                let decision = policy::evaluate(&url_str, &header_map).await;

                // Hop back to main before touching the NSURLProtocolClient.
                // `exec_async` requires `Send + 'static`; `Retained` and the
                // owned `Decision` satisfy that.
                DispatchQueue::main().exec_async(move || {
                    deliver(&self_retained, &url_str, decision);
                });
            });
        }

        /// `-stopLoading` — see module docs ("Known simplifications").
        #[unsafe(method(stopLoading))]
        fn stop_loading(&self) {
            debug!(target: "hook", "[hook][mac] stopLoading (no-op; in-flight policy continues to completion)");
        }
    }
);

impl HookURLProtocol {
    /// Convenience: synthesize an NSError with the given NSURL error code and
    /// hand it to the client. Used from `-startLoading` when we couldn't even
    /// extract a URL or headers.
    fn fail_with(&self, code: isize) {
        let Some(client) = self.client() else {
            warn!(target: "hook", "[hook][mac] no client on protocol; dropping error");
            return;
        };
        let error = make_nserror(code);
        client.URLProtocol_didFailWithError(self.as_super(), &error);
    }
}

// `Retained<HookURLProtocol>` needs Send so we can move it into the spawned
// async task and the dispatch closure. `NSURLProtocol` is not formally
// thread-safe, but: (1) we never touch instance state off the main thread,
// only `policy::evaluate` runs off-main with no Objective-C interaction;
// (2) the main-hop ensures every actual `NSURLProtocolClient` invocation
// happens on the main runloop. This matches the yue C++ blueprint, which
// also captures `protocol_job_` into `dispatch_async`-ed blocks.
unsafe impl Send for HookURLProtocol {}
unsafe impl Sync for HookURLProtocol {}

// =====================================================================
// Decision delivery (runs on the main thread)
// =====================================================================

/// Translate a [`Decision`] into the appropriate sequence of
/// `NSURLProtocolClient` callbacks. Always called on the main thread.
fn deliver(protocol: &HookURLProtocol, url: &str, decision: Decision) {
    let Some(client) = protocol.client() else {
        warn!(target: "hook", "[hook][mac] client gone before delivery url={}", url);
        return;
    };

    match decision {
        Decision::Respond {
            body,
            content_type,
            extra_headers,
        } => {
            if let Err(e) =
                deliver_respond(&client, protocol, url, &body, content_type, &extra_headers)
            {
                warn!(target: "hook", "[hook][mac] deliver_respond failed url={} err={}", url, e);
                let error = make_nserror(NSURLErrorCannotLoadFromNetwork);
                client.URLProtocol_didFailWithError(protocol.as_super(), &error);
            }
        }
        Decision::Bypass => {
            debug!(target: "hook", "[hook][mac] bypass -> didFailWithError url={}", url);
            let error = make_nserror(NSURLErrorNotConnectedToInternet);
            client.URLProtocol_didFailWithError(protocol.as_super(), &error);
        }
    }
}

fn deliver_respond(
    client: &ProtocolObject<dyn NSURLProtocolClient>,
    protocol: &HookURLProtocol,
    url: &str,
    body: &[u8],
    content_type: Option<String>,
    extra_headers: &[(String, String)],
) -> Result<(), String> {
    let url_ns = make_nsurl(url).ok_or_else(|| format!("invalid URL: {url}"))?;

    let header_dict = build_response_header_dict(content_type.as_deref(), body.len(), extra_headers);
    let http_version = NSString::from_str("HTTP/1.1");

    let response = NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(
        NSHTTPURLResponse::alloc(),
        &url_ns,
        200,
        Some(&http_version),
        Some(&header_dict),
    )
    .ok_or_else(|| "NSHTTPURLResponse allocation failed".to_string())?;

    client.URLProtocol_didReceiveResponse_cacheStoragePolicy(
        protocol.as_super(),
        response.as_super(),
        NSURLCacheStoragePolicy::NotAllowed,
    );

    let data = NSData::with_bytes(body);
    client.URLProtocol_didLoadData(protocol.as_super(), &data);
    client.URLProtocolDidFinishLoading(protocol.as_super());
    Ok(())
}

// =====================================================================
// Helpers
// =====================================================================

/// Convert `NSDictionary<NSString, NSString>` (i.e. `request.allHTTPHeaderFields`)
/// into an `http::HeaderMap`. Errors only on header values that don't fit
/// http::HeaderValue's strict ASCII-ish constraints — those are dropped with
/// a debug log rather than failing the whole conversion.
fn nsdict_to_header_map(
    dict: Option<&NSDictionary<NSString, NSString>>,
) -> Result<http::HeaderMap, String> {
    let mut out = http::HeaderMap::new();
    let Some(dict) = dict else { return Ok(out) };

    let keys = dict.allKeys();
    for key in keys.iter() {
        let key_str = key.to_string();
        let Some(value) = dict.objectForKey(&key) else {
            continue;
        };
        let value_str = value.to_string();

        let name = match http::header::HeaderName::try_from(key_str.as_str()) {
            Ok(n) => n,
            Err(e) => {
                debug!(target: "hook", "[hook][mac] dropping invalid header name {}: {}", key_str, e);
                continue;
            }
        };
        let val = match http::header::HeaderValue::try_from(value_str.as_str()) {
            Ok(v) => v,
            Err(e) => {
                debug!(target: "hook", "[hook][mac] dropping invalid header value for {}: {}", key_str, e);
                continue;
            }
        };
        out.append(name, val);
    }

    Ok(out)
}

/// Build the response header dictionary handed to NSHTTPURLResponse.
///
/// In addition to `Content-Type` + `Content-Length`, we forward upstream
/// `extra_headers` (already filtered through policy's hop-by-hop blacklist)
/// so the webview sees `Set-Cookie`, `Cache-Control`, `Location` etc.
///
/// **Multi-value headers**: NSDictionary disallows duplicate keys, but WebKit
/// internally splits values by `\n` for `Set-Cookie` (matching CFNetwork's
/// historical behaviour). We therefore:
///
/// - join multiple `Set-Cookie` values with `\n` (each cookie keeps its own
///   `Path` / `Expires` / `Secure` attributes intact, and WebKit still
///   inserts each cookie into `NSHTTPCookieStorage` separately);
/// - join other repeated headers (Vary, Cache-Control, etc.) with `, ` per
///   RFC 7230 §3.2.2, which is the canonical join for non-Set-Cookie.
fn build_response_header_dict(
    content_type: Option<&str>,
    content_length: usize,
    extra_headers: &[(String, String)],
) -> Retained<NSDictionary<NSString, NSString>> {
    use std::collections::BTreeMap;

    // Group multi-value headers (case-insensitive on the key, but keep the
    // first-seen casing for the NSDictionary key).
    struct Acc {
        canonical_name: String,
        values: Vec<String>,
    }
    let mut grouped: BTreeMap<String, Acc> = BTreeMap::new();
    for (name, value) in extra_headers {
        let lower = name.to_ascii_lowercase();
        grouped
            .entry(lower)
            .or_insert_with(|| Acc {
                canonical_name: name.clone(),
                values: Vec::new(),
            })
            .values
            .push(value.clone());
    }

    let mut keys: Vec<Retained<NSString>> = Vec::with_capacity(grouped.len() + 2);
    let mut values: Vec<Retained<NSString>> = Vec::with_capacity(grouped.len() + 2);

    if let Some(ct) = content_type {
        keys.push(NSString::from_str("Content-Type"));
        values.push(NSString::from_str(ct));
    }
    keys.push(NSString::from_str("Content-Length"));
    values.push(NSString::from_str(&content_length.to_string()));

    for (lower, acc) in grouped {
        let joined = if lower == "set-cookie" {
            // RFC 6265 says Set-Cookie cannot be safely joined with `, ` —
            // we use `\n` which WebKit's CFNetwork-derived parser splits on.
            acc.values.join("\n")
        } else {
            acc.values.join(", ")
        };
        keys.push(NSString::from_str(&acc.canonical_name));
        values.push(NSString::from_str(&joined));
    }

    let key_refs: Vec<&NSString> = keys.iter().map(|k| k.as_ref()).collect();
    let val_refs: Vec<&NSString> = values.iter().map(|v| v.as_ref()).collect();
    NSDictionary::from_slices(&key_refs, &val_refs)
}

fn make_nsurl(url: &str) -> Option<Retained<objc2_foundation::NSURL>> {
    let s = NSString::from_str(url);
    objc2_foundation::NSURL::URLWithString(&s)
}

/// Returns true if `host` (already lower-cased) refers to the local machine.
/// Used by `+canInitWithRequest:` to skip Tauri's dev-server trampoline page,
/// which is served from `127.0.0.1` / `localhost` and must not be hooked.
fn is_local_host(host: &str) -> bool {
    if host.is_empty() {
        return true;
    }
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1") || host.ends_with(".localhost")
}

// NSURLError* constants we care about. objc2-foundation exposes some of
// these as typed constants but the cleanest portable choice is the raw
// integer values from <NSURLError.h>; they're stable ABI.
#[allow(non_upper_case_globals)]
const NSURLErrorBadURL: isize = -1000;
#[allow(non_upper_case_globals)]
const NSURLErrorUnknown: isize = -1;
#[allow(non_upper_case_globals)]
const NSURLErrorCannotLoadFromNetwork: isize = -2000;
#[allow(non_upper_case_globals)]
const NSURLErrorNotConnectedToInternet: isize = -1009;

fn make_nserror(code: isize) -> Retained<NSError> {
    let domain_str = NSString::from_str("NSURLErrorDomain");
    NSError::new(code, domain_str_as_domain(&domain_str))
}

/// Reinterpret `&NSString` as `&NSErrorDomain` — the latter is
/// `typedef NSString * NSErrorDomain;` in Foundation, so the Objective-C class
/// layout is identical.
fn domain_str_as_domain(s: &NSString) -> &NSErrorDomain {
    // SAFETY: NSErrorDomain is a NSString* typedef (Foundation header).
    unsafe { &*(s as *const NSString as *const NSErrorDomain) }
}

#[cfg(test)]
mod tests {
    use super::is_local_host;

    #[test]
    fn localhost_is_local() {
        assert!(is_local_host("localhost"));
    }

    #[test]
    fn loopback_v4_is_local() {
        assert!(is_local_host("127.0.0.1"));
        assert!(is_local_host("0.0.0.0"));
    }

    #[test]
    fn loopback_v6_is_local() {
        assert!(is_local_host("::1"));
    }

    #[test]
    fn dot_localhost_subdomain_is_local() {
        assert!(is_local_host("foo.localhost"));
        assert!(is_local_host("app.dev.localhost"));
    }

    #[test]
    fn empty_host_is_local() {
        // Defensive: a request without a host shouldn't be hooked.
        assert!(is_local_host(""));
    }

    #[test]
    fn remote_host_is_not_local() {
        assert!(!is_local_host("example.com"));
        assert!(!is_local_host("www.leelib.com"));
        assert!(!is_local_host("localhost.example.com"));
    }
}
