//! macOS root CA trust detection + automated install via the `security` CLI.
//!
//! Phase 2b: on launch we check whether the persistent Pouch CA is trusted by
//! the user's keychain for SSL. If not, we put up a native NSAlert explaining
//! what's about to happen and (on user consent) shell out to
//! `security add-trusted-cert -k <login keychain> <ca.pem>`. The `security`
//! invocation triggers Apple's standard "allow modifying the keychain" /
//! authorization dialog — which is non-bypassable by design — so the only UX
//! we control is the explanatory NSAlert before it.
//!
//! macOS-only: this whole module is gated by the `#[cfg(target_os = "macos")]
//! mod mitm` declaration in `lib.rs`; no extra inner cfg is needed.

use std::path::Path;
use std::process::Command;

use objc2::{msg_send, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{NSAlert, NSImage};
use objc2_foundation::{ns_string, MainThreadMarker, NSString};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    Trusted,
    NotTrusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserChoice {
    Install,
    Cancel,
}

/// `NSAlertFirstButtonReturn` — first button (Install Now). Same constant
/// `dialog.rs` uses for its OK button; documented in `<AppKit/NSAlert.h>` and
/// stable since macOS 10.9. Typed as `isize` to match `NSModalResponse`.
const NS_ALERT_FIRST_BUTTON_RETURN: isize = 1000;

/// SF Symbol used as the alert icon. `lock.shield` reads as a
/// security-relevant prompt; available on macOS 11+ (SF Symbols 1).
const ALERT_ICON_SYMBOL: &str = "lock.shield";

/// Check whether `ca_pem_path` is trusted by macOS for SSL. Shells out to
/// `security verify-cert -c <pem> -p ssl`; exit 0 == trusted, anything else
/// (including spawn failure) is treated as not trusted so the install path
/// runs and surfaces the real error.
pub fn check_trust(ca_pem_path: &Path) -> TrustState {
    let Some(p) = ca_pem_path.to_str() else {
        return TrustState::NotTrusted;
    };
    let output = Command::new("security")
        .args(["verify-cert", "-c", p, "-p", "ssl"])
        .output();
    match output {
        Ok(out) if out.status.success() => TrustState::Trusted,
        _ => TrustState::NotTrusted,
    }
}

/// Install the CA into the user's login keychain via
/// `security add-trusted-cert -k <login> <pem>`. macOS will prompt the user
/// for their password; the call is synchronous and blocks until the OS
/// dialog resolves (success or cancel). User-cancel surfaces here as a
/// non-zero exit code and is reported back to the caller via `Err`.
pub fn install_to_login_keychain(ca_pem_path: &Path) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|e| format!("HOME unset: {e}"))?;
    let login_keychain = format!("{home}/Library/Keychains/login.keychain-db");
    let pem = ca_pem_path.to_str().ok_or("CA path not utf8")?;

    let output = Command::new("security")
        .args(["add-trusted-cert", "-k", &login_keychain, pem])
        .output()
        .map_err(|e| format!("spawn security: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("security exit {}: {}", output.status, stderr.trim()));
    }
    Ok(())
}

/// Modal NSAlert explaining the install + asking for consent. Must be
/// invoked on the AppKit main thread (the Tauri `setup` callback runs on
/// the main thread, so the only call site is fine). Returns
/// `UserChoice::Cancel` if the main-thread marker can't be acquired —
/// safer to treat as "user said no" than to panic during startup.
pub fn show_install_prompt() -> UserChoice {
    let Some(mtm) = MainThreadMarker::new() else {
        return UserChoice::Cancel;
    };
    unsafe {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(ns_string!("Install Pouch Root Certificate"));
        alert.setInformativeText(ns_string!(
            "Pouch needs to install a self-signed root certificate into your \
             login keychain. This is required to intercept HTTPS traffic for \
             caching and script injection. The certificate is only used by \
             Pouch on this machine and you can remove it any time from \
             Keychain Access.\n\n\
             You will be asked for your macOS password by the system."
        ));

        let symbol = NSString::from_str(ALERT_ICON_SYMBOL);
        let a11y = ns_string!("Certificate trust");
        if let Some(icon) =
            NSImage::imageWithSystemSymbolName_accessibilityDescription(&symbol, Some(a11y))
        {
            alert.setIcon(Some(&icon));
        }

        // First button → NS_ALERT_FIRST_BUTTON_RETURN (1000); second
        // ("Cancel and Quit") → NSAlertSecondButtonReturn (1001) and gets
        // Escape automatically because its title contains "Cancel".
        let _: Retained<AnyObject> =
            msg_send![&*alert, addButtonWithTitle: ns_string!("Install Now")];
        let _: Retained<AnyObject> =
            msg_send![&*alert, addButtonWithTitle: ns_string!("Cancel and Quit")];

        let response = alert.runModal();
        if response == NS_ALERT_FIRST_BUTTON_RETURN {
            UserChoice::Install
        } else {
            UserChoice::Cancel
        }
    }
}

/// Modal NSAlert shown right before we exit the process because the user
/// declined to install the CA (or the install failed). Pure UX courtesy so
/// the user understands why Pouch is closing instead of silently vanishing.
pub fn show_quit_explanation(reason: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    unsafe {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(ns_string!("Pouch cannot start"));
        let body = format!(
            "Pouch requires a trusted root certificate to intercept HTTPS \
             traffic. It will now exit. Relaunch Pouch to try again.\n\n\
             Detail: {reason}"
        );
        let body_ns = NSString::from_str(&body);
        alert.setInformativeText(&body_ns);
        let _: Retained<AnyObject> = msg_send![&*alert, addButtonWithTitle: ns_string!("Quit")];
        let _ = alert.runModal();
    }
}
