//! Joining adjacent paint quads before they reach the UI thread.
//!
//! Block elements and box drawing produce one rectangle per cell, so a
//! panel border, a rule or a progress bar is a row of identical touching
//! rectangles. Every one of them costs the UI thread a separate GPUI paint
//! call and a separate scene primitive, which is the expensive part of a
//! busy TUI frame; joining them here costs a comparison per cell on the
//! preparing thread. Braille dots never touch, so they are left alone.
//!
//! Dependency-free so it can be tested without GPUI.

/// An axis-aligned rectangle in logical pixels, relative to the pane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Well below a device pixel at any scale: rectangles this close were meant
/// to tile, and joining them cannot move an edge visibly.
const SEAM: f32 = 0.01;

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() <= SEAM
}

/// Whether `next` continues `last` to the right on the same row, so the two
/// can be painted as one rectangle. Only ever called for quads of identical
/// color, in column order.
pub(crate) fn joins(last: Rect, next: Rect) -> bool {
    near(last.y, next.y) && near(last.height, next.height) && near(last.x + last.width, next.x)
}

/// `last` extended to cover `next`.
pub(crate) fn join(last: Rect, next: Rect) -> Rect {
    Rect {
        width: next.x + next.width - last.x,
        ..last
    }
}

#[cfg(test)]
mod tests {
    use super::{Rect, join, joins};

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn touching_cells_on_one_row_become_a_single_rectangle() {
        let left = rect(0.0, 20.0, 9.0, 20.0);
        let right = rect(9.0, 20.0, 9.0, 20.0);
        assert!(joins(left, right));
        assert_eq!(join(left, right), rect(0.0, 20.0, 18.0, 20.0));
        // A rounding-sized seam still joins; a real gap does not.
        assert!(joins(left, rect(9.004, 20.0, 9.0, 20.0)));
        assert!(!joins(left, rect(11.25, 20.0, 2.25, 20.0)));
    }

    #[test]
    fn different_rows_heights_and_directions_never_join() {
        let left = rect(0.0, 20.0, 9.0, 20.0);
        assert!(!joins(left, rect(9.0, 40.0, 9.0, 20.0)), "next row");
        assert!(!joins(left, rect(9.0, 20.0, 9.0, 10.0)), "half height");
        assert!(!joins(left, rect(0.0, 20.0, 9.0, 20.0)), "same column");
        // Braille dots inside and across cells keep their gaps.
        let dot = rect(1.125, 21.375, 2.25, 2.25);
        assert!(!joins(dot, rect(5.625, 21.375, 2.25, 2.25)));
        assert!(!joins(dot, rect(1.125, 26.375, 2.25, 2.25)));
    }
}
