//! macOS titlebar accessory: three small SF Symbol buttons on the right of
//! the window's titlebar.
//!
//! Visual order (left to right):
//!
//!   1. `folder` — Reveal Pouch folder in Finder. Pairs with
//!      `View → Reveal Pouch Folder in Finder` (Cmd+Shift+O); both call
//!      [`crate::util::reveal_pouch_folder`].
//!   2. `arrow.clockwise` — Reload. Pairs with `View → Reload from Config`
//!      (Cmd+R); both call [`crate::reload_from_config`] which re-reads
//!      `hook.config.json`, atomically swaps in the freshly-compiled
//!      `ignore_urls` rule set, rescans `inject/*.js`, and recreates the
//!      main webview window so the dispatcher init-script picks up edits
//!      to inject files — see README §2.5.
//!   3. `wrench.and.screwdriver` / `wrench.and.screwdriver.fill` — Toggle
//!      DevTools. Pairs with the `View → Open DevTools` (F12) menubar entry.
//!      The button's image swaps between the outlined symbol (DevTools
//!      currently closed) and the filled symbol (DevTools currently open) so
//!      the icon doubles as a state indicator. All three triggers (this
//!      button, F12, the menubar entry) call [`update_devtools_button_image`]
//!      after flipping state so the icon stays in sync regardless of which
//!      one fired the toggle.
//!
//! # Why the dance
//!
//! `WebviewWindow::ns_window()` hands back a `*mut c_void` autoreleased
//! NSWindow pointer (see Tauri 2.9 `tauri::window::Window::ns_window` —
//! returns `Retained::autorelease_ptr(ns_window).cast()`). We have to:
//!
//! 1. Be on the main thread (AppKit requirement) — the caller (the `setup`
//!    closure) already runs there, so no extra hop is needed.
//! 2. Cast that pointer back to `&NSWindow` without dropping the
//!    autoreleased ref (we don't own a +1; we borrow for the duration of
//!    install).
//! 3. Build an `NSTitlebarAccessoryViewController`, wrap three `NSButton`s
//!    (image-only SF Symbols) inside, and wire each button's
//!    `target`/`action` to a tiny declared subclass that calls back into
//!    Rust. Add the accessory to the window with layout = Right.
//!
//! Lives only on macOS — `lib.rs` cfg-gates the entire `mod titlebar;`
//! declaration.
//!
//! # AppHandle bridge
//!
//! The reload and devtools actions both need the Tauri `AppHandle` to do
//! their work (`AppHandle::restart` and `WebviewWindow::open_devtools`
//! respectively). `ButtonHandler` itself is `MainThreadOnly` and can't
//! carry an Rc/Arc field through `define_class!`'s ivar machinery without
//! significantly more boilerplate, so we stash the handle in a
//! main-thread-local `OnceCell` at install time. AppKit invokes target /
//! action on the main thread, so the thread-local is read on the same
//! thread that wrote it. `AppHandle` is `Clone + Send + Sync`, so storing
//! a clone is cheap and correct.

use std::cell::OnceCell;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, MainThreadOnly};
use objc2_app_kit::{
    NSButton, NSImage, NSLayoutAttribute, NSTitlebarAccessoryViewController, NSView, NSWindow,
};
use objc2_foundation::{ns_string, MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use tauri::{AppHandle, Manager};

use crate::util::reveal_pouch_folder;

/// SF Symbol name used for the DevTools button when DevTools are **closed**
/// (outlined wrench-and-screwdriver glyph).
const DEVTOOLS_SYMBOL_CLOSED: &str = "wrench.and.screwdriver";
/// SF Symbol name used for the DevTools button when DevTools are **open**
/// (filled wrench-and-screwdriver glyph) — doubles as a state indicator so
/// the user can tell at a glance whether DevTools are currently visible.
const DEVTOOLS_SYMBOL_OPEN: &str = "wrench.and.screwdriver.fill";
/// Accessibility / tooltip label shared between the two DevTools symbol
/// variants. Kept identical because the underlying action (toggle) is the
/// same regardless of state — the visual swap conveys the state delta.
const DEVTOOLS_A11Y_LABEL: &str = "Toggle DevTools";

/// Frame of the accessory view's contentView in points. Apple's HIG-leaning
/// titlebar accessories are usually ~28pt tall (matching toolbar buttons).
/// Each button gets 36pt of width — comfortable hit target for an SF Symbol
/// without crowding the traffic-light controls — and 4pt of horizontal
/// spacing separates adjacent buttons.
const BUTTON_W: f64 = 36.0;
const BUTTON_H: f64 = 28.0;
const BUTTON_SPACING: f64 = 4.0;
const BUTTON_COUNT: f64 = 3.0;
const ACCESSORY_W: f64 = BUTTON_W * BUTTON_COUNT + BUTTON_SPACING * (BUTTON_COUNT - 1.0);
const ACCESSORY_H: f64 = BUTTON_H;

// =====================================================================
// ButtonHandler — declared NSObject subclass owning the buttons' actions.
// =====================================================================
//
// Each button's target/action pair (target=instance, action=@selector(...))
// is the simplest available callback bridge in objc2. We avoid `block2`
// here because NSButton's target-action wiring is built around `Sel`, not
// blocks — adopting a block would require subclassing `NSControl` to gain
// a block-aware action, which is far more code than the trivial declared
// class below.
//
// The class is defined as `MainThreadOnly` because:
//   - NSButton actions only ever fire on the main thread (AppKit invariant),
//     so a hypothetical ivar wouldn't need synchronisation;
//   - `MainThreadOnly` enforces that statically — `ButtonHandler::new(mtm)`
//     is the only way to allocate one, and `mtm` is only obtainable on the
//     main thread.

define_class!(
    /// Tiny NSObject subclass whose only job is to receive the
    /// `revealFolder:` / `reload:` / `openDevTools:` actions from the three
    /// titlebar `NSButton`s and dispatch each into the appropriate Rust /
    /// Tauri call. No ivars — the handlers reach for the AppHandle via the
    /// `APP_HANDLE` thread-local.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "PouchButtonHandler"]
    struct ButtonHandler;

    unsafe impl NSObjectProtocol for ButtonHandler {}

    impl ButtonHandler {
        /// `-revealFolder:` — bound to the "folder" SF Symbol button.
        /// `_sender` is the `NSButton`; we don't need it.
        #[unsafe(method(revealFolder:))]
        fn reveal_folder(&self, _sender: Option<&AnyObject>) {
            if let Err(e) = reveal_pouch_folder() {
                tracing::warn!(
                    target: "hook",
                    "[titlebar] reveal_pouch_folder failed: {e}"
                );
            }
        }

        /// `-reload:` — bound to the "arrow.clockwise" SF Symbol button.
        /// Re-reads `hook.config.json` (atomically swapping in the new
        /// `ignore_urls` rule set), rescans `inject/*.js`, and recreates
        /// the main webview window so the dispatcher init-script reflects
        /// any inject-file edits. Visually presents as a brief
        /// disappearance / reappearance of the window. See
        /// [`crate::reload_from_config`] / README §2.5.
        #[unsafe(method(reload:))]
        fn reload(&self, _sender: Option<&AnyObject>) {
            APP_HANDLE.with(|cell| {
                if let Some(app) = cell.get() {
                    crate::reload_from_config(app);
                } else {
                    tracing::warn!(
                        target: "hook",
                        "[titlebar] reload: AppHandle not initialised; skipping"
                    );
                }
            });
        }

        /// `-toggleDevTools:` — bound to the wrench-and-screwdriver SF
        /// Symbol button. Mirrors the `View → Open DevTools` (F12) menubar
        /// entry. Toggles the Web Inspector's visibility; on each click
        /// also swaps the button's icon between outlined / filled variants
        /// so the icon doubles as a state indicator.
        ///
        /// `is_devtools_open` / `close_devtools` require either
        /// `debug_assertions` or the `devtools` Cargo feature on the `tauri`
        /// crate; pouch enables `devtools` unconditionally (see Cargo.toml)
        /// so the symbols are always present.
        ///
        /// Windows note: `close_devtools` is a no-op on Windows in Tauri
        /// 2.9.5 (per upstream docs), but this entire titlebar accessory
        /// is macOS-only (`#[cfg(target_os = "macos")]` on the `mod
        /// titlebar;` declaration in `lib.rs`), so the toggle is only
        /// reachable from macOS where both APIs work.
        #[unsafe(method(toggleDevTools:))]
        fn toggle_devtools(&self, _sender: Option<&AnyObject>) {
            APP_HANDLE.with(|cell| {
                let Some(app) = cell.get() else {
                    tracing::warn!(
                        target: "hook",
                        "[titlebar] toggleDevTools: AppHandle not initialised; skipping"
                    );
                    return;
                };
                let Some(window) = app.get_webview_window("main") else {
                    tracing::warn!(
                        target: "hook",
                        "[titlebar] toggleDevTools: main webview not found"
                    );
                    return;
                };
                let was_open = window.is_devtools_open();
                if was_open {
                    window.close_devtools();
                } else {
                    window.open_devtools();
                }
                update_devtools_button_image(!was_open);
            });
        }
    }
);

impl ButtonHandler {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let alloc = Self::alloc(mtm);
        unsafe { msg_send![alloc, init] }
    }
}

// Keep the handler instance alive for the process lifetime: NSButton's
// `target` is a *weak* reference (per AppKit docs and the objc2-app-kit
// `setTarget:` Safety note), so if we let the `Retained` drop after install
// the next click would dereference garbage. Stuffing it into a `OnceCell`
// inside a thread-local cell avoids forcing `Sync` on a non-`Sync`
// `Retained<ButtonHandler>`. Because the install path is main-thread-only
// (ButtonHandler is `MainThreadOnly`, and we're called from `setup`), the
// thread-local is accessed exactly once per process.
thread_local! {
    static HANDLER: OnceCell<Retained<ButtonHandler>> = const { OnceCell::new() };

    /// AppHandle bridge for the reload / devtools actions. Populated by
    /// [`install_titlebar_accessory`] and read by `ButtonHandler` selectors.
    /// Main-thread-local because every reader (button action) and the
    /// single writer (the `setup` closure) all run on the main thread.
    static APP_HANDLE: OnceCell<AppHandle> = const { OnceCell::new() };

    /// Retained reference to the DevTools `NSButton` so its image can be
    /// swapped (outlined ↔ filled) when the button itself toggles the
    /// inspector. `Retained` because we don't own a +1 from `add_symbol_button`
    /// (NSButton's host `NSView` retains it on `addSubview:`); we explicitly
    /// `retain` an extra reference for our own bookkeeping. AppKit guarantees
    /// the button's lifetime is at least the window's lifetime, but holding
    /// our own retain is the simplest correct shape.
    static DEVTOOLS_BUTTON: OnceCell<Retained<NSButton>> = const { OnceCell::new() };
}

/// Install the titlebar accessory on `ns_window_ptr`. Idempotent across
/// calls but only ever invoked once today (from `lib.rs::setup`).
///
/// # Safety
///
/// `ns_window_ptr` MUST be a valid `NSWindow *` (autoreleased — we treat
/// it as a borrow). Tauri's `WebviewWindow::ns_window()` is the only
/// caller path we wire up.
unsafe fn install_on_ns_window(ns_window_ptr: *mut c_void, mtm: MainThreadMarker) {
    if ns_window_ptr.is_null() {
        tracing::warn!(target: "hook", "[titlebar] ns_window pointer was null; skipping");
        return;
    }
    // Borrow the autoreleased NSWindow — do NOT take ownership; Tauri's
    // autorelease pool drains it after `setup` returns.
    let window: &NSWindow = unsafe { &*(ns_window_ptr as *mut NSWindow) };

    // Handler instance (held for process lifetime — see HANDLER docs).
    let handler = HANDLER.with(|cell| cell.get_or_init(|| ButtonHandler::new(mtm)).clone());
    // `Retained<ButtonHandler>` derefs to `ButtonHandler`, which inherits
    // from `NSObject`; cast that pointer to `&AnyObject` for the
    // target-action API.
    let target_obj: &AnyObject =
        unsafe { &*((&*handler) as *const ButtonHandler as *const AnyObject) };

    // Container view sized to ACCESSORY_W × ACCESSORY_H — the three
    // buttons are positioned manually inside it. NSStackView would also
    // work but adds an auto-layout dependency and complicates the frame
    // math we already need anyway, so we keep it explicit.
    let content_frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(ACCESSORY_W, ACCESSORY_H),
    );
    let content: Retained<NSView> = NSView::initWithFrame(NSView::alloc(mtm), content_frame);

    // ---- Button 1: Reveal Folder ----
    add_symbol_button(
        &content,
        target_obj,
        mtm,
        0.0,
        ns_string!("folder"),
        ns_string!("Reveal Pouch Folder in Finder"),
        ns_string!("Reveal Pouch Folder in Finder (Cmd+Shift+O)"),
        sel!(revealFolder:),
    );

    // ---- Button 2: Reload ----
    add_symbol_button(
        &content,
        target_obj,
        mtm,
        BUTTON_W + BUTTON_SPACING,
        ns_string!("arrow.clockwise"),
        ns_string!("Reload from Config"),
        ns_string!("Reload from Config (Cmd+R)"),
        sel!(reload:),
    );

    // ---- Button 3: Toggle DevTools ----
    // SF Symbol "wrench.and.screwdriver" (outlined) reads as "tools"
    // instantly to anyone who's used Safari / Xcode; available on macOS 11+
    // which is the same baseline the rest of this file already requires
    // (the existing "folder" symbol is also macOS-11-only). The filled
    // variant ("wrench.and.screwdriver.fill") is swapped in when DevTools
    // are open — see [`update_devtools_button_image`].
    add_devtools_button(
        &content,
        target_obj,
        mtm,
        (BUTTON_W + BUTTON_SPACING) * 2.0,
    );

    // Accessory view controller. `setView` requires the NSView feature.
    let accessory: Retained<NSTitlebarAccessoryViewController> = unsafe {
        let alloc = NSTitlebarAccessoryViewController::alloc(mtm);
        msg_send![alloc, init]
    };
    accessory.setView(&content);
    accessory.setLayoutAttribute(NSLayoutAttribute::Right);

    // Add to NSWindow. `addTitlebarAccessoryViewController:` isn't in
    // objc2-app-kit's typed API surface for our active feature set
    // (the property getter/setter `titlebarAccessoryViewControllers`
    // is, but the convenience `add` method comes from a category that
    // isn't gated on by default), so we call it via `msg_send!`.
    // Safe: the selector exists on every macOS >= 10.10
    // (NSTitlebarAccessoryViewController landed in 10.10).
    let _: () = unsafe {
        msg_send![window, addTitlebarAccessoryViewController: &*accessory]
    };
}

/// Helper: build one image-only NSButton wired to `target`/`action`, place
/// it at `(x_origin, 0)` inside `content`, and register `tooltip` on hover.
/// Returns nothing; on failure to resolve the SF Symbol the button is
/// simply skipped and a warning is logged (matches the previous single-
/// button behaviour — failures are non-fatal because the menubar still
/// has paired entries for every button).
///
/// The 8 args are all independently meaningful (NSView host, ObjC target,
/// MainThreadMarker, layout x, three distinct strings, and the action
/// selector); bundling them into an ad-hoc struct would obscure the
/// per-call differences at the call sites without paying for itself.
#[allow(clippy::too_many_arguments)]
fn add_symbol_button(
    content: &NSView,
    target: &AnyObject,
    mtm: MainThreadMarker,
    x_origin: f64,
    symbol_name: &NSString,
    a11y_label: &NSString,
    tooltip: &NSString,
    action: Sel,
) {
    // SF Symbols are a system asset on macOS 11+.
    let Some(image) =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(symbol_name, Some(a11y_label))
    else {
        tracing::warn!(
            target: "hook",
            "[titlebar] SF Symbol unavailable (macOS < 11?); skipping button"
        );
        return;
    };

    let button = unsafe {
        NSButton::buttonWithImage_target_action(&image, Some(target), Some(action), mtm)
    };
    button.setFrame(NSRect::new(
        NSPoint::new(x_origin, 0.0),
        NSSize::new(BUTTON_W, BUTTON_H),
    ));
    button.setBordered(false);
    // toolTip is on NSView (button.setToolTip is the inherited setter).
    // Hover delay is the system default (~half a second).
    button.setToolTip(Some(tooltip));
    content.addSubview(&button);
}

/// Build the DevTools toggle button (initial outlined symbol — DevTools start
/// closed) and stash a `Retained` clone of it in `DEVTOOLS_BUTTON` so
/// [`update_devtools_button_image`] can later swap its image. Wires the
/// `toggleDevTools:` selector. Failures (SF Symbol unavailable on macOS < 11)
/// are non-fatal — the menubar entry / F12 still toggle DevTools.
fn add_devtools_button(content: &NSView, target: &AnyObject, mtm: MainThreadMarker, x_origin: f64) {
    let symbol_name = NSString::from_str(DEVTOOLS_SYMBOL_CLOSED);
    let a11y_label = NSString::from_str(DEVTOOLS_A11Y_LABEL);
    let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &symbol_name,
        Some(&a11y_label),
    ) else {
        tracing::warn!(
            target: "hook",
            "[titlebar] DevTools SF Symbol unavailable (macOS < 11?); skipping button"
        );
        return;
    };

    let button = unsafe {
        NSButton::buttonWithImage_target_action(&image, Some(target), Some(sel!(toggleDevTools:)), mtm)
    };
    button.setFrame(NSRect::new(
        NSPoint::new(x_origin, 0.0),
        NSSize::new(BUTTON_W, BUTTON_H),
    ));
    button.setBordered(false);
    button.setToolTip(Some(ns_string!("Toggle DevTools (F12)")));
    content.addSubview(&button);

    DEVTOOLS_BUTTON.with(|cell| {
        // First call wins; later calls (if `install_titlebar_accessory` ever
        // runs again on the same thread) silently keep the original button.
        let _ = cell.set(button);
    });
}

/// Swap the DevTools button's image to reflect `is_open`. Called from
/// [`ButtonHandler::toggle_devtools`] right after the toggle so the icon
/// follows the inspector's actual visibility, and from `lib.rs::on_menu_event`
/// so the F12 / `View → Open DevTools` paths keep the icon in sync as well.
/// Silent no-op when the button hasn't been installed (e.g. SF Symbol
/// unavailable at boot) or when the requested symbol can't be resolved.
pub fn update_devtools_button_image(is_open: bool) {
    let symbol_name = if is_open {
        DEVTOOLS_SYMBOL_OPEN
    } else {
        DEVTOOLS_SYMBOL_CLOSED
    };
    let symbol_ns = NSString::from_str(symbol_name);
    let a11y_ns = NSString::from_str(DEVTOOLS_A11Y_LABEL);
    let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &symbol_ns,
        Some(&a11y_ns),
    ) else {
        tracing::warn!(
            target: "hook",
            "[titlebar] update_devtools_button_image: SF Symbol {:?} unavailable",
            symbol_name
        );
        return;
    };
    DEVTOOLS_BUTTON.with(|cell| {
        if let Some(button) = cell.get() {
            button.setImage(Some(&image));
        }
    });
}

/// Public entry point used by `lib.rs::setup`. Resolves the NSWindow handle
/// from the Tauri webview window, stashes the AppHandle for the
/// reload / devtools button actions, and forwards to
/// [`install_on_ns_window`].
pub fn install_titlebar_accessory(
    app: &AppHandle,
    window: &tauri::WebviewWindow,
) -> tauri::Result<()> {
    let mtm = MainThreadMarker::new()
        .expect("install_titlebar_accessory must be called on the main thread");

    // Stash the AppHandle for the reload / devtools button actions.
    // `set` returns Err if already initialised; that's only possible on a
    // hypothetical second call (today we install once from `setup`), and
    // re-using the existing handle is safe, so we ignore the error.
    APP_HANDLE.with(|cell| {
        let _ = cell.set(app.clone());
    });

    let ns_window = window.ns_window()?;
    // SAFETY: Tauri's contract on `ns_window()` returns a valid autoreleased
    // NSWindow pointer; we borrow it without taking ownership.
    unsafe { install_on_ns_window(ns_window, mtm) };
    Ok(())
}
