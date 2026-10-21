use std::{
    ffi::{CString, c_char, c_void},
    ptr::NonNull,
    sync::Arc,
};

use crate::model::{
    Cell, ClipboardLocation, ColorScheme, Cursor, CursorShape, Frame, GridSize, KeyEvent, Link, MouseAction, MouseButton, MouseEvent,
    ProgressReportState, PromptKind, PromptStep, ReportKind, ReportState, ScrollState, SearchState, SearchStatus, Span, TerminalColors,
    TerminalDefaults, TerminalError, TerminalEvents, VtEvent,
};

#[repr(C)]
struct RawCell {
    text: [u8; 256],
    len: u32,
    foreground: u32,
    background: u32,
    flags: u32,
    width: u32,
}

#[repr(C)]
#[derive(Default)]
struct RawFrame {
    scroll_total: u64,
    scroll_offset: u64,
    scroll_len: u64,
    background: u32,
    cursor_x: u32,
    cursor_y: u32,
    cursor_visible: u32,
    cursor_shape: u32,
    cursor_blink: u32,
    cursor_color: u32,
    mouse_tracking: u32,
    dirty: u32,
    at_prompt: u32,
    alt_screen: u32,
    synchronized: u32,
}

#[repr(C)]
struct RawLink {
    row: u32,
    start: u32,
    end: u32,
    len: u32,
    uri: [u8; 1024],
}

#[repr(C)]
#[derive(Default)]
struct RawEvents {
    bells: u32,
    title_changed: u32,
    pwd_changed: u32,
    reserved: u32,
}

/// Hyperlink spans reported per frame; a row with more is truncated.
const MAX_LINKS: usize = 64;
/// Viewport match spans converted per frame.
const MAX_VIEWPORT_MATCHES: usize = 1024;

unsafe extern "C" {
    fn silo_vt_drag_scroll(vt: *mut c_void, lines: i32) -> bool;
    fn silo_vt_cancel_drag(vt: *mut c_void);
    fn silo_vt_clear_selection(vt: *mut c_void) -> bool;
    fn silo_vt_select_all(vt: *mut c_void) -> bool;
    fn silo_vt_copy(vt: *mut c_void, out: *mut u8, cap: usize, written: *mut usize) -> i32;
    fn silo_vt_mouse(vt: *mut c_void, fields: *const u32, timestamp: u64, out: *mut u8, cap: usize, written: *mut usize) -> i32;
    fn silo_vt_new(columns: u16, rows: u16, continuation_max: usize) -> *mut c_void;
    fn silo_vt_free(vt: *mut c_void);
    fn silo_vt_size(vt: *mut c_void, out: *mut u32) -> i32;
    fn silo_vt_ground(vt: *mut c_void, ground: *mut bool) -> i32;
    fn silo_vt_reset(vt: *mut c_void);
    fn silo_vt_snapshot(vt: *mut c_void, out: *mut *mut u8, len: *mut usize) -> i32;
    fn silo_bytes_free(ptr: *mut u8, len: usize);
    fn silo_vt_restore(
        head: *const u8,
        len: usize,
        retain_continuation: bool,
        compress_history: bool,
        history_rows: *mut u64,
        error: *mut i32,
    ) -> *mut c_void;
    fn silo_vt_restore_push(vt: *mut c_void, data: *const u8, len: usize) -> i32;
    fn silo_vt_restore_next(vt: *mut c_void, rows: *mut usize) -> i32;
    fn silo_vt_write(vt: *mut c_void, data: *const u8, len: usize) -> i32;
    fn silo_vt_replies(vt: *mut c_void, out: *mut u8) -> usize;
    fn silo_vt_resize(vt: *mut c_void, columns: u16, rows: u16, width: u16, height: u16) -> i32;
    fn silo_vt_frame(
        vt: *mut c_void,
        frame: *mut RawFrame,
        cells: *mut RawCell,
        capacity: usize,
        columns: u16,
        full: bool,
        dirty_rows: *mut u64,
        links: *mut RawLink,
        link_cap: usize,
        link_count: *mut usize,
    ) -> i32;
    fn silo_vt_events(vt: *mut c_void, out: *mut RawEvents);
    fn silo_vt_clipboard(vt: *mut c_void, slot: i32, out: *mut u8, cap: usize) -> usize;
    fn silo_vt_string(vt: *mut c_void, which: i32, out: *mut u8, cap: usize, written: *mut usize) -> i32;
    fn silo_vt_search_set(vt: *mut c_void, needle: *const u8, len: usize) -> i32;
    fn silo_vt_search_step(vt: *mut c_void, max_ticks: u32, status: *mut u32) -> i32;
    fn silo_vt_search_select(vt: *mut c_void, previous: bool) -> i32;
    fn silo_vt_search_state(vt: *mut c_void, total: *mut u64, selected: *mut u64, status: *mut u32) -> i32;
    fn silo_vt_search_viewport(vt: *mut c_void, out: *mut u32, cap: usize, count: *mut usize) -> i32;
    fn silo_vt_key(
        vt: *mut c_void,
        key: *const c_char,
        text: *const u8,
        len: usize,
        mods: u32,
        release: bool,
        out: *mut u8,
        cap: usize,
        written: *mut usize,
    ) -> i32;
    fn silo_vt_paste(vt: *mut c_void, text: *mut u8, len: usize, line_breaks: bool, out: *mut u8, cap: usize, written: *mut usize) -> i32;
    fn silo_vt_scroll(vt: *mut c_void, delta: i32);
    fn silo_vt_scroll_to_row(vt: *mut c_void, row: u64);
    fn silo_vt_scrollbar(vt: *mut c_void, out: *mut u64) -> i32;
    fn silo_vt_set_colors(vt: *mut c_void, colors: *const u32, dark: bool, out: *mut u8, cap: usize, written: *mut usize) -> i32;
    fn silo_vt_set_defaults(vt: *mut c_void, cursor_style: i32, cursor_blink: i32, scrollback_bytes: u64, scrollback_lines: u64) -> i32;
    fn silo_vt_focus(vt: *mut c_void, focused: bool, out: *mut u8, cap: usize, written: *mut usize) -> i32;
    fn silo_vt_hook(vt: *mut c_void) -> i32;
    fn silo_vt_take_events(vt: *mut c_void, out: *mut u8, cap: usize, lost: *mut bool) -> usize;
    fn silo_tracked_free(row: *mut c_void);
    fn silo_vt_row_from_bottom(vt: *mut c_void, row: *mut c_void, out: *mut u32) -> i32;
    fn silo_vt_track_row(vt: *mut c_void, from_bottom: u32, x: u16, out: *mut *mut c_void) -> i32;
    fn silo_vt_tracked_column(row: *mut c_void, x: *mut u16) -> i32;
    fn silo_vt_output_text(vt: *mut c_void, row: *mut c_void, end: *mut c_void, out: *mut *mut u8, len: *mut usize) -> i32;
}

/// A cell of a server terminal's screen that moves with its content
/// (scrolling, pruning, reflow): an OSC 133 mark's position. Use it only
/// with the terminal that made it.
#[derive(Debug)]
pub struct TrackedRow(NonNull<c_void>);

// SAFETY: an owned handle into its terminal's page list, used only together
// with that terminal (which is `Send` and confined by `&mut`).
unsafe impl Send for TrackedRow {}

impl Drop for TrackedRow {
    fn drop(&mut self) {
        // SAFETY: owned handle, freed exactly once; valid even after its
        // terminal was freed.
        unsafe { silo_tracked_free(self.0.as_ptr()) };
    }
}

impl TrackedRow {
    /// Column of the tracked cell, if it still exists.
    #[must_use]
    pub fn column(&self) -> Option<u16> {
        let mut x = 0;
        // SAFETY: owned live handle; scalar out.
        (unsafe { silo_vt_tracked_column(self.0.as_ptr(), &raw mut x) } == 0).then_some(x)
    }
}

/// Parse the hook event records of `silo_vt_take_events`.
fn parse_events(mut bytes: &[u8]) -> Vec<VtEvent> {
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, rest) = bytes.split_at_checked(n)?;
        *bytes = rest;
        Some(head)
    }
    fn u8_(bytes: &mut &[u8]) -> Option<u8> {
        take(bytes, 1).map(|b| b[0])
    }
    fn u32_(bytes: &mut &[u8]) -> Option<u32> {
        take(bytes, 4).and_then(|b| b.try_into().ok()).map(u32::from_ne_bytes)
    }
    fn string(bytes: &mut &[u8]) -> Option<String> {
        let len = usize::try_from(u32_(bytes)?).ok()?;
        take(bytes, len).map(|b| String::from_utf8_lossy(b).into_owned())
    }
    fn event(record: &mut &[u8]) -> Option<VtEvent> {
        Some(match u8_(record)? {
            1 => {
                let state = match u8_(record)? {
                    0 => ReportState::Idle,
                    1 => ReportState::Working,
                    2 => ReportState::Done,
                    3 => ReportState::Blocked,
                    4 => ReportState::Error,
                    5 => ReportState::Clear,
                    _ => ReportState::Unknown,
                };
                let kind = match u8_(record)? {
                    0 => ReportKind::None,
                    1 => ReportKind::Permission,
                    2 => ReportKind::Question,
                    3 => ReportKind::Auth,
                    _ => ReportKind::Unknown,
                };
                let progress = u8_(record)?.cast_signed();
                VtEvent::Status { state, kind, progress, id: string(record)?, app: string(record)?, title: string(record)?, message: string(record)? }
            }
            2 => {
                let state = match u8_(record)? {
                    0 => ProgressReportState::Remove,
                    1 => ProgressReportState::Set,
                    2 => ProgressReportState::Error,
                    3 => ProgressReportState::Indeterminate,
                    4 => ProgressReportState::Pause,
                    _ => ProgressReportState::Unknown,
                };
                VtEvent::Progress { state, percent: u8_(record)?.cast_signed() }
            }
            3 => VtEvent::Notify { title: string(record)?, body: string(record)? },
            4 => {
                let ptr = usize::from_ne_bytes(take(record, size_of::<usize>())?.try_into().ok()?);
                // SAFETY: the record owned this handle (or null); ownership
                // moves to the `TrackedRow`, freed exactly once.
                let row = NonNull::new(ptr as *mut c_void).map(TrackedRow);
                let step = match u8_(record)? {
                    1 => PromptStep::PromptStart,
                    2 => PromptStep::InputStart,
                    3 => PromptStep::OutputStart,
                    4 => PromptStep::CommandEnd,
                    _ => PromptStep::Unknown,
                };
                let prompt_kind = match u8_(record)? {
                    0 => PromptKind::Primary,
                    1 => PromptKind::Right,
                    2 => PromptKind::Continuation,
                    3 => PromptKind::Secondary,
                    _ => PromptKind::Unknown,
                };
                let has_exit = u8_(record)? != 0;
                let exit = take(record, 4)?.try_into().ok().map(i32::from_ne_bytes)?;
                VtEvent::Prompt { step, prompt_kind, exit: has_exit.then_some(exit), command: string(record)?, error: string(record)?, row }
            }
            5 => VtEvent::Reset,
            _ => return None,
        })
    }
    let mut events = Vec::new();
    while let Some(len) = u32_(&mut &*bytes) {
        let Some(mut record) = usize::try_from(len).ok().and_then(|len| take(&mut bytes, len)) else { break };
        record = &record[4..];
        if let Some(event) = event(&mut record) {
            events.push(event);
        }
    }
    events
}

fn check(result: i32) -> Result<(), TerminalError> {
    if result == 0 { Ok(()) } else { Err(TerminalError(format!("libghostty-vt operation failed ({result})"))) }
}

/// Default continuation limit for terminals that may be snapshotted. Large
/// enough for any APC/OSC the parser buffers (APC is capped at 64 KiB).
pub const DEFAULT_CONTINUATION_MAX: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default)]
pub struct RestoreOptions {
    /// Keep continuation tracking on the restored terminal so it can be
    /// snapshotted again (server-side restore from disk).
    pub retain_continuation: bool,
    /// Compress history pages as they are restored.
    pub compress_history: bool,
}

/// Result of feeding history bytes into an incremental restore.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoreProgress {
    /// Pages applied by this call.
    pub pages: usize,
    /// Rows prepended by this call.
    pub rows: usize,
    /// FINISH was validated; the restore is complete.
    pub finished: bool,
}

/// Thread-confined terminal. Handles and borrowed upstream data never escape.
pub struct TerminalCore {
    handle: NonNull<c_void>,
    size: GridSize,
    /// Reused extraction buffer; the shim fully overwrites every cell it
    /// extracts, so stale contents never leak into a frame.
    raw: Vec<RawCell>,
    /// Cells of the last frame. Rows upstream reports clean are copied from
    /// here instead of being re-read cell by cell over FFI.
    cells: Option<Arc<[Cell]>>,
    /// Hyperlink spans of the last frame, kept for rows that stayed clean.
    links: Vec<Link>,
    /// Reused per-frame link extraction buffer.
    raw_links: Vec<RawLink>,
    searching: bool,
    /// Present while history is still being restored from a snapshot.
    restore: Option<crate::snapshot::RecordSplitter>,
}

// SAFETY: the handle is exclusively owned and every access goes through
// `&mut self`; libghostty-vt has no thread affinity, only no concurrency.
unsafe impl Send for TerminalCore {}

impl TerminalCore {
    /// Cancel the gesture without clearing its installed selection.
    pub fn cancel_drag(&mut self) {
        // SAFETY: exclusive handle; no borrowed output.
        unsafe {
            silo_vt_cancel_drag(self.handle.as_ptr());
        }
    }

    /// Drop the selection (e.g. when the user types). Returns whether there
    /// was one, so the caller knows to redraw.
    pub fn clear_selection(&mut self) -> bool {
        // SAFETY: exclusive handle; no borrowed output.
        unsafe { silo_vt_clear_selection(self.handle.as_ptr()) }
    }

    /// Select the whole screen and scrollback. Returns whether anything
    /// was selectable.
    pub fn select_all(&mut self) -> bool {
        // SAFETY: exclusive handle; no borrowed output.
        unsafe { silo_vt_select_all(self.handle.as_ptr()) }
    }

    /// Scroll only an active native selection, bounded to eight rows per tick.
    pub fn drag_scroll(&mut self, lines: i32) -> bool {
        // SAFETY: exclusive handle and bounded scalar argument.
        unsafe { silo_vt_drag_scroll(self.handle.as_ptr(), lines.clamp(-8, 8)) }
    }
    /// Copy Ghostty's active selection, preserving graphemes and unwrapping soft lines.
    ///
    /// # Errors
    /// Rejects selections over 1 MiB, formatting errors, or invalid UTF-8.
    pub fn copy_selection(&mut self) -> Result<String, TerminalError> {
        let mut bytes = vec![0; 1024 * 1024];
        let mut written = 0;
        // SAFETY: exclusive live handle and initialized bounded output allocation.
        check(unsafe { silo_vt_copy(self.handle.as_ptr(), bytes.as_mut_ptr(), bytes.len(), &raw mut written) })?;
        bytes.truncate(written);
        String::from_utf8(bytes).map_err(|e| TerminalError(e.to_string()))
    }

    /// Encode a pointer event using the child's current tracking and format modes.
    ///
    /// # Errors
    /// Returns upstream allocation/encoding errors. Coordinates outside the grid
    /// are ignored rather than reported to a child as a click inside its content.
    pub fn encode_mouse(&mut self, event: MouseEvent) -> Result<Vec<u8>, TerminalError> {
        self.encode_mouse_at(event, std::time::Instant::now())
    }

    /// Encode with the monotonic receipt time captured before queueing.
    /// # Errors
    /// Returns upstream allocation or encoding errors.
    pub fn encode_mouse_at(&mut self, event: MouseEvent, received: std::time::Instant) -> Result<Vec<u8>, TerminalError> {
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let epoch = *EPOCH.get_or_init(|| received);
        let timestamp = u64::try_from(received.saturating_duration_since(epoch).as_nanos()).unwrap_or(u64::MAX);
        let width = u32::from(self.size.columns) * u32::from(self.size.cell_width);
        let height = u32::from(self.size.rows) * u32::from(self.size.cell_height);
        if u32::from(event.x) >= width || u32::from(event.y) >= height {
            return Ok(Vec::new());
        }
        let action = match event.action {
            MouseAction::Press => 0,
            MouseAction::Release => 1,
            MouseAction::Motion => 2,
        };
        let button = match event.button {
            MouseButton::None => 0,
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 3,
            MouseButton::WheelUp => 4,
            MouseButton::WheelDown => 5,
        };
        let mods = u32::from(event.modifiers.shift) | (u32::from(event.modifiers.control) << 1) | (u32::from(event.modifiers.alt) << 2);
        let fields = [
            action,
            button,
            u32::from(event.x),
            u32::from(event.y),
            mods,
            u32::from(self.size.cell_width),
            u32::from(self.size.cell_height),
            width,
            height,
        ];
        let mut output = vec![0; 128];
        let mut written = 0;
        // SAFETY: nine initialized scalar fields and a bounded output buffer; synchronous exclusive handle access.
        check(unsafe { silo_vt_mouse(self.handle.as_ptr(), fields.as_ptr(), timestamp, output.as_mut_ptr(), output.len(), &raw mut written) })?;
        output.truncate(written);
        Ok(output)
    }

    /// Encode focus only when the child enabled DEC mode 1004.
    ///
    /// # Errors
    /// Returns an error if the upstream mode query or encoder fails.
    pub fn encode_focus(&mut self, focused: bool) -> Result<Vec<u8>, TerminalError> {
        let mut output = vec![0; 8];
        let mut written = 0;
        // SAFETY: exclusive handle and bounded initialized output buffer.
        check(unsafe { silo_vt_focus(self.handle.as_ptr(), focused, output.as_mut_ptr(), output.len(), &raw mut written) })?;
        output.truncate(written);
        Ok(output)
    }

    /// # Errors
    /// Rejects invalid dimensions or an upstream allocation failure.
    pub fn new(size: GridSize) -> Result<Self, TerminalError> {
        Self::with_continuation(size, DEFAULT_CONTINUATION_MAX)
    }

    /// A server terminal: [`Self::new`] plus the status hooks (see
    /// [`Self::hook`]).
    ///
    /// # Errors
    /// As [`Self::new`].
    pub fn server(size: GridSize) -> Result<Self, TerminalError> {
        let mut core = Self::new(size)?;
        core.hook()?;
        Ok(core)
    }

    /// Report OSC 7501 / 9;4 / 9 / 777 / 133 and RIS as ordered
    /// [`VtEvent`]s in [`Self::take_events`] and answer the OSC 7501
    /// support query. Only the terminal that owns the program does this, so
    /// the query is answered once.
    ///
    /// # Errors
    /// Returns the upstream error if a callback is rejected.
    pub fn hook(&mut self) -> Result<(), TerminalError> {
        // SAFETY: exclusive live handle; installs static callbacks whose
        // userdata is the handle itself.
        check(unsafe { silo_vt_hook(self.handle.as_ptr()) })
    }

    /// How many rows lie below `row`'s row on the primary screen; `None`
    /// when the row is gone or the alternate screen is active.
    pub fn rows_from_bottom(&mut self, row: &TrackedRow) -> Option<u32> {
        let mut out = 0;
        // SAFETY: exclusive handle; `row` is a live handle of this terminal.
        (unsafe { silo_vt_row_from_bottom(self.handle.as_ptr(), row.0.as_ptr(), &raw mut out) } == 0).then_some(out)
    }

    /// Track column `column` of the primary screen's row `rows_from_bottom`
    /// rows above its last; `None` when there is no such row or the
    /// alternate screen is active.
    pub fn track_row(&mut self, rows_from_bottom: u32, column: u16) -> Option<TrackedRow> {
        let mut out = std::ptr::null_mut();
        // SAFETY: exclusive handle; the shim returns an owned handle or null.
        let result = unsafe { silo_vt_track_row(self.handle.as_ptr(), rows_from_bottom, column, &raw mut out) };
        if result != 0 {
            return None;
        }
        NonNull::new(out).map(TrackedRow)
    }

    /// The plain text of the command output (OSC 133 C..D) containing
    /// `row`'s cell, cut before `end` (the command's D mark) when given;
    /// empty when that cell is not command output.
    ///
    /// # Errors
    /// Upstream selection or formatting failures.
    pub fn output_text(&mut self, row: &TrackedRow, end: Option<&TrackedRow>) -> Result<String, TerminalError> {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0;
        let end = end.map_or(std::ptr::null_mut(), |end| end.0.as_ptr());
        // SAFETY: exclusive handle; `row` and `end` belong to this terminal;
        // on success the shim returns an allocation of `len` bytes (or null).
        check(unsafe { silo_vt_output_text(self.handle.as_ptr(), row.0.as_ptr(), end, &raw mut ptr, &raw mut len) })?;
        if ptr.is_null() {
            return Ok(String::new());
        }
        // SAFETY: ptr/len describe the initialized allocation returned above.
        let text = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }).into_owned();
        // SAFETY: released exactly once with its length.
        unsafe { silo_bytes_free(ptr, len) };
        Ok(text)
    }

    /// # Errors
    /// Rejects invalid dimensions or an upstream allocation failure.
    pub fn with_continuation(size: GridSize, continuation_max: usize) -> Result<Self, TerminalError> {
        GridSize::new(size.columns, size.rows)?;
        // SAFETY: validated dimensions; the shim returns an exclusively owned handle.
        let handle = NonNull::new(unsafe { silo_vt_new(size.columns, size.rows, continuation_max) })
            .ok_or_else(|| TerminalError("could not create libghostty-vt terminal".into()))?;
        let mut core = Self::from_handle(handle, size);
        if size.cell_width > 0 && size.cell_height > 0 {
            core.resize(size)?;
        }
        Ok(core)
    }

    fn from_handle(handle: NonNull<c_void>, size: GridSize) -> Self {
        Self { handle, size, raw: Vec::new(), cells: None, links: Vec::new(), raw_links: Vec::new(), searching: false, restore: None }
    }

    #[must_use]
    pub fn size(&self) -> GridSize {
        self.size
    }

    fn query_size(&mut self) -> Result<GridSize, TerminalError> {
        let mut out = [0_u32; 4];
        // SAFETY: exclusive handle; the shim writes exactly four values.
        check(unsafe { silo_vt_size(self.handle.as_ptr(), out.as_mut_ptr()) })?;
        let narrow = |v: u32| u16::try_from(v).map_err(|e| TerminalError(e.to_string()));
        let columns = narrow(out[0])?;
        let rows = narrow(out[1])?;
        let mut size = GridSize::new(columns, rows)?;
        if columns > 0 && rows > 0 && out[2] > 0 && out[3] > 0 {
            size.cell_width = narrow(out[2] / out[0])?;
            size.cell_height = narrow(out[3] / out[1])?;
        }
        Ok(size)
    }

    /// Whether the VT parser and UTF-8 decoder are both at ground.
    ///
    /// # Errors
    /// Returns the upstream error if the query fails.
    pub fn at_ground(&mut self) -> Result<bool, TerminalError> {
        let mut ground = false;
        // SAFETY: exclusive handle; scalar out.
        check(unsafe { silo_vt_ground(self.handle.as_ptr(), &raw mut ground) })?;
        Ok(ground)
    }

    /// Full reset (RIS) of the terminal state.
    pub fn reset(&mut self) {
        // SAFETY: exclusive handle.
        unsafe { silo_vt_reset(self.handle.as_ptr()) };
        self.cells = None;
    }

    /// Encode a complete upstream snapshot (`GHOSTSNP`) of this terminal.
    ///
    /// # Errors
    /// Fails if the parser is mid-sequence beyond the continuation limit or on
    /// upstream allocation failure.
    pub fn snapshot(&mut self) -> Result<Vec<u8>, TerminalError> {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0;
        // SAFETY: exclusive handle; on success the shim returns an allocation
        // of `len` bytes owned by us until silo_bytes_free.
        check(unsafe { silo_vt_snapshot(self.handle.as_ptr(), &raw mut ptr, &raw mut len) })?;
        if ptr.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: ptr/len describe the initialized allocation returned above.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        // SAFETY: released exactly once with the length it was allocated with.
        unsafe { silo_bytes_free(ptr, len) };
        Ok(bytes)
    }

    /// Restore a terminal from a snapshot prefix that contains every record
    /// through READY (see [`crate::snapshot::head_len`]). Any bytes after
    /// READY in `head` are applied as history immediately. Returns the
    /// terminal and the advertised primary history rows.
    ///
    /// # Errors
    /// Rejects malformed or truncated snapshot data.
    pub fn restore(head: &[u8], options: RestoreOptions) -> Result<(Self, u64), TerminalError> {
        let split = crate::snapshot::head_len(head)?;
        let mut history_rows = 0;
        let mut error = 0;
        // SAFETY: `head[..split]` is valid for the call; the shim copies it.
        let handle = unsafe {
            silo_vt_restore(head.as_ptr(), split, options.retain_continuation, options.compress_history, &raw mut history_rows, &raw mut error)
        };
        let handle = NonNull::new(handle).ok_or_else(|| TerminalError(format!("snapshot restore failed ({error})")))?;
        let mut core = Self::from_handle(handle, GridSize { columns: 1, rows: 1, cell_width: 0, cell_height: 0 });
        core.size = core.query_size()?;
        if core.size.cell_width == 0 {
            core.size.cell_width = 8;
            core.size.cell_height = 18;
        }
        core.restore = Some(crate::snapshot::RecordSplitter::default());
        if split < head.len() {
            core.restore_history(&head[split..])?;
        }
        Ok((core, history_rows))
    }

    /// True while snapshot history is still being restored.
    #[must_use]
    pub fn restoring(&self) -> bool {
        self.restore.is_some()
    }

    /// Feed snapshot bytes that follow READY, in order and in arbitrary
    /// chunks. Every complete history page is applied immediately; live
    /// output may be written between calls.
    ///
    /// # Errors
    /// Rejects malformed history; the terminal stays usable either way.
    pub fn restore_history(&mut self, bytes: &[u8]) -> Result<RestoreProgress, TerminalError> {
        let mut progress = RestoreProgress::default();
        let Some(mut splitter) = self.restore.take() else {
            return Ok(RestoreProgress { finished: true, ..progress });
        };
        splitter.push(bytes);
        while let Some((tag, record)) = splitter.next_record() {
            // SAFETY: exclusive handle; the shim copies the record bytes.
            check(unsafe { silo_vt_restore_push(self.handle.as_ptr(), record.as_ptr(), record.len()) })?;
            if tag != crate::snapshot::tag::PAGE && tag != crate::snapshot::tag::FINISH {
                continue;
            }
            let mut rows = 0;
            // SAFETY: exclusive handle; every record next() consumes is buffered.
            match unsafe { silo_vt_restore_next(self.handle.as_ptr(), &raw mut rows) } {
                0 => {
                    progress.pages += 1;
                    progress.rows += rows;
                }
                1 => {
                    progress.finished = true;
                    self.cells = None;
                    return Ok(progress);
                }
                error => {
                    self.cells = None;
                    return Err(TerminalError(format!("snapshot history failed ({error})")));
                }
            }
        }
        if progress.rows > 0 {
            self.cells = None;
        }
        self.restore = Some(splitter);
        Ok(progress)
    }

    /// # Errors
    /// Fails if upstream response buffering exceeds its bounded capacity.
    pub fn write_output(&mut self, bytes: &[u8]) -> Result<Vec<u8>, TerminalError> {
        // SAFETY: exclusive handle and input slice valid for the synchronous call.
        check(unsafe { silo_vt_write(self.handle.as_ptr(), bytes.as_ptr(), bytes.len()) })?;
        let mut replies = vec![0; 65536];
        // SAFETY: the shim bounds all accumulated replies to 65536 bytes.
        let len = unsafe { silo_vt_replies(self.handle.as_ptr(), replies.as_mut_ptr()) };
        replies.truncate(len);
        Ok(replies)
    }

    /// # Errors
    /// Rejects invalid dimensions or an upstream resize failure.
    pub fn resize(&mut self, size: GridSize) -> Result<(), TerminalError> {
        GridSize::new(size.columns, size.rows)?;
        // SAFETY: exclusive live handle, scalar arguments only.
        check(unsafe { silo_vt_resize(self.handle.as_ptr(), size.columns, size.rows, size.cell_width, size.cell_height) })?;
        self.size = size;
        Ok(())
    }

    /// # Errors
    /// Fails on upstream snapshot errors or invalid copied cell/cursor data.
    pub fn frame(&mut self) -> Result<Frame, TerminalError> {
        let columns = usize::from(self.size.columns);
        let count = columns * usize::from(self.size.rows);
        self.prepare_buffers(count);
        let previous = self.cells.take().filter(|cells| cells.len() == count);
        let mut frame = RawFrame::default();
        let mut dirty_rows = [0_u64; 4];
        let mut link_count = 0;
        // SAFETY: initialized, correctly aligned output buffers sized to the
        // grid; the shim bounds rows to 256, cells to `capacity` and links
        // to `link_cap`.
        check(unsafe {
            silo_vt_frame(
                self.handle.as_ptr(),
                &raw mut frame,
                self.raw.as_mut_ptr(),
                count,
                self.size.columns,
                previous.is_none(),
                dirty_rows.as_mut_ptr(),
                self.raw_links.as_mut_ptr(),
                MAX_LINKS,
                &raw mut link_count,
            )
        })?;
        let extracted = |row: usize| frame.dirty == 2 || dirty_rows[row / 64] & (1 << (row % 64)) != 0;
        if frame.dirty != 0 {
            self.merge_links(previous.is_none() || frame.dirty == 2, extracted, link_count)?;
        }
        let cells: Arc<[Cell]> = match (frame.dirty, previous) {
            (0, Some(previous)) => previous,
            (_, previous) => {
                let mut cells = Vec::with_capacity(count);
                for (row, raw) in self.raw.chunks(columns.max(1)).enumerate() {
                    match (&previous, extracted(row)) {
                        (Some(previous), false) => {
                            cells.extend_from_slice(&previous[row * columns..(row + 1) * columns]);
                        }
                        _ => {
                            for raw in raw {
                                cells.push(convert(raw)?);
                            }
                        }
                    }
                }
                Arc::from(cells)
            }
        };
        self.cells = Some(cells.clone());
        let search = if self.searching { Some(self.search_state()?) } else { None };
        Ok(Frame {
            size: self.size,
            cells,
            background: frame.background,
            scroll: ScrollState { total: frame.scroll_total, offset: frame.scroll_offset, len: frame.scroll_len },
            mouse_tracking: frame.mouse_tracking != 0,
            at_prompt: frame.at_prompt != 0,
            alt_screen: frame.alt_screen != 0,
            synchronized: frame.synchronized != 0,
            links: Arc::from(self.links.clone()),
            search,
            cursor: Cursor {
                column: u16::try_from(frame.cursor_x).map_err(|e| TerminalError(e.to_string()))?,
                row: u16::try_from(frame.cursor_y).map_err(|e| TerminalError(e.to_string()))?,
                visible: frame.cursor_visible != 0,
                blinking: frame.cursor_blink != 0,
                color: frame.cursor_color,
                shape: match frame.cursor_shape {
                    0 => crate::CursorShape::Bar,
                    1 => crate::CursorShape::Block,
                    2 => crate::CursorShape::Underline,
                    3 => crate::CursorShape::Hollow,
                    _ => return Err(TerminalError("invalid cursor shape".into())),
                },
            },
        })
    }

    /// Size the reused extraction buffers to the grid.
    fn prepare_buffers(&mut self, count: usize) {
        if self.raw.len() != count {
            self.raw = (0..count).map(|_| RawCell { text: [0; 256], len: 0, foreground: 0, background: 0, flags: 0, width: 0 }).collect();
        }
        if self.raw_links.len() != MAX_LINKS {
            self.raw_links = (0..MAX_LINKS).map(|_| RawLink { row: 0, start: 0, end: 0, len: 0, uri: [0; 1024] }).collect();
        }
    }

    /// Replace hyperlink spans of re-extracted rows, keeping those of rows
    /// that stayed clean (or everything when the whole grid was extracted).
    fn merge_links(&mut self, all: bool, extracted: impl Fn(usize) -> bool, link_count: usize) -> Result<(), TerminalError> {
        if all {
            self.links.clear();
        } else {
            self.links.retain(|link| !extracted(usize::from(link.span.row)));
        }
        for raw in &self.raw_links[..link_count.min(MAX_LINKS)] {
            self.links.push(convert_link(raw)?);
        }
        self.links.sort_by_key(|link| (link.span.row, link.span.start));
        Ok(())
    }

    /// Bell, title and pwd effects since the last call, with the current
    /// title/pwd copied when they changed (bounded to 4 KiB each).
    ///
    /// # Errors
    /// Returns the upstream error if a title/pwd query fails.
    pub fn take_events(&mut self) -> Result<TerminalEvents, TerminalError> {
        let mut raw = RawEvents::default();
        // SAFETY: exclusive handle; the shim writes exactly the four fields.
        unsafe {
            silo_vt_events(self.handle.as_ptr(), &raw mut raw);
        }
        let mut events = TerminalEvents { bells: raw.bells, ..TerminalEvents::default() };
        let mut lost = false;
        // SAFETY: exclusive handle; a zero capacity only reports the length.
        let len = unsafe { silo_vt_take_events(self.handle.as_ptr(), std::ptr::null_mut(), 0, &raw mut lost) };
        events.lost = lost;
        if len > 0 {
            let mut bytes = vec![0; len];
            // SAFETY: exclusive handle; `bytes` holds exactly the queued
            // length, so the shim copies and clears the queue.
            let copied = unsafe { silo_vt_take_events(self.handle.as_ptr(), bytes.as_mut_ptr(), bytes.len(), &raw mut lost) };
            bytes.truncate(copied);
            events.vt = parse_events(&bytes);
            events.lost = lost;
        }
        for (slot, location) in [(0, ClipboardLocation::Standard), (1, ClipboardLocation::Primary)] {
            if raw.reserved & (1 << slot) == 0 {
                continue;
            }
            let mut bytes = vec![0; 1024 * 1024];
            // SAFETY: exclusive handle, shim copies no more than the supplied capacity.
            let len = unsafe { silo_vt_clipboard(self.handle.as_ptr(), slot, bytes.as_mut_ptr(), bytes.len()) };
            bytes.truncate(len);
            if let Ok(text) = String::from_utf8(bytes) {
                events.clipboard.push((location, text));
            }
        }
        if raw.title_changed != 0 {
            events.title = Some(self.string(0)?);
        }
        if raw.pwd_changed != 0 {
            events.pwd = Some(self.string(1)?);
        }
        Ok(events)
    }

    fn string(&mut self, which: i32) -> Result<String, TerminalError> {
        let mut bytes = vec![0; 4096];
        let mut written = 0;
        // SAFETY: exclusive handle; the shim copies at most `cap` bytes.
        check(unsafe { silo_vt_string(self.handle.as_ptr(), which, bytes.as_mut_ptr(), bytes.len(), &raw mut written) })?;
        bytes.truncate(written);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Start or restart a scrollback search; an empty needle ends it and
    /// clears the selection it installed. Matching is byte-exact except
    /// ASCII letters, which compare case-insensitively.
    ///
    /// # Errors
    /// Rejects needles over 1 KiB and upstream allocation failures.
    pub fn search(&mut self, needle: &str) -> Result<(), TerminalError> {
        if needle.len() > 1024 {
            return Err(TerminalError("search needle exceeds 1 KiB".into()));
        }
        // SAFETY: exclusive handle; the shim copies the needle bytes.
        check(unsafe { silo_vt_search_set(self.handle.as_ptr(), needle.as_ptr(), needle.len()) })?;
        self.searching = !needle.is_empty();
        Ok(())
    }

    /// One feed (pick up terminal changes) plus a bounded number of ticks.
    /// Call every turn while a search is active so results stay current.
    ///
    /// # Errors
    /// Returns the upstream error if the search cannot read the terminal.
    pub fn search_step(&mut self, max_ticks: u32) -> Result<SearchStatus, TerminalError> {
        let mut status = 2;
        // SAFETY: exclusive handle; scalar in, scalar out.
        check(unsafe { silo_vt_search_step(self.handle.as_ptr(), max_ticks, &raw mut status) })?;
        Ok(search_status(status))
    }

    /// Select the next (older) or previous (newer) match, scrolling it into
    /// view when needed and making it the terminal selection.
    ///
    /// # Errors
    /// Returns the upstream error; no matches is not an error.
    pub fn search_select(&mut self, previous: bool) -> Result<(), TerminalError> {
        // SAFETY: exclusive handle, scalar argument.
        check(unsafe { silo_vt_search_select(self.handle.as_ptr(), previous) })
    }

    /// Results as of the last feed, including viewport match spans.
    ///
    /// # Errors
    /// Returns the upstream error if a query fails.
    pub fn search_state(&mut self) -> Result<SearchState, TerminalError> {
        let (mut total, mut selected, mut status) = (0_u64, u64::MAX, 2_u32);
        // SAFETY: exclusive handle; the shim writes the three scalars.
        check(unsafe { silo_vt_search_state(self.handle.as_ptr(), &raw mut total, &raw mut selected, &raw mut status) })?;
        let mut raw = vec![0_u32; MAX_VIEWPORT_MATCHES * 4];
        let mut count = 0;
        // SAFETY: exclusive handle; the shim writes four values per span and
        // at most `cap` spans.
        check(unsafe { silo_vt_search_viewport(self.handle.as_ptr(), raw.as_mut_ptr(), MAX_VIEWPORT_MATCHES, &raw mut count) })?;
        let mut viewport = Vec::with_capacity(count.min(MAX_VIEWPORT_MATCHES));
        for span in raw.as_chunks::<4>().0.iter().take(count) {
            viewport.extend(viewport_spans((span[0], span[1]), (span[2], span[3]), self.size.columns, self.size.rows));
        }
        Ok(SearchState { status: search_status(status), total, selected: (selected != u64::MAX).then_some(selected), viewport })
    }

    /// # Errors
    /// Rejects oversized text, embedded NUL key names, and encoder failures.
    pub fn encode_key(&mut self, event: &KeyEvent) -> Result<Vec<u8>, TerminalError> {
        if event.text.len() > 4096 {
            return Err(TerminalError("key text exceeds 4096 bytes".into()));
        }
        let key = CString::new(event.key.as_str()).map_err(|e| TerminalError(e.to_string()))?;
        let m = event.modifiers;
        let mods = u32::from(m.shift) | (u32::from(m.control) << 1) | (u32::from(m.alt) << 2) | (u32::from(m.super_key) << 3);
        let mut out = vec![0; 8192];
        let mut written = 0;
        // SAFETY: synchronous call, exclusive handle, valid owned input/output buffers.
        check(unsafe {
            silo_vt_key(
                self.handle.as_ptr(),
                key.as_ptr(),
                event.text.as_ptr(),
                event.text.len(),
                mods,
                event.release,
                out.as_mut_ptr(),
                out.len(),
                &raw mut written,
            )
        })?;
        out.truncate(written);
        Ok(out)
    }

    /// The bytes for a paste, or `None` when it has line breaks outside a
    /// bracketed paste: those run as commands, so the user must confirm
    /// ([`Self::paste_confirmed`]).
    ///
    /// # Errors
    /// Rejects oversized pastes, the bracketed-paste terminator inside one
    /// and upstream encoder failures.
    pub fn paste(&mut self, text: &str) -> Result<Option<Vec<u8>>, TerminalError> {
        self.encode_paste(text, false)
    }

    /// A paste whose line breaks the user allowed; sent as carriage returns
    /// outside a bracketed paste.
    ///
    /// # Errors
    /// As [`Self::paste`], minus the line-break refusal.
    pub fn paste_confirmed(&mut self, text: &str) -> Result<Vec<u8>, TerminalError> {
        self.encode_paste(text, true)?.ok_or_else(|| TerminalError("paste refused line breaks".into()))
    }

    fn encode_paste(&mut self, text: &str, line_breaks: bool) -> Result<Option<Vec<u8>>, TerminalError> {
        if text.len() > 32768 {
            return Err(TerminalError("paste exceeds 32 KiB".into()));
        }
        let mut data = text.as_bytes().to_vec();
        let mut out = vec![0; data.len() + 32];
        let mut written = 0;
        // SAFETY: separate mutable buffers, lengths match allocations, exclusive handle.
        let result =
            unsafe { silo_vt_paste(self.handle.as_ptr(), data.as_mut_ptr(), data.len(), line_breaks, out.as_mut_ptr(), out.len(), &raw mut written) };
        match result {
            -2 => return Ok(None),
            -3 => return Err(TerminalError("Paste refused: it could end the program's bracketed paste".into())),
            _ => check(result)?,
        }
        out.truncate(written);
        Ok(Some(out))
    }

    pub fn scroll(&mut self, lines: i32) {
        // SAFETY: exclusive live handle, scalar argument.
        unsafe {
            silo_vt_scroll(self.handle.as_ptr(), lines);
        }
    }

    /// Move the viewport to an absolute row in [`ScrollState`] space.
    /// Upstream clamps: row 0 pins the top and rows at or past the active
    /// area pin the bottom (follow output).
    pub fn scroll_to_row(&mut self, row: u64) {
        // SAFETY: exclusive live handle, scalar argument; upstream clamps the row.
        unsafe {
            silo_vt_scroll_to_row(self.handle.as_ptr(), row);
        }
    }

    /// Current scrollbar state without producing a frame.
    ///
    /// # Errors
    /// Returns the upstream error if the query fails.
    pub fn scroll_state(&mut self) -> Result<ScrollState, TerminalError> {
        let mut out = [0_u64; 3];
        // SAFETY: exclusive live handle; the shim writes exactly three values.
        check(unsafe { silo_vt_scrollbar(self.handle.as_ptr(), out.as_mut_ptr()) })?;
        Ok(ScrollState { total: out[0], offset: out[1], len: out[2] })
    }

    /// Install theme defaults (foreground, background, cursor, ANSI 0-15) and
    /// the light/dark scheme answered to `CSI ? 996 n`.
    /// Upstream keeps OSC 4/10/11/12 overrides a program has applied, so this
    /// is safe to call again on a theme switch. Takes effect on the next frame.
    /// Returns the mode-2031 scheme report to write to the PTY, which is
    /// empty unless the program opted in and the scheme changed.
    ///
    /// # Errors
    /// Returns the upstream error if any color option is rejected.
    pub fn set_colors(&mut self, colors: &TerminalColors) -> Result<Vec<u8>, TerminalError> {
        let mut packed = [0_u32; 19];
        packed[0] = colors.foreground;
        packed[1] = colors.background;
        packed[2] = colors.cursor;
        packed[3..].copy_from_slice(&colors.ansi);
        let mut out = vec![0; 32];
        let mut written = 0;
        // SAFETY: exclusive live handle; the shim reads exactly 19 initialized
        // values and writes at most `cap` bytes.
        check(unsafe {
            silo_vt_set_colors(
                self.handle.as_ptr(),
                packed.as_ptr(),
                colors.scheme == ColorScheme::Dark,
                out.as_mut_ptr(),
                out.len(),
                &raw mut written,
            )
        })?;
        out.truncate(written);
        Ok(out)
    }

    /// Install the user's non-color defaults: the cursor shape and blink a
    /// DECSCUSR reset restores, and the scrollback bounds. Program choices
    /// are untouched, exactly as for [`Self::set_colors`].
    ///
    /// # Errors
    /// Returns the upstream error if an option is rejected.
    pub fn set_defaults(&mut self, defaults: &TerminalDefaults) -> Result<(), TerminalError> {
        let style = defaults.cursor_shape.map_or(-1, |shape| match shape {
            CursorShape::Bar => 0,
            CursorShape::Block => 1,
            CursorShape::Underline => 2,
            CursorShape::Hollow => 3,
        });
        let blink = defaults.cursor_blink.map_or(-1, i32::from);
        // SAFETY: exclusive live handle; the shim only reads these scalars.
        check(unsafe { silo_vt_set_defaults(self.handle.as_ptr(), style, blink, defaults.scrollback_bytes, defaults.scrollback_lines) })
    }
}

fn convert(cell: &RawCell) -> Result<Cell, TerminalError> {
    let len = usize::try_from(cell.len).unwrap_or(usize::MAX);
    let bytes = cell.text.get(..len).ok_or_else(|| TerminalError("invalid cell length".into()))?;
    let text = std::str::from_utf8(bytes).map_err(|e| TerminalError(e.to_string()))?.to_owned();
    Ok(Cell {
        text,
        foreground: cell.foreground,
        background: cell.background,
        background_explicit: cell.flags & 32 != 0,
        selected: cell.flags & 64 != 0,
        hyperlink: cell.flags & 128 != 0,
        width: u8::try_from(cell.width).map_err(|e| TerminalError(e.to_string()))?,
        bold: cell.flags & 1 != 0,
        italic: cell.flags & 2 != 0,
        faint: cell.flags & 4 != 0,
        underline: cell.flags & 8 != 0,
        strikethrough: cell.flags & 16 != 0,
    })
}

fn convert_link(link: &RawLink) -> Result<Link, TerminalError> {
    let len = usize::try_from(link.len).unwrap_or(usize::MAX);
    let bytes = link.uri.get(..len).ok_or_else(|| TerminalError("invalid hyperlink length".into()))?;
    let narrow = |value: u32| u16::try_from(value).map_err(|e| TerminalError(e.to_string()));
    Ok(Link {
        span: Span { row: narrow(link.row)?, start: narrow(link.start)?, end: narrow(link.end)? },
        // OSC 8 URIs are raw program bytes; the URI is display metadata.
        uri: String::from_utf8_lossy(bytes).into_owned(),
    })
}

fn search_status(status: u32) -> SearchStatus {
    match status {
        0 => SearchStatus::Running,
        1 => SearchStatus::FeedRequired,
        _ => SearchStatus::Complete,
    }
}

/// Split an inclusive viewport range into per-row spans, normalised so the
/// start precedes the end and clipped to the grid.
fn viewport_spans(start: (u32, u32), end: (u32, u32), columns: u16, rows: u16) -> impl Iterator<Item = Span> {
    let (start, end) = if (start.1, start.0) <= (end.1, end.0) { (start, end) } else { (end, start) };
    let last_row = u32::from(rows).saturating_sub(1);
    (start.1..=end.1.min(last_row)).filter_map(move |row| {
        let from = if row == start.1 { start.0 } else { 0 };
        let to = if row == end.1 { end.0.saturating_add(1) } else { u32::from(columns) };
        let from = u16::try_from(from.min(u32::from(columns))).ok()?;
        let to = u16::try_from(to.min(u32::from(columns))).ok()?;
        (from < to).then_some(Span { row: u16::try_from(row).ok()?, start: from, end: to })
    })
}

impl Drop for TerminalCore {
    fn drop(&mut self) {
        // SAFETY: uniquely owned handle is freed exactly once; no callbacks outlive it.
        unsafe {
            silo_vt_free(self.handle.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{GridSize, KeyEvent, Modifiers, TerminalCore};

    #[test]
    fn real_vt_styles_unicode_alt_screen_queries_and_resize() {
        let mut vt = TerminalCore::new(GridSize::new(20, 4).unwrap()).unwrap();
        vt.write_output(b"\x1b[31;1mred\x1b[0m").unwrap();
        let frame = vt.frame().unwrap();
        assert_eq!(frame.cells[0].text, "r");
        assert!(frame.cells[0].bold);
        assert_ne!(frame.cells[0].foreground, frame.cells[3].foreground);
        assert_eq!(frame.cursor.column, 3);
        assert_eq!(vt.write_output(b"\x1b[6n").unwrap(), b"\x1b[1;4R");
        vt.write_output("\x1b[?1049h\x1b[H\u{754c}e\u{301}".as_bytes()).unwrap();
        let frame = vt.frame().unwrap();
        assert_eq!(frame.cells[0].text, "\u{754c}");
        assert_eq!(frame.cells[0].width, 2);
        assert_eq!(frame.cells[1].width, 0);
        assert_eq!(frame.cells[2].text, "e\u{301}");
        vt.write_output(b"\x1b[?1049l").unwrap();
        assert_eq!(vt.frame().unwrap().cells[0].text, "r");
        assert!(!vt.frame().unwrap().synchronized);
        vt.write_output(b"\x1b[?2026h").unwrap();
        assert!(vt.frame().unwrap().synchronized, "mode 2026 reported");
        vt.write_output(b"\x1b[?2026l").unwrap();
        assert!(!vt.frame().unwrap().synchronized);
        vt.resize(GridSize::new(40, 8).unwrap()).unwrap();
        assert_eq!(vt.frame().unwrap().cells.len(), 320);
    }

    #[test]
    fn server_hooks_report_in_order_and_answer_the_support_query() {
        use crate::{PromptStep, ReportState, VtEvent};
        let mut plain = TerminalCore::new(GridSize::new(40, 6).unwrap()).unwrap();
        assert!(plain.write_output(b"\x1b]7501;?\x1b\\").unwrap().is_empty(), "mirrors never answer");
        let mut vt = TerminalCore::server(GridSize::new(40, 6).unwrap()).unwrap();
        let reply = vt.write_output(b"\x1b]7501;?\x1b\\").unwrap();
        assert_eq!(reply, b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\");
        vt.write_output(b"\x1b]7501;state=working:app=cargo\x1b\\\x1b]9;4;1;40\x07\x1b]9;hello\x07").unwrap();
        vt.write_output(b"$ \x1b]133;C;cmdline_url=read%20x\x07out\r\n\x1b]133;D;3\x07\x1b]133;A\x07\x1bc").unwrap();
        let events = vt.take_events().unwrap();
        assert!(!events.lost);
        let names: Vec<String> = events
            .vt
            .iter()
            .map(|e| match e {
                VtEvent::Status { state, app, .. } => format!("status:{state:?}:{app}"),
                VtEvent::Progress { percent, .. } => format!("progress:{percent}"),
                VtEvent::Notify { body, .. } => format!("notify:{body}"),
                VtEvent::Prompt { step, exit, command, row, .. } => format!("prompt:{step:?}:{exit:?}:{command}:{}", row.is_some()),
                VtEvent::Reset => "reset".into(),
            })
            .collect();
        assert_eq!(
            names,
            [
                "status:Working:cargo",
                "progress:40",
                "notify:hello",
                "prompt:OutputStart:None:read x:true",
                "prompt:CommandEnd:Some(3)::true",
                "prompt:PromptStart:None::false",
                "progress:-1",
                "status:Clear:",
                "reset",
            ]
        );
        assert!(matches!(events.vt[0], VtEvent::Status { state: ReportState::Working, .. }));
        assert!(matches!(events.vt[3], VtEvent::Prompt { step: PromptStep::OutputStart, .. }));
        assert!(vt.take_events().unwrap().vt.is_empty(), "taken once");
        let flood: String = (0..1100).map(|i| format!("\x1b]9;n{i}\x07")).collect();
        vt.write_output(flood.as_bytes()).unwrap();
        let events = vt.take_events().unwrap();
        assert!(events.lost);
        assert_eq!(events.vt.len(), 1024);
        assert!(matches!(events.vt.last(), Some(VtEvent::Notify { body, .. }) if body == "n1099"), "keeps the newest");
        assert!(!vt.take_events().unwrap().lost);
    }

    #[test]
    fn tracked_rows_follow_scrolling_and_select_command_output() {
        use crate::VtEvent;
        let mut vt = TerminalCore::server(GridSize::new(20, 4).unwrap()).unwrap();
        vt.write_output(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07one\r\ntwo\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ").unwrap();
        let mut rows: Vec<_> =
            vt.take_events().unwrap().vt.into_iter().filter_map(|e| if let VtEvent::Prompt { row, .. } = e { row } else { None }).collect();
        assert_eq!(rows.len(), 2);
        let end = rows.pop().unwrap();
        let start = rows.pop().unwrap();
        assert_eq!(vt.rows_from_bottom(&end), Some(0));
        assert_eq!(vt.output_text(&start, Some(&end)).unwrap(), "one\ntwo");
        vt.write_output(b"\r\n\r\n\r\n").unwrap();
        assert_eq!(vt.rows_from_bottom(&end), Some(3), "moves with its content");
        let again = vt.track_row(3, 0).expect("re-pin");
        assert_eq!(vt.rows_from_bottom(&again), Some(3));
        assert_eq!(vt.output_text(&start, None).unwrap(), "one\ntwo");
    }

    #[test]
    fn upstream_input_modes_and_paste_safety() {
        let mut vt = TerminalCore::new(GridSize::new(20, 4).unwrap()).unwrap();
        assert!(vt.encode_focus(true).unwrap().is_empty());
        vt.write_output(b"\x1b[?1004h").unwrap();
        assert_eq!(vt.encode_focus(true).unwrap(), b"\x1b[I");
        assert_eq!(vt.encode_focus(false).unwrap(), b"\x1b[O");
        let mut key = KeyEvent { key: "up".into(), text: String::new(), modifiers: Modifiers::default(), release: false };
        assert_eq!(vt.encode_key(&key).unwrap(), b"\x1b[A");
        vt.write_output(b"\x1b[?1h").unwrap();
        assert_eq!(vt.encode_key(&key).unwrap(), b"\x1bOA");
        key.key = "c".into();
        key.modifiers.control = true;
        assert_eq!(vt.encode_key(&key).unwrap(), b"\x03");
        // Terminal's kill-line command (Super+Backspace) is encoded here as
        // Ctrl+U, readline's unix-line-discard, not written to the PTY raw.
        key.key = "u".into();
        assert_eq!(vt.encode_key(&key).unwrap(), b"\x15");
        assert_eq!(vt.paste("echo danger\n").unwrap(), None, "line breaks need confirmation");
        assert_eq!(vt.paste_confirmed("a\nb\n").unwrap(), b"a\rb\r");
        vt.write_output(b"\x1b[?2004h").unwrap();
        assert_eq!(vt.paste("hello").unwrap().unwrap(), b"\x1b[200~hello\x1b[201~");
        assert_eq!(vt.paste("a\nb").unwrap().unwrap(), b"\x1b[200~a\nb\x1b[201~", "line breaks are safe when bracketed");
        assert!(vt.paste("x\x1b[201~rm -rf ~\n").is_err());
        assert!(vt.paste_confirmed("x\x1b[201~rm -rf ~\n").is_err());
    }
}
