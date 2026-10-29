//! The window's status: what is going on (connecting, offline, a failure,
//! a done action) as a pill over the bottom of the panes, or centred when
//! there is nothing to show. Each status says what kind it is; the kind
//! picks its icon and colour.

use std::{
    hash::{Hash as _, Hasher as _},
    time::Duration,
};

use gpui::{AnimationExt as _, AnyElement, App, ElementId, FontWeight, IntoElement, div, ease_out_quint, prelude::*, px, rgb};

use crate::surface::Glassy as _;
use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusKind {
    /// Under way (connecting, starting, downloading): a spinner.
    Progress,
    /// Worth knowing, nothing wrong.
    Info,
    /// Done.
    Success,
    /// Not working right now (offline, stopped, not supported).
    Warning,
    /// Failed.
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Status {
    pub kind: StatusKind,
    pub text: String,
}

impl Status {
    pub fn progress(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Progress, text: text.into() }
    }
    pub fn info(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Info, text: text.into() }
    }
    pub fn success(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Success, text: text.into() }
    }
    pub fn warning(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Warning, text: text.into() }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self { kind: StatusKind::Error, text: text.into() }
    }

    /// Changes with the status, so a new one animates in.
    fn key(&self, name: &str) -> ElementId {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        ElementId::Name(format!("{name}-{:x}", hasher.finish()).into())
    }
}

impl StatusKind {
    pub fn icon(self) -> &'static str {
        match self {
            Self::Progress => "spinner",
            Self::Info => "info-mark",
            Self::Success => "check",
            Self::Warning => "alert",
            Self::Error => "close",
        }
    }

    fn base(self) -> u32 {
        match self {
            Self::Progress | Self::Info => theme::accent(),
            Self::Success => theme::success(),
            Self::Warning => theme::warning(),
            Self::Error => theme::danger(),
        }
    }

    /// The kind's colour as an icon on `bg`.
    pub fn color_on(self, bg: u32) -> u32 {
        readable(self.base(), bg)
    }
}

/// `base`, or the smallest step toward `bg`'s ink that reads on it as an
/// icon (a theme's yellow on a light popover, say).
fn readable(base: u32, bg: u32) -> u32 {
    theme::reading(base, theme::ink_on(bg), &[bg], 0.0, theme::ICON_CONTRAST)
}

/// Fill of the icon's circle: the kind's colour, faint.
const BADGE_ALPHA: f32 = 0.18;
/// The glyph's share of its circle.
const GLYPH: f32 = 0.7;

/// The kind's icon in a tinted circle `size` across, on `bg`.
fn badge(status: &Status, size: f32, bg: u32, cx: &App) -> AnyElement {
    let color = status.kind.color_on(bg);
    let glyph = (size * GLYPH).round();
    let icon = match status.kind {
        StatusKind::Progress => crate::browser_pane::spinner("status", glyph, color, cx),
        kind => crate::assets::icon(kind.icon(), glyph, color).into_any_element(),
    };
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .bg(rgb(color).opacity(BADGE_ALPHA))
        .flex()
        .items_center()
        .justify_center()
        .child(icon)
        .into_any_element()
}

pub const PILL_HEIGHT: f32 = 26.0;
const PILL_BADGE: f32 = 16.0;
/// Between the pill and the bottom edge of the panes.
pub const PILL_CLEARANCE: f32 = 12.0;
/// How far the pill rises as it fades in.
const RISE: f32 = 6.0;
const APPEAR: Duration = Duration::from_millis(180);

/// The pill along the bottom of the panes, `bottom` above the window's
/// bottom edge. An overlay: it takes no layout space.
pub fn pill(status: &Status, bottom: f32, cx: &App) -> AnyElement {
    let pill = card(status, cx);
    div()
        .absolute()
        .bottom(px(bottom))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(pill)
        .with_animation(status.key("status-pill"), gpui::Animation::new(APPEAR).with_easing(ease_out_quint()), move |el, d| {
            el.bottom(px(bottom - RISE * (1.0 - d))).opacity(d)
        })
        .into_any_element()
}

/// The pill itself, for an owner that places it (and may add buttons).
pub fn card(status: &Status, cx: &App) -> gpui::Div {
    let bg = theme::overlay();
    // The badge sits concentric with the pill's rounded start.
    let inset = (PILL_HEIGHT - 2.0 - PILL_BADGE) / 2.0;
    div()
        .h(px(PILL_HEIGHT))
        .max_w(px(560.0))
        .flex()
        .items_center()
        .gap(px(7.0))
        .pl(px(inset))
        .pr(px(12.0))
        .glass(PILL_HEIGHT / 2.0)
        .rounded_full()
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::ink_on(bg)))
        .child(badge(status, PILL_BADGE, bg, cx))
        .child(div().min_w_0().truncate().child(status.text.clone()))
}

/// The status in the middle of an empty window, on `bg`.
pub fn centred(status: &Status, bg: u32, cx: &App) -> AnyElement {
    div()
        .max_w(px(520.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .text_size(px(13.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::muted_on(bg)))
        .child(badge(status, 20.0, bg, cx))
        .child(div().min_w_0().child(status.text.clone()))
        .with_animation(status.key("status-centred"), gpui::Animation::new(APPEAR).with_easing(ease_out_quint()), |el, d| el.opacity(d))
        .into_any_element()
}

/// The status as a line inside a card, on `bg`.
pub fn inline(status: &Status, bg: u32, cx: &App) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(px(12.0))
        .text_color(rgb(theme::muted_on(bg)))
        .child(badge(status, 14.0, bg, cx))
        .child(div().min_w_0().child(status.text.clone()))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{Status, StatusKind};

    const KINDS: [StatusKind; 5] = [StatusKind::Progress, StatusKind::Info, StatusKind::Success, StatusKind::Warning, StatusKind::Error];

    #[test]
    fn every_kind_has_its_own_icon() {
        for kind in KINDS {
            assert!(crate::assets::has_icon(kind.icon()), "{kind:?}: {}", kind.icon());
        }
        let mut icons: Vec<_> = KINDS.iter().map(|k| k.icon()).collect();
        icons.sort_unstable();
        icons.dedup();
        assert_eq!(icons.len(), KINDS.len());
    }

    #[test]
    fn constructors_set_the_kind() {
        assert_eq!(Status::progress("a").kind, StatusKind::Progress);
        assert_eq!(Status::info("a").kind, StatusKind::Info);
        assert_eq!(Status::success("a").kind, StatusKind::Success);
        assert_eq!(Status::warning("a").kind, StatusKind::Warning);
        assert_eq!(Status::error("a").kind, StatusKind::Error);
    }

    #[test]
    fn colours_read_as_icons_on_light_and_dark() {
        let (yellow, green) = (0x00e5_c07b, 0x0098_c379);
        for bg in [0x00ff_ffff, 0x00f5_f5f7, 0x0028_2c34, 0x001d_1d1f] {
            for base in [yellow, green, 0x00e0_6c75, 0x0061_afef] {
                let c = super::readable(base, bg);
                assert!(crate::theme::contrast(c, bg) >= crate::theme::ICON_CONTRAST, "{base:06x} on {bg:06x} -> {c:06x}");
            }
        }
        assert_eq!(super::readable(yellow, 0x0028_2c34), yellow, "a colour that reads is kept");
    }
}
