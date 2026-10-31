//! Fn (🌐) as AppKit reports it, whatever view is first responder: a web
//! page's view takes `flagsChanged:` itself, and GPUI's view sees Fn only
//! when it has focus, so the voice trigger listens to an app-wide local
//! monitor instead.

use std::sync::OnceLock;

use tokio::sync::broadcast;

/// Fn down, and any other modifier down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnState {
    pub function: bool,
    pub others: bool,
}

const FUNCTION: usize = 1 << 23;
const OTHERS: usize = (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20);
/// `NSEventMaskFlagsChanged`.
const FLAGS_CHANGED: u64 = 1 << 12;

static CHANNEL: OnceLock<broadcast::Sender<FnState>> = OnceLock::new();

/// Every modifier change in this app, from the first call on (installs the
/// monitor once, on the main thread). A local monitor sees the app's own
/// events only, so it needs no Input Monitoring.
pub fn subscribe() -> broadcast::Receiver<FnState> {
    CHANNEL
        .get_or_init(|| {
            let (tx, _) = broadcast::channel(64);
            install(tx.clone());
            tx
        })
        .subscribe()
}

#[must_use]
pub fn state_of(flags: usize) -> FnState {
    FnState { function: flags & FUNCTION != 0, others: flags & OTHERS != 0 }
}

#[cfg(target_os = "macos")]
fn install(tx: broadcast::Sender<FnState>) {
    use objc2::{class, msg_send, runtime::AnyObject};
    let handler = block2::RcBlock::new(move |event: *mut AnyObject| -> *mut AnyObject {
        if !event.is_null() {
            let flags: usize = unsafe { msg_send![&*event, modifierFlags] };
            let code: u16 = unsafe { msg_send![&*event, keyCode] };
            tracing::debug!(code, flags = format!("{flags:#x}"), "voice: flags changed");
            let _ = tx.send(state_of(flags));
        }
        event
    });
    let monitor: *mut AnyObject = unsafe { msg_send![class!(NSEvent), addLocalMonitorForEventsMatchingMask: FLAGS_CHANGED, handler: &*handler] };
    if monitor.is_null() {
        tracing::warn!("voice: no Fn monitor");
    } else {
        // Kept for the app's life (AppKit holds the handler while the monitor lives).
        std::mem::forget(handler);
        let _: *mut AnyObject = unsafe { msg_send![monitor, retain] };
    }
}

#[cfg(not(target_os = "macos"))]
fn install(_: broadcast::Sender<FnState>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_read_as_fn_and_others() {
        assert_eq!(state_of(FUNCTION), FnState { function: true, others: false });
        assert_eq!(state_of(FUNCTION | (1 << 20)), FnState { function: true, others: true });
        assert_eq!(state_of(1 << 17), FnState { function: false, others: true });
        // Caps Lock (1 << 16) and device bits are neither.
        assert_eq!(state_of(1 << 16 | 0x100), FnState { function: false, others: false });
    }
}
