//! Windows native interception via the WebView2 `WebResourceRequested` event.
//!
//! # Design
//!
//! 1. `install_for_webview()` is called from the Tauri `setup` hook (lib.rs)
//!    *after* the main `WebviewWindow` has been built programmatically. At
//!    that point `app.get_webview_window("main")` resolves and we can drive
//!    `with_webview` to reach the WebView2 controller.
//!
//! 2. We acquire `ICoreWebView2Controller` + `ICoreWebView2Environment` via
//!    Tauri's `with_webview` bridge. The closure is dispatched onto the UI
//!    thread by Tauri internally; we register the filter + handler synchronously
//!    inside it.
//!
//! 3. Filter registration prefers `ICoreWebView2_22.AddWebResourceRequestedFilterWithRequestSourceKinds`
//!    so iframes / shared workers / service workers are also captured. On
//!    Runtime < 1.0.2210.55 we fall back to `AddWebResourceRequestedFilter`
//!    which only covers document/iframe top-level loads.
//!
//! 4. The `WebResourceRequested` handler runs on the UI thread. We snapshot the
//!    request URL + headers, take a `Deferral`, then drive `policy::evaluate`
//!    on a multi-thread tokio runtime. When the future completes we hop back
//!    to the UI thread (via `AppHandle::run_on_main_thread`) before calling
//!    `args.SetResponse(...)` + `deferral.Complete()` — both COM calls must
//!    happen on the originating thread.
//!
//! 5. To bridge the COM-not-Send gap, we wrap the event args, deferral, and
//!    environment handles in a small `unsafe impl Send` newtype. The wrapper
//!    is only ever read on the UI thread (via `run_on_main_thread`); the
//!    tokio worker holds it as opaque ownership and does not touch any
//!    COM methods. The handles' refcount manipulation that does happen on
//!    Drop in the worker thread (if `run_on_main_thread` fails and we drop
//!    on the tokio thread) is `Release`, which is apartment-neutral for
//!    free-threaded marshallable interfaces.
//!
//! # Local-host bypass
//!
//! Tauri's dev-server (and the production index) is served from
//! `127.0.0.1` / `localhost`. Hooking those would intercept the trampoline
//! page itself — we only want to hook the remote URL the trampoline navigates
//! to. `is_local_host` short-circuits the handler in that case and we do not
//! call `SetResponse`, leaving the webview to handle the request natively.
//!
//! # Conditional headers
//!
//! `http_fetcher::fetch` strips conditional request headers
//! (`If-None-Match` / `If-Modified-Since` / `If-Match` / `If-Unmodified-Since` /
//! `If-Range`) before forwarding upstream — we just transparently forward
//! whatever the webview sent and let the fetcher clean it up.
//!
//! # Why not `tauri::async_runtime`
//!
//! Tauri's runtime is single-threaded and shared with the main loop. We need
//! a worker pool so policy IO can't starve the UI thread.
//!
//! # `Bypass` semantics
//!
//! `Decision::Bypass` from policy means "we tried to handle this and failed
//! (timeout, non-2xx, body read error, etc.); surface a network error to the
//! webview". We return a `502 Bad Gateway` with an `X-Hook-Bypass-Reason`
//! header so the webview shows a network failure (rather than rendering as
//! if the upstream returned content). This is consistent with `policy.rs`'s
//! contract that `Bypass` is **not** "fall through to the default loader" —
//! once we accepted the WebResourceRequested filter for this URL, the webview
//! expects us to produce a response. (The local-host short circuit above
//! never reaches policy and **does** fall through, which is the intended
//! divergence — see module docs in `policy.rs`.)

use std::sync::OnceLock;

use tauri::{AppHandle, Manager, Runtime};
use tokio::runtime::Runtime as TokioRuntime;
use tracing::{debug, warn};
use webview2_com::take_pwstr;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2Deferral, ICoreWebView2Environment,
    ICoreWebView2WebResourceRequestedEventArgs, ICoreWebView2WebResourceResponse, ICoreWebView2_22,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
};
use webview2_com::WebResourceRequestedEventHandler;
use windows::core::{Interface, HSTRING, PWSTR};
use windows::Win32::UI::Shell::SHCreateMemStream;

use crate::hook::policy::{self, Decision};
use crate::hook::protocol_bypass;
use crate::hook::websocket;

/// Multi-thread tokio runtime used to drive `policy::evaluate` from the
/// `WebResourceRequested` handler.
static TOKIO_RT: OnceLock<TokioRuntime> = OnceLock::new();

fn rt() -> &'static TokioRuntime {
    TOKIO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("hook-tokio")
            .build()
            .expect("failed to build tokio runtime for the Windows interceptor")
    })
}

/// Install the global WebView2 interceptor. Idempotent (guarded by `INSTALLED`);
/// subsequent calls are no-ops.
///
/// Called from [`super::install_for_webview`] after the main `WebviewWindow`
/// has been built — we need the live `ICoreWebView2Controller` (reached via
/// `with_webview`) to attach `add_WebResourceRequested`.
///
/// Returns `InstallError::Platform` only for setup-time failures the operator
/// can act on (no main webview window, dispatcher rejected the closure). COM
/// failures inside the closure are logged and surfaced through the closure's
/// shared error slot.
pub fn install_for_webview<R: Runtime>(app: &AppHandle<R>) -> Result<(), super::InstallError> {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.get().is_some() {
        return Ok(());
    }

    let window = app.get_webview_window("main").ok_or_else(|| {
        super::InstallError::Platform("no `main` webview window available at install time".into())
    })?;

    // `with_webview` returns once the closure is dispatched. The closure runs
    // on the UI thread; failures are smuggled out through this channel rather
    // than the closure return type (which is `()`).
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), super::InstallError>>(1);
    let app_handle_for_handler = app.clone();
    window
        .with_webview(move |pw| {
            let result = unsafe { install_webview2_handler(&pw, app_handle_for_handler) };
            let _ = tx.send(result);
        })
        .map_err(|e| super::InstallError::Platform(format!("with_webview dispatch failed: {e}")))?;

    let install_result = rx
        .recv()
        .map_err(|e| super::InstallError::Platform(format!("with_webview channel closed: {e}")))?;
    install_result?;

    // Eagerly initialise the runtime so the very first intercepted request
    // doesn't pay the build cost on the UI thread.
    let _ = rt();

    let _ = INSTALLED.set(());
    tracing::info!(
        target: "hook",
        "[hook][win] WebView2 WebResourceRequested handler installed (https/http filtered)"
    );
    Ok(())
}

/// Inner installer running on the UI thread. Acquires the ICoreWebView2 +
/// environment from the `PlatformWebview` handle, registers the URL filters,
/// and attaches the event handler. The `app` handle is captured by the handler
/// for cross-thread main-thread dispatch.
unsafe fn install_webview2_handler<R: Runtime>(
    pw: &tauri::webview::PlatformWebview,
    app: AppHandle<R>,
) -> Result<(), super::InstallError> {
    let controller = pw.controller();
    let environment = pw.environment();
    let core: ICoreWebView2 = controller
        .CoreWebView2()
        .map_err(|e| super::InstallError::Platform(format!("CoreWebView2() failed: {e}")))?;

    register_filters(&core);

    let environment_for_handler = environment.clone();
    let handler = WebResourceRequestedEventHandler::create(Box::new(move |_sender, args| {
        if let Some(args) = args {
            // The handler closure must not propagate Rust panics into COM.
            // Errors from our internal logic are logged and the request is
            // dropped (the webview will handle it natively as a fallback).
            handle_request(args, &environment_for_handler, &app);
        }
        Ok(())
    }));

    let mut token: i64 = 0;
    core.add_WebResourceRequested(&handler, &mut token)
        .map_err(|e| {
            super::InstallError::Platform(format!("add_WebResourceRequested failed: {e}"))
        })?;
    debug!(target: "hook", "[hook][win] add_WebResourceRequested registered token={}", token);
    Ok(())
}

/// Register URL prefix filters for http/https. Prefers the
/// `ICoreWebView2_22` API (covers iframes / workers / service workers) and
/// falls back to the legacy filter on older WebView2 Runtimes.
unsafe fn register_filters(core: &ICoreWebView2) {
    if let Ok(c22) = core.cast::<ICoreWebView2_22>() {
        for prefix in ["https://*", "http://*"] {
            let h = HSTRING::from(prefix);
            if let Err(e) = c22.AddWebResourceRequestedFilterWithRequestSourceKinds(
                &h,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
            ) {
                warn!(target: "hook", "[hook][win] AddWebResourceRequestedFilterWithRequestSourceKinds({}) failed: {}", prefix, e);
            }
        }
        debug!(target: "hook", "[hook][win] using ICoreWebView2_22 filter (covers iframes/workers)");
    } else {
        warn!(target: "hook", "[hook][win] ICoreWebView2_22 unavailable; falling back to legacy filter (only document/iframe loads will be intercepted). Update WebView2 Runtime to >= 1.0.2210.55 for full coverage.");
        for prefix in ["https://*", "http://*"] {
            let h = HSTRING::from(prefix);
            if let Err(e) =
                core.AddWebResourceRequestedFilter(&h, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)
            {
                warn!(target: "hook", "[hook][win] AddWebResourceRequestedFilter({}) failed: {}", prefix, e);
            }
        }
    }
}

/// Handle a single `WebResourceRequested` event. Runs on the UI thread; must
/// not block on policy. Snapshots the request, takes a deferral, spawns onto
/// the tokio runtime, then hops back to the UI thread to deliver the response.
fn handle_request<R: Runtime>(
    args: ICoreWebView2WebResourceRequestedEventArgs,
    environment: &ICoreWebView2Environment,
    app: &AppHandle<R>,
) {
    let (url, method, header_map) = match unsafe { snapshot_request(&args) } {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "hook", "[hook][win] snapshot_request failed: {}", e);
            return;
        }
    };

    // Only GET is cacheable / hookable. Returning without calling SetResponse
    // lets the webview perform the request natively.
    if !method.eq_ignore_ascii_case("GET") {
        return;
    }

    // WebSocket opening handshakes must not enter the cache/fetch policy.
    // Leaving the event without SetResponse lets WebView2 continue its
    // native upgrade path.
    if websocket::is_websocket_upgrade(&header_map) {
        debug!(
            target: "hook",
            "[hook][win] decision=websocket/bypass_cache method={} url={}",
            method,
            url
        );
        return;
    }

    if let Some(reason) = protocol_bypass::request_bypass_reason(&header_map) {
        debug!(
            target: "hook",
            "[hook][win] decision=protocol/bypass_cache reason={} method={} url={}",
            reason.as_str(),
            method,
            url
        );
        return;
    }

    // Local-host bypass. Tauri's dev server / production trampoline is local;
    // never hook it.
    let host = url::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default();
    if is_local_host(&host) {
        return;
    }

    let deferral = match unsafe { args.GetDeferral() } {
        Ok(d) => d,
        Err(e) => {
            warn!(target: "hook", "[hook][win] GetDeferral failed url={} err={}", url, e);
            return;
        }
    };

    let pending = HandlerHandles {
        args,
        deferral,
        environment: environment.clone(),
    };
    let app_for_dispatch = app.clone();
    let url_for_log = url.clone();

    rt().spawn(async move {
        let decision = policy::evaluate(&url, &header_map).await;

        // Hop back to the UI thread before touching the COM args / deferral.
        // `run_on_main_thread` requires `Send + 'static`; `HandlerHandles`
        // satisfies that via the `unsafe impl Send` justification documented
        // on the type itself. `url` is moved (not borrowed) into the closure
        // so the dispatched function has no lifetime ties back to the future.
        let dispatch_result = app_for_dispatch.run_on_main_thread(move || {
            apply_decision_on_ui_thread(pending, &url, decision);
        });
        if let Err(e) = dispatch_result {
            warn!(target: "hook", "[hook][win] run_on_main_thread failed url={} err={}", url_for_log, e);
            // Without main-thread dispatch we cannot SetResponse / Complete —
            // the request will remain stuck until the webview times it out.
            // There's no safe fallback that doesn't risk calling COM from
            // the wrong thread.
        }
    });
}

/// Snapshot URL + method + headers from the request. All extraction happens on
/// the UI thread before we spawn the tokio task.
unsafe fn snapshot_request(
    args: &ICoreWebView2WebResourceRequestedEventArgs,
) -> Result<(String, String, http::HeaderMap), String> {
    let request = args.Request().map_err(|e| format!("args.Request(): {e}"))?;

    let mut url_pwstr = PWSTR::null();
    request
        .Uri(&mut url_pwstr)
        .map_err(|e| format!("request.Uri(): {e}"))?;
    let url = take_pwstr(url_pwstr);

    let mut method_pwstr = PWSTR::null();
    request
        .Method(&mut method_pwstr)
        .map_err(|e| format!("request.Method(): {e}"))?;
    let method = take_pwstr(method_pwstr);

    let header_map = headers_to_http(&request).unwrap_or_else(|e| {
        warn!(target: "hook", "[hook][win] header iteration failed (forwarding empty header set) err={}", e);
        http::HeaderMap::new()
    });

    Ok((url, method, header_map))
}

/// Iterate `ICoreWebView2HttpRequestHeaders` into `http::HeaderMap`. Invalid
/// header names/values (per `http`'s strict validation) are dropped with a
/// debug log rather than failing the whole conversion.
unsafe fn headers_to_http(
    request: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2WebResourceRequest,
) -> Result<http::HeaderMap, String> {
    let mut out = http::HeaderMap::new();
    let headers = request
        .Headers()
        .map_err(|e| format!("request.Headers(): {e}"))?;
    let iter = headers
        .GetIterator()
        .map_err(|e| format!("Headers.GetIterator(): {e}"))?;

    let mut has_current = windows::core::BOOL::default();
    iter.HasCurrentHeader(&mut has_current)
        .map_err(|e| format!("HasCurrentHeader: {e}"))?;
    while has_current.as_bool() {
        let mut key_pwstr = PWSTR::null();
        let mut val_pwstr = PWSTR::null();
        iter.GetCurrentHeader(&mut key_pwstr, &mut val_pwstr)
            .map_err(|e| format!("GetCurrentHeader: {e}"))?;
        let key = take_pwstr(key_pwstr);
        let val = take_pwstr(val_pwstr);

        match (
            http::header::HeaderName::try_from(key.as_str()),
            http::header::HeaderValue::try_from(val.as_str()),
        ) {
            (Ok(name), Ok(value)) => {
                out.append(name, value);
            }
            (Err(e), _) => {
                debug!(target: "hook", "[hook][win] dropping invalid header name {}: {}", key, e);
            }
            (_, Err(e)) => {
                debug!(target: "hook", "[hook][win] dropping invalid header value for {}: {}", key, e);
            }
        }

        iter.MoveNext(&mut has_current)
            .map_err(|e| format!("MoveNext: {e}"))?;
    }

    Ok(out)
}

/// Translate the policy decision into a WebView2 response. Always called on
/// the UI thread (via `run_on_main_thread`).
fn apply_decision_on_ui_thread(handles: HandlerHandles, url: &str, decision: Decision) {
    let HandlerHandles {
        args,
        deferral,
        environment,
    } = handles;

    let outcome = match decision {
        Decision::Respond {
            body,
            content_type,
            extra_headers,
        } => unsafe {
            build_and_set_response(
                &args,
                &environment,
                &body,
                content_type.as_deref(),
                &extra_headers,
            )
        },
        Decision::Bypass => {
            // Policy already logged the BYPASS reason; surface a generic 502
            // so the webview shows a network error and we don't pretend to
            // have served real content.
            unsafe { build_and_set_error(&args, &environment, "policy bypass") }
        }
    };
    if let Err(e) = outcome {
        warn!(target: "hook", "[hook][win] failed to set response url={} err={}", url, e);
    }

    if let Err(e) = unsafe { deferral.Complete() } {
        warn!(target: "hook", "[hook][win] deferral.Complete() failed url={} err={}", url, e);
    }
}

/// Construct an `ICoreWebView2WebResourceResponse` carrying `body`,
/// `content_type`, and the curated upstream `extra_headers` (already
/// filtered through policy's hop-by-hop blacklist). Then `args.SetResponse(...)`.
///
/// `CreateWebResourceResponse` accepts headers as a single `\r\n`-separated
/// `"Name: Value"` block. WebView2 documentation explicitly says repeating
/// the same header name yields a multi-value header, which is exactly what
/// we need for `Set-Cookie` (one line per cookie).
unsafe fn build_and_set_response(
    args: &ICoreWebView2WebResourceRequestedEventArgs,
    environment: &ICoreWebView2Environment,
    body: &[u8],
    content_type: Option<&str>,
    extra_headers: &[(String, String)],
) -> windows::core::Result<()> {
    let mut headers_map = String::new();
    if let Some(ct) = content_type {
        headers_map.push_str("Content-Type: ");
        headers_map.push_str(ct);
        headers_map.push_str("\r\n");
    }
    for (name, value) in extra_headers {
        // Skip anything containing CR/LF in the value to avoid header
        // injection (defense-in-depth; reqwest's HeaderValue already
        // forbids CR/LF, but `String` here doesn't enforce it).
        if value.contains('\r') || value.contains('\n') {
            continue;
        }
        headers_map.push_str(name);
        headers_map.push_str(": ");
        headers_map.push_str(value);
        headers_map.push_str("\r\n");
    }
    headers_map.push_str("Content-Length: ");
    headers_map.push_str(&body.len().to_string());
    let headers_hstr = HSTRING::from(headers_map);

    let stream = if body.is_empty() {
        None
    } else {
        // SHCreateMemStream copies the buffer internally, so `body` doesn't
        // need to outlive the call. Returns None on allocation failure.
        SHCreateMemStream(Some(body))
    };

    let status_phrase = HSTRING::from("OK");
    let response: ICoreWebView2WebResourceResponse = environment.CreateWebResourceResponse(
        stream.as_ref(),
        200,
        &status_phrase,
        &headers_hstr,
    )?;

    args.SetResponse(&response)
}

/// Construct an error response (no body, 502 Bad Gateway) and set it. Used
/// for policy `Bypass` outcomes where we accepted the request but couldn't
/// produce useful content.
unsafe fn build_and_set_error(
    args: &ICoreWebView2WebResourceRequestedEventArgs,
    environment: &ICoreWebView2Environment,
    reason: &str,
) -> windows::core::Result<()> {
    let status_phrase = HSTRING::from("Bad Gateway");
    let headers = HSTRING::from(format!("X-Hook-Bypass-Reason: {reason}"));
    let response = environment.CreateWebResourceResponse(None, 502, &status_phrase, &headers)?;
    args.SetResponse(&response)
}

/// Returns true if `host` (already lower-cased) refers to the local machine.
/// Filtered out at the platform layer before policy is consulted, so policy
/// itself never sees these requests.
fn is_local_host(host: &str) -> bool {
    if host.is_empty() {
        return true;
    }
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1") || host.ends_with(".localhost")
}

// =====================================================================
// Cross-thread COM handle bundle
// =====================================================================

/// Bundle of COM handles passed from the UI thread → tokio worker → back to
/// the UI thread (via `run_on_main_thread`).
///
/// The COM handles themselves are not used off the UI thread — the tokio
/// worker only owns the bundle for storage purposes and never invokes any
/// method on it. Every actual COM call (`SetResponse`, `Complete`) happens
/// inside the `run_on_main_thread` closure that runs on the originating UI
/// thread.
///
/// Justification for `unsafe impl Send`:
///   1. We never read/write Tauri-owned COM apartment state off the UI thread.
///   2. wry's protocol handler uses the same pattern (raw pointer through
///      PostMessage); ours just leans on `run_on_main_thread`'s typed
///      requirement instead.
///   3. The handles' refcount manipulation that does happen on Drop in the
///      worker thread (if `run_on_main_thread` fails and we drop on the
///      tokio thread) is `Release`, which is documented to be apartment-
///      neutral for free-threaded marshallable interfaces — and even for
///      non-marshallable ones it's the same pattern wry relies on.
struct HandlerHandles {
    args: ICoreWebView2WebResourceRequestedEventArgs,
    deferral: ICoreWebView2Deferral,
    environment: ICoreWebView2Environment,
}

unsafe impl Send for HandlerHandles {}

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
        assert!(is_local_host(""));
    }

    #[test]
    fn remote_host_is_not_local() {
        assert!(!is_local_host("example.com"));
        assert!(!is_local_host("www.leelib.com"));
        assert!(!is_local_host("localhost.example.com"));
    }
}
