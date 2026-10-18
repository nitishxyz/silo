//! Dependency-free placement of a viewport column span inside the grid.

/// `(x, y, width, height)` of columns `start..end` on `row`, in logical
/// pixels relative to the grid origin, or `None` when the span lies outside
/// the grid or is empty. Spans past the last column are clipped to it.
#[must_use]
pub(crate) fn span_rectangle(
    row: u16,
    start: u16,
    end: u16,
    columns: u16,
    rows: u16,
    cell_width: f32,
    cell_height: f32,
) -> Option<(f32, f32, f32, f32)> {
    if row >= rows || start >= end || start >= columns {
        return None;
    }
    let end = end.min(columns);
    Some((
        f32::from(start) * cell_width,
        f32::from(row) * cell_height,
        f32::from(end - start) * cell_width,
        cell_height,
    ))
}

#[cfg(test)]
mod tests {
    use super::span_rectangle;

    #[test]
    fn spans_are_clipped_to_the_grid_and_empty_spans_are_skipped() {
        assert_eq!(
            span_rectangle(2, 3, 7, 80, 24, 8.0, 20.0),
            Some((24.0, 40.0, 32.0, 20.0))
        );
        assert_eq!(
            span_rectangle(0, 78, 90, 80, 24, 8.0, 20.0),
            Some((624.0, 0.0, 16.0, 20.0)),
            "clipped to the last column"
        );
        assert_eq!(span_rectangle(24, 0, 1, 80, 24, 8.0, 20.0), None);
        assert_eq!(span_rectangle(0, 80, 81, 80, 24, 8.0, 20.0), None);
        assert_eq!(span_rectangle(0, 5, 5, 80, 24, 8.0, 20.0), None);
        assert_eq!(span_rectangle(0, 6, 5, 80, 24, 8.0, 20.0), None);
    }
}
