//! macOS titlebar accessory: three small SF Symbol buttons on the right of
//! every window's titlebar (main + extras).
//!
//! Visual order (left to right):
//!
//!   1. `folder` — Reveal Pouch folder in Finder. Pairs with
//!      `View → Reveal Pouch Folder in Finder` (Cmd+Shift+O); both call
//!      [`crate::util::reveal_pouch_folder`]. Process-global action — same
//!      behaviour regardless of which window's button you click.
//!   2. `arrow.clockwise` — Reload. Pairs with `View → Reload from Config`
//!      (Cmd+R); both call [`crate::reload_from_config`] which restarts the
//!      whole process via [`tauri::AppHandle::restart`] — see README §2.5.
//!      Process-global action — same behaviour regardless of which window's
//!      button you click.
//!   3. `wrench.and.screwdriver` / `wrench.and.screwdriver.fill` — Toggle
//!      DevTools for **this window**. Pairs with the `View → Open DevTools`
//!      (F12) menubar entry, which targets the focused window. The button's
//!      image swaps between the outlined symbol (DevTools currently closed)
//!      and the filled symbol (DevTools currently open) so the icon doubles
//!      as a state indicator. All three triggers (this button, F12, the
//!      menubar entry) call [`update_devtools_button_image`] for the right
//!      label after flipping state so the icon stays in sync regardless of
//!      which one fired the toggle.
//!
//! # Per-window plumbing
//!
//! In multi-window builds (main window + `windows` config + Cmd+N) we need
//! the DevTools toggle to act on the **clicked window**, not always on
//! `"main"`. To avoid declaring a separate Objective-C class per window we
//! give `ButtonHandler` a single `RefCell<String>` ivar holding the window
//! label, and instantiate one handler per window at install time. The
//! AppHandle is still shared (process-global) so we keep that in a thread
//! local.
//!
//! `DEVTOOLS_BUTTONS` is a `HashMap<String, Retained<NSButton>>` keyed by
//! window label so [`update_devtools_button_image`] can look up the right
//! button from any code path that knows the label (the menubar handler in
//! `lib.rs` looks up the focused window's label and forwards to it).

use std::cell::OnceCell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
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
// One instance per window. The single `window_label` ivar lets each window's
// DevTools button toggle DevTools on its own webview without us declaring
// a separate Obj-C class per window.

/// Ivars carried by every `ButtonHandler` instance. Declared as a separate
/// type because `define_class!` in objc2 0.6 takes `#[ivars = TypeName]`
/// and does not accept inline struct fields.
///
/// `label` is wrapped in `RefCell` because objc2's class ivars are only
/// mutable through interior mutability (selectors take `&self`); we set it
/// once in [`ButtonHandler::new`] and only ever borrow it immutably
/// afterwards, but `RefCell` is the cheapest "owns a `String`" wrapper
/// that satisfies the contract (`Cell` would force `Copy`, which `String`
/// isn't).
struct ButtonHandlerIvars {
    label: RefCell<String>,
}

define_class!(
    /// NSObject subclass receiving the `revealFolder:` / `reload:` /
    /// `toggleDevTools:` actions. The DevTools handler reads the
    /// `ButtonHandlerIvars::label` ivar (via `self.ivars()`) to dispatch
    /// into the right webview; reveal / reload are process-global and
    /// ignore the ivar.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "PouchButtonHandler"]
    #[ivars = ButtonHandlerIvars]
    struct ButtonHandler;

    unsafe impl NSObjectProtocol for ButtonHandler {}

    impl ButtonHandler {
        /// `-revealFolder:` — process-global; ignores the window label ivar.
        #[unsafe(method(revealFolder:))]
        fn reveal_folder(&self, _sender: Option<&AnyObject>) {
            if let Err(e) = reveal_pouch_folder() {
                tracing::warn!(
                    target: "hook",
                    "[titlebar] reveal_pouch_folder failed: {e}"
                );
            }
        }

        /// `-reload:` — process-global; ignores the window label ivar.
        /// Restarts the whole application — see [`crate::reload_from_config`]
        /// / README §2.5.
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

        /// `-toggleDevTools:` — per-window; reads the `label` ivar to
        /// pick the right webview. Mirrors the `View → Open DevTools` (F12)
        /// menubar entry, which targets the focused window.
        #[unsafe(method(toggleDevTools:))]
        fn toggle_devtools(&self, _sender: Option<&AnyObject>) {
            let label = self.ivars().label.borrow().clone();
            APP_HANDLE.with(|cell| {
                let Some(app) = cell.get() else {
                    tracing::warn!(
                        target: "hook",
                        "[titlebar] toggleDevTools: AppHandle not initialised; skipping"
                    );
                    return;
                };
                let Some(window) = app.get_webview_window(&label) else {
                    tracing::warn!(
                        target: "hook",
                        "[titlebar] toggleDevTools: webview {:?} not found",
                        label
                    );
                    return;
                };
                let was_open = window.is_devtools_open();
                if was_open {
                    window.close_devtools();
                } else {
                    window.open_devtools();
                }
                update_devtools_button_image(&label, !was_open);
            });
        }
    }
);

impl ButtonHandler {
    fn new(mtm: MainThreadMarker, label: String) -> Retained<Self> {
        let alloc = Self::alloc(mtm).set_ivars(ButtonHandlerIvars {
            label: RefCell::new(label),
        });
        unsafe { msg_send![super(alloc), init] }
    }
}

// Per-window handler instances and per-window DevTools button references.
// Both are `MainThreadOnly` (handler instances) or strictly main-thread
// (button references) so a thread-local is the simplest correct shape.
//
// We keep `Retained<ButtonHandler>` alive for the process lifetime: NSButton's
// `target` is a *weak* reference (per AppKit docs and the objc2-app-kit
// `setTarget:` Safety note), so dropping the `Retained` would dangle the
// next click. The map keyed by label means main-window + per-extra-window
// handlers all live until process exit.
thread_local! {
    /// Per-window `ButtonHandler` instances, keyed by window label. Holds
    /// the strong retain so target-action wiring stays valid.
    static HANDLERS: RefCell<HashMap<String, Retained<ButtonHandler>>> = RefCell::new(HashMap::new());

    /// AppHandle bridge for the reload / devtools actions. Populated by
    /// the first [`install_titlebar_accessory`] call and read by every
    /// `ButtonHandler` selector. Main-thread-local because every reader
    /// (button action) and writer (the install path) all run on the main
    /// thread.
    static APP_HANDLE: OnceCell<AppHandle> = const { OnceCell::new() };

    /// Per-window DevTools `NSButton` references so
    /// [`update_devtools_button_image`] can swap the icon (outlined ↔ filled)
    /// for the right window. Retained because we don't own a +1 from
    /// `add_devtools_button` (NSButton's host NSView retains it on
    /// `addSubview:`); we explicitly retain an extra reference for our own
    /// bookkeeping.
    static DEVTOOLS_BUTTONS: RefCell<HashMap<String, Retained<NSButton>>> = RefCell::new(HashMap::new());
}

/// Install the titlebar accessory on `ns_window_ptr` for the given window
/// label. Idempotent across calls but typically only invoked once per
/// window from `lib.rs::setup` (or the New Window dialog).
///
/// # Safety
///
/// `ns_window_ptr` MUST be a valid `NSWindow *` (autoreleased — we treat
/// it as a borrow). Tauri's `WebviewWindow::ns_window()` is the only
/// caller path we wire up.
unsafe fn install_on_ns_window(
    ns_window_ptr: *mut c_void,
    mtm: MainThreadMarker,
    label: &str,
) {
    if ns_window_ptr.is_null() {
        tracing::warn!(target: "hook", "[titlebar] ns_window pointer was null; skipping");
        return;
    }
    // Borrow the autoreleased NSWindow — do NOT take ownership; Tauri's
    // autorelease pool drains it after `setup` returns.
    let window: &NSWindow = unsafe { &*(ns_window_ptr as *mut NSWindow) };

    // One handler instance per window — held in `HANDLERS` for process
    // lifetime so target-action wiring stays valid.
    let handler = HANDLERS.with(|cell| {
        let mut map = cell.borrow_mut();
        map.entry(label.to_string())
            .or_insert_with(|| ButtonHandler::new(mtm, label.to_string()))
            .clone()
    });
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
        label,
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
/// closed) and stash a `Retained` clone of it in `DEVTOOLS_BUTTONS` keyed by
/// `label` so [`update_devtools_button_image`] can later swap its image.
/// Wires the `toggleDevTools:` selector. Failures (SF Symbol unavailable on
/// macOS < 11) are non-fatal — the menubar entry / F12 still toggle DevTools.
fn add_devtools_button(
    content: &NSView,
    target: &AnyObject,
    mtm: MainThreadMarker,
    x_origin: f64,
    label: &str,
) {
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

    DEVTOOLS_BUTTONS.with(|cell| {
        cell.borrow_mut().insert(label.to_string(), button);
    });
}

/// Swap the DevTools button's image for `label` to reflect `is_open`. Called
/// from [`ButtonHandler::toggle_devtools`] right after the toggle so the icon
/// follows the inspector's actual visibility, and from `lib.rs::on_menu_event`
/// (passing the focused window's label) so the F12 / `View → Open DevTools`
/// paths keep the icon in sync as well. Silent no-op when the button hasn't
/// been installed (e.g. SF Symbol unavailable at boot, or `label` doesn't
/// match any installed window) or when the requested symbol can't be resolved.
pub fn update_devtools_button_image(label: &str, is_open: bool) {
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
    DEVTOOLS_BUTTONS.with(|cell| {
        if let Some(button) = cell.borrow().get(label) {
            button.setImage(Some(&image));
        }
    });
}

/// Public entry point used by `lib.rs::setup` and the New Window dialog.
/// Resolves the NSWindow handle from the Tauri webview window, stashes the
/// AppHandle for the reload / devtools button actions on first call, and
/// forwards to [`install_on_ns_window`] with the window's label so the
/// per-window DevTools toggle picks the right webview.
pub fn install_titlebar_accessory(
    app: &AppHandle,
    window: &tauri::WebviewWindow,
) -> tauri::Result<()> {
    let mtm = MainThreadMarker::new()
        .expect("install_titlebar_accessory must be called on the main thread");

    // Stash the AppHandle for the reload / devtools button actions on first
    // call. `set` returns Err if already initialised, which is expected on
    // every install after the first; we just ignore it because subsequent
    // installs are guaranteed to be passed the same AppHandle.
    APP_HANDLE.with(|cell| {
        let _ = cell.set(app.clone());
    });

    let label = window.label().to_string();
    let ns_window = window.ns_window()?;
    // SAFETY: Tauri's contract on `ns_window()` returns a valid autoreleased
    // NSWindow pointer; we borrow it without taking ownership.
    unsafe { install_on_ns_window(ns_window, mtm, &label) };
    Ok(())
}
