//! A QR code drawn with GPUI quads: dark runs per row on a light card.

use gpui::{AnyElement, IntoElement, div, prelude::*, px, rgb};
use qrcode::{Color, QrCode};

use crate::theme::Continuous as _;

const MODULE: f32 = 3.0;
const QUIET: f32 = 4.0 * MODULE;

/// Dark runs of each row as `(start, len)` in modules.
#[must_use]
pub fn runs(text: &str) -> Option<Vec<Vec<(usize, usize)>>> {
    let code = QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let rows = colors
        .chunks(width)
        .map(|row| {
            let mut runs = Vec::new();
            let mut start = None;
            for (x, color) in row.iter().chain(std::iter::once(&Color::Light)).enumerate() {
                match (color, start) {
                    (Color::Dark, None) => start = Some(x),
                    (Color::Light, Some(s)) => {
                        runs.push((s, x - s));
                        start = None;
                    }
                    _ => {}
                }
            }
            runs
        })
        .collect();
    Some(rows)
}

#[must_use]
pub fn render(text: &str) -> Option<AnyElement> {
    let rows = runs(text)?;
    #[allow(clippy::cast_precision_loss)]
    let side = rows.len() as f32 * MODULE;
    let mut grid = div().relative().size(px(side));
    for (y, row) in rows.iter().enumerate() {
        for &(x, len) in row {
            #[allow(clippy::cast_precision_loss)]
            let quad =
                div().absolute().left(px(x as f32 * MODULE)).top(px(y as f32 * MODULE)).w(px(len as f32 * MODULE)).h(px(MODULE)).bg(rgb(0x0000_0000));
            grid = grid.child(quad);
        }
    }
    Some(div().p(px(QUIET)).continuous(8.0).bg(rgb(0x00ff_ffff)).child(grid).into_any_element())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_cover_finder_patterns() {
        let rows = runs("silo1abcdefghijklmnopqrstuvwxyz234567").unwrap();
        // The top-left finder pattern starts with a 7-module dark run.
        assert_eq!(rows[0].first(), Some(&(0, 7)));
        assert!(rows.iter().all(|r| r.iter().all(|&(x, len)| len > 0 && x + len <= rows.len())));
    }
}
