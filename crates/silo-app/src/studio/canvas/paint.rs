//! What the board paints itself: under the nodes, the dot grid and the wires (and the input layer: the
//! board's hitbox, cursor and pointer events); over them, the wire being dragged, the marquee, snap guides
//! and the note or frame being drawn. Built from the board each render, in view points.

use gpui::{
    AnyElement, Bounds, Context, DispatchPhase, Hsla, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder,
    PinchEvent, Pixels, ScrollWheelEvent, Styled, Window, point, px, rgb, size,
};

use super::{CanvasView, mods};
use crate::{studio::board, studio::tiles::ink, theme};

/// Dots of the grid are this far apart (canvas units), doubled until at least [`GRID_MIN`] view points.
const GRID: f64 = 24.0;
const GRID_MIN: f64 = 18.0;

/// A curve to stroke: view points, width, colour, dashed.
struct Stroke {
    pts: [board::Point; 4],
    width: f32,
    color: Hsla,
    dashed: bool,
}

fn hsla(c: u32) -> Hsla {
    rgb(c).into()
}

fn curve(camera: board::Camera, c: &board::Curve) -> [board::Point; 4] {
    [camera.to_view(c.a), camera.to_view(c.c1), camera.to_view(c.c2), camera.to_view(c.b)]
}

fn stroke(origin: gpui::Point<Pixels>, s: &Stroke, window: &mut Window) {
    let p = |q: board::Point| origin + point(px(q.x as f32), px(q.y as f32));
    let mut b = PathBuilder::stroke(px(s.width));
    if s.dashed {
        b = b.dash_array(&[px(6.0), px(5.0)]);
    }
    b.move_to(p(s.pts[0]));
    b.cubic_bezier_to(p(s.pts[3]), p(s.pts[1]), p(s.pts[2]));
    if let Ok(path) = b.build() {
        window.paint_path(path, s.color);
    }
}

fn cursor(c: board::Cursor) -> gpui::CursorStyle {
    use board::{Corner, Cursor as C};
    use gpui::CursorStyle as S;
    match c {
        C::Default => S::Arrow,
        C::Pointer => S::PointingHand,
        C::Grab => S::OpenHand,
        C::Grabbing => S::ClosedHand,
        C::Crosshair => S::Crosshair,
        C::Text => S::IBeam,
        C::Resize(Corner::TopLeft | Corner::BottomRight) => S::ResizeUpLeftDownRight,
        C::Resize(_) => S::ResizeUpRightDownLeft,
    }
}

/// The grid, the wires, and the board's input.
pub(super) fn under(view: &CanvasView, cx: &mut Context<CanvasView>) -> AnyElement {
    let camera = view.board.camera();
    let mut step = GRID * camera.zoom;
    while step < GRID_MIN {
        step *= 2.0;
    }
    let grid = (step as f32, camera.pan.x.rem_euclid(step) as f32, camera.pan.y.rem_euclid(step) as f32);
    let dot = hsla(ink(0.16));
    let wires: Vec<Stroke> = view
        .board
        .wires()
        .map(|w| Stroke {
            pts: curve(camera, &w.curve),
            width: if w.selected {
                2.5
            } else if w.active {
                2.0
            } else {
                1.5
            },
            color: hsla(if w.selected {
                theme::accent()
            } else if w.active {
                ink(0.6)
            } else {
                ink(0.32)
            }),
            dashed: false,
        })
        .collect();
    let cursor = cursor(view.board.cursor());
    let bounds_cell = view.bounds.clone();
    let entity = cx.entity().downgrade();
    gpui::canvas(
        move |bounds, window, _| {
            if bounds_cell.get() != bounds {
                bounds_cell.set(bounds);
                window.refresh();
            }
            window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal)
        },
        move |bounds: Bounds<Pixels>, hitbox, window, _| {
            let origin = bounds.origin;
            let (step, ox, oy) = grid;
            let mut y = oy;
            while y < f32::from(bounds.size.height) {
                let mut x = ox;
                while x < f32::from(bounds.size.width) {
                    window.paint_quad(gpui::fill(Bounds::new(origin + point(px(x - 0.75), px(y - 0.75)), size(px(1.5), px(1.5))), dot));
                    x += step;
                }
                y += step;
            }
            for w in &wires {
                stroke(origin, w, window);
            }
            window.set_cursor_style(cursor, &hitbox);
            let view_point =
                move |p: gpui::Point<Pixels>| board::Point::new(f64::from(f32::from(p.x - origin.x)), f64::from(f32::from(p.y - origin.y)));
            let button = |b: MouseButton| match b {
                MouseButton::Right => Some(board::Button::Right),
                MouseButton::Middle => Some(board::Button::Middle),
                MouseButton::Left => Some(board::Button::Left),
                _ => None,
            };
            {
                let (entity, hitbox) = (entity.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }
                    let Some(b) = button(e.button) else { return };
                    let _ = entity.update(cx, |this, cx| {
                        window.focus(&this.focus, cx);
                        this.commit_inline(window, cx);
                        this.pressed = true;
                        let ev = board::Event::Down { at: view_point(e.position), button: b, mods: mods(&e.modifiers), clicks: e.click_count as u32 };
                        this.input(ev, window, cx);
                    });
                });
            }
            {
                let (entity, hitbox) = (entity.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let over = hitbox.is_hovered(window);
                    let _ = entity.update(cx, |this, cx| {
                        // A release something over the board took (a face's own button): end the press.
                        if this.pressed && e.pressed_button.is_none() {
                            this.pressed = false;
                            this.input(
                                board::Event::Up { at: view_point(e.position), button: board::Button::Left, mods: mods(&e.modifiers) },
                                window,
                                cx,
                            );
                        }
                        if over || this.pressed {
                            this.media.over = true;
                            this.input(board::Event::Move { at: view_point(e.position), mods: mods(&e.modifiers) }, window, cx);
                        } else if this.media.over {
                            this.media.over = false;
                            this.input(board::Event::Leave, window, cx);
                        }
                    });
                });
            }
            {
                let entity = entity.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let Some(b) = button(e.button) else { return };
                    let _ = entity.update(cx, |this, cx| {
                        if std::mem::take(&mut this.pressed) {
                            this.input(board::Event::Up { at: view_point(e.position), button: b, mods: mods(&e.modifiers) }, window, cx);
                        }
                    });
                });
            }
            {
                let (entity, hitbox) = (entity.clone(), hitbox.clone());
                window.on_mouse_event(move |e: &ScrollWheelEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                        return;
                    }
                    let d = e.delta.pixel_delta(px(16.0));
                    let delta = board::Point::new(f64::from(f32::from(d.x)), f64::from(f32::from(d.y)));
                    let _ = entity.update(cx, |this, cx| {
                        this.input(board::Event::Scroll { at: view_point(e.position), delta, mods: mods(&e.modifiers) }, window, cx)
                    });
                    cx.stop_propagation();
                });
            }
            window.on_mouse_event(move |e: &PinchEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let factor = f64::from(1.0 + e.delta);
                let _ = entity.update(cx, |this, cx| this.input(board::Event::Pinch { at: view_point(e.position), factor }, window, cx));
                cx.stop_propagation();
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}

/// Drafts over the nodes: the wire being dragged, the marquee, snap guides, the note or frame being drawn.
pub(super) fn over(view: &CanvasView) -> AnyElement {
    let b = &view.board;
    let camera = b.camera();
    let draft = b.wiring().map(|w| {
        let (color, dashed) = match w.drop {
            board::WireDrop::Wire { .. } => (theme::accent(), false),
            board::WireDrop::Refused { .. } => (theme::danger(), true),
            _ => (ink(0.5), true),
        };
        Stroke { pts: curve(camera, &w.curve), width: 2.0, color: hsla(color), dashed }
    });
    let marquee = b.marquee().map(|r| camera.rect_to_view(r.normalized()));
    let drawing = b.drawing().map(|(_, r)| camera.rect_to_view(r.normalized()));
    let guides: Vec<(board::Point, board::Point)> = b
        .guides()
        .iter()
        .map(|g| {
            let (a, z) = if g.vertical {
                (board::Point::new(g.at, g.from), board::Point::new(g.at, g.to))
            } else {
                (board::Point::new(g.from, g.at), board::Point::new(g.to, g.at))
            };
            (camera.to_view(a), camera.to_view(z))
        })
        .collect();
    let accent = hsla(theme::accent());
    gpui::canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, (), window, _| {
            let origin = bounds.origin;
            let at = |r: board::Rect| Bounds::new(origin + point(px(r.x as f32), px(r.y as f32)), size(px(r.w as f32), px(r.h as f32)));
            if let Some(s) = &draft {
                stroke(origin, s, window);
            }
            if let Some(r) = marquee {
                window.paint_quad(gpui::fill(at(r), accent.opacity(0.08)).border_widths(px(1.0)).border_color(accent.opacity(0.6)));
            }
            if let Some(r) = drawing {
                window.paint_quad(gpui::fill(at(r), accent.opacity(0.05)).border_widths(px(1.0)).border_color(accent));
            }
            for (a, z) in &guides {
                let mut p = PathBuilder::stroke(px(1.0));
                p.move_to(origin + point(px(a.x as f32), px(a.y as f32)));
                p.line_to(origin + point(px(z.x as f32), px(z.y as f32)));
                if let Ok(path) = p.build() {
                    window.paint_path(path, accent);
                }
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .into_any_element()
}
