//! Dock clicks and `open` (Spotlight, Raycast, Finder) on a running Silo.
//!
//! gpui's app delegate answers `applicationShouldHandleReopen:hasVisibleWindows:`
//! only when no window is visible, and without a return value, so with
//! windows open a reopen was left to AppKit and could leave Silo's windows
//! behind another app's. Every reopen comes here instead: the app's
//! handler brings its frontmost window forward and activates Silo.

use std::sync::OnceLock;

use anyhow::{Result, bail};
use objc2::ffi;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::sel;
use tokio::sync::mpsc::UnboundedSender;

static REOPENS: OnceLock<UnboundedSender<bool>> = OnceLock::new();

type ReopenFn = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, Bool) -> Bool;

extern "C-unwind" fn should_handle_reopen(_this: *mut AnyObject, _cmd: Sel, _app: *mut AnyObject, visible: Bool) -> Bool {
    if let Some(reopens) = REOPENS.get() {
        let _ = reopens.send(visible.as_bool());
    }
    // Handled: AppKit does nothing more.
    Bool::NO
}

/// Route every reopen to `reopens` (whether a window was visible). Before
/// the app runs, on the main thread.
pub fn install(reopens: UnboundedSender<bool>) -> Result<()> {
    let Some(class) = AnyClass::get(c"GPUIApplicationDelegate") else { bail!("no GPUIApplicationDelegate class") };
    if REOPENS.set(reopens).is_err() {
        bail!("reopen handler already installed");
    }
    let cls = class as *const AnyClass as *mut AnyClass;
    let imp: ReopenFn = should_handle_reopen;
    // SAFETY: the signature matches `applicationShouldHandleReopen:hasVisibleWindows:`
    // (BOOL (id self, SEL, NSApplication *, BOOL)); the class is registered at load.
    unsafe {
        ffi::class_replaceMethod(
            cls,
            sel!(applicationShouldHandleReopen:hasVisibleWindows:),
            std::mem::transmute::<ReopenFn, Imp>(imp),
            c"B@:@B".as_ptr(),
        );
    }
    Ok(())
}
