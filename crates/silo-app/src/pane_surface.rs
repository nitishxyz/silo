//! One palette for every block's content (agent, files, changes, a browser
//! with no page, and any kind to come). The workspace hands each pane the
//! card it sits on ([`crate::blocks::Card::palette`]); panes paint through
//! these helpers, never `theme::background()` directly:
//!
//! - Solid: the card's colour, opaque.
//! - Frosted: no fill of their own (the card's veil over the window blur
//!   shows through), and what floats over scrolling content (a composer, a
//!   sticky bar, an edge fade) is frosted glass in the card's colour.

use gpui::{Hsla, Styled, px, rgb};

use crate::adaptive::Chrome;

/// Over scrolling content, frosted: the tint over its own blur, enough
/// that text scrolled under it is gone.
pub const BAR_VEIL: f32 = 0.55;
/// The densest a frosted [`PaneSurface::ramp`] gets.
pub const RAMP_MOST: f32 = 0.97;

/// The card a pane sits on, as last set by the workspace (`None` before the
/// first frame: the theme's, solid).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PaneSurface(pub Option<Chrome>);

impl PaneSurface {
    /// The window is frosted: the card is a veil.
    #[must_use]
    pub fn frosted(self) -> bool {
        self.0.is_some_and(|c| c.alpha < 1.0)
    }

    /// The card's colour.
    #[must_use]
    pub fn color(self) -> u32 {
        self.0.map_or_else(crate::theme::background, |c| c.surface)
    }

    /// The pane's own fill: the card's colour when solid, none when frosted.
    #[must_use]
    pub fn fill<E: Styled>(self, el: E) -> E {
        if self.frosted() { el } else { el.bg(rgb(self.color())) }
    }

    /// Colour and greatest alpha of what covers scrolling content (bars,
    /// edge fades).
    #[must_use]
    pub fn shade(self) -> (u32, f32) {
        (self.color(), if self.frosted() { BAR_VEIL } else { 1.0 })
    }

    /// The shade at `t` (0 clear, 1 its greatest alpha), for gradients.
    #[must_use]
    pub fn at(self, t: f32) -> Hsla {
        let (color, alpha) = self.shade();
        let mut c: Hsla = rgb(color).into();
        c.a = alpha * t.clamp(0.0, 1.0);
        c
    }

    /// A long fade over scrolling content with no blur under it (the agent's composer area): at `t`, up
    /// to nearly opaque even when frosted, so text under its bottom is gone.
    #[must_use]
    pub fn ramp(self, t: f32) -> Hsla {
        let mut c: Hsla = rgb(self.color()).into();
        c.a = RAMP_MOST * t.clamp(0.0, 1.0);
        if !self.frosted() {
            c.a = t.clamp(0.0, 1.0);
        }
        c
    }

    /// A bar over scrolling content: opaque when solid; frosted, glass of
    /// the card's colour over a blur of what is under it.
    #[must_use]
    pub fn bar<E: Styled>(self, el: E) -> E {
        let (color, alpha) = self.shade();
        let el = el.bg(rgb(color).opacity(alpha));
        if self.frosted() { el.backdrop_blur(px(crate::surface::BLUR)).backdrop_saturation(crate::surface::SATURATION) } else { el }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_fills_and_frosted_shows_the_veil() {
        let solid = PaneSurface(Some(Chrome::on(0x0012_3456)));
        assert!(!solid.frosted());
        assert_eq!(solid.color(), 0x0012_3456);
        assert_eq!(solid.shade(), (0x0012_3456, 1.0));
        let frosted = PaneSurface(Some(Chrome { alpha: 0.7, ..Chrome::on(0x0012_3456) }));
        assert!(frosted.frosted());
        assert_eq!(frosted.shade(), (0x0012_3456, BAR_VEIL));
        assert!((frosted.at(1.0).a - BAR_VEIL).abs() < 1e-6);
        assert!(frosted.at(0.0).a.abs() < 1e-6);
        assert!(!PaneSurface::default().frosted());
    }
}
