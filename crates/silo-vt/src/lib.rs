mod model;
pub mod snapshot;
#[allow(unsafe_code)]
mod sys;

pub use model::{
    Cell, ClipboardLocation, ColorScheme, Cursor, CursorShape, Frame, GridSize, KeyEvent, Link,
    Modifiers, MouseAction, MouseButton, MouseEvent, ScrollState, SearchState, SearchStatus, Span,
    TerminalColors, TerminalDefaults, TerminalError, TerminalEvents,
};
pub use sys::{RestoreOptions, RestoreProgress, TerminalCore};

/// Ghostty revision this crate is compiled against. Snapshots are only
/// exchanged natively between peers with the same revision.
pub const GHOSTTY_REV: &str = env!("SILO_GHOSTTY_REV");
