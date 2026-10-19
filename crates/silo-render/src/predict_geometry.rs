//! Dependency-free placement of predicted (unconfirmed) cells.

/// `(x, y, width, height)` in logical pixels relative to the grid origin.
pub(crate) type Rect = (f32, f32, f32, f32);

/// The cell rectangle and its underline hairline (at the same place as a
/// cell's own underline: 2 px above the bottom, one device pixel thick), or
/// `None` outside the grid.
#[must_use]
pub(crate) fn predicted_rects(row: u16, column: u16, columns: u16, rows: u16, cell_width: f32, cell_height: f32, scale: f32) -> Option<(Rect, Rect)> {
    let cell = crate::span_geometry::span_rectangle(row, column, column.saturating_add(1), columns, rows, cell_width, cell_height)?;
    let underline = (cell.0, cell.1 + cell.3 - 2.0, cell.2, 1.0 / scale.max(0.5));
    Some((cell, underline))
}

#[cfg(test)]
mod tests {
    use super::predicted_rects;

    #[test]
    fn cell_and_underline_geometry() {
        let (cell, underline) = predicted_rects(2, 3, 80, 24, 8.0, 20.0, 2.0).unwrap();
        assert_eq!(cell, (24.0, 40.0, 8.0, 20.0));
        assert_eq!(underline, (24.0, 58.0, 8.0, 0.5), "1 device px, 2 px above the bottom");
        assert_eq!(predicted_rects(0, 0, 80, 24, 9.0, 18.0, 1.0).unwrap().1.3, 1.0);
    }

    #[test]
    fn outside_the_grid_is_skipped() {
        assert_eq!(predicted_rects(24, 0, 80, 24, 8.0, 20.0, 1.0), None);
        assert_eq!(predicted_rects(0, 80, 80, 24, 8.0, 20.0, 1.0), None);
        assert_eq!(predicted_rects(0, u16::MAX, 80, 24, 8.0, 20.0, 1.0), None);
    }
}
