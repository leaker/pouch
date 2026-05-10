//! Local HTTPS MITM proxy (macOS only). Generates and persists a self-signed
//! root CA, prompts the user once to trust it via NSAlert, and routes the
//! WKWebView through `127.0.0.1:<auto-port>` via Tauri's `macos-proxy` feature.
//!
//! The handler ([`handler::PouchHandler`]) consults [`crate::cache_store`]
//! and [`crate::hook::ignore_filter`] on every request, short-circuiting
//! cache HITs locally and writing back upstream MISSes. The companion
//! [`learned::LearnerLayer`] observes hudsucker error events and self-learns
//! hosts that need to bypass MITM (cert pinning, legacy TLS).
//!
//! Windows uses WebView2's `WebResourceRequested` API natively (see
//! `hook/platform/windows.rs`) so we never need a proxy there. Module
//! visibility is gated by the `#[cfg(target_os = "macos")] mod mitm;`
//! declaration in `lib.rs`; no inner cfg attribute is needed here.

mod ca;
mod handler;
mod learned;
mod trust;

pub use learned::{is_learned_passthrough, load_learned_passthrough, LearnerLayer};

use std::net::SocketAddr;
use std::sync::OnceLock;

use thiserror::Error;
use tokio::runtime::Runtime;

#[derive(Debug, Error)]
pub enum MitmError {
    #[error("CA error: {0}")]
    Ca(String),
    #[error("proxy bind failed: {0}")]
    Bind(String),
    #[error("proxy build failed: {0}")]
    Build(String),
    #[error("CA trust error: {0}")]
    Trust(String),
}

/// Set on successful [`start`] so other modules (the webview builder) can
/// ask "what port did the proxy bind to?" without re-running startup.
static PROXY_PORT: OnceLock<u16> = OnceLock::new();

/// Dedicated multi-thread runtime that drives the hudsucker server task.
/// Kept separate from `hook-tokio` (the Windows path's reqwest runtime) so
/// the two have no shared scheduler.
static MITM_RT: OnceLock<Runtime> = OnceLock::new();

/// Start the MITM proxy. Synchronously installs the rustls aws-lc-rs
/// `CryptoProvider`, loads (or generates) the persistent CA, binds a TCP
/// listener on `127.0.0.1:0`, then spawns the hudsucker server task on a
/// dedicated tokio runtime. Returns the chosen port.
///
/// Idempotent: subsequent calls return the cached port.
pub fn start() -> Result<u16, MitmError> {
    if let Some(&port) = PROXY_PORT.get() {
        return Ok(port);
    }

    // Load learned passthrough hosts before binding so the very first
    // request after restart already benefits from prior learning.
    match load_learned_passthrough() {
        Ok(n) => tracing::info!(target: "hook", "[mitm] loaded {n} learned passthrough entries"),
        Err(e) => tracing::warn!(target: "hook", "[mitm] load learned passthrough failed: {e}"),
    }

    // hudsucker's RcgenAuthority requires a process-wide `CryptoProvider`.
    // `install_default()` returns Err if one was already installed — race-safe
    // because reqwest configures its provider per-client (no clash here).
    let _ = hudsucker::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let authority = ca::load_or_create_authority()?;

    // Bind synchronously via std so we can read `local_addr()` and return the
    // port to the caller before any async work starts. We hand the underlying
    // socket to tokio inside the runtime block below.
    let std_listener = std::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .map_err(|e| MitmError::Bind(e.to_string()))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| MitmError::Bind(format!("set_nonblocking: {e}")))?;
    let port = std_listener
        .local_addr()
        .map_err(|e| MitmError::Bind(format!("local_addr: {e}")))?
        .port();

    let rt = MITM_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("mitm-tokio")
            .build()
            .expect("failed to build tokio runtime for the MITM proxy")
    });

    // `tokio::net::TcpListener::from_std` requires being called inside a
    // tokio runtime context (it registers the fd with the reactor). Use
    // `Handle::block_on` for the async glue but `enter()` for the sync
    // conversion — we're not awaiting anything here, just registering.
    let _enter = rt.enter();
    let tokio_listener = tokio::net::TcpListener::from_std(std_listener)
        .map_err(|e| MitmError::Bind(format!("TcpListener::from_std: {e}")))?;

    // Build a native-tls upstream connector by hand and feed it to hudsucker
    // through the generic `with_http_connector` builder method. We avoid
    // hudsucker 0.24.1's `with_native_tls_connector` because its internal
    // `HttpsConnector::from((HttpConnector::new(), tls))` skips
    // `enforce_http(false)` on the inner http connector — every https URL is
    // then rejected with "invalid URL, scheme is not http". By replicating
    // its body and *adding* `enforce_http(false)` we get the behaviour
    // upstream hyper-tls's own `HttpsConnector::new_` already applies (see
    // hyper-tls 0.6.0 client.rs:53-57), so https URLs flow through correctly.
    //
    // The native-tls path matters because rustls (with webpki-roots and the
    // strict default cipher / version policy) refuses some legacy TLS
    // servers reachable from the OS keychain (older intermediate chains,
    // ECDSA-on-old-curves, etc.). Switching to native-tls makes the upstream
    // connector match what curl / Safari already accept, eliminating the
    // need to "learn" passthrough hosts at runtime for cert reasons.
    //
    // We deliberately do NOT call `.with_websocket_connector(...)`. When
    // `websocket_connector` is left as `None`, hudsucker forwards
    // `connect_async_tls_with_config(.., None, ..)` to tokio-tungstenite,
    // which falls back to a default native-tls `TlsConnector::new()` — same
    // OS trust store as the http path, no separate dep needed.
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.enforce_http(false);
    // Request ALPN h2 (with http/1.1 fallback) so the upstream client can
    // multiplex many concurrent requests onto a single TCP connection per
    // host instead of opening N parallel TCP — the latter trips per-IP
    // connection limits on some servers (manifests as floods of "connection
    // reset" errors when loading sites with high request fan-out).
    // hyper-tls's `alpn` feature wires `negotiated_alpn() == "h2"` into
    // `Connected::negotiated_h2()`, which hyper-util's legacy Client uses
    // to switch the pool slot to HTTP/2 automatically. Servers that don't
    // advertise h2 in ALPN transparently fall back to HTTP/1.1.
    let tls = native_tls::TlsConnector::builder()
        .request_alpns(&["h2", "http/1.1"])
        .build()
        .map_err(|e| MitmError::Build(format!("native_tls::TlsConnector::new: {e}")))?;
    let https = hyper_tls::HttpsConnector::from((http, tokio_native_tls::TlsConnector::from(tls)));

    let proxy = hudsucker::Proxy::builder()
        .with_listener(tokio_listener)
        .with_ca(authority)
        .with_http_connector(https)
        .with_http_handler(handler::PouchHandler::default())
        .build()
        .map_err(|e| MitmError::Build(format!("{e:?}")))?;

    rt.spawn(async move {
        if let Err(e) = proxy.start().await {
            tracing::error!(target: "hook", "[mitm] proxy exited with error: {e}");
        }
    });

    let _ = PROXY_PORT.set(port);
    tracing::info!(target: "hook", "[mitm] listening on 127.0.0.1:{port}");

    Ok(port)
}

/// Port the proxy is listening on, if [`start`] succeeded. Read by
/// [`apply_proxy_to_builder`] when configuring each WKWebView.
pub fn proxy_port() -> Option<u16> {
    PROXY_PORT.get().copied()
}

/// Stable, well-known data-store identifier for every Pouch WKWebView
/// (`WKWebsiteDataStore::dataStoreForIdentifier:`). Hard-coded so it
/// never changes across launches — switching identifiers would create a
/// fresh empty data store and orphan all previously-persisted cookies /
/// `localStorage` / IndexedDB.
///
/// The identifier itself is not user-identifying — every Pouch install
/// uses the same UUID; it's effectively a namespace tag the WebKit
/// network process uses to derive the on-disk container at
/// `~/Library/WebKit/<bundle>/WebsiteDataStore/<UUID>/`.
///
/// **Why this exists:** WKWebView's `defaultDataStore` resolves its
/// container path via `_WKWebsiteDataStoreConfiguration` defaults that
/// rely on a code-signed app container. For unsigned / ad-hoc-signed
/// debug binaries (everything except a notarised release on macOS),
/// container resolution fails inside the NetworkProcess and cookies
/// silently never reach disk. Calling
/// `dataStoreForIdentifier:` instead routes the data store through the
/// per-identifier path which **does** persist for unsigned binaries —
/// confirmed by inspecting `~/Library/WebKit/.../Cookies/` after a
/// relaunch.
///
/// Generated via `uuidgen` on 2026-05-10. UUID:
/// `1E295A15-F723-4732-A4E4-62A8E89946C6`. Decoded big-endian to bytes
/// below; do not edit by hand without re-decoding.
#[cfg(target_os = "macos")]
const POUCH_DATA_STORE_ID: [u8; 16] = [
    0x1E, 0x29, 0x5A, 0x15, 0xF7, 0x23, 0x47, 0x32, 0xA4, 0xE4, 0x62, 0xA8, 0xE8, 0x99, 0x46, 0xC6,
];

/// Configure a [`tauri::WebviewWindowBuilder`] for the macOS WKWebView path.
/// Two responsibilities, intentionally chained in this order:
///
/// 1. **`.data_store_identifier(...)`** — pin the WKWebView to a
///    `dataStoreForIdentifier:`-backed `WKWebsiteDataStore` so cookies
///    and other site data persist across launches even for
///    unsigned / ad-hoc-signed binaries (see [`POUCH_DATA_STORE_ID`]
///    docs for the full rationale). Must be set before `.proxy_url(...)`
///    in case wry's builder applies them in declaration order — proxy
///    config lives on the data store object on macOS 14+, so the data
///    store has to exist first.
/// 2. **`.proxy_url(...)`** — route the WKWebView's network stack
///    through the local MITM proxy. Requires the `macos-proxy` Tauri
///    feature flag (enabled in `Cargo.toml`); under the hood wry
///    forwards this to macOS 14+'s `nw_proxy_config_create_http_connect`
///    for `WKWebsiteDataStore.proxyConfigurations`.
///
/// No-ops gracefully (returns `builder` unchanged with a log line) when
/// either the proxy hasn't started yet or the localhost URL fails to
/// parse — neither should happen in practice (setup guarantees `start()`
/// runs before any webview is created), but we don't want a spurious
/// panic to take down the entire window-creation path.
///
/// macOS-only: Windows uses WebView2's native `WebResourceRequested`
/// API (see `hook/platform/windows.rs`), so the windows builder does
/// not call this helper.
#[cfg(target_os = "macos")]
pub fn apply_proxy_to_builder<R: tauri::Runtime, M: tauri::Manager<R>>(
    builder: tauri::WebviewWindowBuilder<'_, R, M>,
) -> tauri::WebviewWindowBuilder<'_, R, M> {
    // Pin to a per-identifier data store so cookies persist on unsigned
    // binaries. Always applied regardless of proxy state — the two configs
    // are independent and the cookie fix matters even if the proxy somehow
    // failed to start.
    let builder = builder.incognito(false).data_store_identifier(POUCH_DATA_STORE_ID);
    tracing::info!(
        target: "hook",
        "[mitm] webview data_store_identifier set"
    );

    let Some(port) = proxy_port() else {
        tracing::warn!(
            target: "hook",
            "[mitm] proxy_port() unavailable, webview not routed through proxy"
        );
        return builder;
    };
    match format!("http://127.0.0.1:{port}").parse::<url::Url>() {
        Ok(proxy_url) => {
            tracing::info!(
                target: "hook",
                "[mitm] webview proxy_url set to 127.0.0.1:{port}"
            );
            builder.proxy_url(proxy_url)
        }
        Err(e) => {
            tracing::error!(target: "hook", "[mitm] proxy_url parse failed: {e}");
            builder
        }
    }
}

/// Ensure the persistent Pouch CA is trusted by macOS for SSL.
/// Called once during `lib.rs::setup` *after* [`start`] has materialised the
/// CA on disk. Flow:
///
/// 1. `security verify-cert -p ssl` — already trusted? return `Ok` and move on.
/// 2. Not trusted → modal NSAlert ("Install Pouch Root Certificate" /
///    "Cancel and Quit"). Cancel returns `Err(MitmError::Trust)` and the
///    caller decides whether to bail.
/// 3. User clicked "Install Now" → `security add-trusted-cert -k login` (the
///    OS pops the password dialog). Non-zero exit (including user-cancel of
///    the password dialog) returns `Err(MitmError::Trust)`.
/// 4. Re-run `verify-cert` — still not trusted (e.g. user denied trust in
///    the System Roots dialog) → `Err(MitmError::Trust)`. Otherwise `Ok`.
///
/// Must be invoked on the AppKit main thread (the Tauri `setup` callback
/// runs there). Blocks the setup callback for the duration of the modal +
/// the OS authorization prompt; the proxy task already running on the
/// dedicated `mitm-tokio` runtime is unaffected and just sits idle until
/// the first webview request arrives post-trust.
///
/// macOS-only — gated by the module's `#[cfg(target_os = "macos")]` in
/// `lib.rs`, no inner cfg needed.
pub fn ensure_ca_trusted() -> Result<(), MitmError> {
    let ca_pem = ca::ca_pem_path()
        .ok_or_else(|| MitmError::Trust("ca_pem_path unavailable ($HOME unset?)".into()))?;

    if trust::check_trust(&ca_pem) == trust::TrustState::Trusted {
        tracing::info!(target: "hook", "[mitm] CA already trusted");
        return Ok(());
    }

    tracing::info!(target: "hook", "[mitm] CA not trusted, prompting user");
    match trust::show_install_prompt() {
        trust::UserChoice::Install => {
            tracing::info!(target: "hook", "[mitm] user chose Install Now");
            trust::install_to_login_keychain(&ca_pem)
                .map_err(|e| MitmError::Trust(format!("install failed: {e}")))?;

            if trust::check_trust(&ca_pem) != trust::TrustState::Trusted {
                return Err(MitmError::Trust(
                    "install reported success but verify-cert still fails".into(),
                ));
            }
            tracing::info!(target: "hook", "[mitm] CA installed and trusted");
            Ok(())
        }
        trust::UserChoice::Cancel => Err(MitmError::Trust("user cancelled CA install".into())),
    }
}

/// Show a final NSAlert explaining why Pouch is exiting, after
/// [`ensure_ca_trusted`] returned `Err`. Thin re-export of
/// [`trust::show_quit_explanation`] so `lib.rs` doesn't need to know the
/// trust submodule exists.
pub fn show_trust_quit_alert(reason: &str) {
    trust::show_quit_explanation(reason);
}
