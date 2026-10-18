//! Corner radius tokens, and the radii derived from the native window
//! corner so nested surfaces stay concentric with it.
//!
//! Every radius here is in the GPUI fork's continuous-corner units: the
//! corner is the superellipse of exponent [`theme::CORNER_SMOOTHING`]
//! spanning `radius` along each edge. That curve is much squarer than a
//! circle (or AppKit's continuous window corner) of the same nominal
//! radius, so a native radius converts with [`CONTINUOUS_SCALE`], and an
//! inset shrinks a continuous radius by `CONTINUOUS_SCALE` per point.
//!
//! [`theme::CORNER_SMOOTHING`]: crate::theme::CORNER_SMOOTHING

/// Continuous radius per point of circular (native) radius. Fitted
/// numerically: the exponent-4 superellipse closest to a circle of radius
/// 1 has radius 1.76 (diagonal match: 1.84), the one closest to the native
/// window corner mask about 1.6 per point of `_cornerRadius`, and the
/// parallel curve of an exponent-4 corner inset by `d` is best matched at
/// `r - 1.7 d`.
pub const CONTINUOUS_SCALE: f32 = 1.7;

/// Native window corner radius (`NSWindow._cornerRadius`, macOS 27, titled
/// window with a full-size content view) when it cannot be queried.
pub const NATIVE_WINDOW_FALLBACK: f32 = 16.0;

/// Comfortable pane card corners away from the window corners (and every
/// corner in fullscreen, where the window has none).
pub const CARD: f32 = 10.0;
/// Small icon buttons (pane header actions).
pub const CONTROL: f32 = 6.0;
/// Rows of dropdown menus and popovers.
pub const MENU_ROW: f32 = 7.0;
/// Tooltips.
pub const TOOLTIP: f32 = 7.0;

/// The continuous radius matching a native (circular-equivalent) radius.
#[must_use]
pub fn from_native(native: f32) -> f32 {
    native.max(0.0) * CONTINUOUS_SCALE
}

/// The radius of a surface inset by `inset` points inside one with
/// continuous radius `outer`, so both corners share a center:
/// `max(0, outer - inset * CONTINUOUS_SCALE)`.
#[must_use]
pub fn concentric(outer: f32, inset: f32) -> f32 {
    (outer - inset.max(0.0) * CONTINUOUS_SCALE).max(0.0)
}

/// The window's visible corner radius in continuous units, or `None` when
/// it shows no rounded corners (fullscreen).
#[must_use]
pub fn window_corner(window: &gpui::Window) -> Option<f32> {
    if window.is_fullscreen() {
        return None;
    }
    Some(from_native(native_window_radius(window).unwrap_or(NATIVE_WINDOW_FALLBACK)))
}

/// `NSWindow._cornerRadius` of the window hosting `window`. Private API,
/// guarded by `respondsToSelector:`; `None` when unavailable.
#[cfg(target_os = "macos")]
fn native_window_radius(window: &gpui::Window) -> Option<f32> {
    use objc2::{msg_send, runtime::AnyObject, sel};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return None };
    // SAFETY: the AppKit handle wraps the window's live NSView for the
    // duration of the borrow; every message is checked or nil-safe.
    unsafe {
        let view = appkit.ns_view.cast::<AnyObject>().as_ref();
        let ns_window: *mut AnyObject = msg_send![view, window];
        let ns_window = ns_window.as_ref()?;
        let responds: bool = msg_send![ns_window, respondsToSelector: sel!(_cornerRadius)];
        if !responds {
            return None;
        }
        let radius: f64 = msg_send![ns_window, _cornerRadius];
        #[allow(clippy::cast_possible_truncation)]
        (radius.is_finite() && radius >= 0.0).then_some(radius as f32)
    }
}

#[cfg(not(target_os = "macos"))]
fn native_window_radius(_: &gpui::Window) -> Option<f32> {
    None
}

/// Radii of one Comfortable pane card's corners.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardCorners {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl CardCorners {
    /// The same radius on every corner.
    #[must_use]
    pub const fn uniform(radius: f32) -> Self {
        Self { top_left: radius, top_right: radius, bottom_right: radius, bottom_left: radius }
    }

    /// Corners of a card inset `gutter` points from the window's left,
    /// right and bottom edges. A corner that sits in a window corner is
    /// concentric with it; the rest (and all of them when the window shows
    /// no corners) use [`CARD`]. The top corners meet the titlebar, never a
    /// window corner.
    #[must_use]
    pub fn for_card(window: Option<f32>, gutter: f32, left: bool, right: bool, bottom: bool) -> Self {
        let mut corners = Self::uniform(CARD);
        if let Some(window) = window {
            let nested = concentric(window, gutter);
            if bottom && left {
                corners.bottom_left = nested;
            }
            if bottom && right {
                corners.bottom_right = nested;
            }
        }
        corners
    }

    /// The corners of a fill `inset` points inside these (e.g. inside the
    /// card's border).
    #[must_use]
    pub fn inset(self, inset: f32) -> Self {
        Self {
            top_left: concentric(self.top_left, inset),
            top_right: concentric(self.top_right, inset),
            bottom_right: concentric(self.bottom_right, inset),
            bottom_left: concentric(self.bottom_left, inset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn nested_radii_shrink_with_the_inset() {
        assert!(close(from_native(16.0), 27.2));
        assert!(close(concentric(from_native(16.0), 8.0), from_native(8.0)));
        assert!(close(concentric(12.0, 1.0), 10.3));
        assert_eq!(concentric(10.0, 20.0), 0.0);
        assert_eq!(concentric(10.0, -3.0), 10.0);
        assert_eq!(from_native(-1.0), 0.0);
    }

    #[test]
    fn only_window_corner_cards_follow_the_window() {
        let window = Some(from_native(16.0));
        let nested = from_native(8.0);
        let lone = CardCorners::for_card(window, 8.0, true, true, true);
        assert_eq!((lone.top_left, lone.top_right), (CARD, CARD));
        assert!(close(lone.bottom_left, nested) && close(lone.bottom_right, nested));

        let bottom_left = CardCorners::for_card(window, 8.0, true, false, true);
        assert!(close(bottom_left.bottom_left, nested));
        assert_eq!(bottom_left.bottom_right, CARD);

        // Not on the bottom edge, or no visible window corners.
        assert_eq!(CardCorners::for_card(window, 8.0, true, true, false), CardCorners::uniform(CARD));
        assert_eq!(CardCorners::for_card(None, 8.0, true, true, true), CardCorners::uniform(CARD));
    }

    #[test]
    fn fills_inside_the_border_stay_concentric() {
        let fill = CardCorners::for_card(Some(from_native(16.0)), 8.0, true, false, true).inset(1.0);
        assert!(close(fill.top_left, CARD - CONTINUOUS_SCALE));
        assert!(close(fill.bottom_left, from_native(7.0)));
    }
}
