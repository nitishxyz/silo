//! Look Up: the system's definition popover (Dictionary, Wikipedia…) for
//! selected page text, shown by the window's view at the menu's point.

use objc2::{msg_send, rc::Retained, runtime::AnyObject};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Show `text`'s definition at `at` (window coordinates, top-left origin).
pub(super) fn look_up(text: &str, at: gpui::Point<gpui::Pixels>, window: &gpui::Window) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return };
    // SAFETY: the AppKit handle wraps the window's live NSView for the
    // duration of the borrow; NSAttributedString is created and released here.
    unsafe {
        let view: &NSView = appkit.ns_view.cast::<NSView>().as_ref();
        let Some(class) = objc2::runtime::AnyClass::get(c"NSAttributedString") else { return };
        let string = NSString::from_str(text);
        let attributed: *mut AnyObject = msg_send![class, alloc];
        let attributed: Option<Retained<AnyObject>> = Retained::from_raw(msg_send![attributed, initWithString: &*string]);
        let Some(attributed) = attributed else { return };
        // gpui's view isn't flipped: AppKit's y grows upward from the bottom.
        let height = view.bounds().size.height;
        let point = NSPoint::new(f64::from(f32::from(at.x)), height - f64::from(f32::from(at.y)));
        let _: () = msg_send![view, showDefinitionForAttributedString: &*attributed, atPoint: point];
    }
}
