//! Window material: Solid, or Frosted (the compositor blurs what is behind
//! the window and the app paints veils over it). Frosted renders only where
//! blur is known to work and transparency is not reduced; otherwise the
//! window is solid while the user's choice is kept.

use serde::{Deserialize, Serialize};

use crate::theme;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Material {
    #[default]
    Solid,
    Frosted,
}

/// Frosted: a colour the window's material is tinted with, as macOS tints
/// its glass. `None` is the theme's own background.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrostTint {
    #[default]
    None,
    Graphite,
    Blue,
    Purple,
    Pink,
    Red,
    Orange,
    Green,
    Teal,
}

impl FrostTint {
    pub const ALL: [Self; 9] = [Self::None, Self::Graphite, Self::Blue, Self::Purple, Self::Pink, Self::Red, Self::Orange, Self::Green, Self::Teal];
    /// How far the theme background moves toward the tint's hue: a hint of
    /// colour, as macOS's wallpaper tinting is, never the window's character.
    const STRENGTH: f32 = 0.07;

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Graphite => "Graphite",
            Self::Blue => "Blue",
            Self::Purple => "Purple",
            Self::Pink => "Pink",
            Self::Red => "Red",
            Self::Orange => "Orange",
            Self::Green => "Green",
            Self::Teal => "Teal",
        }
    }

    /// The swatch colour (macOS's accent colours); `None` has none.
    #[must_use]
    pub const fn hue(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Graphite => Some(0x008c_8c8c),
            Self::Blue => Some(0x0000_7aff),
            Self::Purple => Some(0x00af_52de),
            Self::Pink => Some(0x00ff_2d55),
            Self::Red => Some(0x00ff_3b30),
            Self::Orange => Some(0x00ff_9500),
            Self::Green => Some(0x0034_c759),
            Self::Teal => Some(0x0030_b0c7),
        }
    }

    /// The window surface for a theme background `theme_bg`.
    #[must_use]
    pub fn apply(self, theme_bg: u32) -> u32 {
        self.hue().map_or(theme_bg, |hue| theme::mix(theme_bg, hue, Self::STRENGTH))
    }
}

/// Why Frosted cannot render right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The platform has no known-good blur.
    Unsupported,
    /// Accessibility › Display › Reduce transparency is on.
    ReduceTransparency,
}

impl Unavailable {
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Unsupported => "Not available on this system",
            Self::ReduceTransparency => "Reduce transparency is on",
        }
    }
}

/// Whether the window compositor blurs behind translucent windows. macOS
/// always does; Windows 11 (build 22000+) and blurring Linux compositors
/// would qualify once the app runs there.
#[must_use]
pub fn blur_supported() -> bool {
    cfg!(target_os = "macos")
}

/// Frosted's availability given the platform and the accessibility setting.
pub fn availability(supported: bool, reduce_transparency: bool) -> Result<(), Unavailable> {
    if !supported {
        Err(Unavailable::Unsupported)
    } else if reduce_transparency {
        Err(Unavailable::ReduceTransparency)
    } else {
        Ok(())
    }
}

/// What actually renders for the user's `choice`.
#[must_use]
pub fn effective(choice: Material, available: Result<(), Unavailable>) -> Material {
    if choice == Material::Frosted && available.is_ok() { Material::Frosted } else { Material::Solid }
}

/// Frosted is one material: the titlebar, gutters and every card are a
/// veil at the same alpha over the blur, at least this, raised per frame
/// until all text reads (see [`readable_alpha`]).
pub const VEIL: f32 = 0.62;
/// Explicit terminal cell backgrounds other than the backdrop stay nearly
/// opaque so a TUI's structure stays crisp.
pub const CELL_OPAQUE: f32 = 0.9;
/// Cards are this much denser than the window around them, as a macOS
/// window's content is over its sidebar: the chrome stays the frost, the
/// work sits on a calmer surface.
pub const CARD_DENSER: f32 = 0.12;
/// ...but never quite opaque.
pub const CARD_MOST: f32 = 0.92;

/// Composite `color` at `alpha` over `under`.
#[must_use]
pub fn over(color: u32, alpha: f32, under: u32) -> u32 {
    theme::mix(under, color, alpha.clamp(0.0, 1.0))
}

/// Over a bright backdrop, macOS shows a veil this much lighter (toward
/// white) than a plain composite: measured at 0.62, 0.70 and 0.96 alpha.
const BLUR_LIFT: f32 = 0.12;

/// What a veil of `color` at `alpha` may look like: composited over the
/// theme background, black and white (the extremes the blur can show).
#[must_use]
pub fn seen(color: u32, alpha: f32, theme_bg: u32) -> [u32; 3] {
    let white = over(color, alpha, 0x00ff_ffff);
    let white = if alpha < 1.0 { theme::mix(white, 0x00ff_ffff, BLUR_LIFT) } else { white };
    [over(color, alpha, theme_bg), over(color, alpha, 0), white]
}

/// A typical desktop as the blur shows it: mid grey.
const BACKDROP: u32 = 0x0080_8080;

/// The material's look as one opaque colour: a veil of `color` at `alpha`
/// over a typical blurred desktop. Floating surfaces tint their glass with
/// it ([`crate::surface`]); panes over panes are it, opaque.
#[must_use]
pub fn look(color: u32, alpha: f32) -> u32 {
    over(color, alpha, BACKDROP)
}

/// Colours of surfaces over the app's content, for a window veiled
/// `window` at `window_alpha`: its look where the theme's text reads on
/// it, else the theme background.
#[must_use]
pub fn overlay(window: u32, window_alpha: f32, theme_bg: u32, fg: u32) -> theme::Veils {
    let tinted = look(window, window_alpha);
    let overlay = if theme::contrast(fg, tinted) >= theme::TEXT_CONTRAST { tinted } else { theme_bg };
    theme::Veils { overlay, glass: window, muted: theme::toward_ink(overlay, fg, 0.55, theme::TEXT_CONTRAST) }
}

/// Whether some ink, and each of `text`, reads at 4.5:1 on a veil of
/// `color` at `alpha` whatever is behind it.
#[must_use]
pub fn reads(color: u32, alpha: f32, theme_bg: u32, text: &[u32]) -> bool {
    let seen = seen(color, alpha, theme_bg);
    theme::reads_on_all(&seen).is_some() && text.iter().all(|&t| seen.iter().all(|&b| theme::contrast(t, b) >= theme::TEXT_CONTRAST))
}

/// The smallest alpha from `floor` up at which [`reads`] holds.
#[must_use]
pub fn readable_alpha(color: u32, floor: f32, theme_bg: u32, text: &[u32]) -> f32 {
    type Key = (u32, u32, u32, u64);
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<Key, f32>> = Default::default();
    }
    let text_key = text.iter().fold(0u64, |h, &t| h.wrapping_mul(0x0100_0000_01b3).wrapping_add(u64::from(t) + 1));
    let key = (color, floor.to_bits(), theme_bg, text_key);
    if let Some(a) = CACHE.with_borrow(|cache| cache.get(&key).copied()) {
        return a;
    }
    let a = search_alpha(color, floor, theme_bg, text);
    CACHE.with_borrow_mut(|cache| {
        // Blends pass through many colours; keep the cache small.
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(key, a);
    });
    a
}

fn search_alpha(color: u32, floor: f32, theme_bg: u32, text: &[u32]) -> f32 {
    (0..).map(|step| (floor + 0.01 * step as f32).min(1.0)).find(|&a| a >= 1.0 || reads(color, a, theme_bg, text)).unwrap_or(1.0)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::OnceLock;

    use objc2::{class, msg_send, runtime::AnyObject};

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        static NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification: *mut AnyObject;
    }

    /// `SILO_DEBUG_REDUCE_TRANSPARENCY=1|true|0|false` overrides the
    /// system setting.
    fn forced() -> Option<bool> {
        std::env::var("SILO_DEBUG_REDUCE_TRANSPARENCY").ok().map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    }

    pub fn reduce_transparency() -> bool {
        forced().unwrap_or_else(|| {
            // SAFETY: NSWorkspace is a singleton available on the main thread
            // and from any thread for this read-only property.
            unsafe {
                let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
                msg_send![workspace, accessibilityDisplayShouldReduceTransparency]
            }
        })
    }

    static CHANGES: OnceLock<tokio::sync::watch::Sender<bool>> = OnceLock::new();

    /// Changes of the setting, from NSWorkspace's display-options notification.
    pub fn watch() -> tokio::sync::watch::Receiver<bool> {
        CHANGES
            .get_or_init(|| {
                let (tx, _) = tokio::sync::watch::channel(reduce_transparency());
                if forced().is_none() {
                    let block = block2::RcBlock::new(|_: *mut AnyObject| {
                        if let Some(tx) = CHANGES.get() {
                            tx.send_replace(reduce_transparency());
                        }
                    });
                    // SAFETY: the notification centre copies the block and
                    // keeps the observer for the life of the process.
                    unsafe {
                        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
                        let center: *mut AnyObject = msg_send![workspace, notificationCenter];
                        let nil: *mut AnyObject = std::ptr::null_mut();
                        let _: *mut AnyObject = msg_send![center, addObserverForName: NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification, object: nil, queue: nil, usingBlock: &*block];
                    }
                }
                tx
            })
            .subscribe()
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn watch() -> tokio::sync::watch::Receiver<bool> {
        tokio::sync::watch::channel(false).1
    }
}

pub use platform::watch;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frosted_renders_only_when_available() {
        assert_eq!(availability(false, false), Err(Unavailable::Unsupported));
        assert_eq!(availability(false, true), Err(Unavailable::Unsupported));
        assert_eq!(availability(true, true), Err(Unavailable::ReduceTransparency));
        assert_eq!(availability(true, false), Ok(()));
        assert_eq!(effective(Material::Frosted, Ok(())), Material::Frosted);
        assert_eq!(effective(Material::Frosted, Err(Unavailable::ReduceTransparency)), Material::Solid);
        assert_eq!(effective(Material::Frosted, Err(Unavailable::Unsupported)), Material::Solid);
        assert_eq!(effective(Material::Solid, Ok(())), Material::Solid);
        assert_eq!(Unavailable::ReduceTransparency.reason(), "Reduce transparency is on");
    }

    #[test]
    fn veils_composite() {
        assert_eq!(over(0x00ff_ffff, 1.0, 0), 0x00ff_ffff);
        assert_eq!(over(0x00ff_ffff, 0.0, 0x0012_3456), 0x0012_3456);
        assert_eq!(over(0x00ff_ffff, 0.5, 0), 0x0080_8080);
        assert_eq!(seen(0, 1.0, 0x00ff_ffff), [0, 0, 0]);
        assert_eq!(seen(0, 0.5, 0)[2], theme::mix(0x0080_8080, 0x00ff_ffff, BLUR_LIFT), "a veil reads lifted over white");
    }

    #[test]
    fn overlays_read() {
        for (theme_bg, fg) in [(0x0016_1719, 0x00d6_d8dc), (0x00fa_fafa, 0x0038_3a42), (0x0028_2c34, 0x00ab_b2bf)] {
            for window in [theme_bg, 0x0018_2d44, 0x0068_7d93, 0x00ff_ffff] {
                let v = overlay(window, VEIL, theme_bg, fg);
                assert!(theme::contrast(fg, v.overlay) >= theme::TEXT_CONTRAST, "fg on {:06x}", v.overlay);
                assert!(theme::contrast(v.muted, v.overlay) >= theme::TEXT_CONTRAST, "muted on {:06x}", v.overlay);
            }
        }
        assert_eq!(seen(0x0012_3456, 0.0, 0x0028_2c34), [0x0028_2c34, 0, 0x00ff_ffff]);
    }

    #[test]
    fn veils_are_raised_until_text_reads() {
        for (theme_bg, fg) in [(0x0028_2c34, 0x00ab_b2bf), (0x00fa_fafa, 0x0038_3a42)] {
            for color in [0x00ff_ffff, 0x0080_8080, 0x0076_7676, 0x003a_6ea5, 0, 0x00ff_0000, 0x0000_ff00, 0x0013_2026, theme_bg] {
                let text: &[u32] = if color == theme_bg { &[fg] } else { &[] };
                let a = readable_alpha(color, VEIL, theme_bg, text);
                assert!((VEIL..=1.0).contains(&a));
                assert!(a >= 1.0 || reads(color, a, theme_bg, text), "{color:06x} at {a}");
                if a < 1.0 {
                    for bg in seen(color, a, theme_bg) {
                        assert!(text.iter().all(|&t| theme::contrast(t, bg) >= theme::TEXT_CONTRAST), "fg on {bg:06x}");
                    }
                }
            }
        }
        // Over white, a dark theme's own foreground needs a dense veil; a
        // dim one, an opaque one.
        let dark = readable_alpha(0x0016_1719, VEIL, 0x0016_1719, &[0x00d6_d8dc]);
        assert!(dark > 0.75 && dark < 0.9, "{dark}");
        assert!(readable_alpha(0x0028_2c34, VEIL, 0x0028_2c34, &[0x00ab_b2bf]) > 0.95);
    }
}

#[cfg(test)]
mod tint_tests {
    use super::*;

    /// A tint is a hint: on the default dark and light backgrounds every
    /// channel moves only a little, and None leaves the theme as it is.
    #[test]
    fn tints_stay_subtle() {
        for bg in [0x0016_1719, 0x00fa_fafa, 0] {
            assert_eq!(FrostTint::None.apply(bg), bg);
            for tint in FrostTint::ALL {
                let c = tint.apply(bg);
                for shift in [16, 8, 0] {
                    let d = ((c >> shift) & 0xff).abs_diff((bg >> shift) & 0xff);
                    assert!(d <= 18, "{tint:?} on {bg:06x}: {c:06x}");
                }
            }
        }
    }
}
