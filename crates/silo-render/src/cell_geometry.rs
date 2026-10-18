/// Keep grid advances on device pixels without systematically expanding every
/// glyph by rounding upward. Only the font's advance, not its ink box, sets width.
pub(super) fn cell_width(advance: f32, scale: f32) -> f32 {
    let scale = sane_scale(scale);
    let advance = if advance.is_finite() {
        advance.max(1.0)
    } else {
        8.0
    };
    (advance * scale).round().max(1.0) / scale
}

/// The row height is exactly the font's ascent plus descent, rounded up once
/// to a whole device pixel. GPUI centres the shaped glyph box inside the row,
/// so any extra leading here would surround every glyph with empty space and
/// stretch the cursor beyond the glyph cell.
pub(super) fn cell_height(ascent: f32, descent: f32, scale: f32) -> f32 {
    let scale = sane_scale(scale);
    let extent = if ascent.is_finite() && descent.is_finite() {
        (ascent + descent).max(1.0)
    } else {
        18.0
    };
    (extent * scale).ceil().max(1.0) / scale
}

fn sane_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.max(0.5)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::{cell_height, cell_width};
    #[test]
    fn fractional_scale_rounds_once_without_added_tracking() {
        assert!((cell_width(8.4, 1.0) - 8.0).abs() < f32::EPSILON);
        assert!((cell_width(8.4, 1.25) - 8.8).abs() < 0.001);
        assert!((cell_width(8.4, 2.0) - 8.5).abs() < f32::EPSILON);
        assert!(cell_width(f32::NAN, 1.0).is_finite());
        assert!(cell_width(0.0, 0.0) > 0.0);
    }

    #[test]
    fn row_height_is_the_glyph_box_without_extra_leading() {
        // Noto Sans Mono at 14 px: ascent 14.97, descent 4.10.
        let height = cell_height(14.97, 4.10, 1.0);
        assert!((height - 20.0).abs() < f32::EPSILON, "{height}");
        assert!(
            height < 24.0,
            "the former +4 px leading made rows and cursor too tall"
        );
        let scaled = cell_height(14.97, 4.10, 2.0);
        assert!((scaled - 19.5).abs() < f32::EPSILON, "{scaled}");
        assert!((scaled * 2.0).fract().abs() < f32::EPSILON);
        assert!(cell_height(f32::NAN, 1.0, 1.0).is_finite());
        assert!(cell_height(0.0, 0.0, f32::INFINITY) >= 1.0);
    }
}
