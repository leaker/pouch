//! macOS-only "New Window" prompt — a native `NSAlert` carrying an
//! `NSComboBox` accessory view that asks the user for an http(s) URL and,
//! on OK, opens it as an additional `WebviewWindow`. The combobox dropdown
//! lists the user's recently submitted URLs (persisted by
//! [`crate::recent_urls`]) so common destinations are one click away.
//!
//! Why a hand-rolled `NSAlert` instead of `tauri-plugin-dialog`:
//!
//! - We're already linking `objc2-app-kit` for the titlebar accessory work
//!   (see [`crate::titlebar`]), so the marginal cost of an `NSAlert` here
//!   is just the few lines below — no new crate.
//! - `tauri-plugin-dialog`'s `ask` returns yes/no; it does not surface a
//!   text-input prompt. Adding a plugin for one screen is more dependency
//!   churn than this is worth.
//! - The result UI is the canonical AppKit text-input alert that every
//!   Mac user already recognises — escape cancels, return submits — without
//!   us reproducing keyboard handling in a webview.
//!
//! Module-level cfg-gate (this file is only `mod`'d into the tree on
//! macOS — see `lib.rs`); we still keep `#[cfg(target_os = "macos")]` on
//! the public functions so a stray non-macOS `mod dialog;` would still
//! fail to link rather than silently ship a broken stub.

use std::cell::OnceCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::config::{WindowDimensions, WindowDimensionsMode};
use crate::util::{DEFAULT_WINDOW_HEIGHT, DEFAULT_WINDOW_WIDTH};

#[cfg(target_os = "macos")]
use objc2::{msg_send, rc::Retained, runtime::AnyObject, MainThreadOnly};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSAlert, NSComboBox, NSImage};
#[cfg(target_os = "macos")]
use objc2_foundation::{ns_string, MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

/// `NSAlertFirstButtonReturn` — AppKit return code for "OK" (the first
/// button we add). Explicit constant because objc2-app-kit doesn't re-export
/// the named constant for our active feature set; `1000` is documented in
/// `<AppKit/NSAlert.h>` and stable since macOS 10.9. Typed as the platform
/// `NSInteger` (which `NSModalResponse` is just a type alias for) so the
/// equality compare against `runModal()`'s return value type-checks without
/// a cast.
#[cfg(target_os = "macos")]
const NS_ALERT_FIRST_BUTTON_RETURN: isize = 1000;

/// Frame width / height of the accessory `NSComboBox`. 500pt is wide enough
/// to show realistic URLs (deep paths, query strings, OAuth callbacks)
/// without horizontal scrolling; 28pt is the default `NSComboBox` height
/// (a touch taller than `NSTextField`'s 24pt because the dropdown chevron
/// adds vertical padding) so the alert layout looks native.
#[cfg(target_os = "macos")]
const ACCESSORY_FIELD_W: f64 = 1280.0;
#[cfg(target_os = "macos")]
const ACCESSORY_FIELD_H: f64 = 28.0;

/// How many history rows the `NSComboBox` dropdown reveals before scrolling.
/// `NSComboBox`'s default is 5; we lift it to 8 so users with a small but
/// growing history rarely have to scroll, while staying short enough that
/// the dropdown doesn't dominate the alert vertically.
#[cfg(target_os = "macos")]
const COMBOBOX_VISIBLE_ITEMS: isize = 8;

/// SF Symbol used as the alert icon. Replaces the default app-bundle icon
/// (the blue Pouch.app folder badge) which is loud and off-topic for a
/// "type a URL" prompt — `globe` reads as a generic, neutral "go to a web
/// address" affordance and matches the http(s)-only contract spelled out
/// in the informative text. Available on macOS 11+ (SF Symbols 1).
#[cfg(target_os = "macos")]
const ALERT_ICON_SYMBOL: &str = "globe";

/// Process-wide counter producing unique `WebviewWindow` labels for windows
/// created at runtime (the New Window dialog, plus the startup `startup_urls`
/// tail entries — both go through this counter so labels never collide).
/// The main window is always `"main"`; everything else is `"window-2"`,
/// `"window-3"`, ... in creation order.
///
/// Starts at 2 so the first dynamically-created window is `"window-2"` (the
/// "1" slot conceptually belongs to the main window — keeps the numbering
/// reading naturally).
static NEXT_LABEL: AtomicUsize = AtomicUsize::new(2);

/// Allocate the next unique window label. Called by both the startup
/// `startup_urls` tail iteration in `lib.rs::run` and the New Window menu
/// handler so neither path can hand out a duplicate label (Tauri rejects
/// duplicate labels in `WebviewWindowBuilder::build`).
pub fn next_window_label() -> String {
    let n = NEXT_LABEL.fetch_add(1, Ordering::SeqCst);
    format!("window-{n}")
}

thread_local! {
    /// Snapshot of `Config.window_dimensions` cached at startup so the Cmd+N
    /// New Window handler — which has only an `&AppHandle` and no access to
    /// the original `Config` — can apply the same `WindowDimensions` mode
    /// that the main window (and any extra windows from the startup
    /// `startup_urls` tail) used. We use a `thread_local` because both
    /// `cache_window_dimensions` (called from `lib.rs::run`) and
    /// `current_window_dimensions` (called from the menu handler / `NSAlert`
    /// callback path) run on the AppKit main thread.
    static CACHED_WINDOW_DIMENSIONS: OnceCell<WindowDimensions> = const { OnceCell::new() };
}

/// Cache the resolved `WindowDimensions` once at startup. Called from
/// `lib.rs::setup` exactly once with `cfg.window_dimensions`. Subsequent
/// `set` calls are silently ignored (`OnceCell::set` semantics) — there is
/// currently no path that re-issues this; reload is implemented as a full
/// process restart so the cached value is reset alongside everything else.
pub fn cache_window_dimensions(window_dimensions: WindowDimensions) {
    CACHED_WINDOW_DIMENSIONS.with(|c| {
        let _ = c.set(window_dimensions);
    });
}

/// Read the cached `WindowDimensions`, falling back to
/// `WindowDimensions::default()` (`Mode(Maximized)`) if
/// `cache_window_dimensions` was never called — defensive against a future
/// refactor that drops the setup-time cache call; the fallback matches what
/// the JSON loader picks for a missing `window_dimensions` field in
/// `hook.config.json`.
fn current_window_dimensions() -> WindowDimensions {
    CACHED_WINDOW_DIMENSIONS.with(|c| c.get().copied().unwrap_or_default())
}

/// Prompt the user for a URL and, on OK with a valid http(s) URL, open it
/// as an additional `WebviewWindow`. Bound to `File → New Window` (Cmd+N).
///
/// Failure modes (each is non-fatal; the dialog simply closes / no window
/// opens):
/// - User clicks Cancel or hits Escape → silent return.
/// - Submitted text is empty / not http(s) → warn log, no window.
/// - `WebviewWindowBuilder::build` fails (e.g. label collision, which
///   shouldn't happen given [`next_window_label`]) → warn log, no window.
#[cfg(target_os = "macos")]
pub fn show_new_window_dialog(app: &AppHandle) {
    let raw = match prompt_url_for_new_window() {
        Some(s) => s,
        None => return, // user cancelled
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }

    let url: url::Url = match trimmed.parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(
                target: "hook",
                "[new-window] URL parse failed for {:?}: {}",
                trimmed, e
            );
            return;
        }
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        tracing::warn!(
            target: "hook",
            "[new-window] URL is not http(s); ignoring: {}",
            url
        );
        return;
    }

    let label = next_window_label();
    let window_dimensions = current_window_dimensions();
    match open_extra_window(app, &label, url, window_dimensions) {
        Ok(_) => {
            // Only record on success: a failed `build()` (e.g. label
            // collision, which shouldn't happen given `next_window_label`)
            // shouldn't pollute the dropdown with a URL the user can't
            // actually visit. We persist the user's original input rather
            // than the parsed `url::Url::to_string()` so the dropdown
            // reflects exactly what they typed (e.g. preserving the
            // trailing slash that `Url` would synthesise).
            crate::recent_urls::add_recent_url(trimmed);
        }
        Err(e) => {
            tracing::warn!(
                target: "hook",
                "[new-window] failed to create window {label}: {e}"
            );
        }
    }
}

/// Cmd+N "New Window" entry point — runs the modal `NSAlert` titled "New
/// Window" and returns the user-typed (or selected-from-history) URL. See
/// [`prompt_url_via_alert`] for the shared dialog implementation and
/// failure-mode contract.
#[cfg(target_os = "macos")]
fn prompt_url_for_new_window() -> Option<String> {
    prompt_url_via_alert("New Window", "Enter a URL (http or https):")
}

/// First-launch entry point — used by `lib.rs::run` when the resolved
/// `startup_urls` list is empty (config file missing / field omitted / every
/// entry was non-http(s)). Wraps the same shared `NSAlert` implementation
/// with a "Welcome to Pouch" framing so the user understands this is the
/// initial bootstrap rather than a regular New-Window prompt. On Cancel /
/// Escape `lib.rs::run` calls `std::process::exit(0)` rather than spawning
/// any window — see the matching arm in `lib.rs`.
#[cfg(target_os = "macos")]
pub fn prompt_initial_url() -> Option<String> {
    prompt_url_via_alert("Welcome to Pouch", "Enter a URL to open:")
}

/// Run the modal `NSAlert` and return the contents of its accessory
/// `NSComboBox` on OK, or `None` on Cancel / non-main-thread / a failed
/// `MainThreadMarker::new` (the menu handler always runs on the main thread,
/// so the latter never fires in practice).
///
/// `title` becomes the alert's `messageText` (bold heading) and `info`
/// becomes its `informativeText` (smaller body). Parameterising both lets a
/// single AppKit implementation back both the regular Cmd+N "New Window"
/// prompt and the first-launch "Welcome to Pouch" prompt without duplicating
/// the `NSComboBox` / icon / button wiring below.
#[cfg(target_os = "macos")]
fn prompt_url_via_alert(title: &str, info: &str) -> Option<String> {
    let mtm = MainThreadMarker::new()?;
    let title_ns = NSString::from_str(title);
    let info_ns = NSString::from_str(info);
    unsafe {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&title_ns);
        alert.setInformativeText(&info_ns);

        // Replace the default app-bundle icon (loud blue Pouch folder
        // badge) with a neutral SF Symbol that reads as "go to a web
        // address". Falls through to the AppKit default icon if the symbol
        // isn't available (macOS < 11 — SF Symbols shipped in macOS 11).
        let symbol = NSString::from_str(ALERT_ICON_SYMBOL);
        let a11y = ns_string!("URL");
        if let Some(icon) =
            NSImage::imageWithSystemSymbolName_accessibilityDescription(&symbol, Some(a11y))
        {
            alert.setIcon(Some(&icon));
        }

        // Editable combobox: behaves like an `NSTextField` for typing
        // (same defaults — editable, single-line, bezeled — because
        // `NSComboBox` inherits from `NSTextField`) but exposes a dropdown
        // chevron showing the user's recent URLs as quick-pick rows.
        let field_frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(ACCESSORY_FIELD_W, ACCESSORY_FIELD_H),
        );
        let combobox: Retained<NSComboBox> =
            NSComboBox::initWithFrame(NSComboBox::alloc(mtm), field_frame);
        combobox.setNumberOfVisibleItems(COMBOBOX_VISIBLE_ITEMS);

        // Populate the dropdown with the persisted history (most-recent
        // first — see `recent_urls::load_recent_urls`). Each
        // `addItemWithObjectValue:` call takes any `id` (Objective-C
        // object reference); we pass `NSString`s, the natural object value
        // for a URL-typed combobox. We hold the `NSString`s in a `Vec` so
        // the AppKit retain count is the only thing keeping them alive
        // after the call returns — the locals would otherwise drop at the
        // end of each loop iteration before AppKit could observe the
        // strings. (Each `addItemWithObjectValue:` retains internally, so
        // the `Vec` could in principle drop before `runModal`; keeping it
        // until the end of the unsafe block is the conservatively-safe
        // choice and costs nothing.)
        let recent = crate::recent_urls::load_recent_urls();
        let _ns_recent: Vec<Retained<NSString>> = recent
            .iter()
            .map(|url| {
                let ns = NSString::from_str(url);
                combobox.addItemWithObjectValue(&ns);
                ns
            })
            .collect();

        // `setAccessoryView:` takes `Option<&NSView>`. `NSComboBox` →
        // `NSTextField` → `NSControl` → `NSView`, so the upcast goes
        // through objc2's `Deref` chain just like the previous
        // `NSTextField` accessory did.
        alert.setAccessoryView(Some(&combobox));

        // First button = "Open" → bound to NS_ALERT_FIRST_BUTTON_RETURN
        // (1000). Second = "Cancel" → bound to NSAlertSecondButtonReturn
        // (1001) and gets the Escape key equivalent automatically because
        // its title is exactly "Cancel" (AppKit convention).
        let _: Retained<AnyObject> = msg_send![&*alert, addButtonWithTitle: ns_string!("Open")];
        let _: Retained<AnyObject> = msg_send![&*alert, addButtonWithTitle: ns_string!("Cancel")];

        // `runModal` returns `NSModalResponse` (a typedef for `NSInteger` /
        // Rust `isize`); compare directly against the OK return code.
        let response = alert.runModal();
        if response != NS_ALERT_FIRST_BUTTON_RETURN {
            return None;
        }

        // `stringValue` is inherited from `NSTextField` — returns whatever
        // the user typed (or selected from the dropdown, which AppKit
        // copies into the field on selection).
        let value: Retained<NSString> = combobox.stringValue();
        Some(value.to_string())
    }
}

/// Build a `WebviewWindow` for an extra URL. Shared by the startup
/// `startup_urls`-config tail path (`lib.rs::run`) and the New Window dialog so
/// both go through the same defaults: devtools enabled, native title-sync,
/// no fixed title (lets upstream `<title>` win on first paint), and the
/// shared `DEFAULT_WINDOW_{WIDTH,HEIGHT}` baseline so extra windows match
/// the main window's default sizing instead of falling through to wry's
/// 800x600 platform default — see [`crate::util::DEFAULT_WINDOW_WIDTH`].
///
/// `window_dimensions` carries the same `WindowDimensions` the main window
/// uses so extra windows (whether spawned from the startup `startup_urls`
/// tail or via Cmd+N) honour `hook.config.json -> window_dimensions`
/// (Default / Maximized / Fullscreen / Size) identically to the main window
/// — the maximize / fullscreen / fixed-size match below mirrors
/// `create_main_window_with_url` in `lib.rs` exactly.
///
/// Cookies / storage are shared with the main window — Tauri v2's default
/// is one shared `WKWebViewConfiguration` / `ICoreWebView2Environment` per
/// process, which keeps the WebKit data store / WebView2 user-data folder
/// shared across windows.
pub fn open_extra_window(
    app: &AppHandle,
    label: &str,
    url: url::Url,
    window_dimensions: WindowDimensions,
) -> tauri::Result<tauri::WebviewWindow> {
    let url_for_log = url.to_string();
    let mut builder = WebviewWindowBuilder::new(app, label, WebviewUrl::External(url))
        // Set the loading title at builder time so the NSWindow / HWND is
        // born with `⏳ Loading...` as its initial title — closes the visual
        // gap between window creation and the first post-build `set_title`,
        // where the user could otherwise glimpse the default label-derived
        // title for a frame. The post-build `set_title` below is kept as a
        // redundant fallback in case `.title` silently no-ops on some
        // platform.
        .title(crate::LOADING_TITLE)
        .resizable(true)
        .devtools(true)
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
        .on_page_load(crate::page_load_handler());

    // Mirror `lib.rs::setup`'s mode-application match. We always set
    // `fullscreen` and `maximized` explicitly so the extra window's mode is
    // deterministic (never inheriting wry leftover state) and `inner_size`
    // is set on every branch so an un-maximize / un-fullscreen gesture
    // restores to a sensible 1280x960 instead of wry's 800x600 default.
    builder = match window_dimensions {
        WindowDimensions::Mode(WindowDimensionsMode::Default) => builder
            .fullscreen(false)
            .maximized(false)
            .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT),
        WindowDimensions::Mode(WindowDimensionsMode::Inherit) => crate::apply_inherit_mode(builder),
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
                "[extra-window] window size {{ width: {}, height: {} }} has a zero dimension; falling back to default (maximized)",
                width,
                height
            );
            builder
                .fullscreen(false)
                .maximized(true)
                .inner_size(DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT)
        }
    };

    let window = builder.build()?;
    tracing::debug!(
        target: "hook",
        "[window] created label={} url={}",
        label,
        url_for_log
    );
    // Set the loading title prefix immediately on window creation so the
    // user sees feedback the moment the window appears — the page-load
    // `Started` event only fires after the WKWebView has received
    // navigation first-byte, which can lag the window's first paint by
    // hundreds of ms (webview process spin-up + DNS / TLS / server
    // response). The shared `on_page_load(Started)` handler re-sets the
    // same title later (idempotent), and `Finished` only swaps it for
    // the host-derived fallback if the page never produced a `<title>`
    // — see [`crate::page_load_handler`].
    if let Err(e) = window.set_title(crate::LOADING_TITLE) {
        tracing::warn!(
            target: "hook",
            "[new-window] initial set_title(loading) failed for {label}: {e}"
        );
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(w) = app.get_webview_window(label) {
            if let Err(e) = crate::titlebar::install_titlebar_accessory(app, &w) {
                tracing::warn!(
                    target: "hook",
                    "[new-window] titlebar accessory install failed for {label}: {e}"
                );
            }
        }
    }

    // Cross-platform: persist this extra window's resize / move events into
    // `storage.json` for the next session's `inherit`-mode restore. Mirrors
    // the main-window hookup in `lib.rs::create_main_window_with_url`.
    crate::install_window_state_listener(&window);

    Ok(window)
}
