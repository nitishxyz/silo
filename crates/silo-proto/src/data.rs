//! Binary per-block data streams.
//!
//! After `DataHello` -> `Welcome`, the server sends either the snapshot
//! (`SNAPSHOT_HEAD`, then `SNAPSHOT_HISTORY`* and `SNAPSHOT_DONE`) or nothing
//! (resume), followed by ordered `OUTPUT`/`RESIZED`/`EXITED` frames. The
//! snapshot history may interleave with live frames; clients apply both in
//! arrival order. Offsets count raw PTY output bytes since the block's epoch
//! began, so a client can resume after a disconnect without a snapshot.

use std::io;

use serde::{Deserialize, Serialize};

use crate::{
    Frame, kind,
    model::{TermSize, TerminalConfig},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataHello {
    pub protocol: u32,
    pub ticket: String,
    pub ghostty_rev: String,
    pub snapshot_version: u16,
    #[serde(default)]
    pub resume: Option<Resume>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resume {
    pub epoch: u64,
    pub offset: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StartMode {
    /// Output continues from the client's resume offset.
    Resume,
    /// A native snapshot follows; output continues from `offset`.
    Snapshot,
    /// Snapshot formats differ: the raw output ring is replayed from
    /// `offset` into a fresh terminal (lossy if the ring was trimmed).
    Replay,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Welcome {
    pub epoch: u64,
    pub mode: StartMode,
    /// Offset of the first OUTPUT byte that follows.
    pub offset: u64,
    pub size: TermSize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reject {
    pub code: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerData {
    SnapshotHead(Vec<u8>),
    SnapshotHistory(Vec<u8>),
    SnapshotDone,
    Output { offset: u64, bytes: Vec<u8> },
    /// The PTY was resized at this point of the output stream.
    Resized { offset: u64, size: TermSize },
    Exited { code: i32 },
    /// The effective terminal configuration from this point of the output
    /// stream on (sent after `Welcome` and whenever it changes).
    Configured(TerminalConfig),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientData {
    Input(Vec<u8>),
    /// This client's desired size.
    Resize(TermSize),
    Ack(u64),
    /// This client's desired terminal configuration; applied while it owns
    /// the block (same ownership as size).
    Config(TerminalConfig),
}

fn size_bytes(size: TermSize) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0..2].copy_from_slice(&size.columns.to_le_bytes());
    out[2..4].copy_from_slice(&size.rows.to_le_bytes());
    out[4..6].copy_from_slice(&size.cell_width.to_le_bytes());
    out[6..8].copy_from_slice(&size.cell_height.to_le_bytes());
    out
}

fn parse_size(bytes: &[u8]) -> io::Result<TermSize> {
    if bytes.len() < 8 {
        return Err(invalid("short size"));
    }
    let u = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    Ok(TermSize { columns: u(0), rows: u(2), cell_width: u(4), cell_height: u(6) })
}

fn parse_u64(bytes: &[u8]) -> io::Result<u64> {
    bytes.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| invalid("short u64"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

impl ServerData {
    #[must_use]
    pub fn to_frame(&self) -> Frame {
        match self {
            Self::SnapshotHead(bytes) => Frame::new(kind::SNAPSHOT_HEAD, bytes.clone()),
            Self::SnapshotHistory(bytes) => Frame::new(kind::SNAPSHOT_HISTORY, bytes.clone()),
            Self::SnapshotDone => Frame::new(kind::SNAPSHOT_DONE, Vec::new()),
            Self::Output { offset, bytes } => {
                let mut payload = Vec::with_capacity(8 + bytes.len());
                payload.extend_from_slice(&offset.to_le_bytes());
                payload.extend_from_slice(bytes);
                Frame::new(kind::OUTPUT, payload)
            }
            Self::Resized { offset, size } => {
                let mut payload = offset.to_le_bytes().to_vec();
                payload.extend_from_slice(&size_bytes(*size));
                Frame::new(kind::RESIZED, payload)
            }
            Self::Exited { code } => Frame::new(kind::EXITED, code.to_le_bytes().to_vec()),
            Self::Configured(config) => Frame::json(kind::CONFIGURED, config).expect("config serializes"),
        }
    }

    /// # Errors
    /// Unknown kind or malformed payload.
    pub fn from_frame(frame: Frame) -> io::Result<Self> {
        let p = frame.payload;
        Ok(match frame.kind {
            kind::SNAPSHOT_HEAD => Self::SnapshotHead(p),
            kind::SNAPSHOT_HISTORY => Self::SnapshotHistory(p),
            kind::SNAPSHOT_DONE => Self::SnapshotDone,
            kind::OUTPUT => Self::Output { offset: parse_u64(&p)?, bytes: p[8..].to_vec() },
            kind::RESIZED => Self::Resized { offset: parse_u64(&p)?, size: parse_size(&p[8..])? },
            kind::EXITED => {
                let code = p.get(..4).ok_or_else(|| invalid("short exit"))?;
                Self::Exited { code: i32::from_le_bytes(code.try_into().unwrap()) }
            }
            kind::CONFIGURED => Self::Configured(parse_config(&p)?),
            other => return Err(invalid(&format!("unexpected data frame kind {other:#x}"))),
        })
    }
}

impl ClientData {
    #[must_use]
    pub fn to_frame(&self) -> Frame {
        match self {
            Self::Input(bytes) => Frame::new(kind::INPUT, bytes.clone()),
            Self::Resize(size) => Frame::new(kind::RESIZE, size_bytes(*size).to_vec()),
            Self::Ack(offset) => Frame::new(kind::ACK, offset.to_le_bytes().to_vec()),
            Self::Config(config) => Frame::json(kind::CONFIG, config).expect("config serializes"),
        }
    }

    /// # Errors
    /// Unknown kind or malformed payload.
    pub fn from_frame(frame: Frame) -> io::Result<Self> {
        Ok(match frame.kind {
            kind::INPUT => Self::Input(frame.payload),
            kind::RESIZE => Self::Resize(parse_size(&frame.payload)?),
            kind::ACK => Self::Ack(parse_u64(&frame.payload)?),
            kind::CONFIG => Self::Config(parse_config(&frame.payload)?),
            other => return Err(invalid(&format!("unexpected data frame kind {other:#x}"))),
        })
    }
}

fn parse_config(bytes: &[u8]) -> io::Result<TerminalConfig> {
    serde_json::from_slice::<TerminalConfig>(bytes).map(TerminalConfig::sanitized).map_err(|e| invalid(&e.to_string()))
}

/// Encode/decode a 16-byte ticket as lowercase hex.
#[must_use]
pub fn ticket_hex(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_frames_round_trip() {
        let size = TermSize { columns: 120, rows: 40, cell_width: 9, cell_height: 18 };
        for message in [
            ServerData::Output { offset: 42, bytes: b"hi".to_vec() },
            ServerData::Resized { offset: 7, size },
            ServerData::Exited { code: -1 },
            ServerData::SnapshotDone,
            ServerData::Configured(TerminalConfig::default()),
        ] {
            assert_eq!(ServerData::from_frame(message.to_frame()).unwrap(), message);
        }
        let light = TerminalConfig { background: 0x00ff_ffff, foreground: 0, scrollback_lines: 500, ..TerminalConfig::default() };
        assert!(light.is_light() && !TerminalConfig::default().is_light());
        for message in [ClientData::Input(b"ls\r".to_vec()), ClientData::Resize(size), ClientData::Ack(99), ClientData::Config(light)] {
            assert_eq!(ClientData::from_frame(message.to_frame()).unwrap(), message);
        }
        // Out-of-range values are clamped, missing optional fields default.
        let frame = Frame::new(
            kind::CONFIG,
            br#"{"foreground":4294967295,"background":0,"cursor":0,"palette":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"scrollback_lines":99999999999}"#.to_vec(),
        );
        let ClientData::Config(config) = ClientData::from_frame(frame).unwrap() else { panic!() };
        assert_eq!(config.foreground, 0x00ff_ffff);
        assert_eq!(config.scrollback_lines, TerminalConfig::MAX_SCROLLBACK);
        assert!(config.cursor_blink);
    }
}
