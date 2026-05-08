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

pub mod bootstrap;
pub mod cache_store;
pub mod config;
pub mod hook;
pub mod http_fetcher;
pub mod inject;
#[cfg(target_os = "macos")]
mod titlebar;
pub mod util;

use tauri::{
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};

use crate::config::{WindowConfig, WindowMode};
use tracing_subscriber::{fmt::time::ChronoLocal, EnvFilter};

/// Menu item id for the "Open DevTools" entry. Matched in `on_menu_event` to
/// dispatch into [`tauri::WebviewWindow::open_devtools`].
const MENU_ID_OPEN_DEVTOOLS: &str = "pouch.open_devtools";

/// Default window inner size used when the user has not pinned an explicit
/// `{ width, height }` in `hook.config.json`. Set on the `Screen` /
/// `Fullscreen` / fallback branches so an unmaximize / un-fullscreen
/// gesture restores the window to a sensible 1280x800 — without this,
/// wry/Tauri falls back to the platform default of 800x600 which is too
/// cramped for the kind of dashboards pouch typically targets.
const DEFAULT_WINDOW_WIDTH: f64 = 1280.0;
const DEFAULT_WINDOW_HEIGHT: f64 = 800.0;

/// Menu item id for the macOS-only "Reveal Pouch Folder in Finder" entry
/// (Cmd+Shift+O). The title is also a paired `NSButton` accessory in the
/// titlebar — both routes call [`util::reveal_pouch_folder`].
#[cfg(target_os = "macos")]
const MENU_ID_REVEAL_FOLDER: &str = "pouch.reveal_folder";

/// Menu item id for the macOS-only "Reload from Config" entry (Cmd+R).
/// Paired with the `arrow.clockwise` titlebar button — both call
/// [`reload_from_config`], which restarts the application via
/// [`AppHandle::restart`] so changes to `hook.config.json` and `inject/*.js`
/// take effect on the fresh launch. macOS-only because the paired titlebar
/// button is macOS-only; on Windows / Linux there's no analogous accessory
/// and the keep-it-uniform argument from the reveal entry applies here too.
#[cfg(target_os = "macos")]
const MENU_ID_RELOAD: &str = "pouch.reload";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

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

            // The "Reveal Pouch Folder in Finder" item is macOS-only: on
            // Windows pouch is portable (the .exe sits next to its
            // `inject/` and `overrides/` folders, no reveal entry point
            // needed), and Linux likewise doesn't have a single canonical
            // user-data dir to reveal. Cfg-gating the item — rather than
            // a no-op stub — keeps the menubar visually accurate on each
            // platform.
            let view_builder = SubmenuBuilder::new(handle, "View").item(&open_devtools);
            #[cfg(target_os = "macos")]
            let view_builder = {
                let reveal_folder = MenuItemBuilder::with_id(
                    MENU_ID_REVEAL_FOLDER,
                    "Reveal Pouch Folder in Finder",
                )
                .accelerator("CmdOrCtrl+Shift+O")
                .build(handle)?;
                let reload = MenuItemBuilder::with_id(MENU_ID_RELOAD, "Reload from Config")
                    .accelerator("CmdOrCtrl+R")
                    .build(handle)?;
                view_builder.item(&reveal_folder).item(&reload)
            };
            let view = view_builder.build()?;
            MenuBuilder::new(handle).item(&view).build()
        })
        .on_menu_event(|app, event| {
            if event.id() == MENU_ID_OPEN_DEVTOOLS {
                // Toggle (open / close) so F12 / menu match the
                // titlebar button's behaviour. `is_devtools_open` and
                // `close_devtools` require the `devtools` Cargo feature
                // (or `debug_assertions`) on `tauri`; pouch enables
                // `devtools` unconditionally — see Cargo.toml. Note:
                // `close_devtools` is documented as unsupported on
                // Windows in Tauri 2.9.5; on Windows the `else` arm is
                // a quiet no-op, which is acceptable (parity with the
                // upstream platform limit).
                if let Some(webview) = app.get_webview_window("main") {
                    let was_open = webview.is_devtools_open();
                    if was_open {
                        webview.close_devtools();
                    } else {
                        webview.open_devtools();
                    }
                    // Mirror the new state on the titlebar accessory
                    // button so the icon stays in sync regardless of
                    // which trigger (button / F12 / menu) flipped it.
                    #[cfg(target_os = "macos")]
                    titlebar::update_devtools_button_image(!was_open);
                }
            }
            #[cfg(target_os = "macos")]
            if event.id() == MENU_ID_REVEAL_FOLDER {
                if let Err(e) = util::reveal_pouch_folder() {
                    tracing::warn!(
                        target: "hook",
                        "[menu] reveal_pouch_folder failed: {e}"
                    );
                }
            }
            // Reload menu / titlebar button: restart the application so
            // changes to hook.config.json and inject/*.js take effect on
            // the fresh launch — see `reload_from_config` doc / README §2.5.
            #[cfg(target_os = "macos")]
            if event.id() == MENU_ID_RELOAD {
                reload_from_config(app);
            }
        })
        .setup(|app| {
            // 0. First-run bootstrap: on macOS prod the user-data
            //    directory at `~/Library/Application Support/Pouch/`
            //    doesn't exist yet on first launch. Copy the bundled
            //    sample (hook.config.json + inject/) out of
            //    `Pouch.app/Contents/Resources/sample/` so the resolver
            //    chain in step 2 / `config::load` finds defaults to read.
            //    No-op on dev / Windows / Linux.
            bootstrap::bootstrap_macos_user_dir(app.handle());

            // 1a. Load config (now that bootstrap, if applicable, has
            //     populated the user-data dir).
            let cfg = config::load();
            tracing::info!(target: "hook", "[startup] target_url = {}", cfg.target_url);
            tracing::info!(
                target: "hook",
                "[startup] cache root = {}",
                cache_store::cache_root().display()
            );

            // 1b. Pre-webview platform setup (macOS NSURLProtocol +
            //    WKBrowsingContextController; no-op on Windows).
            hook::platform::install_global()?;

            // 2. Scan inject/ rules. Editing files under inject/ at
            //    runtime is picked up on the next Cmd+R via a clean
            //    `app.restart()` (see [`reload_from_config`] doc /
            //    README §2.5).
            let rules = inject::scan_inject_dir();

            // 3. Build the main webview programmatically. The label is
            //    hard-wired to `"main"` because Windows hook installation
            //    (`hook::platform::windows::install_for_webview`) looks up
            //    the main webview by that label.
            let dispatcher = inject::build_dispatcher_js(&rules);
            tracing::info!(
                target: "hook",
                "[main-window] inject rules = {} (dispatcher {} attached)",
                rules.len(),
                if dispatcher.is_some() { "WILL be" } else { "will NOT be" },
            );

            // `tauri::Error::InvalidUrl(url::ParseError)` is the natural
            // conversion for a target_url string that doesn't parse — the
            // variant exists for exactly this case. We log the offending
            // string ourselves first so the `url::ParseError`'s opaque
            // message ("relative URL without a base" etc.) doesn't leave
            // the operator guessing which value triggered it.
            let target_url: url::Url = cfg.target_url.parse().map_err(|e| {
                tracing::error!(
                    target: "hook",
                    "[main-window] invalid target_url {:?}: {}",
                    cfg.target_url, e
                );
                tauri::Error::InvalidUrl(e)
            })?;

            let mut builder = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::External(target_url),
            )
            .resizable(true)
            // Enable the Web Inspector for both debug and release builds —
            // pouch is a hook-debugging tool, not a shrink-wrapped end-user
            // product. Pairs with the `tauri = { features = ["devtools"] }`
            // flag in Cargo.toml so the underlying `open_devtools` symbol
            // is compiled in for release as well.
            .devtools(true)
            // Native bridge from WKWebView/WebView2's title KVO to the
            // Tauri window title. Fires on initial load AND on every
            // SPA-style `document.title = ...` mutation, so we don't need
            // a JS MutationObserver / IPC trampoline.
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
            // `fullscreen` and `maximized` explicitly (defaulting to false)
            // so mode switches in `hook.config.json` are deterministic
            // across launches — never depending on a previous build's
            // leftover state.
            //
            // Builder-time `.maximized(true)` is the cross-platform idiom
            // for "fill the work area" (excludes macOS menubar/dock and
            // Windows taskbar) — wry forwards it to NSWindow.zoom: /
            // ShowWindow(SW_MAXIMIZE) which both honour the OS work area
            // natively. See
            // <https://docs.rs/tauri/2.9.5/tauri/webview/struct.WebviewWindowBuilder.html#method.maximized>.
            //
            // We also chain `.inner_size(...)` on every branch (using
            // DEFAULT_WINDOW_{WIDTH,HEIGHT} when the user didn't pin an
            // explicit size) so the un-maximize / un-fullscreen gesture
            // restores the window to a sensible 1280x800 instead of wry's
            // 800x600 platform default.
            builder = match cfg.window {
                WindowConfig::Mode(WindowMode::Screen) => builder
                    .fullscreen(false)
                    .maximized(true)
                    .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT),
                WindowConfig::Mode(WindowMode::Fullscreen) => builder
                    .fullscreen(true)
                    .maximized(false)
                    .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT),
                WindowConfig::Size { width, height } if width > 0 && height > 0 => builder
                    .fullscreen(false)
                    .maximized(false)
                    .inner_size(f64::from(width), f64::from(height)),
                WindowConfig::Size { width, height } => {
                    tracing::warn!(
                        target: "hook",
                        "[main-window] window size {{ width: {}, height: {} }} has a zero dimension; falling back to default (screen)",
                        width,
                        height
                    );
                    builder
                        .fullscreen(false)
                        .maximized(true)
                        .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT)
                }
            };

            if let Some(js) = dispatcher.as_deref() {
                builder = builder.initialization_script(js);
            }

            let _main = builder.build()?;

            // 3b. macOS-only: drop three SF Symbol buttons into the right
            //     side of the titlebar — Reveal Folder / Reload / Open
            //     DevTools. Each pairs with a `View` submenu entry and
            //     they share the same handlers (so the keyboard shortcut
            //     and the button do exactly the same thing). Failures
            //     here are non-fatal (we still have the menubar entries).
            #[cfg(target_os = "macos")]
            if let Some(window) = app.get_webview_window("main") {
                if let Err(e) = titlebar::install_titlebar_accessory(app.handle(), &window) {
                    tracing::warn!(
                        target: "hook",
                        "[startup] titlebar accessory install failed: {}",
                        e
                    );
                }
            }

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

/// Reload by restarting the application. `hook.config.json` and
/// `inject/*.js` are re-read on the fresh launch, so any user edits take
/// effect predictably without ad-hoc in-process state-swapping. Visually
/// presents as a brief flicker, comparable to a webview rebuild but with
/// far simpler semantics — see README §2.5.
///
/// [`AppHandle::restart`] is documented as `-> !` (never returns; the
/// process is replaced), so no result handling is required at the call
/// site.
pub fn reload_from_config(app: &AppHandle) {
    tracing::info!(
        target: "hook",
        "[reload] restarting application to apply new config"
    );
    app.restart();
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
