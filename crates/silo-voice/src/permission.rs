//! Silo's own microphone permission (macOS privacy, AVFoundation): the app
//! records, so it is asked for, not Silo Server.

use std::ptr::NonNull;

use objc2::{class, msg_send, runtime::Bool};
use objc2_foundation::NSString;

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Never asked: the first recording asks.
    NotAsked,
    Granted,
    /// Refused, or restricted by the Mac's management.
    Denied,
}

/// `AVMediaTypeAudio`.
fn audio() -> objc2::rc::Retained<NSString> {
    NSString::from_str("soun")
}

#[must_use]
pub fn access() -> Access {
    // SAFETY: a class method taking an NSString, returning an NSInteger.
    let status: isize = unsafe { msg_send![class!(AVCaptureDevice), authorizationStatusForMediaType: &*audio()] };
    match status {
        0 => Access::NotAsked,
        3 => Access::Granted,
        _ => Access::Denied,
    }
}

/// Ask macOS (its prompt shows once); `done` gets the answer, on any thread.
pub fn request(done: impl Fn(bool) + Send + Sync + 'static) {
    let block = block2::RcBlock::new(move |granted: Bool| done(granted.as_bool()));
    let handler: NonNull<block2::Block<dyn Fn(Bool)>> = NonNull::from(&*block);
    // SAFETY: the block is copied by AVFoundation for the async callback.
    let () = unsafe { msg_send![class!(AVCaptureDevice), requestAccessForMediaType: &*audio(), completionHandler: handler.as_ptr()] };
}
