//! Client-side terminal mirror: restored from the server snapshot, then fed
//! the same raw output bytes, so rendering, scrolling, selection and input
//! encoding are local.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use silo_proto::{
    data::{Resume, ServerData, StartMode, Welcome},
    model::{CursorShape, TermSize, TerminalConfig},
};
use silo_vt::{ColorScheme, GridSize, RestoreOptions, TerminalColors, TerminalCore, TerminalDefaults};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Nothing visible changed.
    None,
    /// The terminal changed; redraw.
    Redraw,
    /// The process exited with this code; `None` when unknown (a block
    /// restored from disk).
    Exited(Option<i32>),
}

/// Acknowledge at least every this many applied output bytes...
pub const ACK_BYTES: u64 = 64 * 1024;
/// ...or after this long with unacknowledged output.
pub const ACK_INTERVAL: Duration = Duration::from_millis(50);

pub struct Mirror {
    vt: Option<TerminalCore>,
    epoch: u64,
    next_offset: u64,
    size: TermSize,
    /// Local cell metrics in device pixels, used for mouse encoding.
    cell: (u16, u16),
    exited: Option<Option<i32>>,
    /// A snapshot was announced and its head has not arrived yet.
    awaiting_head: bool,
    /// The local state fully reflects the stream up to `next_offset`
    /// (snapshot head and all history applied, or a full replay/resume).
    complete: bool,
    /// An error (gap, malformed frame, failed resume) broke the mirror;
    /// it must not be resumed or checkpointed.
    invalid: bool,
    /// Last acknowledged offset on the current stream and when.
    acked: Option<u64>,
    acked_at: Instant,
    /// The block's effective configuration as announced by the server,
    /// re-installed into every restored terminal. Never a local guess: the
    /// server terminal answers color queries with exactly these values.
    config: Option<TerminalConfig>,
}

impl Default for Mirror {
    fn default() -> Self {
        Self::new()
    }
}

impl Mirror {
    #[must_use]
    pub fn new() -> Self {
        Self {
            vt: None,
            epoch: 0,
            next_offset: 0,
            size: TermSize::new(80, 24),
            cell: (8, 16),
            exited: None,
            awaiting_head: false,
            complete: false,
            invalid: false,
            acked: None,
            acked_at: Instant::now(),
            config: None,
        }
    }

    /// The local state exactly mirrors the server stream up to the resume
    /// offset: safe to resume from or to checkpoint.
    #[must_use]
    pub fn coherent(&self) -> bool {
        self.vt.is_some() && self.complete && !self.invalid && !self.awaiting_head
    }

    /// The point a checkpoint of the current state corresponds to, when
    /// the state is [`coherent`](Self::coherent).
    #[must_use]
    pub fn checkpointable(&self) -> Option<Resume> {
        self.resume()
    }

    /// The effective configuration the server last announced.
    #[must_use]
    pub fn config(&self) -> Option<TerminalConfig> {
        self.config
    }

    fn install(&mut self, mut vt: TerminalCore) {
        if let Some(config) = &self.config {
            install_config(&mut vt, config);
        }
        self.vt = Some(vt);
    }

    fn grid(&self, size: TermSize) -> GridSize {
        GridSize {
            columns: size.columns.clamp(1, 512),
            rows: size.rows.clamp(1, 256),
            cell_width: self.cell.0.max(1),
            cell_height: self.cell.1.max(1),
        }
    }

    /// Begin applying a new data stream.
    ///
    /// A `Resume` welcome is accepted only when the mirror is coherent and
    /// both the epoch and the offset match it exactly.
    ///
    /// # Errors
    /// Terminal allocation failure, or a resume that does not match the
    /// local state (the mirror is invalidated; reconnect without resume).
    pub fn begin(&mut self, welcome: &Welcome) -> anyhow::Result<()> {
        self.acked = None;
        self.acked_at = Instant::now();
        match welcome.mode {
            StartMode::Resume => {
                if !self.coherent() || welcome.epoch != self.epoch || welcome.offset != self.next_offset {
                    let local = self.resume();
                    self.invalid = true;
                    bail!("resume at {}:{} does not match local state {local:?}", welcome.epoch, welcome.offset);
                }
            }
            // Keep showing the previous state until the snapshot head arrives.
            StartMode::Snapshot => {
                self.awaiting_head = true;
                self.complete = false;
            }
            StartMode::Unknown => {
                self.invalid = true;
                bail!("the server starts the stream in a mode this build does not know");
            }
        }
        self.invalid = false;
        self.size = welcome.size;
        self.epoch = welcome.epoch;
        self.next_offset = welcome.offset;
        self.exited = None;
        Ok(())
    }

    /// Apply one server frame in order.
    ///
    /// # Errors
    /// Malformed snapshot data or an output gap; the caller should
    /// reconnect without resume.
    pub fn apply(&mut self, data: ServerData) -> anyhow::Result<Applied> {
        if self.invalid {
            bail!("mirror is out of sync");
        }
        let applied = self.apply_inner(data);
        if applied.is_err() {
            self.invalid = true;
        }
        applied
    }

    fn apply_inner(&mut self, data: ServerData) -> anyhow::Result<Applied> {
        match data {
            ServerData::SnapshotHead(bytes) => {
                if !self.awaiting_head {
                    bail!("unexpected snapshot head");
                }
                let options = RestoreOptions { retain_continuation: false, compress_history: true };
                let (mut vt, _) = TerminalCore::restore(&bytes, options).map_err(|e| anyhow!("{e}"))?;
                let size = vt.size();
                self.size = TermSize { columns: size.columns, rows: size.rows, ..self.size };
                vt.resize(self.grid(self.size)).map_err(|e| anyhow!("{e}"))?;
                self.install(vt);
                self.awaiting_head = false;
                Ok(Applied::Redraw)
            }
            ServerData::SnapshotHistory(bytes) => {
                if self.awaiting_head || self.complete {
                    bail!("unexpected snapshot history");
                }
                let vt = self.vt.as_mut().ok_or_else(|| anyhow!("history before snapshot head"))?;
                let progress = vt.restore_history(&bytes).map_err(|e| anyhow!("{e}"))?;
                Ok(if progress.rows > 0 { Applied::Redraw } else { Applied::None })
            }
            ServerData::SnapshotDone => {
                if self.awaiting_head || self.vt.is_none() {
                    bail!("snapshot done before head");
                }
                self.complete = true;
                Ok(Applied::None)
            }
            ServerData::Output { offset, bytes } => {
                if self.awaiting_head {
                    bail!("output before snapshot head");
                }
                let len = u64::try_from(bytes.len())?;
                let end = offset.checked_add(len).ok_or_else(|| anyhow!("output offset overflow"))?;
                if offset != self.next_offset {
                    bail!("output gap: expected offset {}, got {offset}", self.next_offset);
                }
                let vt = self.vt.as_mut().ok_or_else(|| anyhow!("output before terminal state"))?;
                // Replies (DA, DSR, ...) are answered by the server's
                // authoritative terminal; the mirror's are discarded.
                let _ = vt.write_output(&bytes);
                self.next_offset = end;
                Ok(if bytes.is_empty() { Applied::None } else { Applied::Redraw })
            }
            ServerData::Resized { offset, size } => {
                if offset > self.next_offset {
                    bail!("resize gap: expected offset {}, got {offset}", self.next_offset);
                }
                self.size = size;
                let grid = self.grid(size);
                if let Some(vt) = self.vt.as_mut() {
                    vt.resize(grid).map_err(|e| anyhow!("{e}"))?;
                }
                Ok(Applied::Redraw)
            }
            ServerData::Exited { code } => {
                self.exited = Some(code);
                Ok(Applied::Exited(code))
            }
            ServerData::Configured(config) => {
                self.config = Some(config);
                if let Some(vt) = self.vt.as_mut() {
                    install_config(vt, &config);
                }
                Ok(Applied::Redraw)
            }
            // Prediction metadata: the caller hands these to its
            // `Predictor::server`; they never move `next_offset` or acks.
            // A newer server's frames are skipped.
            ServerData::InputAck { .. }
            | ServerData::TtyMode { .. }
            | ServerData::ClipboardWrite { .. }
            | ServerData::ClipboardRead { .. }
            | ServerData::ClipboardReadUnknown { .. }
            | ServerData::Unknown(_) => Ok(Applied::None),
        }
    }

    /// Set local cell metrics (device pixels) without changing the grid.
    pub fn set_cell_size(&mut self, width: u16, height: u16) {
        if self.cell == (width, height) || width == 0 || height == 0 {
            return;
        }
        self.cell = (width, height);
        let grid = self.grid(self.size);
        if let Some(vt) = self.vt.as_mut() {
            let _ = vt.resize(grid);
        }
    }

    /// Resume point for reconnecting without a snapshot; `None` unless the
    /// mirror is [`coherent`](Self::coherent).
    #[must_use]
    pub fn resume(&self) -> Option<Resume> {
        self.coherent().then_some(Resume { epoch: self.epoch, offset: self.next_offset })
    }

    /// Offset to acknowledge now, if any (send it as `ClientData::Ack`).
    ///
    /// Throttled: due once [`ACK_BYTES`] were applied or [`ACK_INTERVAL`]
    /// passed since the last ack, and always when `idle` (the caller has
    /// drained every frame received so far), so the server sees the client
    /// caught up.
    pub fn take_ack(&mut self, idle: bool) -> Option<u64> {
        if self.invalid || self.awaiting_head || self.vt.is_none() || self.acked == Some(self.next_offset) {
            return None;
        }
        let pending = self.acked.map_or(u64::MAX, |acked| self.next_offset.saturating_sub(acked));
        if idle || pending >= ACK_BYTES || self.acked_at.elapsed() >= ACK_INTERVAL {
            self.acked = Some(self.next_offset);
            self.acked_at = Instant::now();
            return self.acked;
        }
        None
    }

    #[must_use]
    pub fn terminal(&mut self) -> Option<&mut TerminalCore> {
        self.vt.as_mut()
    }

    /// End of the output applied so far.
    #[must_use]
    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    /// Applied (server) grid size.
    #[must_use]
    pub fn size(&self) -> TermSize {
        self.size
    }

    /// `Some(code)` once the program exited; the code is `None` when
    /// unknown (a block restored from disk).
    #[must_use]
    pub fn exited(&self) -> Option<Option<i32>> {
        self.exited
    }

    /// Visible screen as plain text lines (trailing blanks trimmed).
    #[must_use]
    pub fn screen_text(&mut self) -> String {
        let Some(vt) = self.vt.as_mut() else {
            return String::new();
        };
        let Ok(frame) = vt.frame() else {
            return String::new();
        };
        let columns = usize::from(frame.size.columns).max(1);
        let mut lines: Vec<String> = frame
            .cells
            .chunks(columns)
            .map(|row| row.iter().map(|c| if c.text.is_empty() { " " } else { c.text.as_str() }).collect::<String>().trim_end().to_owned())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }
}

/// The engine colors and defaults for a configuration (the same mapping the
/// server uses for its authoritative terminal).
#[must_use]
pub fn engine_config(config: &TerminalConfig) -> (TerminalColors, TerminalDefaults) {
    let colors = TerminalColors {
        scheme: if config.is_light() { ColorScheme::Light } else { ColorScheme::Dark },
        foreground: config.foreground,
        background: config.background,
        cursor: config.cursor,
        ansi: config.palette,
    };
    let defaults = TerminalDefaults {
        cursor_shape: Some(match config.cursor_shape {
            CursorShape::Block | CursorShape::Unknown => silo_vt::CursorShape::Block,
            CursorShape::Bar => silo_vt::CursorShape::Bar,
            CursorShape::Underline => silo_vt::CursorShape::Underline,
            CursorShape::Hollow => silo_vt::CursorShape::Hollow,
        }),
        cursor_blink: Some(config.cursor_blink),
        scrollback_bytes: config.scrollback_bytes(),
        scrollback_lines: config.scrollback_lines,
    };
    (colors, defaults)
}

fn install_config(vt: &mut TerminalCore, config: &TerminalConfig) {
    let (colors, defaults) = engine_config(config);
    // Scheme reports are the server's to send; the mirror's are dropped.
    let _ = vt.set_colors(&colors);
    let _ = vt.set_defaults(&defaults);
}
