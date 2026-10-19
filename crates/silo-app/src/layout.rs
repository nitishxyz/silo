//! The pane area: the active window's split layout in Compact density
//! (flush panes meeting at 1px dividers) or Comfortable density (rounded
//! pane cards with small gaps), optional slim pane headers with per-block
//! actions, and the ghosts of closing panes.

use std::time::Instant;

use gpui::{AnyElement, Context, IntoElement, Window, div, prelude::*, px, relative, rgb};
use silo_proto::{
    control::Method,
    model::{Axis, BlockInfo, Layout, WindowInfo},
};

use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    hosts::Density,
    motion::{self, Edge},
    radius::{self, CardCorners},
    theme,
    tooltip::Tooltip,
    workspace::Workspace,
};

/// Height of a pane header.
pub const HEADER_HEIGHT: f32 = 28.0;
/// Pane header action buttons.
const BUTTON: f32 = 22.0;
/// Comfortable: margin around the cards and gap between them. Card radii
/// come from [`radius`].
const OUTER: f32 = 8.0;
const GAP: f32 = 6.0;
/// Width of the corner ring over browser surfaces: covers a corner of
/// radius ≤ 12 (r·(√2−1)) and stays inside the gap between cards.
const CAP: f32 = 5.0;

/// Which window edges a pane card lies along, and the window's corner
/// radius when its corners are visible behind the cards.
#[derive(Clone, Copy)]
struct Placement {
    window: Option<f32>,
    left: bool,
    right: bool,
    bottom: bool,
}

impl Placement {
    fn corners(self) -> CardCorners {
        CardCorners::for_card(self.window, OUTER, self.left, self.right, self.bottom)
    }

    /// The placements of the two halves of a split along `axis`.
    fn split(self, axis: Axis) -> (Self, Self) {
        match axis {
            Axis::Horizontal => (Self { right: false, ..self }, Self { left: false, ..self }),
            Axis::Vertical => (Self { bottom: false, ..self }, self),
        }
    }
}

impl Workspace {
    /// The active window's panes, zoomed to the focused one if set.
    pub(crate) fn render_body(&self, w: &WindowInfo, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let comfortable = self.density() == Density::Comfortable;
        // Cards on the bottom edge sit in the window's bottom corners unless
        // the status bar is below them. Compact panes are flush: the window
        // clips them itself.
        let corner = if comfortable && self.status_bar().is_none() { radius::window_corner(window) } else { None };
        let place = Placement { window: corner, left: true, right: true, bottom: true };
        let content = if w.zoomed {
            self.render_layout(&Layout::block(w.focused.clone()), Some(&w.focused), false, true, None, place, now, cx)
        } else {
            let focused = self.focused_block(window, cx);
            self.render_layout(&w.layout, focused.as_deref(), w.layout.blocks().len() > 1, false, swap_window(w), place, now, cx)
        };
        div()
            .size_full()
            .min_h_0()
            .when(comfortable, |el| el.px(px(OUTER)).pb(px(OUTER)))
            .child(content)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_layout(
        &self,
        layout: &Layout,
        focused: Option<&str>,
        multiple: bool,
        zoomed: bool,
        swap: Option<&str>,
        place: Placement,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match layout {
            Layout::Block { id } => {
                let active = focused == Some(id.as_str());
                // No focus ring: inactive panes get a translucent wash.
                self.render_cell(id, active, multiple && !active, zoomed, swap, place.corners(), now, cx)
            }
            Layout::Split { axis, ratio, first, second } => {
                let ratio = ratio.clamp(0.05, 0.95);
                let (place_a, place_b) = place.split(*axis);
                let a = self.render_layout(first, focused, multiple, zoomed, swap, place_a, now, cx);
                let b = self.render_layout(second, focused, multiple, zoomed, swap, place_b, now, cx);
                // Compact: a neutral hairline. Comfortable: empty space.
                let (size, fill) = match self.density() {
                    Density::Compact => (1.0, Some(theme::border())),
                    Density::Comfortable => (GAP, None),
                };
                let container = div().size_full().min_w_0().min_h_0().flex();
                match axis {
                    Axis::Horizontal => container
                        .flex_row()
                        .child(div().h_full().w(relative(ratio)).flex_none().min_w_0().child(a))
                        .child(div().h_full().w(px(size)).flex_none().when_some(fill, |el, c| el.bg(rgb(c))))
                        .child(div().h_full().flex_1().min_w_0().child(b))
                        .into_any_element(),
                    Axis::Vertical => container
                        .flex_col()
                        .child(div().w_full().h(relative(ratio)).flex_none().min_h_0().child(a))
                        .child(div().w_full().h(px(size)).flex_none().when_some(fill, |el, c| el.bg(rgb(c))))
                        .child(div().w_full().flex_1().min_h_0().child(b))
                        .into_any_element(),
                }
            }
        }
    }

    /// One pane: header and terminal together in one card (Comfortable) or
    /// flush (Compact). The terminal is laid out at its final size once; a
    /// new split only animates the card's offset inside its clipped slot.
    #[allow(clippy::too_many_arguments)]
    fn render_cell(
        &self,
        id: &str,
        active: bool,
        dimmed: bool,
        zoomed: bool,
        swap: Option<&str>,
        corners: CardCorners,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let comfortable = self.density() == Density::Comfortable;
        let headers = self.pane_headers();
        // Fills inside the 1px outline (the content mask is rectangular).
        let fill = corners.inset(1.0);
        let entering = self
            .motion
            .pane_enter
            .get(id)
            .and_then(|(started, edge)| motion::progress(*started, motion::PANE_ENTER, now).map(|p| edge.offset(1.0 - p)));
        let inner: AnyElement = match (self.panes.get(id), self.browsers.get(id)) {
            (Some((pane, _)), _) => pane.clone().into_any_element(),
            (None, Some((pane, _))) => pane.clone().into_any_element(),
            (None, None) => div().size_full().into_any_element(),
        };
        let content = div().relative().flex_1().min_h_0().min_w_0().child(inner);
        // Header and terminal are one surface; an inactive pane is dimmed
        // as a whole so no band appears between them. No listeners on the
        // wash: clicks and hovers reach the header and terminal below.
        let dim = if self.browsers.contains_key(id) { theme::INACTIVE_DIM_PAGE } else { theme::INACTIVE_DIM };
        let wash = dimmed.then(|| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(rgb(theme::background()).opacity(dim))
                .when(comfortable, |el| el.continuous_corners(fill))
        });
        // Drop target for a pane dragged by its header: an accent outline
        // and tint while another pane of this window hovers it.
        let drop = swap.map(|window_id| {
            let (window_id, target) = (window_id.to_owned(), id.to_owned());
            let check = (window_id.clone(), target.clone());
            div()
                .id(gpui::ElementId::Name(format!("pane-drop-{id}").into()))
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .when(comfortable, |el| el.continuous_corners(fill))
                .can_drop(move |d, _, _| d.downcast_ref::<DraggedPane>().is_some_and(|d| can_swap(d, &check.0, &check.1)))
                .drag_over::<DraggedPane>(|s, _, _, _| s.bg(rgb(theme::accent()).opacity(0.08)).border_2().border_color(rgb(theme::accent())))
                .on_drop(cx.listener(move |this, d: &DraggedPane, _, cx| {
                    if can_swap(d, &window_id, &target) {
                        this.swap_panes(&window_id, &d.block_id, &target, cx);
                    }
                }))
        });
        let card = div()
            .group(card_group(id))
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(rgb(theme::background()))
            .when(comfortable, |el| el.continuous_corners(corners).border_1().border_color(rgb(theme::card_outline())))
            .when(headers, |el| el.child(self.render_pane_header(id, active, zoomed, swap, cx)))
            .child(content)
            .children(wash)
            .children(drop);
        let card: AnyElement = match entering {
            Some((dx, dy)) => div().absolute().left(relative(dx)).top(relative(dy)).size_full().child(card).into_any_element(),
            None => card.into_any_element(),
        };
        // The page surface is clipped to a rectangle: a ring of window
        // colour just outside the card's rounded shape hides its corners.
        let caps = (comfortable && entering.is_none() && self.browsers.contains_key(id)).then(|| {
            let ring = |r: f32| r + CAP;
            let outer = CardCorners {
                top_left: ring(corners.top_left),
                top_right: ring(corners.top_right),
                bottom_right: ring(corners.bottom_right),
                bottom_left: ring(corners.bottom_left),
            };
            let ring = div().absolute().top(px(-CAP)).left(px(-CAP)).right(px(-CAP)).bottom(px(-CAP)).border(px(CAP)).border_color(rgb(theme::surface()));
            let outline = div().absolute().inset_0().border_1().border_color(rgb(theme::card_outline())).continuous_corners(corners);
            [ring.continuous_corners(outer), outline]
        });
        div()
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .when(entering.is_some(), |el| el.overflow_hidden())
            .child(card)
            .children(caps.into_iter().flatten())
            .into_any_element()
    }

    fn render_pane_header(&self, id: &str, active: bool, zoomed: bool, swap: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let info = self
            .panes
            .get(id)
            .and_then(|(p, _)| p.read(cx).info.clone())
            .or_else(|| self.view.as_ref().and_then(|v| v.blocks.iter().find(|b| b.id == id).cloned()));
        let browser = self.browsers.get(id).map(|(p, _)| p.read(cx).title());
        let glyph = self.block_icon(id);
        let (name, cwd) = match browser {
            Some(title) => (title, None),
            None => header_labels(info.as_ref()),
        };
        let dragged = swap.map(|w| DraggedPane { window_id: w.to_owned(), block_id: id.to_owned(), label: name.clone().into(), icon: glyph });
        let block = id.to_owned();
        // Comfortable: the corner button sits inside the card's top corner,
        // 1pt border + (header - button) / 2 in, so it nests concentrically.
        let control = match self.density() {
            Density::Comfortable => radius::concentric(radius::CARD, 1.0 + (HEADER_HEIGHT - BUTTON) / 2.0),
            Density::Compact => radius::CONTROL,
        };
        let button = |action: &'static str, glyph: &'static str, tip: &'static str| {
            div()
                .id(gpui::ElementId::Name(format!("pane-{action}-{id}").into()))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(BUTTON))
                .continuous(control)
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::tab_hover_on(theme::background()))))
                .tooltip(Tooltip::text(tip))
                .child(icon(glyph, 13.0, theme::muted()))
        };
        let on = |f: fn(&mut Workspace, String, &mut Context<Workspace>)| {
            let block = block.clone();
            cx.listener(move |this: &mut Workspace, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Workspace>| {
                cx.stop_propagation();
                f(this, block.clone(), cx);
            })
        };
        let split_right = on(|this, b, cx| this.split_block(b, Axis::Horizontal, cx));
        let split_down = on(|this, b, cx| this.split_block(b, Axis::Vertical, cx));
        let zoom = on(|this, b, _| this.zoom_block(b));
        let close = on(|this, b, _| this.close_block_id(b));
        let focus_block = block.clone();
        div()
            .id(gpui::ElementId::Name(format!("pane-header-{id}").into()))
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(px(HEADER_HEIGHT))
            .pl(px(10.0))
            .pr(px(4.0))
            .gap(px(6.0))
            .font_family(theme::ui_font())
            .text_size(px(11.5))
            .child(icon(glyph, 12.0, if active { theme::accent() } else { theme::muted() }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.0))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().flex_none().text_color(rgb(if active { theme::foreground() } else { theme::muted() })).child(name))
                    .children(cwd.map(|c| div().min_w_0().text_ellipsis().text_color(rgb(theme::muted())).child(c))),
            )
            // Actions show on the active pane, and on any pane while the
            // pointer is over it; they keep their space so nothing shifts.
            // In a pane too narrow for every button the row shrinks and,
            // right-aligned, is clipped from the left: Close stays visible.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink(1.0)
                    .min_w_0()
                    .overflow_hidden()
                    .justify_end()
                    .items_center()
                    .when(!active, |el| el.opacity(0.0).group_hover(card_group(id), |s| s.opacity(1.0)))
                    .child(button("split-right", "split-right", "Split Right  ⌘D").on_click(split_right))
                    .child(button("split-down", "split-down", "Split Down  ⇧⌘D").on_click(split_down))
                    .child(
                        button("zoom", if zoomed { "unzoom" } else { "zoom" }, if zoomed { "Restore Pane  ⇧⌘↩" } else { "Zoom Pane  ⇧⌘↩" })
                            .on_click(zoom),
                    )
                    .child(button("close", "close", "Close Pane  ⌘W").on_click(close)),
            )
            // Clicking the header focuses its pane.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.focus_block(&focus_block, window, cx);
                }),
            )
            .when_some(dragged, |el, d| el.on_drag(d, |d, offset, _, cx| cx.new(|_| PanePill { label: d.label.clone(), icon: d.icon, offset })))
            .into_any_element()
    }

    /// Swaps two panes of a window: locally at once, then on the server,
    /// whose session.changed confirms it.
    fn swap_panes(&mut self, window_id: &str, a: &str, b: &str, cx: &mut Context<Self>) {
        let swap = {
            let (window_id, a, b) = (window_id.to_owned(), a.to_owned(), b.to_owned());
            move |this: &mut Self| {
                let window = this.view.as_mut().and_then(|v| v.session.windows.iter_mut().find(|w| w.id == window_id));
                window.is_some_and(|w| w.layout.swap(&a, &b))
            }
        };
        if !swap.clone()(self) {
            return;
        }
        cx.notify();
        let method = Method::BlockSwap { block_id: a.to_owned(), other_block_id: b.to_owned() };
        self.call_optimistic(method, "Couldn't swap panes", move |this| _ = swap(this), cx);
    }

    /// Closed panes sliding out of their old slots towards the outer edge,
    /// over the survivor already laid out at its final size. Visual only:
    /// no listeners, so input goes to the panes below.
    pub(crate) fn render_pane_ghosts(&self, now: Instant, window: &mut Window) -> Vec<AnyElement> {
        let metrics = silo_render::CellMetrics::for_window(window);
        let cursor = silo_render::CursorPresentation { focused: false, phase_visible: true };
        let selection = rgb(theme::selection()).opacity(0.55);
        let comfortable = self.density() == Density::Comfortable;
        let corner = if comfortable && self.status_bar().is_none() { radius::window_corner(window) } else { None };
        let viewport = window.viewport_size();
        self.motion
            .pane_ghosts
            .iter()
            .filter_map(|ghost| {
                let p = motion::progress(ghost.started, motion::PANE_EXIT, now)?;
                let (dx, dy) = ghost.edge.offset(p);
                // The slot is the whole card: keep the corners it had.
                let near = |gap: gpui::Pixels| f32::from(gap) <= OUTER + 0.5;
                let slot = ghost.slot;
                let corners = CardCorners::for_card(
                    corner,
                    OUTER,
                    near(slot.origin.x),
                    near(viewport.width - slot.origin.x - slot.size.width),
                    near(viewport.height - slot.origin.y - slot.size.height),
                );
                let canvas = silo_render::terminal_canvas(ghost.frame.clone(), metrics, cursor, selection, silo_render::Overlay::default());
                let header = ghost.title.clone().map(|title| {
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.0))
                        .h(px(HEADER_HEIGHT))
                        .pl(px(10.0))
                        .font_family(theme::ui_font())
                        .text_size(px(11.5))
                        .text_color(rgb(theme::muted()))
                        .child(icon("terminal", 12.0, theme::muted()))
                        .child(title)
                });
                Some(
                    div()
                        .absolute()
                        .left(ghost.slot.origin.x)
                        .top(ghost.slot.origin.y)
                        .w(ghost.slot.size.width)
                        .h(ghost.slot.size.height)
                        .overflow_hidden()
                        .child(
                            div()
                                .absolute()
                                .left(relative(dx))
                                .top(relative(dy))
                                .size_full()
                                .flex()
                                .flex_col()
                                .bg(rgb(theme::background()))
                                .when(comfortable, |el| el.continuous_corners(corners).border_1().border_color(rgb(theme::card_outline())))
                                .opacity(1.0 - 0.6 * p)
                                .children(header)
                                .child(
                                    div()
                                        .flex_1()
                                        .p(px(crate::pane::PADDING))
                                        .child(div().relative().size_full().child(div().absolute().top_0().left_0().size_full().child(canvas))),
                                ),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }
}

/// A pane being dragged by its header to swap places with another.
#[derive(Clone)]
pub(crate) struct DraggedPane {
    window_id: String,
    block_id: String,
    label: gpui::SharedString,
    icon: &'static str,
}

/// The pill following the pointer while a pane is dragged. GPUI places
/// its root at the pointer minus the grab offset; padding by `offset` puts
/// the pill just below-right of the pointer.
struct PanePill {
    label: gpui::SharedString,
    icon: &'static str,
    offset: gpui::Point<gpui::Pixels>,
}

impl Render for PanePill {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let pill = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(24.0))
            .px(px(10.0))
            .continuous(radius::CONTROL)
            .bg(rgb(theme::background()))
            .border_1()
            .border_color(rgb(theme::accent()))
            .font_family(theme::ui_font())
            .text_size(px(11.5))
            .text_color(rgb(theme::foreground()))
            .child(icon(self.icon, 12.0, theme::accent()))
            .child(self.label.clone());
        // Padding, not margin: the root's margins are ignored by layout.
        div().pl(self.offset.x + px(10.0)).pt(self.offset.y + px(10.0)).child(pill)
    }
}

/// The window whose panes can be swapped by dragging: more than one pane
/// and not zoomed.
fn swap_window(w: &WindowInfo) -> Option<&str> {
    (!w.zoomed && w.layout.blocks().len() > 1).then_some(w.id.as_str())
}

/// Whether `dragged` may be dropped on pane `target` of window `window`.
fn can_swap(dragged: &DraggedPane, window: &str, target: &str) -> bool {
    dragged.window_id == window && dragged.block_id != target
}

/// Hover group of one pane card (header actions appear on hover).
fn card_group(id: &str) -> gpui::SharedString {
    format!("pane-card-{id}").into()
}

/// Header labels: the running program (or shell) and the abbreviated cwd.
pub fn header_labels(info: Option<&BlockInfo>) -> (String, Option<String>) {
    let Some(info) = info else { return ("shell".into(), None) };
    let title = info.title.trim().trim_start_matches('-');
    let name = if title.is_empty() { "shell".to_owned() } else { title.to_owned() };
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = info.cwd.as_deref().map(|cwd| match cwd.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => cwd.to_owned(),
    });
    // Programs that put the cwd in their title would repeat it.
    let cwd = cwd.filter(|c| *c != name);
    (name, cwd)
}

/// The axis of the split directly holding block `id`.
pub(crate) fn parent_axis(layout: &Layout, id: &str) -> Option<Axis> {
    match layout {
        Layout::Block { .. } => None,
        Layout::Split { axis, first, second, .. } => {
            let direct = [first, second].iter().any(|child| matches!(&***child, Layout::Block { id: b } if b == id));
            if direct { Some(*axis) } else { parent_axis(first, id).or_else(|| parent_axis(second, id)) }
        }
    }
}

/// The edge a direct split child `id` leaves through: away from its sibling.
pub(crate) fn sibling_side(layout: &Layout, id: &str) -> Option<Edge> {
    let Layout::Split { axis, first, second, .. } = layout else { return None };
    let is = |child: &Layout| matches!(child, Layout::Block { id: b } if b == id);
    match (axis, is(first), is(second)) {
        (Axis::Horizontal, true, _) => Some(Edge::Left),
        (Axis::Horizontal, _, true) => Some(Edge::Right),
        (Axis::Vertical, true, _) => Some(Edge::Top),
        (Axis::Vertical, _, true) => Some(Edge::Bottom),
        _ => sibling_side(first, id).or_else(|| sibling_side(second, id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: &str) -> Layout {
        Layout::Block { id: id.into() }
    }

    fn split(axis: Axis, first: Layout, second: Layout) -> Layout {
        Layout::Split { axis, ratio: 0.5, first: Box::new(first), second: Box::new(second) }
    }

    #[test]
    fn closing_pane_leaves_away_from_its_sibling() {
        // [a | b] | c: b sits mid-left but its sibling a takes its space.
        let layout = split(Axis::Horizontal, split(Axis::Horizontal, block("a"), block("b")), block("c"));
        assert_eq!(sibling_side(&layout, "b"), Some(Edge::Right));
        assert_eq!(sibling_side(&layout, "a"), Some(Edge::Left));
        assert_eq!(sibling_side(&layout, "c"), Some(Edge::Right));
        let layout = split(Axis::Vertical, block("a"), block("b"));
        assert_eq!(sibling_side(&layout, "a"), Some(Edge::Top));
        assert_eq!(sibling_side(&layout, "b"), Some(Edge::Bottom));
    }

    #[test]
    fn new_split_enters_along_its_axis() {
        let layout = split(Axis::Horizontal, block("a"), split(Axis::Vertical, block("b"), block("c")));
        assert_eq!(parent_axis(&layout, "a"), Some(Axis::Horizontal));
        assert_eq!(parent_axis(&layout, "c"), Some(Axis::Vertical));
        assert_eq!(parent_axis(&block("a"), "a"), None);
    }

    #[test]
    fn panes_swap_only_within_one_unzoomed_multi_pane_window() {
        let window = |layout: Layout, zoomed: bool| WindowInfo { id: "w".into(), name: None, layout, focused: "a".into(), zoomed };
        assert_eq!(swap_window(&window(split(Axis::Horizontal, block("a"), block("b")), false)), Some("w"));
        assert_eq!(swap_window(&window(split(Axis::Horizontal, block("a"), block("b")), true)), None);
        assert_eq!(swap_window(&window(block("a"), false)), None);
        let dragged = DraggedPane { window_id: "w".into(), block_id: "a".into(), label: "zsh".into(), icon: "terminal" };
        assert!(can_swap(&dragged, "w", "b"));
        assert!(!can_swap(&dragged, "w", "a"));
        assert!(!can_swap(&dragged, "other", "b"));
    }

    #[test]
    fn header_labels_show_program_and_short_cwd() {
        let home = std::env::var("HOME").unwrap();
        let info = |title: &str, cwd: Option<String>| BlockInfo {
            id: "b".into(),
            session_id: "s".into(),
            kind: silo_proto::model::BlockKind::Terminal,
            title: title.into(),
            cwd,
            exited: None,
            lifecycle: silo_proto::model::Lifecycle::Running,
            size: silo_proto::model::TermSize { columns: 80, rows: 24, cell_width: 0, cell_height: 0 },
            owner: None,
            pid: None,
            browser: None,
        };
        assert_eq!(header_labels(Some(&info("-zsh", Some(format!("{home}/dev"))))), ("zsh".into(), Some("~/dev".into())));
        assert_eq!(header_labels(Some(&info("", None))), ("shell".into(), None));
        assert_eq!(header_labels(Some(&info("~/dev", Some(format!("{home}/dev"))))), ("~/dev".into(), None));
        assert_eq!(header_labels(None), ("shell".into(), None));
    }
}
