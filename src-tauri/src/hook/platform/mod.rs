//! Platform-specific native interception entry point.
//!
//! - **macOS**: handled entirely by the MITM proxy (`crate::mitm`). The old
//!   `NSURLProtocol` + `WKBrowsingContextController` private-selector path
//!   was removed once the MITM proxy reached parity (cache_store /
//!   ignore_filter / cookie / POST body / wss). `install_global` /
//!   `install_for_webview` are no-ops on macOS — the proxy is started from
//!   `lib.rs` directly.
//! - **Windows**: WebView2 `WebResourceRequested` via `ICoreWebView2_22`. The
//!   handler can only be attached *after* the webview has been built (Tauri's
//!   `with_webview` requires a live `WebviewWindow`), so it lives in
//!   `install_for_webview`.

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
compile_error!("pouch only supports macOS and Windows");

use thiserror::Error;

/// Errors produced while installing the platform interceptor at startup.
#[derive(Debug, Error)]
pub enum InstallError {
    /// The platform-specific implementation reported a setup failure (failed
    /// to acquire the WebView2 controller, COM cast failure, etc.).
    #[error("platform interceptor install failed: {0}")]
    Platform(String),
}

/// Pre-webview platform setup. Currently a no-op on every supported platform:
/// macOS uses the MITM proxy (started from `lib.rs`) and Windows attaches its
/// WebView2 handler per-webview in [`install_for_webview`]. Kept as a slot
/// for future global-scope hooks.
pub fn install_global() -> Result<(), InstallError> {
    Ok(())
}

/// Post-webview platform setup. Runs once from the Tauri `setup` callback
/// *after* the main `WebviewWindow` has been built — required on Windows so
/// we can resolve the `ICoreWebView2Controller` and register the
/// `WebResourceRequested` handler. No-op on macOS (the MITM proxy already
/// covers every webview).
pub fn install_for_webview<R: tauri::Runtime>(
    _app: &tauri::AppHandle<R>,
) -> Result<(), InstallError> {
    #[cfg(target_os = "windows")]
    {
        windows::install_for_webview(_app)?;
    }
    Ok(())
}
