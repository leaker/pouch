//! Platform-specific native interception entry point.
//!
//! v2 native interception is split across two startup phases because the
//! per-platform requirements are mutually exclusive:
//!
//! - **macOS** registers `HookURLProtocol` with `NSURLProtocol` and asks
//!   `WKBrowsingContextController` to route `https`/`http` through it. This
//!   MUST happen *before* any webview navigates, otherwise the first request
//!   bypasses the protocol class.
//! - **Windows** attaches a `WebResourceRequested` handler to the WebView2
//!   `ICoreWebView2Controller`. The controller is only obtainable *after* the
//!   webview has been built (Tauri's `with_webview` requires a live
//!   `WebviewWindow`).
//!
//! `install_global` runs before webview creation (real on macOS, no-op on
//! Windows). `install_for_webview` runs after webview creation (real on
//! Windows, no-op on macOS). Unsupported targets fall through to a `warn` log
//! so Pouch still builds for development convenience; real interception only
//! works on macOS and Windows.

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

use thiserror::Error;

/// Errors produced while installing the platform interceptor at startup.
#[derive(Debug, Error)]
pub enum InstallError {
    /// The platform-specific implementation reported a setup failure (failed
    /// to acquire the WebView2 controller, COM cast failure, objc class
    /// declaration failure, etc.).
    #[error("platform interceptor install failed: {0}")]
    Platform(String),
}

/// Pre-webview platform setup. Runs once at app startup *before* any webview
/// is built — required on macOS so `NSURLProtocol` + the
/// `WKBrowsingContextController` private selector are wired in before the
/// first navigation. No-op on Windows (the WebView2 handler can only be
/// attached once a controller exists, see [`install_for_webview`]).
pub fn install_global() -> Result<(), InstallError> {
    #[cfg(target_os = "macos")]
    {
        macos::install_global()
    }
    #[cfg(target_os = "windows")]
    {
        tracing::debug!(
            target: "hook",
            "[hook][win] install_global no-op (WebView2 handler attaches in install_for_webview)"
        );
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        tracing::warn!(
            target: "hook",
            "[hook] platform not supported (only macOS + Windows); native interception disabled"
        );
        Ok(())
    }
}

/// Post-webview platform setup. Runs once from the Tauri `setup` callback
/// *after* the main `WebviewWindow` has been built — required on Windows so
/// we can resolve the `ICoreWebView2Controller` and register the
/// `WebResourceRequested` handler. No-op on macOS (the global registration
/// already covers every webview).
pub fn install_for_webview<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<(), InstallError> {
    #[cfg(target_os = "windows")]
    {
        windows::install_for_webview(app)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        tracing::debug!(
            target: "hook",
            "[hook][mac] install_for_webview no-op (NSURLProtocol registered globally in install_global)"
        );
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = app;
        Ok(())
    }
}
