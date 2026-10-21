//! Client-side terminal mirror: restored from the server snapshot, then fed
//! the same raw output bytes, so rendering, scrolling, selection and input
//! encoding are local.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail};
use silo_proto::{
    data::{CommandAnchor, Resume, ServerData, StartMode, Welcome},
    model::{CursorShape, TermSize, TerminalConfig},
};
use silo_vt::{ColorScheme, GridSize, Placement, RestoreOptions, TerminalColors, TerminalCore, TerminalDefaults, TrackedRow};

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

/// Most command rows a mirror tracks (the server keeps as many records).
pub const MAX_COMMAND_ROWS: usize = 1000;

/// The D rows of finished commands (`docs/PHASE7.md` §7.3), by record
/// index, pinned in the mirror's own terminal so they move with scrolling,
/// pruning and reflow.
#[derive(Default)]
struct CommandRows {
    rows: BTreeMap<u64, TrackedRow>,
    /// Anchors above the rows a snapshot head restored: `(record, rows
    /// above top)`, pinned as history pages arrive.
    pending: Vec<(u64, u32)>,
    /// The head's top row when `pending` was made.
    top: Option<TrackedRow>,
}

impl CommandRows {
    fn insert(&mut self, record: u64, row: TrackedRow) {
        self.rows.insert(record, row);
        while self.rows.len() > MAX_COMMAND_ROWS {
            self.rows.pop_first();
        }
    }

    /// Pin `anchors`, which address rows of `vt` as it is now.
    fn pin(&mut self, vt: &mut TerminalCore, anchors: &[CommandAnchor]) {
        let total = vt.scroll_state().map_or(0, |s| s.total);
        let top_from_bottom = u32::try_from(total.saturating_sub(1)).unwrap_or(u32::MAX);
        for anchor in anchors {
            match vt.track_row(anchor.rows_from_bottom, 0) {
                Some(row) => self.insert(anchor.record, row),
                None if anchor.rows_from_bottom > top_from_bottom && total > 0 => {
                    self.pending.push((anchor.record, anchor.rows_from_bottom - top_from_bottom));
                }
                None => {}
            }
        }
        if !self.pending.is_empty() && self.top.is_none() {
            self.top = vt.track_row(top_from_bottom, 0);
        }
    }

    /// After history was prepended: pin the pending rows that now exist.
    fn pin_pending(&mut self, vt: &mut TerminalCore) {
        let Some(top) = self.top.as_ref().and_then(|top| vt.rows_from_bottom(top)) else {
            self.pending.clear();
            self.top = None;
            return;
        };
        for (record, above) in std::mem::take(&mut self.pending) {
            match vt.track_row(top.saturating_add(above), 0) {
                Some(row) => self.insert(record, row),
                None => self.pending.push((record, above)),
            }
        }
        if self.pending.is_empty() {
            self.top = None;
        }
    }

    fn settle(&mut self) {
        self.pending.clear();
        self.top = None;
    }
}

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
    /// The block's effective configuration as announced by the server: the
    /// values its terminal answers color queries with.
    config: Option<TerminalConfig>,
    /// This client's own configuration: when set, the mirror renders with
    /// it (re-installed into every restored terminal) instead of `config`.
    local: Option<TerminalConfig>,
    commands: CommandRows,
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
            local: None,
            commands: CommandRows::default(),
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

    /// Render with this client's `config` (colours are client-local; the
    /// server's stay what programs are told). Program overrides survive.
    pub fn set_local_config(&mut self, config: TerminalConfig) {
        self.local = Some(config);
        if let Some(vt) = self.vt.as_mut() {
            install_config(vt, &config);
        }
    }

    /// The configuration the mirror renders with.
    #[must_use]
    pub fn render_config(&self) -> Option<TerminalConfig> {
        self.local.or(self.config)
    }

    fn install(&mut self, mut vt: TerminalCore) {
        if let Some(config) = self.render_config() {
            install_config(&mut vt, &config);
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
                // Rows of the previous terminal mean nothing in this one.
                self.commands = CommandRows::default();
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
                if progress.rows > 0 && !self.commands.pending.is_empty() {
                    self.commands.pin_pending(vt);
                }
                Ok(if progress.rows > 0 { Applied::Redraw } else { Applied::None })
            }
            ServerData::SnapshotDone => {
                if self.awaiting_head || self.vt.is_none() {
                    bail!("snapshot done before head");
                }
                self.commands.settle();
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
                if let Some(vt) = self.vt.as_mut().filter(|_| self.local.is_none()) {
                    install_config(vt, &config);
                }
                Ok(Applied::Redraw)
            }
            ServerData::CommandEnd(end) => {
                // Its row is counted right after the output up to `offset`.
                let row = self.vt.as_mut().filter(|_| end.offset == self.next_offset).and_then(|vt| vt.track_row(end.rows_from_bottom, 0));
                let Some(row) = row else { return Ok(Applied::None) };
                self.commands.insert(end.record, row);
                Ok(Applied::Redraw)
            }
            ServerData::Commands(anchors) => {
                if self.awaiting_head || self.complete {
                    bail!("unexpected command rows");
                }
                let vt = self.vt.as_mut().ok_or_else(|| anyhow!("command rows before snapshot head"))?;
                self.commands.pin(vt, &anchors);
                Ok(Applied::None)
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

    /// Finished commands' rows on screen: `(record, viewport row)` of each
    /// D mark the viewport shows. Rows whose content is gone are dropped.
    pub fn command_marks(&mut self) -> Vec<(u64, u16)> {
        let Some(vt) = self.vt.as_mut() else { return Vec::new() };
        let mut marks = Vec::new();
        self.commands.rows.retain(|&record, row| match vt.placement(row) {
            Placement::Gone => false,
            Placement::Visible(y) => {
                marks.push((record, y));
                true
            }
            Placement::Above | Placement::Below | Placement::Hidden => true,
        });
        marks
    }

    /// The newest command whose row the mirror holds.
    #[must_use]
    pub fn last_command(&self) -> Option<u64> {
        self.commands.rows.last_key_value().map(|(&record, _)| record)
    }

    /// Select command `record`'s output (scrolled into view); whether it
    /// had any.
    pub fn select_command_output(&mut self, record: u64) -> bool {
        let (Some(vt), Some(row)) = (self.vt.as_mut(), self.commands.rows.get(&record)) else { return false };
        vt.select_output_before(row)
    }

    /// Anchors of the tracked command rows, for a checkpoint.
    #[must_use]
    pub fn command_anchors(&mut self) -> Vec<CommandAnchor> {
        let Some(vt) = self.vt.as_mut() else { return Vec::new() };
        self.commands.rows.iter().filter_map(|(&record, row)| Some(CommandAnchor { record, rows_from_bottom: vt.rows_from_bottom(row)? })).collect()
    }

    /// Re-pin anchors taken with [`command_anchors`](Self::command_anchors)
    /// on the same content.
    pub fn restore_command_anchors(&mut self, anchors: &[CommandAnchor]) {
        if let Some(vt) = self.vt.as_mut() {
            self.commands.pin(vt, anchors);
            self.commands.settle();
        }
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

#[cfg(test)]
mod tests {
    use silo_proto::data::CommandEnd;
    use silo_vt::VtEvent;

    use super::*;
    use crate::cache::MirrorCheckpoint;

    const FAILED: &[u8] = b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07one\r\ntwo\r\n\x1b]133;D;1\x07\x1b]133;A\x07$ ";

    /// A server terminal that ran a failed command and then enough output
    /// to push its D row into the scrollback, with that row's anchor.
    fn server() -> (TerminalCore, Vec<CommandAnchor>) {
        let mut vt = TerminalCore::server(GridSize::new(20, 4).unwrap()).unwrap();
        vt.set_defaults(&TerminalDefaults { scrollback_lines: 1000, ..TerminalDefaults::default() }).unwrap();
        vt.write_output(FAILED).unwrap();
        let mut rows: Vec<TrackedRow> =
            vt.take_events().unwrap().vt.into_iter().filter_map(|e| if let VtEvent::Prompt { row, .. } = e { row } else { None }).collect();
        let end = rows.pop().expect("D row");
        let mut more = b"\x1b]133;B\x07seq\r\n\x1b]133;C\x07".to_vec();
        for i in 0..40 {
            more.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }
        vt.write_output(&more).unwrap();
        let anchors = vec![CommandAnchor { record: 0, rows_from_bottom: vt.rows_from_bottom(&end).unwrap() }];
        assert!(anchors[0].rows_from_bottom > 4, "in the scrollback");
        (vt, anchors)
    }

    /// Attach `mirror` to `vt` with a fresh snapshot, as a server does.
    fn attach(mirror: &mut Mirror, vt: &mut TerminalCore, anchors: Vec<CommandAnchor>) {
        let mut head = vt.snapshot().unwrap();
        let history = head.split_off(silo_vt::snapshot::head_len(&head).unwrap());
        mirror.begin(&Welcome { epoch: 1, mode: StartMode::Snapshot, offset: 0, size: TermSize::new(20, 4) }).unwrap();
        mirror.apply(ServerData::Configured(TerminalConfig::default())).unwrap();
        mirror.apply(ServerData::SnapshotHead(head)).unwrap();
        mirror.apply(ServerData::Commands(anchors)).unwrap();
        for chunk in history.chunks(512) {
            mirror.apply(ServerData::SnapshotHistory(chunk.to_vec())).unwrap();
        }
        mirror.apply(ServerData::SnapshotDone).unwrap();
    }

    #[test]
    fn command_rows_survive_a_snapshot_attach_and_a_checkpoint() {
        let (mut vt, anchors) = server();
        let mut mirror = Mirror::new();
        attach(&mut mirror, &mut vt, anchors);
        assert_eq!(mirror.last_command(), Some(0), "pinned once its history page arrived");
        assert!(mirror.command_marks().is_empty(), "scrolled out of view");
        assert!(mirror.select_command_output(0));
        assert_eq!(mirror.terminal().unwrap().copy_selection().unwrap(), "one\ntwo");
        mirror.terminal().unwrap().clear_selection();

        let checkpoint = MirrorCheckpoint::capture(&mut mirror).unwrap().expect("coherent");
        assert_eq!(checkpoint.commands.len(), 1);
        let mut restored = checkpoint.restore().unwrap();
        assert_eq!(restored.last_command(), Some(0));
        assert!(restored.select_command_output(0));
        assert_eq!(restored.terminal().unwrap().copy_selection().unwrap(), "one\ntwo");
    }

    #[test]
    fn a_live_command_end_pins_its_row() {
        let (mut vt, anchors) = server();
        let mut mirror = Mirror::new();
        attach(&mut mirror, &mut vt, anchors);
        let bytes = b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07false\r\n\x1b]133;C\x07\x1b]133;D;1\x07\x1b]133;A\x07$ ".to_vec();
        let end = bytes.len() as u64;
        mirror.apply(ServerData::Output { offset: 0, bytes }).unwrap();
        // A frame for another offset addresses another line: skipped.
        assert_eq!(mirror.apply(ServerData::CommandEnd(CommandEnd { offset: 1, record: 1, rows_from_bottom: 0 })).unwrap(), Applied::None);
        assert_eq!(mirror.apply(ServerData::CommandEnd(CommandEnd { offset: end, record: 2, rows_from_bottom: 0 })).unwrap(), Applied::Redraw);
        assert_eq!(mirror.last_command(), Some(2));
        assert_eq!(mirror.command_marks(), vec![(2, 3)]);
        assert!(!mirror.select_command_output(2), "false printed nothing");
        mirror.apply(ServerData::Output { offset: end, bytes: b"\x1bc".to_vec() }).unwrap();
        assert!(mirror.command_marks().is_empty());
        assert_eq!(mirror.last_command(), None, "RIS dropped every row");
    }
}
