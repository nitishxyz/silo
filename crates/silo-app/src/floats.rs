//! Floating panes: cards over a window's tiled layout, back to front.
//! Rects are fractions of the window body, so every client agrees.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Bounds, Context, DragMoveEvent, Pixels, Point, Window, div, prelude::*, px, relative, rgb};
use silo_proto::{
    compat::feature,
    control::{Dock, Method},
    model::{Axis, Float, FloatRect, WindowInfo},
};

use crate::theme::Continuous as _;
use crate::{
    adaptive::Chrome,
    layout::{PaneDrop, Tiling},
    pane_header::HeaderMode,
    radius::{self, CardCorners},
    theme,
    workspace::Workspace,
};

impl Workspace {
    /// The rect of the float holding `block_id` in any window of the
    /// current session; `None` for a tiled or unknown block.
    pub(crate) fn float_rect(&self, block_id: &str) -> Option<FloatRect> {
        let windows = &self.view.as_ref()?.session.windows;
        windows.iter().find_map(|w| w.float_of(block_id)).map(|f| f.rect)
    }

    /// The id of the float holding `block_id`.
    pub(crate) fn float_id(&self, block_id: &str) -> Option<String> {
        let windows = &self.view.as_ref()?.session.windows;
        windows.iter().find_map(|w| w.float_of(block_id)).map(|f| f.id.clone())
    }

    /// Float the focused tiled pane above the layout.
    pub(crate) fn float_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.focused_block(window, cx) else { return };
        if self.active_window().is_some_and(|w| w.layout.contains(&block)) {
            self.call(Method::BlockFloat { block_id: block, rect: None });
        }
    }

    /// Dock the focused float (all its panes) to the right of the first
    /// tiled pane.
    pub(crate) fn dock_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.focused_block(window, cx).filter(|b| self.float_rect(b).is_some()) else { return };
        let Some(target) = self.active_window().and_then(|w| w.layout.blocks().first().cloned()) else { return };
        self.call(Method::BlockMove { block_id: block, rect: None, dock: Some(Dock { target, axis: Axis::Horizontal, before: false }) });
    }

    /// ⇧⌘M: minimize the float holding the focused block.
    pub(crate) fn minimize_focused_float(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(block) = self.focused_block(window, cx) {
            self.minimize_float(&block, true, cx);
        }
    }

    /// Minimize (or show again) the float holding `block`: here at once,
    /// then on the server, which moves focus out of (or into) it.
    pub(crate) fn minimize_float(&mut self, block: &str, minimized: bool, cx: &mut Context<Self>) {
        if !self.host_supports(feature::FLOAT_PANES) {
            return;
        }
        let set = |block: String, minimized: bool| {
            move |this: &mut Self| {
                let floats = this.view.as_mut().into_iter().flat_map(|v| v.session.windows.iter_mut()).flat_map(|w| w.floats.iter_mut());
                for f in floats.filter(|f| f.contains(&block)) {
                    f.minimized = minimized;
                }
            }
        };
        if self.view.as_ref().and_then(|v| v.session.windows.iter().find_map(|w| w.float_of(block))).is_none_or(|f| f.minimized == minimized) {
            return;
        }
        crate::sound::play(crate::sound::Sound::Select);
        set(block.to_owned(), minimized)(self);
        // The server's focus moves; follow it.
        self.server_focus = None;
        let method = Method::BlockMinimize { block_id: block.to_owned(), minimized };
        self.call_optimistic(method, "Couldn't minimize the float", set(block.to_owned(), !minimized), cx);
        cx.notify();
    }

    /// Double-click on a float's header: fill the body, or go back to the
    /// rect it had.
    fn toggle_float_fill(&mut self, float: &str, cx: &mut Context<Self>) {
        let Some(rect) = self.float_rect(float) else { return };
        let next = match self.float_restore.remove(float) {
            Some(before) if near(rect, FILL) => before,
            _ => {
                self.float_restore.insert(float.to_owned(), rect);
                FILL
            }
        };
        self.set_float_rect(float, rect, next, cx);
    }

    /// Show float `float` at `rect` here, then on the server (back to
    /// `before` if it refuses).
    fn set_float_rect(&mut self, float: &str, before: FloatRect, rect: FloatRect, cx: &mut Context<Self>) {
        let set = |block: String, rect: FloatRect| {
            move |this: &mut Self| {
                let floats = this.view.as_mut().into_iter().flat_map(|v| v.session.windows.iter_mut()).flat_map(|w| w.floats.iter_mut());
                for f in floats.filter(|f| f.contains(&block)) {
                    f.rect = rect;
                }
            }
        };
        set(float.to_owned(), rect)(self);
        let method = Method::BlockMove { block_id: float.to_owned(), rect: Some(rect), dock: None };
        self.call_optimistic(method, "Couldn't move pane", set(float.to_owned(), before), cx);
        cx.notify();
    }

    /// Pointer moved during a float drag; `ev.bounds` is the window body.
    pub(crate) fn float_drag_move(&mut self, ev: &DragMoveEvent<Dragged>, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.float_drag.as_mut() else { return };
        let rect = dragged_rect(drag.start, drag.grab, ev.event.position, drag.press, drag.offset, ev.bounds);
        let moving = drag.grab == Grab::Move;
        let changed = rect != drag.rect;
        drag.rect = rect;
        let dock = if moving { self.dock_target(ev.event.position) } else { None };
        let Some(drag) = self.float_drag.as_mut() else { return };
        if changed || drag.dock != dock {
            drag.dock = dock;
            cx.notify();
        }
    }

    /// The drag ended over a pane's edge: dock there; else commit the new rect.
    fn float_drag_end(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.float_drag.take() else { return };
        cx.notify();
        if let Some((target, side)) = drag.dock {
            let (axis, before) = side.dock().unwrap_or((Axis::Horizontal, false));
            self.float_restore.remove(&drag.block);
            self.call(Method::BlockMove { block_id: drag.block, rect: None, dock: Some(Dock { target, axis, before }) });
            return;
        }
        if drag.rect == drag.start {
            return;
        }
        // Moved by hand: no longer filling the body.
        self.float_restore.remove(&drag.block);
        self.set_float_rect(&drag.block, drag.start, drag.rect, cx);
    }

    /// A float's colours for a card of colour `fill`. Frosted: glass of the
    /// card's colour over the float's own backdrop blur (of the app beneath,
    /// which the window blur doesn't reach), at an alpha its text reads
    /// at; Solid, or where no alpha reads: the card's opaque look.
    pub(crate) fn float_chrome(&self, fill: u32) -> Chrome {
        let card = self.card_chrome(fill);
        if card.alpha < 1.0 {
            let glass = float_glass(card.surface);
            if glass.frosted() {
                return Chrome::veiled(card.surface, glass.alpha);
            }
        }
        card.floating()
    }
}

/// [`crate::surface::glass`] of `tint` for the theme, cached.
fn float_glass(tint: u32) -> crate::surface::Glass {
    type Key = (u32, u32, u32);
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<Key, crate::surface::Glass>> = Default::default();
    }
    let key = (tint, theme::background(), theme::foreground());
    CACHE.with_borrow_mut(|cache| {
        if cache.len() >= 64 {
            cache.clear();
        }
        *cache.entry(key).or_insert_with(|| crate::surface::glass(key.0, key.1, key.2, true))
    })
}

impl Workspace {
    /// `w`'s floats over the body, back to front, then the minimized ones'
    /// stack. A float is a window of its own: its panes tile as cards in
    /// one frosted (or, Solid, opaque) frame that moves and resizes as one.
    /// Pressing a single pane's float focuses it; the server raises a
    /// focused float.
    pub(crate) fn render_floats(&self, w: &WindowInfo, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let focused = self.focused_block(window, cx);
        let shown: Vec<&Float> = w.floats.iter().filter(|f| !f.minimized).collect();
        let top = shown.last().map(|f| f.id.clone());
        let mut out: Vec<AnyElement> = shown
            .into_iter()
            .map(|f| {
                let dragged = self.float_drag.as_ref().filter(|d| d.block == f.id);
                // Over a pane's edge it would dock: see the ghost through it.
                let docking = dragged.is_some_and(|d| d.dock.is_some());
                let (id, r) = (f.id.clone(), dragged.map_or(f.rect, |d| d.rect).clamped());
                let layout = f.layout();
                let blocks = layout.blocks();
                let several = blocks.len() > 1;
                // A full header moves it itself; otherwise a thin strip
                // along the top does (over a browser's shrunk header too).
                let mover = blocks.iter().any(|b| self.header_mode(b) != HeaderMode::Full).then(|| {
                    self.float_grab(grip(&id, Grab::Move), &id, r, Grab::Move, 0.0, cx).absolute().top_0().left_0().right(px(MOVER_INSET)).h(px(GRIP))
                });
                let resizers = Sides::ALL.into_iter().map(|sides| self.float_resizer(&id, r, sides, cx)).collect::<Vec<_>>();
                let raise = top.as_deref() != Some(id.as_str());
                let chrome = self.float_chrome(self.card_fill(&id, cx));
                let frosted = chrome.alpha < 1.0;
                let tiling = Tiling { float: Some(id.clone()), veil: frosted.then(|| chrome.fill()) };
                let place = crate::layout::Placement::FLOAT;
                let body = self.render_layout(&layout, &[], focused.as_deref(), true, false, None, place, &tiling, now, cx);
                // Where it's drawn, for its exit when it closes.
                let (floats, measured) = (self.motion.measured.floats.clone(), id.clone());
                let measure = gpui::canvas(move |b, _, _| _ = floats.borrow_mut().insert(measured, b), |_, (), _, _| {}).absolute().inset_0();
                // The frame: one blur under every card and gap (Frosted),
                // or the gaps' opaque fill (Solid), clipped to its corners.
                // Several panes sit inside it the same gap from its edges
                // as from each other, its corners concentric with theirs;
                // Frosted, the margin is tinted as the gaps are (once).
                let pad = if several { crate::layout::GAP } else { 0.0 };
                let margin = |el: gpui::Div| match tiling.veil.filter(|_| several) {
                    Some(veil) => el.child(div().absolute().top_0().left_0().right_0().h(px(pad)).bg(veil))
                        .child(div().absolute().bottom_0().left_0().right_0().h(px(pad)).bg(veil))
                        .child(div().absolute().top(px(pad)).bottom(px(pad)).left_0().w(px(pad)).bg(veil))
                        .child(div().absolute().top(px(pad)).bottom(px(pad)).right_0().w(px(pad)).bg(veil)),
                    None => el,
                };
                let frame = margin(
                    div()
                        .absolute()
                        .inset_0()
                        .continuous(radius::CARD + pad)
                        .shadow_lg()
                        .when(frosted, |el| el.backdrop_blur(px(crate::surface::BLUR)).backdrop_saturation(crate::surface::SATURATION))
                        .when(several && !frosted, |el| el.bg(chrome.fill()))
                        .when(several, |el| el.border_1().border_color(theme::card_outline(chrome.surface)))
                        .rounded_clip(),
                )
                .child(div().absolute().inset(px(pad)).child(body));
                let single = (!several).then(|| id.clone());
                div()
                    .id(gpui::ElementId::Name(format!("float-{id}").into()))
                    .absolute()
                    .left(relative(r.x))
                    .top(relative(r.y))
                    .w(relative(r.w))
                    .h(relative(r.h))
                    .when(self.float_drag.is_none(), |el| el.occlude())
                    .when(docking, |el| el.opacity(0.5))
                    .capture_any_mouse_down(cx.listener(move |this, _, window, cx| {
                        // Several panes: the one pressed takes focus itself
                        // (and the server raises its float).
                        if let Some(id) = &single {
                            this.focus_block(id, window, cx);
                            if raise {
                                this.call(Method::BlockFocus { block_id: id.clone() });
                            }
                        }
                    }))
                    .child(frame)
                    .child(measure)
                    .children(mover)
                    .children(resizers)
                    .into_any_element()
            })
            .collect();
        out.extend(self.render_minimized(w, now, window, cx));
        out
    }

    /// The edge or corner `sides` of float `id` at `r`: drags resize it.
    fn float_resizer(&self, id: &str, r: FloatRect, sides: Sides, cx: &mut Context<Self>) -> AnyElement {
        let grab = Grab::Resize(sides);
        let el = self.float_grab(grip(id, grab), id, r, grab, 0.0, cx).absolute();
        // Straddling the edge: half outside, so little of the pane is covered.
        let (half, corner) = (px(-EDGE / 2.0), px(CORNER));
        let el = match (sides.left, sides.top, sides.right, sides.bottom) {
            (true, false, false, false) => el.left(half).top(corner).bottom(corner).w(px(EDGE)),
            (false, false, true, false) => el.right(half).top(corner).bottom(corner).w(px(EDGE)),
            (false, true, false, false) => el.top(half).left(corner).right(corner).h(px(EDGE)),
            (false, false, false, true) => el.bottom(half).left(corner).right(corner).h(px(EDGE)),
            (l, t, _, _) => {
                let el = el.size(px(CORNER));
                let el = if l { el.left(px(-EDGE / 2.0)) } else { el.right(px(-EDGE / 2.0)) };
                if t { el.top(px(-EDGE / 2.0)) } else { el.bottom(px(-EDGE / 2.0)) }
            }
        };
        el.cursor(sides.cursor()).into_any_element()
    }

    /// The minimized floats as round buttons stacked in the body's
    /// bottom-right corner, the latest in front; hovering fans them out to
    /// the left. Pressing one shows its float again.
    fn render_minimized(&self, w: &WindowInfo, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let minimized: Vec<&Float> = w.floats.iter().rev().filter(|f| f.minimized).collect();
        if minimized.is_empty() {
            return None;
        }
        let (fanned, since) = self.minimized_fan;
        let p = crate::motion::progress(since, FAN, now);
        if p.is_some() {
            window.request_animation_frame();
        }
        let p = p.unwrap_or(1.0);
        let open = if fanned { p } else { 1.0 - p };
        let count = minimized.len();
        // Stacked: each a few points left of the one in front, smaller and
        // fainter; fanned: side by side.
        let offset = |i: usize| {
            let i = i as f32;
            let stacked = i * STACK_STEP;
            stacked + (i * (MINI + MINI_GAP) - stacked) * open
        };
        let width = MINI + offset(count - 1);
        let palette = self.float_chrome(self.chrome.surface);
        let buttons = minimized.into_iter().enumerate().rev().map(|(i, f)| {
            let id = f.id.clone();
            let panes = f.blocks().len();
            let fill = self.card_fill(&id, cx);
            let title = self.header_label(&id, fill, cx);
            let label = if panes > 1 { format!("{title} + {}", panes - 1) } else { title };
            let fade = if i == 0 { 1.0 } else { (1.0 - 0.25 * i as f32).max(0.0) + (1.0 - (1.0 - 0.25 * i as f32).max(0.0)) * open };
            let scale = 1.0 - (1.0 - open) * (0.08 * i as f32).min(0.24);
            let size = MINI * scale;
            let status = self.render_status_mark(&id, palette, cx);
            let glyph = self.block_icon_view(&id, "minimized", 15.0, palette.icon, cx);
            div()
                .id(gpui::ElementId::Name(format!("minimized-{id}").into()))
                .absolute()
                .right(px(offset(i) + (MINI - size) / 2.0))
                .bottom(px((MINI - size) / 2.0))
                .size(px(size))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .opacity(fade)
                .bg(palette.fill())
                .when(palette.alpha < 1.0, |el| el.backdrop_blur(px(crate::surface::BLUR)).backdrop_saturation(crate::surface::SATURATION))
                .border_1()
                .border_color(theme::card_outline(fill))
                .shadow_md()
                .cursor_pointer()
                .hover(|s| s.bg(palette.selected()))
                .tooltip(crate::tooltip::Tooltip::text(label))
                .child(glyph)
                .children(status.map(|s| div().absolute().top(px(-2.0)).right(px(-2.0)).child(s)))
                .children((panes > 1).then(|| {
                    div()
                        .absolute()
                        .bottom(px(-3.0))
                        .right(px(-3.0))
                        .min_w(px(15.0))
                        .h(px(15.0))
                        .px(px(3.0))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(rgb(palette.accent))
                        .text_color(rgb(theme::ink_on(palette.accent)))
                        .text_size(px(9.5))
                        .font_family(theme::ui_font())
                        .child(panes.to_string())
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.minimize_float(&id, false, cx);
                }))
        })
        .collect::<Vec<_>>();
        Some(
            div()
                .id("minimized-floats")
                .absolute()
                .right(px(MINI_MARGIN))
                .bottom(px(MINI_MARGIN))
                .w(px(width))
                .h(px(MINI))
                .occlude()
                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                    if this.minimized_fan.0 != *hovered {
                        // Reversed mid-way: from where it is now.
                        let now = Instant::now();
                        let left = crate::motion::progress(this.minimized_fan.1, FAN, now).map_or(0.0, |p| 1.0 - p);
                        this.minimized_fan = (*hovered, now - FAN.mul_f32(left));
                        cx.notify();
                    }
                }))
                .children(buttons)
                .into_any_element(),
        )
    }
}

impl Workspace {
    /// `el` drags the float holding block `id` (a header or strip: move;
    /// an edge or corner: resize); `inset`: how far inside the float `el`
    /// starts (a header, inside the card's border). Double-clicking a move
    /// handle fills the body with the float, or puts it back.
    pub(crate) fn float_grab<E: StatefulInteractiveElement>(
        &self,
        el: E,
        id: &str,
        rect: FloatRect,
        grab: Grab,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> E {
        let workspace = cx.entity().downgrade();
        let float = self.float_id(id).unwrap_or_else(|| id.to_owned());
        let block = float.clone();
        let (press, pressed) = (self.float_press.clone(), self.float_press.clone());
        let measured = self.motion.measured.floats.clone();
        let fill = workspace.clone();
        // gpui measures the offset where the drag starts, past its threshold:
        // measured from the press instead, the float doesn't jump.
        el.on_mouse_down(gpui::MouseButton::Left, move |e, _, cx| {
            press.set(e.position);
            if grab == Grab::Move && e.click_count == 2 {
                _ = fill.update(cx, |this, cx| this.toggle_float_fill(&float, cx));
            }
        })
        .on_drag(Dragged, move |_, offset, window, cx| {
            let press = pressed.get();
            // The press inside the float as drawn (a pane's header in a split float sits inside its margin,
            // and away from its corner); else inside the handle, `inset` into the float.
            let offset = match measured.borrow().get(&block) {
                Some(bounds) => press - bounds.origin,
                None => offset - (window.mouse_position() - press) + gpui::point(px(inset), px(inset)),
            };
            let drag = FloatDrag { block: block.clone(), grab, offset, press, start: rect, rect, dock: None };
            _ = workspace.update(cx, |this, _| this.float_drag = Some(drag));
            let workspace = workspace.clone();
            // The preview lives exactly as long as the drag.
            cx.new(|cx| {
                cx.on_release(move |_, cx| _ = workspace.update(cx, |this, cx| this.float_drag_end(cx))).detach();
                Grip
            })
        })
    }
}

impl Workspace {
    /// While a float is moved: the ghost of where it would dock on tiled
    /// pane `target`, if the pointer is in that pane's edge band there.
    pub(crate) fn dock_ghost(&self, target: &str, comfortable: bool, fill: CardCorners) -> Option<gpui::Div> {
        let (over, side) = self.float_drag.as_ref()?.dock.as_ref()?;
        (over == target).then(|| crate::layout::drop_ghost(*side, comfortable, fill))
    }

    /// The tiled pane under `at` and the side a float dropped there docks
    /// on, if `at` is within [`DOCK_BAND`] of one of its edges. Hosts
    /// without `dock_sides` dock only right or below.
    fn dock_target(&self, at: Point<Pixels>) -> Option<(String, PaneDrop)> {
        let w = self.active_window()?;
        let sides = self.host_supports(feature::DOCK_SIDES);
        let cells = self.motion.measured.cells.borrow();
        w.layout.blocks().into_iter().find_map(|b| {
            let bounds = *cells.get(&b)?;
            let side = dock_side(at, bounds, sides)?;
            Some((b, side))
        })
    }
}

/// Points from a pane's edge within which a moved float docks there.
const DOCK_BAND: f32 = 36.0;

/// The side of `bounds` within [`DOCK_BAND`] of `at` (inside), nearest
/// first; only right and bottom without `sides`.
fn dock_side(at: Point<Pixels>, bounds: Bounds<Pixels>, sides: bool) -> Option<PaneDrop> {
    if !bounds.contains(&at) {
        return None;
    }
    let (x, y) = (f32::from(at.x - bounds.origin.x), f32::from(at.y - bounds.origin.y));
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    [(x, PaneDrop::Left), (w - x, PaneDrop::Right), (y, PaneDrop::Top), (h - y, PaneDrop::Bottom)]
        .into_iter()
        .filter(|&(d, side)| d < DOCK_BAND && (sides || matches!(side, PaneDrop::Right | PaneDrop::Bottom)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, side)| side)
}

/// A handle over float `id`. Occluding: the terminal below must not start
/// a selection.
fn grip(id: &str, grab: Grab) -> gpui::Stateful<gpui::Div> {
    div().id(gpui::ElementId::Name(format!("float-{grab:?}-{id}").into())).occlude()
}

/// The rect a double-clicked float fills: the body, a margin around.
const FILL: FloatRect = FloatRect { x: 0.02, y: 0.02, w: 0.96, h: 0.96 };

fn near(a: FloatRect, b: FloatRect) -> bool {
    [a.x - b.x, a.y - b.y, a.w - b.w, a.h - b.h].iter().all(|d| d.abs() < 0.005)
}

/// The (invisible) drag preview: the float itself follows the pointer.
struct Grip;

impl Render for Grip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Header width left to a float's Close button (22 px + padding).
const MOVER_INSET: f32 = 30.0;
/// Height of the move strip over a float without a full header.
const GRIP: f32 = 10.0;
/// Thickness of a float's resize edges, centred on its outline.
const EDGE: f32 = 8.0;
/// Side of its resize corners.
const CORNER: f32 = 14.0;
/// The minimized floats' buttons: diameter, gap when fanned out, step
/// when stacked, and margin from the body's corner.
const MINI: f32 = 38.0;
const MINI_GAP: f32 = 8.0;
const STACK_STEP: f32 = 5.0;
const MINI_MARGIN: f32 = 14.0;
/// How long the stack takes to fan out or fold.
const FAN: Duration = Duration::from_millis(200);

/// What a float drag changes: its place, or the sides it resizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Grab {
    Move,
    Resize(Sides),
}

/// The edges a resize moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sides {
    left: bool,
    top: bool,
    right: bool,
    bottom: bool,
}

impl Sides {
    const fn new(left: bool, top: bool, right: bool, bottom: bool) -> Self {
        Self { left, top, right, bottom }
    }

    /// Four edges, then four corners (on top of them).
    const ALL: [Self; 8] = [
        Self::new(true, false, false, false),
        Self::new(false, true, false, false),
        Self::new(false, false, true, false),
        Self::new(false, false, false, true),
        Self::new(true, true, false, false),
        Self::new(false, true, true, false),
        Self::new(false, false, true, true),
        Self::new(true, false, false, true),
    ];

    fn cursor(self) -> gpui::CursorStyle {
        match (self.left || self.right, self.top || self.bottom) {
            (true, false) => gpui::CursorStyle::ResizeLeftRight,
            (false, _) => gpui::CursorStyle::ResizeUpDown,
            (true, true) if self.left == self.top => gpui::CursorStyle::ResizeUpLeftDownRight,
            (true, true) => gpui::CursorStyle::ResizeUpRightDownLeft,
        }
    }
}

/// The drag payload: GPUI hands it to drop targets (dock zones).
#[derive(Clone)]
pub(crate) struct Dragged;

/// An in-flight float drag (`block`: the float's id): the rect shown
/// until release.
pub(crate) struct FloatDrag {
    pub(crate) block: String,
    grab: Grab,
    /// Pointer offset inside the grabbed handle.
    offset: Point<Pixels>,
    /// Where the press was.
    press: Point<Pixels>,
    start: FloatRect,
    rect: FloatRect,
    /// The tiled pane and side it docks on if released now.
    dock: Option<(String, PaneDrop)>,
}

/// `start` after the pointer pressed at `press` moved to `pos` in `body`:
/// moved (the grabbed handle held at `offset`), or resized by its `sides`,
/// the others staying put, no smaller than [`FloatRect::MIN`].
fn dragged_rect(start: FloatRect, grab: Grab, pos: Point<Pixels>, press: Point<Pixels>, offset: Point<Pixels>, body: Bounds<Pixels>) -> FloatRect {
    let (w, h) = (f32::from(body.size.width).max(1.0), f32::from(body.size.height).max(1.0));
    match grab {
        Grab::Move => {
            let x = f32::from(pos.x - offset.x - body.origin.x) / w;
            let y = f32::from(pos.y - offset.y - body.origin.y) / h;
            FloatRect { x, y, ..start }.clamped()
        }
        Grab::Resize(sides) => {
            let (dx, dy) = (f32::from(pos.x - press.x) / w, f32::from(pos.y - press.y) / h);
            let min = FloatRect::MIN;
            let (mut l, mut t, mut r, mut b) = (start.x, start.y, start.x + start.w, start.y + start.h);
            if sides.left {
                l = (l + dx).clamp(0.0, (r - min).max(0.0));
            }
            if sides.right {
                r = (r + dx).clamp(l + min, 1.0);
            }
            if sides.top {
                t = (t + dy).clamp(0.0, (b - min).max(0.0));
            }
            if sides.bottom {
                b = (b + dy).clamp(t + min, 1.0);
            }
            FloatRect { x: l, y: t, w: r - l, h: b - t }.clamped()
        }
    }
}

/// Every block of `w`: tiled in reading order, then each float's, back to
/// front.
pub(crate) fn window_blocks(w: &WindowInfo) -> Vec<String> {
    w.blocks()
}

/// [`window_blocks`] but those of minimized floats.
pub(crate) fn shown_blocks(w: &WindowInfo) -> Vec<String> {
    let mut blocks = w.layout.blocks();
    blocks.extend(w.floats.iter().filter(|f| !f.minimized).flat_map(Float::blocks));
    blocks
}

/// Whether `block` floats in `w`.
pub(crate) fn is_float(w: &WindowInfo, block: &str) -> bool {
    w.float_of(block).is_some()
}

/// Floats of `old` (by id) gone from `new` (the same window, a moment
/// later): every block closed, parked, or moved to another window, or the
/// float minimized. One docked into `new`'s layout isn't leaving: it slides
/// into its slot there.
pub(crate) fn floats_leaving(old: &WindowInfo, new: &WindowInfo) -> Vec<String> {
    let tiled = new.layout.blocks();
    old.floats
        .iter()
        .filter(|f| !f.minimized)
        .filter(|f| {
            let blocks = f.blocks();
            let floats = blocks.iter().any(|b| new.float_of(b).is_some_and(|n| !n.minimized));
            !floats && !blocks.iter().any(|b| tiled.contains(b))
        })
        .map(|f| f.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use gpui::{bounds, point, size};

    use super::*;

    #[test]
    fn drags_move_and_resize_within_the_body() {
        let body = bounds(point(px(0.0), px(40.0)), size(px(1000.0), px(500.0)));
        let rect = FloatRect { x: 0.2, y: 0.2, w: 0.5, h: 0.5 };
        let grab = point(px(10.0), px(5.0));
        let press = point(px(0.0), px(0.0));
        let moved = dragged_rect(rect, Grab::Move, point(px(110.0), px(95.0)), press, grab, body);
        assert_eq!((moved.x, moved.y, moved.w), (0.1, 0.1, 0.5));
        // Past the edge: kept inside, size unchanged.
        let moved = dragged_rect(rect, Grab::Move, point(px(990.0), px(95.0)), press, grab, body);
        assert_eq!((moved.x, moved.w), (0.5, 0.5));
        // The bottom-right corner follows the pointer; the top-left stays.
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        let corner = Grab::Resize(Sides::new(false, false, true, true));
        let press = point(px(700.0), px(390.0));
        let resized = dragged_rect(rect, corner, point(px(600.0), px(340.0)), press, grab, body);
        assert!(near(resized.x, 0.2) && near(resized.y, 0.2) && near(resized.w, 0.4) && near(resized.h, 0.4), "{resized:?}");
        let resized = dragged_rect(rect, corner, point(px(5000.0), px(0.0)), press, grab, body);
        assert!(near(resized.x, 0.2) && near(resized.w, 0.8) && near(resized.h, FloatRect::MIN), "{resized:?}");
        // The left edge: the right one stays.
        let left = Grab::Resize(Sides::new(true, false, false, false));
        let resized = dragged_rect(rect, left, point(px(100.0), px(0.0)), point(px(200.0), px(0.0)), grab, body);
        assert!(near(resized.x, 0.1) && near(resized.w, 0.6) && near(resized.y, 0.2) && near(resized.h, 0.5), "{resized:?}");
        let resized = dragged_rect(rect, left, point(px(1000.0), px(0.0)), point(px(200.0), px(0.0)), grab, body);
        assert!(near(resized.x + resized.w, 0.7) && near(resized.w, FloatRect::MIN), "{resized:?}");
        // The top edge, past the body: kept inside.
        let top = Grab::Resize(Sides::new(false, true, false, false));
        let resized = dragged_rect(rect, top, point(px(0.0), px(-900.0)), point(px(0.0), px(0.0)), grab, body);
        assert!(near(resized.y, 0.0) && near(resized.h, 0.7), "{resized:?}");
    }

    #[test]
    fn floats_dock_only_near_an_edge() {
        let pane = bounds(point(px(100.0), px(50.0)), size(px(400.0), px(300.0)));
        let at = |x: f32, y: f32| dock_side(point(px(100.0 + x), px(50.0 + y)), pane, true);
        assert_eq!(at(200.0, 150.0), None, "the middle: no dock");
        assert_eq!(at(200.0, 60.0), None, "a quarter in: still none");
        assert_eq!(at(200.0, 10.0), Some(PaneDrop::Top));
        assert_eq!(at(390.0, 150.0), Some(PaneDrop::Right));
        assert_eq!(at(5.0, 290.0), Some(PaneDrop::Left), "the nearer edge");
        assert_eq!(at(-5.0, 150.0), None, "outside");
        assert_eq!(dock_side(point(px(110.0), px(200.0)), pane, false), None, "left needs dock_sides");
        assert_eq!(dock_side(point(px(300.0), px(345.0)), pane, false), Some(PaneDrop::Bottom));
    }

    #[test]
    fn closed_and_parked_floats_leave_docked_ones_do_not() {
        let float = |id: &str| silo_proto::model::Float::new(id, FloatRect { x: 0.1, y: 0.1, w: 0.5, h: 0.5 });
        let window = |layout: silo_proto::model::Layout, floats: Vec<silo_proto::model::Float>| WindowInfo {
            id: "w".into(),
            name: None,
            layout,
            focused: "base".into(),
            zoomed: false,
            floats,
        };
        let base = || silo_proto::model::Layout::Block { id: "base".into() };
        let old = window(base(), vec![float("a"), float("b"), float("c")]);
        let docked = silo_proto::model::Layout::Split {
            axis: silo_proto::model::Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(base()),
            second: Box::new(silo_proto::model::Layout::Block { id: "c".into() }),
        };
        // `a` closed or parked, `b` still floats, `c` docked beside the base.
        assert_eq!(floats_leaving(&old, &window(docked, vec![float("b")])), ["a"]);
        assert!(floats_leaving(&old, &old).is_empty(), "nothing changed");
        // A float of panes whose named block closed stays; a minimized one leaves the screen.
        let mut panes = float("p");
        panes.edit(|l| l.split("p", "q".into(), Axis::Horizontal));
        let before = window(base(), vec![panes.clone()]);
        assert!(panes.remove("p") && panes.id == "q");
        assert!(floats_leaving(&before, &window(base(), vec![panes.clone()])).is_empty());
        panes.minimized = true;
        assert_eq!(floats_leaving(&before, &window(base(), vec![panes])), ["p"]);
    }
}
