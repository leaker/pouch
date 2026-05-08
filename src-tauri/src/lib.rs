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
//!   `WebviewUrl::External(<first startup_urls entry>)` so the webview
//!   navigates straight to the upstream origin; no frontend trampoline page
//!   exists. The `initialization_script` carrying the URL-rule JS dispatcher
//!   (see [`inject`]) runs on every top-level navigation, so user-supplied
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
pub mod dialog;
pub mod hook;
pub mod http_fetcher;
pub mod inject;
#[cfg(target_os = "macos")]
mod recent_urls;
#[cfg(target_os = "macos")]
mod titlebar;
pub mod util;

#[cfg(target_os = "macos")]
use tauri::menu::{AboutMetadata, PredefinedMenuItem};
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    webview::PageLoadEvent,
    AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

use crate::config::{WindowDimensions, WindowDimensionsMode};
use crate::util::{DEFAULT_WINDOW_HEIGHT, DEFAULT_WINDOW_WIDTH};
use tracing_subscriber::{fmt::time::ChronoLocal, EnvFilter};

/// Menu item id for the "Open DevTools" entry. Matched in `on_menu_event` to
/// dispatch into [`tauri::WebviewWindow::open_devtools`].
const MENU_ID_OPEN_DEVTOOLS: &str = "pouch.open_devtools";

/// Placeholder title shown while a navigation is in flight. Set
/// immediately after `build()` (see main + extra window paths) and
/// re-asserted on `PageLoadEvent::Started`. The `Finished` arm of
/// [`page_load_handler`] uses this exact string as the marker for
/// "the page never set its own `<title>`": if the current window title
/// still starts with this prefix when `Finished` fires, the page didn't
/// produce a real title and we fall back to the host string; otherwise
/// the `on_document_title_changed` listener already swapped in the real
/// `<title>` and we leave it alone. Centralised as a constant so
/// dialog.rs / lib.rs (initial set) and the Started/Finished arms here
/// can never drift apart and silently break the marker check.
pub(crate) const LOADING_TITLE: &str = "\u{23f3} Loading...";

/// Menu item id for the macOS-only "File → New Window" entry (Cmd+N). Pops
/// up a native `NSAlert` text-input prompt asking for a URL and, on OK,
/// opens that URL as an additional `WebviewWindow` sharing cookies / cache
/// with the main window. macOS-only because the prompt UI is hand-rolled
/// against `NSAlert` + `NSTextField`; Windows / Linux would need a separate
/// implementation that we don't currently ship.
#[cfg(target_os = "macos")]
const MENU_ID_NEW_WINDOW: &str = "pouch.new_window";

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
    install_panic_hook();

    let build_result = tauri::Builder::default()
        // Standard macOS menu bar: <App> / File / Edit / View / Window.
        // F12 works on both macOS and Windows for opening DevTools
        // (matches Chrome on both platforms); the accelerator only fires
        // while pouch has focus, so it never fights the host IDE's
        // bindings — which is exactly why we don't reach for
        // `tauri-plugin-global-shortcut`.
        //
        // On non-macOS we keep the slim historical View-only bar: every
        // companion entry (New Window's NSAlert prompt, Reveal Folder,
        // Reload-from-Config's titlebar pairing) is macOS-only by design
        // — see the `MENU_ID_*` doc comments above. There's nothing to
        // gain by erecting empty File / Edit / Window submenus on
        // Windows; the OS supplies window controls via the system menu.
        .menu(|handle| {
            let open_devtools = MenuItemBuilder::with_id(MENU_ID_OPEN_DEVTOOLS, "Open DevTools")
                .accelerator("F12")
                .build(handle)?;

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
                view_builder
                    .item(&reveal_folder)
                    .separator()
                    .item(&reload)
            };
            let view = view_builder.build()?;

            let menu_builder = MenuBuilder::new(handle);

            // The full <App>/File/Edit/Window scaffolding is macOS-only.
            // Tauri normally synthesises a default macOS menu when no
            // `.menu(...)` is configured (see
            // `Menu::default(app_handle)`); calling `.menu(...)` here
            // *replaces* that default, so we have to ship the standard
            // app / Edit / Window submenus ourselves to keep the macOS
            // experience native — otherwise Cmd+Q / Cmd+H / Cmd+W /
            // copy-paste / window-list-in-Window-menu all silently
            // disappear from the menu bar (their key equivalents fall
            // back to OS defaults but the visible menu UI vanishes).
            #[cfg(target_os = "macos")]
            let menu_builder = {
                let new_window = MenuItemBuilder::with_id(MENU_ID_NEW_WINDOW, "New Window")
                    .accelerator("CmdOrCtrl+N")
                    .build(handle)?;

                let pkg = handle.package_info();
                // Display name shown in the macOS menu bar — must match the
                // `productName` in `tauri.conf.json` ("Pouch") and the
                // bundle name macOS displays in About / Hide / Quit, so the
                // three menu entries read consistently. We deliberately do
                // NOT use `pkg.name` here: that comes from Cargo's
                // `package.name` ("pouch", lowercase per cargo convention)
                // and would render "Hide pouch" / "Quit pouch" with a
                // lowercase 'p' next to "About Pouch".
                let product_name = "Pouch";
                let about_metadata = AboutMetadata {
                    name: Some(product_name.to_string()),
                    version: Some(pkg.version.to_string()),
                    ..Default::default()
                };

                // <AppName> menu — Apple HIG-standard layout. Tauri
                // automatically labels this submenu using the running
                // process name on macOS (NSApp swaps in the bundle name),
                // so the title we pass is just a placeholder.
                let app_submenu = SubmenuBuilder::new(handle, product_name)
                    .item(&PredefinedMenuItem::about(
                        handle,
                        Some(&format!("About {product_name}")),
                        Some(about_metadata),
                    )?)
                    .separator()
                    .services()
                    .separator()
                    .item(&PredefinedMenuItem::hide(
                        handle,
                        Some(&format!("Hide {product_name}")),
                    )?)
                    .hide_others()
                    .show_all()
                    .separator()
                    .item(&PredefinedMenuItem::quit(
                        handle,
                        Some(&format!("Quit {product_name}")),
                    )?)
                    .build()?;

                let file = SubmenuBuilder::new(handle, "File")
                    .item(&new_window)
                    .separator()
                    .close_window()
                    .build()?;

                let edit = SubmenuBuilder::new(handle, "Edit")
                    .cut()
                    .copy()
                    .paste()
                    .separator()
                    .select_all()
                    .build()?;

                // Tagging this submenu with `WINDOW_SUBMENU_ID` is the
                // key step that hands ownership to NSApp: Tauri's
                // `init_app_menu` (see tauri/src/app.rs) looks up this
                // id and calls `set_as_windows_menu_for_nsapp()`, which
                // in turn makes macOS auto-populate the running window
                // list (with `Cmd+`` cycling and a checkmark on the
                // focused window) and append items like "Bring All to
                // Front" — none of which we have to track ourselves.
                let window = SubmenuBuilder::with_id(
                    handle,
                    tauri::menu::WINDOW_SUBMENU_ID,
                    "Window",
                )
                .minimize()
                .maximize()
                .separator()
                .close_window()
                .build()?;

                menu_builder
                    .item(&app_submenu)
                    .item(&file)
                    .item(&edit)
                    .item(&view)
                    .item(&window)
            };

            #[cfg(not(target_os = "macos"))]
            let menu_builder = menu_builder.item(&view);

            menu_builder.build()
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
                //
                // Multi-window: target the **focused window** so F12 acts
                // on whichever window the user is looking at. Falls back
                // to `"main"` if no window is focused (rare — e.g.
                // accelerator fired while focus is in another app's
                // window which somehow got the keystroke). We can't use
                // `Manager::get_focused_window` directly because that's
                // gated on Tauri's `unstable` feature; iterating
                // `webview_windows()` and matching `is_focused()` is the
                // stable equivalent (cheap — typically a small map).
                let target = focused_webview_window(app)
                    .or_else(|| app.get_webview_window("main"));
                if let Some(webview) = target {
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
                    titlebar::update_devtools_button_image(webview.label(), !was_open);
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
            // File → New Window: pop a native NSAlert prompt for a URL
            // and open it as an additional `WebviewWindow`. macOS-only
            // (see `dialog.rs` for why).
            #[cfg(target_os = "macos")]
            if event.id() == MENU_ID_NEW_WINDOW {
                dialog::show_new_window_dialog(app);
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
            tracing::info!(
                target: "hook",
                "[startup] startup_urls = {} entrie(s)",
                cfg.startup_urls.len()
            );
            tracing::info!(
                target: "hook",
                "[startup] cache root = {}",
                cache_store::cache_root().display()
            );

            // Cache the resolved window dimensions so the Cmd+N "New Window"
            // handler — which only has `&AppHandle`, not the original
            // `Config` — applies the same default / maximized / fullscreen /
            // fixed-size mode to runtime-spawned extra windows that the main
            // window and the startup_urls array use. See
            // `dialog::cache_window_dimensions` for the storage rationale.
            dialog::cache_window_dimensions(cfg.window_dimensions);

            // 1b. Pre-webview platform setup (macOS NSURLProtocol +
            //    WKBrowsingContextController; no-op on Windows).
            hook::platform::install_global()?;

            // 2. Scan inject/ rules. Editing files under inject/ at
            //    runtime is picked up on the next Cmd+R via a clean
            //    `app.restart()` (see [`reload_from_config`] doc /
            //    README §2.5).
            let rules = inject::scan_inject_dir();
            let dispatcher = inject::build_dispatcher_js(&rules);
            tracing::info!(
                target: "hook",
                "[main-window] inject rules = {} (dispatcher {} attached)",
                rules.len(),
                if dispatcher.is_some() { "WILL be" } else { "will NOT be" },
            );

            // 3. Build the main webview + any extra startup windows from
            //    `cfg.startup_urls`. The first valid entry becomes the main
            //    window (label "main" — hard-wired because the Windows hook
            //    installation `hook::platform::windows::install_for_webview`
            //    looks the main webview up by that label); subsequent valid
            //    entries become extra windows with auto-allocated labels.
            //
            //    Empty resolved list → prompt the user via NSAlert for a
            //    single URL. Cancel exits the process; otherwise the typed
            //    URL becomes the main window. Non-macOS builds fall back to
            //    the historical default URL because `dialog::prompt_initial_url`
            //    is macOS-only.
            if cfg.startup_urls.is_empty() {
                let entered = {
                    #[cfg(target_os = "macos")]
                    {
                        dialog::prompt_initial_url()
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        tracing::warn!(
                            target: "hook",
                            "[startup] startup_urls is empty and NSAlert prompt is macOS-only; \
                             populate hook.config.json -> startup_urls to launch on non-macOS."
                        );
                        None::<String>
                    }
                };
                let Some(raw) = entered else {
                    tracing::info!(
                        target: "hook",
                        "[startup] no URL provided; exiting"
                    );
                    std::process::exit(0);
                };
                let trimmed = raw.trim();
                let url: url::Url = trimmed.parse().map_err(|e| {
                    tracing::error!(
                        target: "hook",
                        "[main-window] entered URL {:?} did not parse: {}",
                        trimmed, e
                    );
                    tauri::Error::InvalidUrl(e)
                })?;
                if url.scheme() != "http" && url.scheme() != "https" {
                    tracing::error!(
                        target: "hook",
                        "[main-window] entered URL is not http(s); exiting: {}",
                        url
                    );
                    std::process::exit(0);
                }
                create_main_window_with_url(app, url, cfg.window_dimensions, dispatcher.as_deref())?;
            } else {
                for (i, url_str) in cfg.startup_urls.iter().enumerate() {
                    // We pre-validated http(s) prefix at config load time,
                    // but `url::Url::parse` can still reject malformed
                    // values (e.g. `https://`). Skip those with a warn
                    // rather than crashing the whole launch.
                    let url: url::Url = match url_str.parse() {
                        Ok(u) => u,
                        Err(e) => {
                            tracing::warn!(
                                target: "hook",
                                "[startup] startup_urls[{}] parse failed for {:?}: {}",
                                i, url_str, e
                            );
                            continue;
                        }
                    };
                    if i == 0 {
                        create_main_window_with_url(app, url, cfg.window_dimensions, dispatcher.as_deref())?;
                    } else {
                        let label = dialog::next_window_label();
                        if let Err(e) =
                            dialog::open_extra_window(app.handle(), &label, url, cfg.window_dimensions)
                        {
                            tracing::warn!(
                                target: "hook",
                                "[startup] failed to create extra window {label}: {e}"
                            );
                        }
                    }
                }

                // Defensive: if every entry failed parsing above, we never
                // built a "main" window. Fall through to the prompt path so
                // the user can rescue the launch instead of staring at a
                // dockless background process.
                if app.get_webview_window("main").is_none() {
                    tracing::warn!(
                        target: "hook",
                        "[startup] startup_urls had entries but none parsed; prompting user"
                    );
                    let entered = {
                        #[cfg(target_os = "macos")]
                        {
                            dialog::prompt_initial_url()
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            None::<String>
                        }
                    };
                    let Some(raw) = entered else {
                        tracing::info!(
                            target: "hook",
                            "[startup] no URL provided; exiting"
                        );
                        std::process::exit(0);
                    };
                    let trimmed = raw.trim();
                    let url: url::Url = trimmed.parse().map_err(|e| {
                        tracing::error!(
                            target: "hook",
                            "[main-window] entered URL {:?} did not parse: {}",
                            trimmed, e
                        );
                        tauri::Error::InvalidUrl(e)
                    })?;
                    if url.scheme() != "http" && url.scheme() != "https" {
                        tracing::error!(
                            target: "hook",
                            "[main-window] entered URL is not http(s); exiting: {}",
                            url
                        );
                        std::process::exit(0);
                    }
                    create_main_window_with_url(app, url, cfg.window_dimensions, dispatcher.as_deref())?;
                }
            }

            // 4. Post-webview platform setup (Windows WebView2
            //    WebResourceRequested handler; no-op on macOS). Runs once
            //    after the main window exists regardless of which startup
            //    branch above produced it.
            if let Err(e) = hook::platform::install_for_webview(app.handle()) {
                tracing::error!(
                    target: "hook",
                    "[startup] install_for_webview failed: {}",
                    e
                );
            }

            Ok(())
        })
        .build(tauri::generate_context!());

    match build_result {
        Ok(app) => {
            use std::sync::atomic::{AtomicBool, Ordering};
            static EXITING: AtomicBool = AtomicBool::new(false);

            app.run(|_app_handle, event| {
                tracing::trace!(target: "hook", "[runevent] {:?}", event);
                if let tauri::RunEvent::ExitRequested { code, .. } = event {
                    // Tauri 在所有 window 关闭后发出 ExitRequested（macOS 不自动退）。
                    // hook-tokio runtime 是 OnceLock 永不 drop，无法 graceful shutdown，
                    // 必须强杀。直接 std::process::exit 绕开 Tauri 派发，避免
                    // AppHandle::exit 内部 re-emit ExitRequested 形成无限递归
                    // (RuntimeRunEvent::ExitRequested -> RunEvent::ExitRequested -> callback
                    //  -> AppHandle::exit -> RuntimeRunEvent::ExitRequested ... 22 次实证)。
                    // EXITING swap 是 defensive：万一 callback 被并发派发也只走一次。
                    if !EXITING.swap(true, Ordering::SeqCst) {
                        std::process::exit(code.unwrap_or(0));
                    }
                }
            });
        }
        Err(e) => {
            tracing::error!(target: "hook", "tauri build failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Build the `"main"`-labelled `WebviewWindow` that anchors a Pouch launch.
/// Shared by every startup path that has to produce a main window — the
/// regular `cfg.startup_urls[0]` case, the empty-config NSAlert prompt
/// fallback, and the every-entry-failed defensive prompt — so the builder
/// chain (devtools / title-sync / page-load / window-mode / dispatcher
/// init-script) and the post-build wiring (initial set_title +
/// macOS titlebar accessory) live in exactly one place.
///
/// The label is hard-wired to `"main"` because Windows hook installation
/// (`hook::platform::windows::install_for_webview`) looks the main webview
/// up by that label — extra windows go through
/// [`dialog::open_extra_window`] instead, which assigns labels via
/// [`dialog::next_window_label`].
///
/// `dispatcher` is the optional `inject/*.js` rule dispatcher built by
/// [`inject::build_dispatcher_js`]; passed by reference so the same string
/// can be applied to multiple windows without cloning.
fn create_main_window_with_url(
    app: &tauri::App,
    url: url::Url,
    window_dimensions: WindowDimensions,
    dispatcher: Option<&str>,
) -> tauri::Result<WebviewWindow> {
    let url_for_log = url.to_string();
    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        // Set the loading title at builder time so the NSWindow / HWND
        // is born with `⏳ Loading...` as its initial title — this
        // closes the visual gap between window creation and the first
        // post-build `set_title` call, where the user could otherwise
        // glimpse the default Tauri / label-derived title for a frame.
        // Builder-time `.title(...)` forwards to wry, which calls
        // `NSWindow.setTitle:` / `SetWindowTextW` before the window is
        // ordered front. The post-build `set_title` below is kept as a
        // redundant fallback in case `.title` silently no-ops on some
        // platform.
        .title(LOADING_TITLE)
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
        })
        .on_page_load(page_load_handler());

    // Apply the user-configured window dimensions. We always set
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
    // restores the window to a sensible 1280x960 instead of wry's
    // 800x600 platform default.
    builder = match window_dimensions {
        WindowDimensions::Mode(WindowDimensionsMode::Default) => builder
            .fullscreen(false)
            .maximized(false)
            .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT),
        WindowDimensions::Mode(WindowDimensionsMode::Maximized) => builder
            .fullscreen(false)
            .maximized(true)
            .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT),
        WindowDimensions::Mode(WindowDimensionsMode::Fullscreen) => builder
            .fullscreen(true)
            .maximized(false)
            .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT),
        WindowDimensions::Size { width, height } if width > 0 && height > 0 => builder
            .fullscreen(false)
            .maximized(false)
            .inner_size(f64::from(width), f64::from(height)),
        WindowDimensions::Size { width, height } => {
            tracing::warn!(
                target: "hook",
                "[main-window] window size {{ width: {}, height: {} }} has a zero dimension; falling back to default (maximized)",
                width,
                height
            );
            builder
                .fullscreen(false)
                .maximized(true)
                .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT)
        }
    };

    if let Some(js) = dispatcher {
        builder = builder.initialization_script(js);
    }

    let main_window = builder.build()?;
    tracing::debug!(
        target: "hook",
        "[window] created label=main url={}",
        url_for_log
    );
    // Set the loading title prefix immediately on window creation so
    // the user sees feedback the moment the window appears — see the
    // builder-time `.title(...)` comment above for the reasoning.
    if let Err(e) = main_window.set_title(LOADING_TITLE) {
        tracing::warn!(
            target: "hook",
            "[startup] main window initial set_title(loading) failed: {e}"
        );
    }

    // macOS-only: drop three SF Symbol buttons into the right side of the
    // titlebar — Reveal Folder / Reload / Open DevTools. Failures here are
    // non-fatal (we still have the menubar entries).
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

    Ok(main_window)
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

/// Stable equivalent of `Manager::get_focused_window` (which lives behind
/// the `unstable` Cargo feature). Iterates the small `webview_windows()`
/// map and returns the first window whose `is_focused()` is `Ok(true)`.
/// Returns `None` if no window is focused (e.g. focus is in another app)
/// or if every `is_focused()` call errored.
fn focused_webview_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.webview_windows()
        .into_values()
        .find(|w| w.is_focused().unwrap_or(false))
}

/// Page-load callback factory shared by the main window builder and the
/// extra-window helper in [`crate::dialog::open_extra_window`]. Wires the
/// "loading" UI on both axes:
///
/// - **Window title**: prefixes with `⏳ Loading...` on `PageLoadEvent::Started`
///   so users immediately see the navigation is in flight, even before the
///   first paint. On `PageLoadEvent::Finished` we set a sensible host-derived
///   fallback so the prefix doesn't linger if the upstream page never sets
///   a `<title>`; if the page does set one, the existing
///   [`tauri::webview::WebviewWindowBuilder::on_document_title_changed`]
///   listener naturally overrides this fallback.
/// - **macOS titlebar spinner**: starts / stops a small `NSProgressIndicator`
///   alongside the existing reveal / reload / devtools buttons via
///   [`crate::titlebar::set_loading`]. cfg-gated to macOS — non-macOS builds
///   only get the title prefix.
///
/// `on_page_load` is a `WebviewWindowBuilder` method (i.e. registered at
/// build time, not after `.build()`), so the helper returns a closure that
/// the caller plugs into the builder chain.
pub(crate) fn page_load_handler(
) -> impl Fn(WebviewWindow, tauri::webview::PageLoadPayload<'_>) + Send + Sync + 'static {
    |window, payload| {
        match payload.event() {
            PageLoadEvent::Started => {
                if let Err(e) = window.set_title(LOADING_TITLE) {
                    tracing::warn!(
                        target: "hook",
                        "[page-load] set_title(loading) failed: {e}"
                    );
                }
                #[cfg(target_os = "macos")]
                titlebar::set_loading(window.label(), true);
            }
            PageLoadEvent::Finished => {
                #[cfg(target_os = "macos")]
                titlebar::set_loading(window.label(), false);
                // Host-derived fallback only when the page never produced
                // its own `<title>`. The `on_document_title_changed`
                // listener fires during HTML head parsing (well before
                // `Finished`, which is `didFinishNavigation` /
                // WebView2 `NavigationCompleted` — i.e. after every
                // synchronously-loaded subresource), so by the time we
                // get here the window title has *already* been swapped
                // to the real `<title>` for any normal site. Detecting
                // that via the LOADING_TITLE marker — rather than
                // unconditionally overwriting — preserves the real
                // title; previously this arm clobbered every page's
                // `<title>` back to the bare host (e.g. "github.com")
                // because `Finished` ran last and won.
                let still_loading = window
                    .title()
                    .map(|t| t.starts_with(LOADING_TITLE))
                    .unwrap_or(false);
                if !still_loading {
                    return;
                }
                let fallback = payload
                    .url()
                    .host_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Pouch".to_string());
                if let Err(e) = window.set_title(&fallback) {
                    tracing::warn!(
                        target: "hook",
                        "[page-load] set_title(fallback {:?}) failed: {e}",
                        fallback
                    );
                }
            }
        }
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

/// Install a process-wide panic hook that funnels panics through `tracing`
/// (target `hook`, level `error`) so panics on background threads —
/// `hook-tokio` workers, `dispatch_async` blocks, Tauri event listeners —
/// surface in the standard log stream instead of being silently swallowed
/// when the default hook's stderr message races with the close path. Keeps
/// the panic location/payload but does **not** abort: matches the default
/// hook's "log + unwind" semantics.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(target: "hook", "[panic] {info}");
    }));
}
