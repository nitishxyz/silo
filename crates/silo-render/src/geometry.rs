use std::sync::{
    Arc, OnceLock, RwLock, RwLockReadGuard,
    atomic::{AtomicU32, Ordering},
};

use silo_vt::GridSize;

/// The font the grid is measured and painted with.
///
/// Terminal reads one configuration file for the whole process, so these
/// settings are process-wide rather than per pane; `platform-terminal-ui`
/// installs them from the resolved configuration and every pane's metrics,
/// shaping and IME overlay then agree by construction.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSettings {
    pub family: String,
    pub size: f32,
    /// OpenType features as `(tag, value)`, e.g. `("liga", 0)`.
    pub features: Vec<(String, u32)>,
    /// Cell width as `advance * scale + offset`, from `adjust-cell-width`.
    pub width_scale: f32,
    pub width_offset: f32,
    /// Cell height as `(ascent + descent) * scale + offset`.
    pub height_scale: f32,
    pub height_offset: f32,
}

impl Default for FontSettings {
    fn default() -> Self {
        Self {
            // Platform's installed mono family, not a CSS generic name that
            // GPUI may resolve as a proportional fallback.
            family: "Menlo".to_string(),
            size: 14.0,
            features: Vec::new(),
            width_scale: 1.0,
            width_offset: 0.0,
            height_scale: 1.0,
            height_offset: 0.0,
        }
    }
}

static FONT: OnceLock<RwLock<FontSettings>> = OnceLock::new();
/// Bumped on every change so cached metrics and shaped rows are invalidated.
static GENERATION: AtomicU32 = AtomicU32::new(0);

fn settings() -> RwLockReadGuard<'static, FontSettings> {
    FONT.get_or_init(|| RwLock::new(FontSettings::default())).read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Install the configured font. Returns whether anything changed, so the
/// caller can repaint only then.
pub fn set_terminal_font(font: FontSettings) -> bool {
    let cell = FONT.get_or_init(|| RwLock::new(FontSettings::default()));
    let mut current = cell.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    if *current == font {
        return false;
    }
    *current = font;
    GENERATION.fetch_add(1, Ordering::AcqRel);
    true
}

/// Identifies the installed font; part of every frame's identity.
#[must_use]
pub fn font_generation() -> u32 {
    GENERATION.load(Ordering::Acquire)
}

/// The GPUI font metrics and glyphs must share.
#[must_use]
pub fn terminal_font() -> gpui::Font {
    let settings = settings();
    let mut font = gpui::font(settings.family.as_str());
    if !settings.features.is_empty() {
        font.features = gpui::FontFeatures(Arc::new(settings.features.clone()));
    }
    font
}

/// The configured point size.
#[must_use]
pub fn terminal_font_size() -> f32 {
    settings().size
}

#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    pub font_size: f32,
    pub scale: f32,
    /// The [`font_generation`] these metrics were measured with.
    pub font: u32,
}

impl CellMetrics {
    /// Derive the grid from the same GPUI font used to paint terminal glyphs.
    pub fn for_window(window: &mut gpui::Window) -> Self {
        let scale = window.scale_factor().max(0.5);
        let (font_size, width_scale, width_offset, height_scale, height_offset) = {
            let settings = settings();
            (settings.size, settings.width_scale, settings.width_offset, settings.height_scale, settings.height_offset)
        };
        let run = gpui::TextRun {
            len: 1,
            font: terminal_font(),
            // Measurement only; nothing is painted in this color.
            color: gpui::rgb(0x00ff_ffff).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window.text_system().shape_line("M".into(), gpui::px(font_size), &[run], None);
        Self {
            width: crate::cell_geometry::cell_width(f32::from(line.width()).mul_add(width_scale, width_offset), scale),
            height: crate::cell_geometry::cell_height(
                f32::from(line.ascent).mul_add(height_scale, height_offset),
                f32::from(line.descent) * height_scale,
                scale,
            ),
            font_size,
            scale,
            font: font_generation(),
        }
    }

    #[must_use]
    pub fn new(scale: f32) -> Self {
        let scale = scale.max(0.5);
        Self { width: (9.0 * scale).ceil() / scale, height: (20.0 * scale).ceil() / scale, font_size: 14.0, scale, font: font_generation() }
    }

    #[must_use]
    pub fn grid(self, width: f32, height: f32) -> GridSize {
        let cell_width = bounded_dimension((self.width * self.scale).round(), u16::MAX);
        let cell_height = bounded_dimension((self.height * self.scale).round(), u16::MAX);
        GridSize {
            columns: bounded_dimension((width * self.scale).floor() / f32::from(cell_width), 512),
            rows: bounded_dimension((height * self.scale).floor() / f32::from(cell_height), 256),
            cell_width,
            cell_height,
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Finite dimensions are floored and clamped to the u16 range before conversion"
)]
fn bounded_dimension(value: f32, maximum: u16) -> u16 {
    if !value.is_finite() {
        return 1;
    }
    value.floor().clamp(1.0, f32::from(maximum)) as u16
}

#[cfg(test)]
mod tests {
    use super::{CellMetrics, FontSettings, font_generation, set_terminal_font};

    #[test]
    fn grids_use_complete_device_pixel_cells() {
        let metrics = CellMetrics::new(1.25);
        assert!((metrics.width * metrics.scale - 12.0).abs() < f32::EPSILON);
        let grid = metrics.grid(960.0, 600.0);
        assert_eq!(grid.columns, 100);
        assert_eq!(grid.rows, 30);
        assert_eq!(metrics.grid(0.0, 0.0).columns, 1);
    }

    #[test]
    fn resizing_keeps_the_cell_stride_and_leaves_residue_as_trailing_padding() {
        for scale in [1.0_f32, 1.25, 2.0] {
            let metrics = CellMetrics::new(scale);
            let first = metrics.grid(400.0, 300.0);
            for width in (300..1200).map(|w| w as f32 * 0.5) {
                let grid = metrics.grid(width, 300.0);
                // One fixed advance; spare pixels never spread between cells.
                assert_eq!(grid.cell_width, first.cell_width, "scale {scale} width {width}");
                let used = u32::from(grid.columns) * u32::from(grid.cell_width);
                let available = (width * scale).floor() as u32;
                assert!(used <= available, "scale {scale} width {width}");
                assert!(available - used < u32::from(grid.cell_width), "whole columns only: scale {scale} width {width}");
            }
        }
    }

    #[test]
    fn installing_a_font_bumps_the_generation_only_when_it_changed() {
        let before = font_generation();
        let settings = FontSettings { family: "Iosevka Term".to_string(), size: 13.0, ..FontSettings::default() };
        assert!(set_terminal_font(settings.clone()));
        assert_ne!(font_generation(), before);
        let generation = font_generation();
        assert!(!set_terminal_font(settings));
        assert_eq!(font_generation(), generation);
        assert!(set_terminal_font(FontSettings::default()));
    }
}
