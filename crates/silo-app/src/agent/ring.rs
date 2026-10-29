//! A small progress ring: a faint track and the used share over it,
//! tinted by how full it is.

use gpui::{Hsla, IntoElement, Styled as _, point, px, rgb};

use crate::theme;

/// `used` of `total` (0–1), `None` without a known total.
pub(super) fn fraction(used: u64, total: u64) -> Option<f32> {
    #[allow(clippy::cast_precision_loss)]
    (total > 0).then(|| (used as f32 / total as f32).clamp(0.0, 1.0))
}

/// The ring's (and its label's) colour: muted, amber above 80%, red above 95%.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fill {
    Calm,
    Warn,
    Full,
}

impl Fill {
    pub(super) fn of(fraction: f32) -> Self {
        if fraction > 0.95 {
            Self::Full
        } else if fraction > 0.8 {
            Self::Warn
        } else {
            Self::Calm
        }
    }

    pub(super) fn color(self) -> u32 {
        match self {
            Self::Calm => theme::muted(),
            Self::Warn => theme::warning(),
            Self::Full => theme::danger(),
        }
    }
}

/// The 14 pt ring at `fill` (0–1) in `color`.
pub(crate) fn ring(fill: f32, color: u32) -> impl IntoElement {
    // A `fill` arc clockwise from 12 o'clock, as a stroked polyline.
    fn arc(center: gpui::Point<gpui::Pixels>, radius: f32, fill: f32, width: f32) -> Option<gpui::Path<gpui::Pixels>> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let steps = ((64.0 * fill).ceil() as usize).max(2);
        let mut path = gpui::PathBuilder::stroke(px(width));
        for i in 0..=steps {
            #[allow(clippy::cast_precision_loss)]
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * fill * i as f32 / steps as f32;
            let p = point(center.x + px(radius * a.cos()), center.y + px(radius * a.sin()));
            if i == 0 { path.move_to(p) } else { path.line_to(p) }
        }
        path.build().ok()
    }
    gpui::canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            let (center, radius, width) = (bounds.center(), 5.25, 1.75);
            if let Some(track) = arc(center, radius, 1.0, width) {
                window.paint_path(track, Hsla::from(rgb(theme::mix(theme::background(), theme::foreground(), 0.16))));
            }
            if fill > 0.0
                && let Some(used) = arc(center, radius, fill, width)
            {
                window.paint_path(used, Hsla::from(rgb(color)));
            }
        },
    )
    .size(px(14.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions_and_levels() {
        assert_eq!(fraction(10, 0), None, "unknown total: no ring");
        assert_eq!(fraction(50, 200), Some(0.25));
        assert_eq!(fraction(300, 200), Some(1.0), "clamped");
        assert_eq!((Fill::of(0.8), Fill::of(0.81), Fill::of(0.95), Fill::of(0.96)), (Fill::Calm, Fill::Warn, Fill::Warn, Fill::Full));
    }
}
