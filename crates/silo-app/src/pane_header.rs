//! The pane header. The workspace draws its frame (`layout.rs`): the
//! block's icon, status mark, the pane buttons (split, zoom, close), drag
//! to swap, click to focus and the progress line. Each block type fills it
//! through [`BlockHeader`]: a middle that stretches, optional actions just
//! before the pane buttons, and optionally its own progress. A block's
//! parts are built from small item functions, so a per-item setting would
//! only filter them.

use gpui::{AnyElement, App, Context, Entity, SharedString, div, prelude::*, px, rgb};
use serde::{Deserialize, Serialize};
use silo_proto::model::BlockInfo;

use crate::{browser_pane::Kind, layout::HEADER_HEIGHT, theme::Continuous as _};

/// Header buttons.
pub const BUTTON: f32 = 22.0;

/// The block types whose header can be hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderBlock {
    Terminal,
    Agent,
}

/// Which block types show their header (`app.json`). Browsers always do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneHeaders {
    pub terminal: bool,
    pub agent: bool,
}

impl Default for PaneHeaders {
    fn default() -> Self {
        Self::all(true)
    }
}

impl PaneHeaders {
    #[must_use]
    pub fn all(show: bool) -> Self {
        Self { terminal: show, agent: show }
    }

    #[must_use]
    pub fn get(self, block: HeaderBlock) -> bool {
        match block {
            HeaderBlock::Terminal => self.terminal,
            HeaderBlock::Agent => self.agent,
        }
    }

    pub fn set(&mut self, block: HeaderBlock, show: bool) {
        match block {
            HeaderBlock::Terminal => self.terminal = show,
            HeaderBlock::Agent => self.agent = show,
        }
    }

    /// Every hideable type shows its header.
    #[must_use]
    pub fn every(self) -> bool {
        self.terminal && self.agent
    }

    /// No hideable type shows its header.
    #[must_use]
    pub fn none(self) -> bool {
        !self.terminal && !self.agent
    }
}

/// How much of a block's header shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderMode {
    Full,
    /// The block's middle alone: no icon, status or pane buttons.
    Middle,
    Hidden,
}

impl HeaderMode {
    #[must_use]
    pub fn height(self) -> f32 {
        if self == Self::Hidden { 0.0 } else { HEADER_HEIGHT }
    }
}

/// A pane's content `slot` grown up over its header: its whole card.
#[must_use]
pub fn with_header(slot: gpui::Bounds<gpui::Pixels>, mode: HeaderMode) -> gpui::Bounds<gpui::Pixels> {
    let header = px(mode.height());
    gpui::Bounds::new(gpui::point(slot.origin.x, slot.origin.y - header), gpui::size(slot.size.width, slot.size.height + header))
}

/// A browser's header can't hide (it holds the address); with every other
/// header hidden it shrinks to its middle.
#[must_use]
pub fn header_mode(kind: Kind, headers: PaneHeaders) -> HeaderMode {
    let shown = |on: bool| if on { HeaderMode::Full } else { HeaderMode::Hidden };
    match kind {
        Kind::Browser if headers.none() => HeaderMode::Middle,
        Kind::Browser => HeaderMode::Full,
        Kind::Agent => shown(headers.get(HeaderBlock::Agent)),
        Kind::Terminal | Kind::Unsupported => shown(headers.get(HeaderBlock::Terminal)),
    }
}

/// What a block's parts are drawn with.
#[derive(Clone, Copy)]
pub struct HeaderStyle {
    pub active: bool,
    pub palette: crate::adaptive::Chrome,
    /// Button radius: nested in the card's corner (Comfortable).
    pub control: f32,
}

impl HeaderStyle {
    /// Primary text: full ink on the active pane.
    #[must_use]
    pub fn ink(self) -> u32 {
        if self.active { self.palette.ink } else { self.palette.muted }
    }
}

/// What a block type puts in its header.
pub struct HeaderParts {
    /// Stretches between the icon and the actions.
    pub middle: AnyElement,
    /// Just before the pane buttons, shown with them.
    pub actions: Vec<AnyElement>,
    /// Replaces the status progress line (0.0–1.0).
    pub progress: Option<f32>,
    /// A hairline under the header (over a page).
    pub rule: bool,
    /// The drag pill's label.
    pub label: SharedString,
}

impl HeaderParts {
    #[must_use]
    pub fn new(middle: AnyElement, label: impl Into<SharedString>) -> Self {
        Self { middle, actions: Vec::new(), progress: None, rule: false, label: label.into() }
    }
}

/// A block type's header parts. `info` is the server's view of the block.
pub trait BlockHeader: 'static + Sized {
    fn header(&mut self, style: HeaderStyle, info: Option<&BlockInfo>, cx: &mut Context<Self>) -> HeaderParts;
}

pub fn parts<T: BlockHeader>(pane: &Entity<T>, style: HeaderStyle, info: Option<&BlockInfo>, cx: &mut App) -> HeaderParts {
    pane.update(cx, |pane, cx| pane.header(style, info, cx))
}

/// A name, and muted detail after it.
#[must_use]
pub fn labels(name: String, detail: Option<String>, style: HeaderStyle) -> HeaderParts {
    let middle = div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(6.0))
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .child(div().flex_none().text_color(rgb(style.ink())).child(name.clone()))
        .children(detail.map(|d| div().min_w_0().text_ellipsis().text_color(rgb(style.palette.muted)).child(d)));
    HeaderParts::new(middle.into_any_element(), name)
}

/// A block with no pane here (a newer kind, an agent on a remote host).
#[must_use]
pub fn fallback(info: Option<&BlockInfo>, style: HeaderStyle) -> HeaderParts {
    match info.filter(|i| i.kind == silo_proto::model::BlockKind::Agent) {
        Some(i) => labels(Some(i.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Agent".into()), None, style),
        None => {
            let (name, cwd) = crate::layout::header_labels(info);
            labels(name, cwd, style)
        }
    }
}

/// The remote host a block runs on, after its name: a globe and the host, muted.
#[must_use]
pub fn host_label(host: &str, style: HeaderStyle) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(3.0))
        .max_w(px(160.0))
        .whitespace_nowrap()
        .text_color(rgb(style.palette.muted))
        .child(crate::assets::icon("globe", 11.0, style.palette.muted))
        .child(div().min_w_0().overflow_hidden().text_ellipsis().child(host.to_owned()))
        .into_any_element()
}

impl BlockHeader for crate::pane::Pane {
    fn header(&mut self, style: HeaderStyle, info: Option<&BlockInfo>, _: &mut Context<Self>) -> HeaderParts {
        let (name, cwd) = crate::layout::header_labels(self.info.as_ref().or(info));
        labels(name, cwd, style)
    }
}

/// A header button; `color` is its glyph's.
pub fn button(
    id: impl Into<gpui::ElementId>,
    glyph: &'static str,
    tip: &'static str,
    color: u32,
    enabled: bool,
    style: HeaderStyle,
) -> gpui::Stateful<gpui::Div> {
    let palette = style.palette;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(BUTTON))
        .continuous(style.control)
        .when(enabled, |el| el.cursor_pointer().hover(move |s| s.bg(palette.hover())))
        .tooltip(crate::tooltip::Tooltip::text(tip))
        .child(crate::assets::icon(glyph, 13.0, color))
}

/// A block's own control keeps its press: no pane drag, no focus change.
pub fn keep_press<E: InteractiveElement>(el: E) -> E {
    el.on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_block_type_follows_its_own_setting() {
        let mixed = PaneHeaders { terminal: false, agent: true };
        assert_eq!(header_mode(Kind::Terminal, mixed), HeaderMode::Hidden);
        assert_eq!(header_mode(Kind::Unsupported, mixed), HeaderMode::Hidden);
        assert_eq!(header_mode(Kind::Agent, mixed), HeaderMode::Full);
        assert_eq!(header_mode(Kind::Browser, mixed), HeaderMode::Full, "another type still shows headers");
        let other = PaneHeaders { terminal: true, agent: false };
        assert_eq!((header_mode(Kind::Terminal, other), header_mode(Kind::Agent, other)), (HeaderMode::Full, HeaderMode::Hidden));
        assert_eq!(header_mode(Kind::Browser, other), HeaderMode::Full);
        for kind in [Kind::Terminal, Kind::Agent, Kind::Browser, Kind::Unsupported] {
            assert_eq!(header_mode(kind, PaneHeaders::default()), HeaderMode::Full, "{kind:?}");
        }
    }

    #[test]
    fn a_browser_keeps_a_shrunk_header_when_the_others_hide() {
        let none = PaneHeaders::all(false);
        assert_eq!(header_mode(Kind::Browser, none), HeaderMode::Middle);
        assert_eq!(header_mode(Kind::Terminal, none), HeaderMode::Hidden);
        assert_eq!(header_mode(Kind::Agent, none), HeaderMode::Hidden);
        assert!((HeaderMode::Middle.height() - HEADER_HEIGHT).abs() < f32::EPSILON, "the same height, shrunk sideways");
    }

    #[test]
    fn header_heights_per_block_in_a_mixed_layout() {
        // A terminal (hidden), an agent and a browser side by side.
        let headers = PaneHeaders { terminal: false, agent: true };
        let heights: Vec<f32> = [Kind::Terminal, Kind::Agent, Kind::Browser].iter().map(|k| header_mode(*k, headers).height()).collect();
        assert_eq!(heights, [0.0, HEADER_HEIGHT, HEADER_HEIGHT]);
        // A closing pane's ghost covers its card: content plus its own header.
        let slot = gpui::Bounds::new(gpui::point(px(10.0), px(100.0)), gpui::size(px(300.0), px(200.0)));
        let terminal = with_header(slot, header_mode(Kind::Terminal, headers));
        assert_eq!(terminal, slot, "no header: the content is the card");
        let agent = with_header(slot, header_mode(Kind::Agent, headers));
        assert_eq!((agent.origin.y, agent.size.height), (px(100.0 - HEADER_HEIGHT), px(200.0 + HEADER_HEIGHT)));
        assert_eq!((agent.origin.x, agent.size.width), (slot.origin.x, slot.size.width));
        let shrunk = with_header(slot, header_mode(Kind::Browser, PaneHeaders::all(false)));
        assert_eq!(shrunk.size.height, px(200.0 + HEADER_HEIGHT), "a browser's shrunk header keeps its height");
    }

    #[test]
    fn settings_default_to_shown_and_round_trip() {
        let old: PaneHeaders = serde_json::from_str("{}").unwrap();
        assert_eq!(old, PaneHeaders::all(true));
        let partial: PaneHeaders = serde_json::from_str(r#"{"agent":false}"#).unwrap();
        assert_eq!(partial, PaneHeaders { terminal: true, agent: false });
        let mut h = PaneHeaders::default();
        h.set(HeaderBlock::Terminal, false);
        assert!(!h.get(HeaderBlock::Terminal) && h.get(HeaderBlock::Agent) && !h.every() && !h.none());
        assert_eq!(serde_json::from_str::<PaneHeaders>(&serde_json::to_string(&h).unwrap()).unwrap(), h);
    }
}
