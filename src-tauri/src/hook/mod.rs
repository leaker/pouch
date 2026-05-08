//! Native webview interception hook (v2).
//!
//! `policy` is the platform-agnostic decision logic; `platform` cfg-dispatches
//! to the per-OS interceptor (Windows = WebView2 `WebResourceRequested`,
//! macOS = `NSURLProtocol` + `WKBrowsingContextController` private selector).
//! `ignore_filter` is shared between platforms.

pub mod ignore_filter;
pub mod platform;
pub mod policy;
