//! Pouch — library entry point.
//!
//! Wires up the v2 native interception path:
//! - Loads `hook.config.json` (via [`config::load`]) into a [`config::Config`]
//!   captured by `move` into the `setup` closure. There is no front-end and
//!   no IPC, so the config never needs to live in Tauri's state map.
//! - Calls [`hook::platform::install_global`] *before* building the webview so
//!   the macOS `NSURLProtocol` + `WKBrowsingContextController` private-selector
//!   registration is in place when the very first https navigation fires
//!   (Windows `install_global` is a no-op — see `hook/platform/mod.rs`).
//! - Builds the main `WebviewWindow` programmatically with
//!   `WebviewUrl::External(target_url)` so the webview navigates straight to
//!   the upstream origin; no frontend trampoline page exists. The
//!   `initialization_script` carrying the URL-rule JS dispatcher (see
//!   [`inject`]) runs on every top-level navigation, so user-supplied
//!   `inject/*.js` rules apply on the live target site.
//!   We deliberately do NOT call `.title(...)` so the upstream `<title>` wins.
//!   To actually propagate `document.title` -> window title we register
//!   [`tauri::webview::WebviewWindowBuilder::on_document_title_changed`],
//!   which Tauri v2 wires to the underlying wry/`WKWebView` title KVO on
//!   macOS and `DocumentTitleChanged` on WebView2; the closure simply calls
//!   `window.set_title(&title)`. No JS injection or IPC is required for
//!   this — see <https://docs.rs/tauri/2.9.5/tauri/webview/struct.WebviewWindowBuilder.html#method.on_document_title_changed>.
//! - Calls [`hook::platform::install_for_webview`] *after* the webview is
//!   built so the Windows WebView2 `WebResourceRequested` handler can attach
//!   to the live controller (macOS `install_for_webview` is a no-op).

pub mod cache_store;
pub mod config;
pub mod hook;
pub mod http_fetcher;
pub mod inject;
pub mod util;

use tauri::{
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    Manager, WebviewUrl, WebviewWindowBuilder,
};

use crate::config::{WindowConfig, WindowMode};
use tracing_subscriber::{fmt::time::ChronoLocal, EnvFilter};

/// Menu item id for the "Open DevTools" entry. Matched in `on_menu_event` to
/// dispatch into [`tauri::WebviewWindow::open_devtools`].
const MENU_ID_OPEN_DEVTOOLS: &str = "pouch.open_devtools";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    let cfg = config::load();
    tracing::info!(target: "hook", "[startup] target_url = {}", cfg.target_url);
    tracing::info!(
        target: "hook",
        "[startup] cache root = {}",
        cache_store::cache_root().display()
    );

    let result = tauri::Builder::default()
        // App menu carrying a single "Open DevTools" entry. F12 works on
        // both macOS and Windows for opening DevTools (matches Chrome on
        // both platforms); the accelerator only fires while pouch has
        // focus, so it never fights the host IDE's bindings — which is
        // exactly why we don't reach for `tauri-plugin-global-shortcut`.
        .menu(|handle| {
            let open_devtools = MenuItemBuilder::with_id(MENU_ID_OPEN_DEVTOOLS, "Open DevTools")
                .accelerator("F12")
                .build(handle)?;
            let view = SubmenuBuilder::new(handle, "View")
                .item(&open_devtools)
                .build()?;
            MenuBuilder::new(handle).item(&view).build()
        })
        .on_menu_event(|app, event| {
            if event.id() == MENU_ID_OPEN_DEVTOOLS {
                if let Some(webview) = app.get_webview_window("main") {
                    webview.open_devtools();
                }
            }
        })
        .setup(move |app| {
            // 1. Pre-webview platform setup (macOS NSURLProtocol +
            //    WKBrowsingContextController; no-op on Windows).
            hook::platform::install_global()?;

            // 2. Scan inject/ rules and assemble the dispatcher JS that will
            //    run on every top-level navigation (target site included).
            let rules = inject::scan_inject_dir();
            let dispatcher = inject::build_dispatcher_js(&rules);
            tracing::info!(
                target: "hook",
                "[startup] inject rules = {} (dispatcher {} attached)",
                rules.len(),
                if dispatcher.is_some() { "WILL be" } else { "will NOT be" },
            );

            // 3. Build the main webview programmatically so we can navigate
            //    directly to the external target_url and (optionally) attach
            //    the URL-rule dispatcher as the initialization script. The
            //    label MUST stay "main" — hook::platform::windows looks it
            //    up by that label to attach the WebView2 interceptor.
            let target_url: url::Url = cfg
                .target_url
                .parse()
                .map_err(|e| format!("invalid target_url {:?}: {}", cfg.target_url, e))?;
            let mut builder =
                WebviewWindowBuilder::new(app, "main", WebviewUrl::External(target_url))
                    .resizable(true)
                    // Enable the Web Inspector for both debug and release
                    // builds — pouch is a hook-debugging tool, not a
                    // shrink-wrapped end-user product. Pairs with the
                    // `tauri = { features = ["devtools"] }` flag in
                    // Cargo.toml so the underlying `open_devtools` symbol is
                    // compiled in for release as well.
                    .devtools(true)
                    // Native bridge from WKWebView/WebView2's title KVO to the
                    // Tauri window title. Fires on initial load AND on every
                    // SPA-style `document.title = ...` mutation, so we don't
                    // need a JS MutationObserver / IPC trampoline.
                    .on_document_title_changed(|window, title| {
                        if title.trim().is_empty() {
                            return;
                        }
                        if let Err(e) = window.set_title(&title) {
                            tracing::warn!(
                                target: "hook",
                                "[title-sync] set_title({:?}) failed: {}",
                                title,
                                e
                            );
                        }
                    });

            // Apply the user-configured window-size mode. We always set
            // `fullscreen` and `maximized` explicitly (defaulting to false) so
            // mode switches in `hook.config.json` are deterministic across
            // launches — never depending on a previous build's leftover state.
            //
            // Builder-time `.maximized(true)` is the cross-platform idiom for
            // "fill the work area" (excludes macOS menubar/dock and Windows
            // taskbar) — wry forwards it to NSWindow.zoom:/ShowWindow(SW_MAXIMIZE)
            // which both honour the OS work area natively. See
            // <https://docs.rs/tauri/2.9.5/tauri/webview/struct.WebviewWindowBuilder.html#method.maximized>.
            builder = match cfg.window {
                WindowConfig::Mode(WindowMode::Screen) => {
                    builder.fullscreen(false).maximized(true)
                }
                WindowConfig::Mode(WindowMode::Fullscreen) => {
                    builder.fullscreen(true).maximized(false)
                }
                WindowConfig::Size { width, height } if width > 0 && height > 0 => builder
                    .fullscreen(false)
                    .maximized(false)
                    .inner_size(f64::from(width), f64::from(height)),
                WindowConfig::Size { width, height } => {
                    tracing::warn!(
                        target: "hook",
                        "[startup] window size {{ width: {}, height: {} }} has a zero dimension; falling back to default (screen)",
                        width,
                        height
                    );
                    builder.fullscreen(false).maximized(true)
                }
            };

            if let Some(js) = dispatcher.as_deref() {
                builder = builder.initialization_script(js);
            }

            let _main = builder.build()?;

            // 4. Post-webview platform setup (Windows WebView2
            //    WebResourceRequested handler; no-op on macOS).
            if let Err(e) = hook::platform::install_for_webview(app.handle()) {
                tracing::error!(
                    target: "hook",
                    "[startup] install_for_webview failed: {}",
                    e
                );
            }

            Ok(())
        })
        .run(tauri::generate_context!());

    if let Err(e) = result {
        tracing::error!(target: "hook", "tauri runtime exited with error: {e}");
        std::process::exit(1);
    }
}

/// Initialise the global tracing subscriber.
///
/// Reads `TAURI_HOOK_LOG` (e.g. `info`, `tauri_hook=debug`) and falls back to
/// `info`. Using `try_init` so a host application that already installed a
/// subscriber (tests, embedding) doesn't panic.
fn init_tracing() {
    let env_filter =
        EnvFilter::try_from_env("TAURI_HOOK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S".to_string()))
        .try_init();
}
