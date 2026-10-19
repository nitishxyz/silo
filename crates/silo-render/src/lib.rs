mod cell_geometry;
mod cursor;
mod cursor_geometry;
mod geometry;
pub use cursor::CursorPresentation;
mod predict_geometry;
mod prepare;
mod profile;
mod quads;
mod runs;
mod span_geometry;
mod sprite;
mod surface;

pub use geometry::{
    CellMetrics, FontSettings, font_generation, set_terminal_font, terminal_font,
    terminal_font_size,
};
pub use prepare::{FrameOptions, FramePreparer, PreparedFrame};
pub use surface::{Overlay, Predicted, terminal_canvas};
