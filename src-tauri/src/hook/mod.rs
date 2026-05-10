//! Native webview interception hook.
//!
//! - `policy` is the platform-agnostic decision logic used by the Windows
//!   `WebResourceRequested` interceptor.
//! - `platform` cfg-dispatches per-OS install hooks (Windows attaches the
//!   WebView2 handler; macOS is a no-op since the MITM proxy now handles
//!   every interception responsibility — see `crate::mitm`).
//! - `ignore_filter` is shared between the policy layer (Windows) and the
//!   MITM handler (macOS).

pub mod ignore_filter;
pub mod platform;
pub mod policy;
