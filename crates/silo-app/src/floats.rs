//! Floating panes: cards over a window's tiled layout, back to front.
//! Rects are fractions of the window body, so every client agrees.

use std::time::Instant;

use gpui::{AnyElement, Bounds, Context, DragMoveEvent, Pixels, Point, Window, div, prelude::*, px, relative, rgb};
use silo_proto::{
    control::{Dock, Method},
    model::{Axis, FloatRect, WindowInfo},
};

use crate::theme::Continuous as _;
use crate::{
    pane_header::HeaderMode,
    radius::{self, CardCorners},
    theme,
    workspace::Workspace,
};

impl Workspace {
    /// The rect of `block_id` if it floats in any window of the current
    /// session; `None` for a tiled or unknown block.
    pub(crate) fn float_rect(&self, block_id: &str) -> Option<FloatRect> {
        let windows = &self.view.as_ref()?.session.windows;
        windows.iter().flat_map(|w| &w.floats).find(|f| f.id == block_id).map(|f| f.rect)
    }

    /// Float the focused tiled pane above the layout.
    pub(crate) fn float_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.focused_block(window, cx) else { return };
        if self.active_window().is_some_and(|w| w.layout.contains(&block)) {
            self.call(Method::BlockFloat { block_id: block, rect: None });
        }
    }

    /// Dock the focused float to the right of the first tiled pane.
    pub(crate) fn dock_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(block) = self.focused_block(window, cx).filter(|b| self.float_rect(b).is_some()) else { return };
        let Some(target) = self.active_window().and_then(|w| w.layout.blocks().first().cloned()) else { return };
        self.call(Method::BlockMove { block_id: block, rect: None, dock: Some(Dock { target, axis: Axis::Horizontal, before: false }) });
    }

    /// Pointer moved during a float drag; `ev.bounds` is the window body.
    pub(crate) fn float_drag_move(&mut self, ev: &DragMoveEvent<Dragged>, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.float_drag.as_mut() else { return };
        let rect = dragged_rect(drag.rect, drag.grab, ev.event.position, drag.offset, ev.bounds);
        if rect != drag.rect {
            drag.rect = rect;
            cx.notify();
        }
    }

    /// The drag ended: commit the new rect unless it docked.
    fn float_drag_end(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.float_drag.take() else { return };
        cx.notify();
        if drag.docked || drag.rect == drag.start {
            return;
        }
        let set = |block: String, rect: FloatRect| {
            move |this: &mut Self| {
                let Some(w) = this.view.as_mut().and_then(|v| v.session.windows.iter_mut().find(|w| is_float(w, &block))) else { return };
                if let Some(f) = w.floats.iter_mut().find(|f| f.id == block) {
                    f.rect = rect;
                }
            }
        };
        set(drag.block.clone(), drag.rect)(self);
        let method = Method::BlockMove { block_id: drag.block.clone(), rect: Some(drag.rect), dock: None };
        self.call_optimistic(method, "Couldn't move pane", set(drag.block, drag.start), cx);
    }
}

impl Workspace {
    /// `w`'s floats as cards over the body, back to front. Pressing one
    /// focuses it; the server raises a focused float.
    pub(crate) fn render_floats(&self, w: &WindowInfo, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let focused = self.focused_block(window, cx);
        let top = w.floats.last().map(|f| f.id.clone());
        w.floats
            .iter()
            .map(|f| {
                let dragged = self.float_drag.as_ref().filter(|d| d.block == f.id);
                let (id, r) = (f.id.clone(), dragged.map_or(f.rect, |d| d.rect).clamped());
                // A full header moves it itself; otherwise a thin strip
                // along the top does (over a browser's shrunk header too).
                let mover = (self.header_mode(&id) != HeaderMode::Full).then(|| {
                    self.float_grab(grip(&id, Grab::Move), &id, r, Grab::Move, 0.0, cx).absolute().top_0().left_0().right(px(MOVER_INSET)).h(px(GRIP))
                });
                let resizer = self
                    .float_grab(grip(&id, Grab::Resize), &id, r, Grab::Resize, 0.0, cx)
                    .absolute()
                    .right_0()
                    .bottom_0()
                    .size(px(GRIP))
                    .cursor(gpui::CursorStyle::ResizeUpLeftDownRight);
                let active = focused.as_deref() == Some(id.as_str());
                let raise = top.as_deref() != Some(id.as_str());
                let card = self.render_cell(&id, active, !active, false, None, CardCorners::uniform(radius::CARD), now, cx);
                div()
                    .id(gpui::ElementId::Name(format!("float-{id}").into()))
                    .absolute()
                    .left(relative(r.x))
                    .top(relative(r.y))
                    .w(relative(r.w))
                    .h(relative(r.h))
                    .continuous(radius::CARD)
                    .shadow_lg()
                    .when(self.float_drag.is_none(), |el| el.occlude())
                    .capture_any_mouse_down(cx.listener(move |this, _, window, cx| {
                        this.focus_block(&id, window, cx);
                        if raise {
                            this.call(Method::BlockFocus { block_id: id.clone() });
                        }
                    }))
                    .child(card)
                    .children(mover)
                    .child(resizer)
                    .into_any_element()
            })
            .collect()
    }
}

impl Workspace {
    /// `el` drags float `id` (header: move, corner: resize); `inset`: how far
    /// inside the float `el` starts (a header, inside the card's border).
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
        let block = id.to_owned();
        let (press, pressed) = (self.float_press.clone(), self.float_press.clone());
        // gpui measures the offset where the drag starts, past its threshold:
        // measured from the press instead, the float doesn't jump.
        el.on_mouse_down(gpui::MouseButton::Left, move |e, _, _| press.set(e.position)).on_drag(
            Dragged { block: id.to_owned() },
            move |_, offset, window, cx| {
                let offset = offset - (window.mouse_position() - pressed.get()) + gpui::point(px(inset), px(inset));
                let drag = FloatDrag { block: block.clone(), grab, offset, start: rect, rect, docked: false };
                _ = workspace.update(cx, |this, _| this.float_drag = Some(drag));
                let workspace = workspace.clone();
                // The preview lives exactly as long as the drag.
                cx.new(|cx| {
                    cx.on_release(move |_, cx| _ = workspace.update(cx, |this, cx| this.float_drag_end(cx))).detach();
                    Grip
                })
            },
        )
    }
}

impl Workspace {
    /// While a float is dragged: edge zones on tiled pane `target` that dock
    /// it there. The server docks a block second, so only the right and
    /// bottom edges are offered.
    pub(crate) fn dock_zones(&self, target: &str, fill: CardCorners, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.float_drag.as_ref().is_none_or(|d| d.grab != Grab::Move) {
            return Vec::new();
        }
        [Axis::Horizontal, Axis::Vertical]
            .into_iter()
            .map(|axis| {
                let target = target.to_owned();
                let zone = div().id(gpui::ElementId::Name(format!("dock-{axis:?}-{target}").into())).absolute().right_0().bottom_0().occlude();
                let corners = match axis {
                    Axis::Horizontal => CardCorners { top_left: 0.0, bottom_left: 0.0, ..fill },
                    Axis::Vertical => CardCorners { top_left: 0.0, top_right: 0.0, ..fill },
                };
                let zone =
                    match axis {
                        Axis::Horizontal => zone.top_0().w(relative(ZONE)).continuous_corners(corners).drag_over::<Dragged>(|s, _, _, _| {
                            s.bg(rgb(theme::accent()).opacity(0.08)).border_r_2().border_color(rgb(theme::accent()))
                        }),
                        Axis::Vertical => zone.left_0().h(relative(ZONE)).continuous_corners(corners).drag_over::<Dragged>(|s, _, _, _| {
                            s.bg(rgb(theme::accent()).opacity(0.08)).border_b_2().border_color(rgb(theme::accent()))
                        }),
                    };
                zone.on_drop(cx.listener(move |this, d: &Dragged, _, _| this.dock_float(&d.block, &target, axis))).into_any_element()
            })
            .collect()
    }

    fn dock_float(&mut self, block: &str, target: &str, axis: Axis) {
        if let Some(drag) = self.float_drag.as_mut() {
            drag.docked = true;
        }
        self.call(Method::BlockMove { block_id: block.to_owned(), rect: None, dock: Some(Dock { target: target.to_owned(), axis, before: false }) });
    }
}

/// Share of a pane's width or height that docks a dropped float.
const ZONE: f32 = 0.3;

/// A handle over float `id`. Occluding: the terminal below must not start
/// a selection.
fn grip(id: &str, grab: Grab) -> gpui::Stateful<gpui::Div> {
    div().id(gpui::ElementId::Name(format!("float-{grab:?}-{id}").into())).occlude()
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
/// Side of the resize grip in the float's bottom-right corner.
const GRIP: f32 = 10.0;

/// What a float drag changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Grab {
    Move,
    Resize,
}

/// The drag payload: GPUI hands it to drop targets (dock zones).
#[derive(Clone)]
pub(crate) struct Dragged {
    block: String,
}

/// An in-flight float drag: the rect shown until release.
pub(crate) struct FloatDrag {
    block: String,
    grab: Grab,
    /// Pointer offset inside the grabbed handle.
    offset: Point<Pixels>,
    start: FloatRect,
    rect: FloatRect,
    docked: bool,
}

/// `rect` after the pointer moved to `pos` in `body`, the grabbed handle
/// held at `offset`.
fn dragged_rect(rect: FloatRect, grab: Grab, pos: Point<Pixels>, offset: Point<Pixels>, body: Bounds<Pixels>) -> FloatRect {
    let (w, h) = (f32::from(body.size.width).max(1.0), f32::from(body.size.height).max(1.0));
    let x = f32::from(pos.x - offset.x - body.origin.x) / w;
    let y = f32::from(pos.y - offset.y - body.origin.y) / h;
    match grab {
        Grab::Move => FloatRect { x, y, ..rect },
        // Capped to the space left so the top-left corner stays put.
        Grab::Resize => FloatRect { w: (x + GRIP / w - rect.x).min(1.0 - rect.x), h: (y + GRIP / h - rect.y).min(1.0 - rect.y), ..rect },
    }
    .clamped()
}

/// Every block of `w`: tiled in reading order, then floats back to front.
pub(crate) fn window_blocks(w: &WindowInfo) -> Vec<String> {
    let mut blocks = w.layout.blocks();
    blocks.extend(w.floats.iter().map(|f| f.id.clone()));
    blocks
}

/// Whether `block` floats in `w`.
pub(crate) fn is_float(w: &WindowInfo, block: &str) -> bool {
    w.floats.iter().any(|f| f.id == block)
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
        let moved = dragged_rect(rect, Grab::Move, point(px(110.0), px(95.0)), grab, body);
        assert_eq!((moved.x, moved.y, moved.w), (0.1, 0.1, 0.5));
        // Past the edge: kept inside, size unchanged.
        let moved = dragged_rect(rect, Grab::Move, point(px(990.0), px(95.0)), grab, body);
        assert_eq!((moved.x, moved.w), (0.5, 0.5));
        // The corner follows the pointer; the top-left stays.
        let grip = point(px(GRIP), px(GRIP));
        let resized = dragged_rect(rect, Grab::Resize, point(px(600.0), px(340.0)), grip, body);
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(near(resized.x, 0.2) && near(resized.y, 0.2) && near(resized.w, 0.4) && near(resized.h, 0.4), "{resized:?}");
        let resized = dragged_rect(rect, Grab::Resize, point(px(5000.0), px(0.0)), grip, body);
        assert!(near(resized.x, 0.2) && near(resized.w, 0.8) && near(resized.h, FloatRect::MIN), "{resized:?}");
    }
}
