use crate::{
    CellMetrics,
    cursor_geometry::{Shape, rectangles},
};
use gpui::{Bounds, Pixels, Window, fill, point, px, rgb, size};
use silo_vt::{Cursor, CursorShape};

#[derive(Clone, Copy)]
pub struct CursorPresentation {
    pub focused: bool,
    pub phase_visible: bool,
}

fn shape(shape: CursorShape) -> Shape {
    match shape {
        CursorShape::Bar => Shape::Bar,
        CursorShape::Block => Shape::Block,
        CursorShape::Underline => Shape::Underline,
        CursorShape::Hollow => Shape::Hollow,
    }
}

pub(super) fn paint(cursor: Cursor, presentation: CursorPresentation, bounds: Bounds<Pixels>, metrics: CellMetrics, window: &mut Window) {
    if !cursor.visible || (presentation.focused && !presentation.phase_visible) {
        return;
    }
    let shape = shape(cursor.shape);
    let x = f32::from(bounds.left()) + f32::from(cursor.column) * metrics.width;
    let y = f32::from(bounds.top()) + f32::from(cursor.row) * metrics.height;
    let mut color: gpui::Hsla = rgb(cursor.color).into();
    if presentation.focused && shape == Shape::Block {
        color.a = 0.45;
    }
    for rect in rectangles(shape, presentation.focused, x, y, metrics.width, metrics.height, metrics.scale) {
        window.paint_quad(fill(Bounds::new(point(px(rect.x), px(rect.y)), size(px(rect.width), px(rect.height))), color));
    }
}
