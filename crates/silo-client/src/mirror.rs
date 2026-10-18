//! Client-side terminal mirror: restored from the server snapshot, then fed
//! the same raw output bytes, so rendering, scrolling, selection and input
//! encoding are local.

use anyhow::{anyhow, bail};
use silo_proto::{
    data::{Resume, ServerData, StartMode, Welcome},
    model::TermSize,
};
use silo_vt::{GridSize, RestoreOptions, TerminalColors, TerminalCore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Nothing visible changed.
    None,
    /// The terminal changed; redraw.
    Redraw,
    /// The process exited with this code.
    Exited(i32),
}

pub struct Mirror {
    vt: Option<TerminalCore>,
    epoch: u64,
    next_offset: u64,
    size: TermSize,
    /// Local cell metrics in device pixels, used for mouse encoding.
    cell: (u16, u16),
    exited: Option<i32>,
    /// Local default colors, re-installed into every restored terminal.
    colors: Option<TerminalColors>,
}

impl Default for Mirror {
    fn default() -> Self {
        Self::new()
    }
}

impl Mirror {
    #[must_use]
    pub fn new() -> Self {
        Self { vt: None, epoch: 0, next_offset: 0, size: TermSize::new(80, 24), cell: (8, 16), exited: None, colors: None }
    }

    /// Default colors for rendering. Program-set colors (OSC 4/10/11) from
    /// the snapshot and output still take precedence.
    pub fn set_colors(&mut self, colors: TerminalColors) {
        self.colors = Some(colors);
        if let Some(vt) = self.vt.as_mut() {
            let _ = vt.set_colors(&colors);
        }
    }

    fn install(&mut self, mut vt: TerminalCore) {
        if let Some(colors) = &self.colors {
            let _ = vt.set_colors(colors);
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
    /// # Errors
    /// Terminal allocation failure.
    pub fn begin(&mut self, welcome: &Welcome) -> anyhow::Result<()> {
        self.size = welcome.size;
        match welcome.mode {
            StartMode::Resume => {
                if self.vt.is_none() || welcome.epoch != self.epoch {
                    bail!("resume without matching local state");
                }
            }
            // Keep showing the previous state until the snapshot head arrives.
            StartMode::Snapshot => {}
            StartMode::Replay => {
                let vt = TerminalCore::new(self.grid(welcome.size)).map_err(|e| anyhow!("{e}"))?;
                self.install(vt);
            }
        }
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
        match data {
            ServerData::SnapshotHead(bytes) => {
                let options = RestoreOptions { retain_continuation: false, compress_history: true };
                let (mut vt, _) = TerminalCore::restore(&bytes, options).map_err(|e| anyhow!("{e}"))?;
                let size = vt.size();
                self.size = TermSize { columns: size.columns, rows: size.rows, ..self.size };
                vt.resize(self.grid(self.size)).map_err(|e| anyhow!("{e}"))?;
                self.install(vt);
                Ok(Applied::Redraw)
            }
            ServerData::SnapshotHistory(bytes) => {
                let vt = self.vt.as_mut().ok_or_else(|| anyhow!("history before snapshot head"))?;
                let progress = vt.restore_history(&bytes).map_err(|e| anyhow!("{e}"))?;
                Ok(if progress.rows > 0 { Applied::Redraw } else { Applied::None })
            }
            ServerData::SnapshotDone => Ok(Applied::None),
            ServerData::Output { offset, bytes } => {
                let end = offset + bytes.len() as u64;
                if end <= self.next_offset {
                    return Ok(Applied::None);
                }
                if offset > self.next_offset {
                    bail!("output gap: expected offset {}, got {offset}", self.next_offset);
                }
                let skip = (self.next_offset - offset) as usize;
                let vt = self.vt.as_mut().ok_or_else(|| anyhow!("output before terminal state"))?;
                // Replies (DA, DSR, ...) are answered by the server's
                // authoritative terminal; the mirror's are discarded.
                let _ = vt.write_output(&bytes[skip..]);
                self.next_offset = end;
                Ok(Applied::Redraw)
            }
            ServerData::Resized { size, .. } => {
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

    /// Resume point for reconnecting without a snapshot.
    #[must_use]
    pub fn resume(&self) -> Option<Resume> {
        self.vt.as_ref().map(|_| Resume { epoch: self.epoch, offset: self.next_offset })
    }

    #[must_use]
    pub fn terminal(&mut self) -> Option<&mut TerminalCore> {
        self.vt.as_mut()
    }

    /// Applied (server) grid size.
    #[must_use]
    pub fn size(&self) -> TermSize {
        self.size
    }

    #[must_use]
    pub fn exited(&self) -> Option<i32> {
        self.exited
    }

    /// Visible screen as plain text lines (trailing blanks trimmed).
    #[must_use]
    pub fn screen_text(&mut self) -> String {
        let Some(vt) = self.vt.as_mut() else { return String::new() };
        let Ok(frame) = vt.frame() else { return String::new() };
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
