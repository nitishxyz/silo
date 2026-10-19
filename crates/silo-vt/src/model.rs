use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub columns: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl GridSize {
    /// # Errors
    /// Rejects empty grids and grids exceeding the allocation limits.
    pub fn new(columns: u16, rows: u16) -> Result<Self, TerminalError> {
        if columns == 0 || rows == 0 || columns > 512 || rows > 256 {
            return Err(TerminalError("grid must be 1..512 columns by 1..256 rows".into()));
        }
        Ok(Self { columns, rows, cell_width: 8, cell_height: 18 })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "Independent VT style attributes can be combined arbitrarily")]
pub struct Cell {
    pub text: String,
    pub foreground: u32,
    pub background: u32,
    pub background_explicit: bool,
    /// Inside Ghostty's active selection. Colors are the cell's own; the
    /// renderer overlays the theme selection.
    pub selected: bool,
    /// Part of an OSC 8 hyperlink; the URI is in [`Frame::links`].
    pub hyperlink: bool,
    pub width: u8,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

/// Columns `start..end` of one viewport row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub row: u16,
    pub start: u16,
    /// Exclusive.
    pub end: u16,
}

/// An OSC 8 hyperlink span on a viewport row, in Ghostty's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub span: Span,
    pub uri: String,
}

/// Progress of an active scrollback search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchStatus {
    /// More ticks can make progress without touching the terminal.
    Running,
    /// Blocked until the next feed.
    FeedRequired,
    /// Caught up with the terminal as of the last feed.
    #[default]
    Complete,
}

/// Search results as of the last feed. Match ordering is newest to oldest
/// (bottom of the screen up through scrollback); `selected` indexes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchState {
    pub status: SearchStatus,
    pub total: u64,
    pub selected: Option<u64>,
    /// Viewport-row spans of matches visible in the current viewport. The
    /// selected match is additionally Ghostty's selection.
    pub viewport: Vec<Span>,
}

/// Effects reported by upstream callbacks since the previous collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEvents {
    pub bells: u32,
    /// New title after OSC 0/2 (`Some("")` when cleared).
    pub title: Option<String>,
    /// New raw pwd after OSC 7/9/1337 (`Some("")` when cleared). OSC 7 is a
    /// URI; decoding it is the embedder's responsibility.
    pub pwd: Option<String>,
    /// Program-requested clipboard write (standard or primary selection).
    pub clipboard: Vec<(ClipboardLocation, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardLocation {
    Standard,
    Primary,
}

impl TerminalEvents {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bells == 0 && self.title.is_none() && self.pwd.is_none() && self.clipboard.is_empty()
    }
}

/// Light/dark answer to a program's color-scheme query (`CSI ? 996 n`) and
/// mode 2031 change reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
}

/// Embedder defaults installed into Ghostty: the effective colors a program
/// sees unless it overrides them with OSC 4/10/11/12. Values are `0x00RRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalColors {
    pub scheme: ColorScheme,
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    /// ANSI 0-15; indices 16-255 are regenerated upstream from these plus
    /// the background/foreground.
    pub ansi: [u32; 16],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    Bar,
    #[default]
    Block,
    Underline,
    Hollow,
}

/// Embedder defaults that are not colors: the cursor DECSCUSR reset restores
/// and the scrollback bounds. `None` restores upstream's built-in value, so
/// removing a key from the configuration file really does go back to the
/// default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalDefaults {
    pub cursor_shape: Option<CursorShape>,
    pub cursor_blink: Option<bool>,
    pub scrollback_bytes: u64,
    pub scrollback_lines: u64,
}

impl Default for TerminalDefaults {
    fn default() -> Self {
        Self {
            cursor_shape: None,
            // Upstream's built-in default is a steady cursor; Platform's is
            // blinking (see `adapter.c`).
            cursor_blink: Some(true),
            scrollback_bytes: 16 * 1024 * 1024,
            scrollback_lines: 10_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub column: u16,
    pub row: u16,
    pub visible: bool,
    pub blinking: bool,
    pub shape: CursorShape,
    pub color: u32,
}

/// Ghostty's viewport position in its scrollable row space.
///
/// `offset` counts rows from the top of scrollback; `len` is the viewport
/// height; `total` is scrollback plus the active area. Being at the bottom
/// means `offset + len == total`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollState {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

impl ScrollState {
    /// Highest valid viewport offset.
    #[must_use]
    pub fn max_offset(self) -> u64 {
        self.total.saturating_sub(self.len)
    }

    /// True when the viewport shows the active area.
    #[must_use]
    pub fn at_bottom(self) -> bool {
        self.offset >= self.max_offset()
    }
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub size: GridSize,
    /// Row-major grid, shared with the core's row cache so a frame whose
    /// rows did not change is a reference copy.
    pub cells: std::sync::Arc<[Cell]>,
    pub cursor: Cursor,
    pub background: u32,
    pub scroll: ScrollState,
    /// Any X10/normal/button/any-event mouse tracking mode is active.
    pub mouse_tracking: bool,
    /// The cursor is on an OSC 133 prompt/input row (shell integration).
    pub at_prompt: bool,
    /// The alternate screen is active.
    pub alt_screen: bool,
    /// OSC 8 hyperlink spans on viewport rows, shared like `cells`.
    pub links: std::sync::Arc<[Link]>,
    /// Present while a search needle is set.
    pub search: Option<SearchState>,
}

#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools, reason = "Independent physical modifier keys, not mutually exclusive states")]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub key: String,
    pub text: String,
    pub modifiers: Modifiers,
    pub release: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug)]
pub enum MouseButton {
    None,
    Left,
    Right,
    Middle,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug)]
pub struct MouseEvent {
    pub action: MouseAction,
    pub button: MouseButton,
    /// Device pixels relative to the terminal content, excluding padding.
    pub x: u16,
    pub y: u16,
    pub modifiers: Modifiers,
}

#[derive(Clone, Debug)]
pub struct TerminalError(pub String);

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for TerminalError {}

#[cfg(test)]
mod tests {
    use super::GridSize;
    #[test]
    fn bounds_grid_allocations() {
        assert!(GridSize::new(0, 24).is_err());
        assert!(GridSize::new(513, 24).is_err());
        assert!(GridSize::new(80, 24).is_ok());
    }
}
