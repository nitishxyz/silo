//! Runtime `CrAppControlProtocol` conformance for the process's NSApplication class.
//!
//! CEF requires NSApp to answer `isHandlingSendEvent` / `setHandlingSendEvent:` and to set the
//! flag around `sendEvent:`. gpui's `GPUIApplication` does neither, so we add the methods and
//! the protocols to `[NSApp class]` with the objc runtime before `cef::initialize`.

use std::ffi::{CStr, c_char};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use objc2::ffi;
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, Imp, Sel};
use objc2::sel;

static HANDLING: AtomicBool = AtomicBool::new(false);
static ORIGINAL_SEND_EVENT: OnceLock<Imp> = OnceLock::new();

type SendEventFn = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);
type GetFn = extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool;
type SetFn = extern "C-unwind" fn(*mut AnyObject, Sel, Bool);
type WrapFn = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject);

extern "C-unwind" fn is_handling_send_event(_this: *mut AnyObject, _cmd: Sel) -> Bool {
    Bool::new(HANDLING.load(Ordering::Relaxed))
}

extern "C-unwind" fn set_handling_send_event(_this: *mut AnyObject, _cmd: Sel, value: Bool) {
    HANDLING.store(value.as_bool(), Ordering::Relaxed);
}

extern "C-unwind" fn send_event(this: *mut AnyObject, cmd: Sel, event: *mut AnyObject) {
    let original = ORIGINAL_SEND_EVENT.get().expect("sendEvent: swizzled without original");
    let original = unsafe { std::mem::transmute::<Imp, SendEventFn>(*original) };
    let previous = HANDLING.swap(true, Ordering::Relaxed);
    unsafe { original(this, cmd, event) };
    HANDLING.store(previous, Ordering::Relaxed);
}

fn protocol(name: &CStr) -> Result<&'static AnyProtocol> {
    match AnyProtocol::get(name) {
        Some(protocol) => Ok(protocol),
        None => bail!("objc protocol {name:?} is not registered (CEF framework not loaded?)"),
    }
}

/// Makes `class` (the NSApplication subclass behind NSApp) conform to `CrAppControlProtocol`.
/// Must run once, on the main thread, after the CEF framework is loaded.
pub(crate) fn install(class: &AnyClass) -> Result<()> {
    let control = protocol(c"CrAppControlProtocol")?;
    let cls = class as *const AnyClass as *mut AnyClass;
    let send = sel!(sendEvent:);
    unsafe {
        let inherited = ffi::class_getInstanceMethod(cls, send);
        if inherited.is_null() {
            bail!("{} has no sendEvent:", class.name().to_string_lossy());
        }
        if ORIGINAL_SEND_EVENT.set((*inherited).implementation()).is_err() {
            bail!("CrAppControlProtocol already installed");
        }
        let get: GetFn = is_handling_send_event;
        let set: SetFn = set_handling_send_event;
        let wrap: WrapFn = send_event;
        add(cls, sel!(isHandlingSendEvent), std::mem::transmute::<GetFn, Imp>(get), c"B@:")?;
        add(cls, sel!(setHandlingSendEvent:), std::mem::transmute::<SetFn, Imp>(set), c"v@:B")?;
        ffi::class_replaceMethod(cls, send, std::mem::transmute::<WrapFn, Imp>(wrap), c"v@:@".as_ptr());
        for name in [c"CrAppProtocol", c"CrAppControlProtocol"] {
            ffi::class_addProtocol(cls, protocol(name)?);
        }
    }
    if !class.conforms_to(control) {
        bail!("{} does not conform to CrAppControlProtocol", class.name().to_string_lossy());
    }
    Ok(())
}

unsafe fn add(cls: *mut AnyClass, sel: Sel, imp: Imp, types: &CStr) -> Result<()> {
    let types: *const c_char = types.as_ptr();
    if unsafe { !ffi::class_addMethod(cls, sel, imp, types).as_bool() } {
        bail!("class already implements {sel}");
    }
    Ok(())
}
