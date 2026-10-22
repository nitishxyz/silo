//! The pane area: the active window's split layout in Compact density
//! (flush panes meeting at 1px dividers) or Comfortable density (rounded
//! pane cards with small gaps), optional slim pane headers with per-block
//! actions, and the ghosts of closing panes.

use std::time::Instant;

use gpui::{AnyElement, Context, IntoElement, Window, canvas, div, prelude::*, px, relative, rgb};
use silo_proto::{
    control::Method,
    layout_ops::{self, MIN_RATIO},
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
/// Pointer target of a divider, centred on it.
const DIVIDER_HIT: f32 = 6.0;

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
    /// Space between the window's bottom edge and the panes' bottom edge
    /// (their cards' outline in Comfortable).
    pub(crate) fn panes_bottom(&self) -> f32 {
        if self.density() == Density::Comfortable { OUTER } else { 0.0 }
    }

    /// The active window's panes, zoomed to the focused one if set.
    pub(crate) fn render_body(&self, w: &WindowInfo, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let comfortable = self.density() == Density::Comfortable;
        // Cards on the bottom edge sit in the window's bottom corners.
        // Compact panes are flush: the window clips them itself.
        let corner = if comfortable { radius::window_corner(window) } else { None };
        let place = Placement { window: corner, left: true, right: true, bottom: true };
        let zoom = self.motion.zoom_frame(now).filter(|(block, ..)| w.layout.blocks().iter().any(|b| b == block));
        let content = if zoom.is_some() {
            // Siblings stay tiled under the pane growing over them, and are
            // gone once it is whole (Frosted: it then settles to its veil).
            let focused = self.focused_block(window, cx);
            if self.motion.zoom_state(now).is_some_and(|(extent, _)| extent >= 1.0) {
                div().size_full().into_any_element()
            } else {
                self.render_layout(&w.layout, &[], focused.as_deref(), w.layout.blocks().len() > 1, false, None, place, now, cx)
            }
        } else if w.zoomed {
            self.render_layout(&Layout::block(w.focused.clone()), &[], Some(&w.focused), false, true, None, place, now, cx)
        } else {
            let focused = self.focused_block(window, cx);
            self.render_layout(&w.layout, &[], focused.as_deref(), w.layout.blocks().len() > 1, false, swap_window(w), place, now, cx)
        };
        let zoom = zoom.map(|(block, rect, area)| self.render_zoom(block, w.zoomed, rect, area, place, now, cx));
        let area = self.motion.measured.area.clone();
        let floats = self.render_floats(w, now, window, cx);
        // Frosted: the margins around the cards carry the window veil; the
        // cards carry their own, never stacked on it.
        let margins = (comfortable && self.frosted()).then(|| {
            let strip = || div().absolute().bg(self.chrome.fill());
            [
                strip().top_0().bottom_0().left_0().w(px(OUTER)),
                strip().top_0().bottom_0().right_0().w(px(OUTER)),
                strip().bottom_0().left(px(OUTER)).right(px(OUTER)).h(px(OUTER)),
            ]
        });
        div()
            .relative()
            .size_full()
            .min_h_0()
            .when(comfortable, |el| el.px(px(OUTER)).pb(px(OUTER)))
            .children(margins.into_iter().flatten())
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(content)
                    .child(canvas(move |bounds, _, _| area.set(Some(bounds)), |_, (), _, _| {}).absolute().top_0().left_0().size_full())
                    .children(zoom),
            )
            .children(floats)
            .on_drag_move(cx.listener(Self::float_drag_move))
            // A divider drag ends wherever the button is released; its
            // ratio goes to the server then rather than after the debounce.
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| this.end_divider_drag(cx)))
            .on_mouse_up_out(gpui::MouseButton::Left, cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| this.end_divider_drag(cx)))
            .into_any_element()
    }

    fn end_divider_drag(&mut self, cx: &mut Context<Self>) {
        if self.divider_drag.take().is_some() {
            self.flush_ratios(cx);
            cx.notify();
        }
    }

    /// The pane being zoomed or unzoomed, clipped to `rect`, between its
    /// slot and the whole pane `area`. Inside, it keeps the zoomed size;
    /// the clip has the card's corners, and an outline borders it like one.
    #[allow(clippy::too_many_arguments)]
    fn render_zoom(
        &self,
        block: &str,
        zoomed: bool,
        rect: gpui::Bounds<gpui::Pixels>,
        area: gpui::Bounds<gpui::Pixels>,
        place: Placement,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let corners = place.corners();
        let cell = self.render_cell(block, true, false, zoomed, None, corners, now, cx);
        let outline = (self.density() == Density::Comfortable).then(|| card_outline(corners, self.card_fill(block, cx)));
        div()
            .absolute()
            .left(rect.origin.x - area.origin.x)
            .top(rect.origin.y - area.origin.y)
            .w(rect.size.width)
            .h(rect.size.height)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .continuous_corners(corners)
                    .rounded_clip()
                    .child(div().absolute().top_0().left_0().w(area.size.width).h(area.size.height).child(cell)),
            )
            .children(outline)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_layout(
        &self,
        layout: &Layout,
        path: &[usize],
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
                // A zooming pane is drawn over the layout; its slot stays empty.
                let cell = if self.motion.zoom_frame(now).is_some_and(|(block, ..)| block == id) {
                    div().size_full().into_any_element()
                } else {
                    // No focus ring: inactive panes get a translucent wash.
                    self.render_cell(id, active, multiple && !active, zoomed, swap, place.corners(), now, cx)
                };
                let cells = self.motion.measured.cells.clone();
                let id = id.clone();
                div()
                    .relative()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .child(cell)
                    .child(
                        canvas(move |bounds, _, _| _ = cells.borrow_mut().insert(id, bounds), |_, (), _, _| {})
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                    )
                    .into_any_element()
            }
            Layout::Split { axis, ratio, first, second } => {
                let ratio = ratio.clamp(0.05, 0.95);
                let (place_a, place_b) = place.split(*axis);
                let child = |i: usize| [path, &[i]].concat();
                let a = self.render_layout(first, &child(0), focused, multiple, zoomed, swap, place_a, now, cx);
                let b = self.render_layout(second, &child(1), focused, multiple, zoomed, swap, place_b, now, cx);
                // Compact: a neutral hairline. Comfortable: empty space.
                let (size, fill) = match self.density() {
                    Density::Compact => (1.0, Some(theme::border())),
                    Density::Comfortable => (GAP, None),
                };
                // Drag a divider to resize its split; double-click it to even
                // the split out. Its hit area is wider than a Compact hairline
                // and overlays both panes.
                let split = path.to_vec();
                let dragging = self.divider_drag.as_ref().is_some_and(|d| d.path == path);
                let divider = div()
                    .flex_none()
                    .when_some(fill, |el, c| el.bg(rgb(c)))
                    .when(fill.is_none() && self.frosted(), |el| el.bg(self.chrome.fill()))
                    .when(dragging, |el| el.child(div().size_full().bg(rgb(theme::accent()).opacity(0.35))));
                let horizontal = *axis == Axis::Horizontal;
                let along = move |p: gpui::Point<gpui::Pixels>| f32::from(if horizontal { p.x } else { p.y });
                let hit = div()
                    .id(gpui::ElementId::Name(format!("divider-{split:?}").into()))
                    .absolute()
                    .occlude()
                    .cursor(if horizontal { gpui::CursorStyle::ResizeLeftRight } else { gpui::CursorStyle::ResizeUpDown })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener({
                            let split = split.clone();
                            move |this, e: &gpui::MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                let Some(w) = this.active_window() else { return };
                                let window_id = w.id.clone();
                                if e.click_count == 2 {
                                    let change = layout_ops::even_split(&w.layout, &split);
                                    this.divider_drag = None;
                                    this.set_ratios(&window_id, change.into_iter().collect(), cx);
                                } else {
                                    this.divider_drag =
                                        Some(DividerDrag { window_id, path: split.clone(), start_ratio: ratio, start: along(e.position) });
                                }
                            }
                        }),
                    )
                    .on_drag(DividerGrab, |_, _, _, cx| cx.new(|_| DividerGrab));
                let inset = px((size - DIVIDER_HIT) / 2.0);
                // The split's own container maps the drag to its ratio.
                let container = div().relative().size_full().min_w_0().min_h_0().flex().on_drag_move(cx.listener(
                    move |this, e: &gpui::DragMoveEvent<DividerGrab>, _, cx| {
                        let Some(d) = this.divider_drag.as_ref().filter(|d| d.path == split) else { return };
                        let extent = f32::from(if horizontal { e.bounds.size.width } else { e.bounds.size.height });
                        let new = dragged_ratio(d.start_ratio, along(e.event.position) - d.start, extent, size);
                        let window_id = d.window_id.clone();
                        let old = this.active_window().filter(|w| w.id == window_id).and_then(|w| layout_ops::split_ratio(&w.layout, &split));
                        if let Some(c) = old.and_then(|(_, old)| layout_ops::change(&split, old, new)) {
                            this.set_ratios(&window_id, vec![c], cx);
                        }
                    },
                ));
                match axis {
                    Axis::Horizontal => container
                        .flex_row()
                        .child(div().h_full().w(relative(ratio)).flex_none().min_w_0().child(a))
                        .child(divider.h_full().w(px(size)))
                        .child(div().h_full().flex_1().min_w_0().child(b))
                        .child(hit.top_0().bottom_0().left(relative(ratio)).ml(inset).w(px(DIVIDER_HIT)))
                        .into_any_element(),
                    Axis::Vertical => container
                        .flex_col()
                        .child(div().w_full().h(relative(ratio)).flex_none().min_h_0().child(a))
                        .child(divider.w_full().h(px(size)))
                        .child(div().w_full().flex_1().min_h_0().child(b))
                        .child(hit.left_0().right_0().top(relative(ratio)).mt(inset).h(px(DIVIDER_HIT)))
                        .into_any_element(),
                }
            }
            Layout::Unknown => unsupported(),
        }
    }

    /// One pane: header and terminal together in one card (Comfortable) or
    /// flush (Compact). The terminal is laid out at its final size once; a
    /// new split only animates the card's offset inside its clipped slot.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_cell(
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
        // Floats are always cards, so they read as floating.
        let floating = self.active_window().is_some_and(|w| crate::floats::is_float(w, id));
        let comfortable = floating || self.density() == Density::Comfortable;
        let frosted = self.frosted();
        let browser = self.browsers.contains_key(id);
        let headers = self.pane_headers();
        // Corners just inside the 1px outline, for overlays (wash, drop
        // outline, docks) and the page that fill the card's rounded clip.
        let fill = corners.inset(1.0);
        let docks = if floating { Vec::new() } else { self.dock_zones(id, fill, cx) };
        let entering = self
            .motion
            .pane_enter
            .get(id)
            .and_then(|(started, edge)| motion::progress(*started, motion::PANE_ENTER, now).map(|p| edge.offset(1.0 - p)));
        let inner: AnyElement = match (self.panes.get(id), self.browsers.get(id), self.agents.get(id)) {
            (Some((pane, _)), _, _) => pane.clone().into_any_element(),
            (None, Some((pane, _)), _) => pane.clone().into_any_element(),
            (None, None, Some((pane, _))) => pane.clone().into_any_element(),
            (None, None, None)
                if matches!(
                    crate::browser_pane::kind_of(self.block_info(id)),
                    crate::browser_pane::Kind::Unsupported | crate::browser_pane::Kind::Agent
                ) =>
            {
                unsupported()
            }
            (None, None, None) => div().size_full().into_any_element(),
        };
        let fill_color = self.card_fill(id, cx);
        // A float, or the pane zooming over its siblings, sits over the
        // app's own content, which the window blur does not reach: opaque.
        // A zoom is a veil in its slot and when whole, opaque in between.
        let zoom = self.motion.zoom_frame(now).filter(|(block, ..)| *block == id).and(self.motion.zoom_state(now));
        let palette = match zoom {
            _ if floating => self.card_chrome(fill_color).floating(),
            Some((_, cover)) => self.card_chrome(fill_color).toward_floating(cover),
            None => self.card_chrome(fill_color),
        };
        if let Some((pane, _)) = self.browsers.get(id) {
            let page = if comfortable { (fill.bottom_left, fill.bottom_right) } else { (0.0, 0.0) };
            pane.update(cx, |pane, cx| {
                pane.set_palette(palette, cx);
                pane.set_corners(page, cx);
            });
        }
        if let Some((pane, _)) = self.panes.get(id) {
            let translucency = self.translucency(id, fill_color, palette, cx);
            pane.update(cx, |pane, cx| {
                pane.set_translucency(translucency, cx);
                pane.set_card_corner(corners.bottom_right, cx);
            });
        }
        // Header and terminal are one surface; an inactive pane is dimmed
        // as a whole so no band appears between them. Solid: a wash of the
        // card's colour (no listeners: clicks and hovers reach below).
        // Frosted: the content fades instead, so no second veil appears.
        let dim = if browser { theme::INACTIVE_DIM_PAGE } else { theme::INACTIVE_DIM };
        let fade = (dimmed && frosted).then_some(1.0 - dim);
        let wash = (dimmed && !frosted).then(|| {
            div().absolute().top_0().left_0().size_full().bg(rgb(fill_color).opacity(dim)).when(comfortable, |el| el.continuous_corners(fill))
        });
        let content = div().relative().flex_1().min_h_0().min_w_0().when_some(fade, |el, f| el.opacity(f)).child(inner);
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
            .bg(palette.fill())
            .when(comfortable, |el| el.continuous_corners(corners).border_1().border_color(rgb(theme::card_outline_on(fill_color))))
            .rounded_clip()
            .when(headers, |el| {
                el.child(
                    div().flex_none().when_some(fade, |el, f| el.opacity(f)).child(self.render_pane_header(id, active, zoomed, swap, palette, cx)),
                )
            })
            .child(content)
            .children(wash)
            .children(drop)
            .children(docks);
        let card: AnyElement = match entering {
            Some((dx, dy)) => div().absolute().left(relative(dx)).top(relative(dy)).size_full().child(card).into_any_element(),
            None => card.into_any_element(),
        };
        // Frosted: the window veil fills the slot outside the card, meeting
        // the card's veil without overlapping it. Not while zooming over
        // other panes: its own rounded clip shapes it there.
        let settled = comfortable && entering.is_none() && zoom.is_none();
        let veil = (settled && frosted && !floating).then(|| slot_veil(corners, self.chrome.fill()));
        div()
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .when(entering.is_some(), |el| el.overflow_hidden())
            .children(veil)
            .child(card)
            .into_any_element()
    }

    fn render_pane_header(
        &self,
        id: &str,
        active: bool,
        zoomed: bool,
        swap: Option<&str>,
        palette: crate::adaptive::Chrome,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (ink, muted, icon_color, accent) = (palette.ink, palette.muted, palette.icon, palette.accent);
        let info = self
            .panes
            .get(id)
            .and_then(|(p, _)| p.read(cx).info.clone())
            .or_else(|| self.view.as_ref().and_then(|v| v.blocks.iter().find(|b| b.id == id).cloned()));
        let browser = self.browsers.get(id).map(|(p, _)| p.read(cx).title());
        let glyph = self.block_icon(id);
        // An agent on its chat screen: the session title opens the switcher,
        // and History / New Session join the pane buttons (not on a sub-agent:
        // its strip leads back to the main agent instead).
        let agent = self.agents.get(id).map(|(p, _)| p.clone());
        let session = agent.as_ref().and_then(|p| p.read(cx).header_title());
        let (name, cwd) = match browser {
            Some(title) => (title, None),
            None if session.is_some() => (session.clone().unwrap_or_default(), None),
            None if info.as_ref().is_some_and(|i| i.kind == silo_proto::model::BlockKind::Agent) => {
                (info.map(|i| i.title).filter(|t| !t.is_empty()).unwrap_or_else(|| "Agent".into()), None)
            }
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
                .hover(|s| s.bg(palette.hover()))
                .tooltip(Tooltip::text(tip))
                .child(icon(glyph, 13.0, icon_color))
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
        let close = {
            let block = block.clone();
            cx.listener(move |this: &mut Workspace, _: &gpui::ClickEvent, window: &mut Window, cx: &mut Context<Workspace>| {
                cx.stop_propagation();
                this.close_block_id(block.clone(), window, cx);
            })
        };
        let session_controls = session.is_some().then(|| agent.clone()).flatten().filter(|p| !p.read(cx).is_subagent()).map(|pane| {
            let (p1, p2, p3) = (pane.clone(), pane.clone(), pane);
            let title = div()
                .id(gpui::ElementId::Name(format!("pane-agent-title-{id}").into()))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .min_w_0()
                .pl(px(5.0))
                .pr(px(3.0))
                .h(px(BUTTON))
                .continuous(control)
                .cursor_pointer()
                .hover(|s| s.bg(palette.hover()))
                .child(div().min_w_0().overflow_hidden().text_ellipsis().text_color(rgb(if active { ink } else { muted })).child(name.clone()))
                .child(icon("chevron-down", 11.0, icon_color))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| p1.update(cx, |p, cx| p.open_sessions(window, cx)));
            let history = button("agent-history", "history", "Sessions  ⌘J")
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| p2.update(cx, |p, cx| p.open_sessions(window, cx)));
            let new = button("agent-new", "new-session", "New Session  /new")
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| p3.update(cx, |p, cx| p.start_new_session(window, cx)));
            (title, history, new)
        });
        let (agent_title, agent_buttons) = match session_controls {
            Some((title, history, new)) => (Some(title), Some([history, new])),
            None => (None, None),
        };
        let focus_block = block.clone();
        let floating = self.active_window().is_some_and(|w| crate::floats::is_float(w, id));
        let status = self.render_status_mark(id, palette, cx);
        let progress = self.render_progress_bar(id, palette);
        div()
            .id(gpui::ElementId::Name(format!("pane-header-{id}").into()))
            .relative()
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
            .child({
                let color = if active { accent } else { icon_color };
                self.block_icon_view(id, "header", 12.0, color, cx)
            })
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
                    .map(|el| match agent_title {
                        // The title's hover fill reaches past its text; the text keeps its place.
                        Some(title) => el.items_center().ml(px(-5.0)).child(title),
                        None => el.child(div().flex_none().text_color(rgb(if active { ink } else { muted })).child(name)),
                    })
                    .children(cwd.map(|c| div().min_w_0().text_ellipsis().text_color(rgb(muted)).child(c))),
            )
            .children(status)
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
                    .children(agent_buttons.into_iter().flatten())
                    // A float can't be split or zoomed: Close only.
                    .when(!floating, |el| {
                        el.child(button("split-right", "split-right", "Split Right  ⌘D").on_click(split_right))
                            .child(button("split-down", "split-down", "Split Down  ⇧⌘D").on_click(split_down))
                            .child(
                                button("zoom", if zoomed { "unzoom" } else { "zoom" }, if zoomed { "Restore Pane  ⇧⌘↩" } else { "Zoom Pane  ⇧⌘↩" })
                                    .on_click(zoom),
                            )
                    })
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
            .children(progress)
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
        let corner = if comfortable { radius::window_corner(window) } else { None };
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
                // Drawn over the panes growing into its place: opaque.
                let palette = self.card_chrome(ghost.fill).floating();
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
                        .text_color(rgb(palette.muted))
                        .child(icon("terminal", 12.0, palette.icon))
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
                                .bg(palette.fill())
                                .when(comfortable, |el| {
                                    el.continuous_corners(corners).border_1().border_color(rgb(theme::card_outline_on(ghost.fill)))
                                })
                                .rounded_clip()
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
            .bg(if theme::veils().is_some() { theme::overlay_fill() } else { rgb(theme::background()) })
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

/// A card's outline, for content clipped to the card's shape but drawn at
/// another size. `fill`: the card's colour.
fn card_outline(corners: CardCorners, fill: u32) -> gpui::Div {
    div().absolute().inset_0().border_1().border_color(rgb(theme::card_outline_on(fill))).continuous_corners(corners)
}

/// The window veil over a card slot outside the card's continuous corners,
/// as one surface: an inset shadow whose hole is exactly the card's shape
/// (same SDF as the card's quad), so the two veils meet without overlapping.
/// The shadow's element overhangs the slot by `SPREAD` (radii grown to
/// match) and is clipped to it, leaving only the slot's corners.
fn slot_veil(corners: CardCorners, window: gpui::Rgba) -> gpui::Div {
    const SPREAD: f32 = 16.0;
    let grow = |r: f32| if r > 0.0 { r + SPREAD } else { 0.0 };
    let outer = CardCorners {
        top_left: grow(corners.top_left),
        top_right: grow(corners.top_right),
        bottom_right: grow(corners.bottom_right),
        bottom_left: grow(corners.bottom_left),
    };
    let veil =
        gpui::BoxShadow { color: window.into(), offset: gpui::point(px(0.0), px(0.0)), blur_radius: px(0.0), spread_radius: px(SPREAD), inset: true };
    div().absolute().inset_0().overflow_hidden().child(
        div().absolute().top(px(-SPREAD)).left(px(-SPREAD)).right(px(-SPREAD)).bottom(px(-SPREAD)).continuous_corners(outer).shadow(vec![veil]),
    )
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

/// What a newer host's pane or layout node shows here.
pub(crate) const UNSUPPORTED: &str = "Update Silo to show this pane";

fn unsupported() -> AnyElement {
    div().size_full().flex().items_center().justify_center().text_size(px(12.0)).text_color(rgb(theme::muted())).child(UNSUPPORTED).into_any_element()
}

/// The axis of the split directly holding block `id`.
pub(crate) fn parent_axis(layout: &Layout, id: &str) -> Option<Axis> {
    match layout {
        Layout::Block { .. } | Layout::Unknown => None,
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

/// Axis of an automatic split of a pane laid out at `rect`: right when it
/// is wider than tall, down otherwise; right before its first layout.
pub(crate) fn auto_axis(rect: Option<gpui::Bounds<gpui::Pixels>>) -> Axis {
    match rect {
        Some(r) if r.size.height >= r.size.width => Axis::Vertical,
        _ => Axis::Horizontal,
    }
}

impl Workspace {
    /// [`auto_axis`] of `block`'s slot as last laid out.
    pub(crate) fn auto_axis(&self, block: &str) -> Axis {
        auto_axis(self.motion.measured.cells.borrow().get(block).copied())
    }
}

/// Smallest size
/// Smallest size (pt) either side of a split keeps while its divider is dragged.
const MIN_PANE: f32 = 60.0;

/// A divider being dragged: its split, and the ratio and pointer
/// position along the split's axis when it was pressed.
#[derive(Clone)]
pub(crate) struct DividerDrag {
    window_id: String,
    path: Vec<usize>,
    start_ratio: f32,
    start: f32,
}

/// The (invisible) view a divider drag carries; the cursor shows the drag.
struct DividerGrab;

impl gpui::Render for DividerGrab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The ratio of a split `extent` long, `divider` of it the divider, after
/// its divider is dragged `delta` from where it was at ratio `start`.
/// Either side keeps [`MIN_PANE`] and [`MIN_RATIO`]; a split too small for
/// both keeps `start`.
pub(crate) fn dragged_ratio(start: f32, delta: f32, extent: f32, divider: f32) -> f32 {
    if extent <= 0.0 {
        return start;
    }
    let lo = MIN_RATIO.max(MIN_PANE / extent);
    let hi = (1.0 - MIN_RATIO).min((extent - divider - MIN_PANE) / extent);
    if lo > hi {
        return start;
    }
    (start + delta / extent).clamp(lo, hi)
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
    fn automatic_splits_follow_the_longer_side() {
        let rect = |w: f32, h: f32| Some(gpui::Bounds::new(gpui::point(px(10.0), px(20.0)), gpui::size(px(w), px(h))));
        assert_eq!(auto_axis(rect(800.0, 600.0)), Axis::Horizontal);
        assert_eq!(auto_axis(rect(400.0, 600.0)), Axis::Vertical);
        assert_eq!(auto_axis(rect(500.0, 500.0)), Axis::Vertical);
        assert_eq!(auto_axis(None), Axis::Horizontal);
    }

    #[test]
    fn dragging_a_divider_moves_its_ratio_within_the_minimum_sizes() {
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        // 1000pt split, 6pt divider: 100pt is a tenth.
        assert!(near(dragged_ratio(0.5, 100.0, 1000.0, 6.0), 0.6));
        assert!(near(dragged_ratio(0.5, -250.0, 1000.0, 6.0), 0.25));
        // Either side keeps MIN_PANE (60pt), the divider included on the far side.
        assert!(near(dragged_ratio(0.5, -900.0, 1000.0, 6.0), 0.06));
        assert!(near(dragged_ratio(0.5, 900.0, 1000.0, 6.0), (1000.0 - 6.0 - 60.0) / 1000.0));
        // Large splits still keep MIN_RATIO.
        assert!(near(dragged_ratio(0.5, -5000.0, 4000.0, 1.0), MIN_RATIO));
        assert!(near(dragged_ratio(0.5, 5000.0, 4000.0, 1.0), 1.0 - MIN_RATIO));
        // Too small for both minimums, or unmeasured: unchanged.
        assert!(near(dragged_ratio(0.4, 30.0, 100.0, 6.0), 0.4));
        assert!(near(dragged_ratio(0.4, 30.0, 0.0, 6.0), 0.4));
    }

    #[test]
    fn panes_swap_only_within_one_unzoomed_multi_pane_window() {
        let window =
            |layout: Layout, zoomed: bool| WindowInfo { id: "w".into(), name: None, layout, focused: "a".into(), zoomed, floats: Vec::new() };
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
            owner_name: None,
            pid: None,
            browser: None,
            agent: None,
        };
        assert_eq!(header_labels(Some(&info("-zsh", Some(format!("{home}/dev"))))), ("zsh".into(), Some("~/dev".into())));
        assert_eq!(header_labels(Some(&info("", None))), ("shell".into(), None));
        assert_eq!(header_labels(Some(&info("~/dev", Some(format!("{home}/dev"))))), ("~/dev".into(), None));
        assert_eq!(header_labels(None), ("shell".into(), None));
    }
}
