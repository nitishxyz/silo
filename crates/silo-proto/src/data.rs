//! Binary per-block data streams.
//!
//! After `DataHello` -> `Welcome`, the server sends either the snapshot
//! (`SNAPSHOT_HEAD`, then `SNAPSHOT_HISTORY`* and `SNAPSHOT_DONE`) or nothing
//! (resume), followed by ordered `OUTPUT`/`RESIZED`/`EXITED` frames. The
//! snapshot history may interleave with live frames; clients apply both in
//! arrival order. Offsets count raw PTY output bytes since the block's epoch
//! began, so a client can resume after a disconnect without a snapshot.
//!
//! On resume or replay, `Welcome.size` is the size in effect at
//! `Welcome.offset`; later `RESIZED`/`CONFIGURED` transitions arrive at
//! their original offsets. A server that ends the stream without `EXITED`
//! cut the client off (too slow); reconnect, the resume offset is likely
//! gone and a snapshot follows.

use std::io;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Frame, kind,
    model::{TermSize, TerminalConfig},
};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DataHello {
    /// The client's [`crate::PROTOCOL`] and [`crate::VT`]; must equal the
    /// server's.
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub vt: String,
    /// The client's [`crate::BUILD`], for display.
    pub build: String,
    pub ticket: String,
    #[serde(default)]
    pub resume: Option<Resume>,
}

impl DataHello {
    /// This build's hello for `ticket`.
    #[must_use]
    pub fn ours(ticket: String, resume: Option<Resume>) -> Self {
        Self { protocol: crate::PROTOCOL, vt: crate::VT.to_owned(), build: crate::BUILD.to_owned(), ticket, resume }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct Resume {
    pub epoch: u64,
    pub offset: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StartMode {
    /// Output continues from the client's resume offset.
    Resume,
    /// A native snapshot follows; output continues from `offset`.
    Snapshot,
    /// A newer server's mode: the stream cannot be applied.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Welcome {
    pub epoch: u64,
    pub mode: StartMode,
    /// Offset of the first OUTPUT byte that follows.
    pub offset: u64,
    pub size: TermSize,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Reject {
    pub code: String,
    pub reason: String,
}

/// Largest clipboard text a program may set (OSC 52, `silo clipboard copy`).
pub const MAX_CLIP_TEXT: usize = 1024 * 1024;
/// Largest clipboard content a client may answer a read with.
pub const MAX_CLIP_BYTES: usize = 10 * 1024 * 1024;

/// Payload of [`kind::COMMAND_END`]: command `record` (its index) ended on
/// the line `rows_from_bottom` rows above the last, after the `Output`
/// frame ending at `offset` was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandEnd {
    pub offset: u64,
    pub record: u64,
    pub rows_from_bottom: u32,
}

/// An element of [`kind::COMMANDS`], in snapshot coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandAnchor {
    pub record: u64,
    pub rows_from_bottom: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardLocation {
    Standard,
    Primary,
}

/// What a clipboard read asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Text,
    /// An image, preferably PNG.
    Image,
    /// The MIME types on offer.
    Types,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Clip {
    Text(String),
    Image {
        mime: String,
        /// Base64 in JSON.
        #[serde(with = "base64")]
        #[schemars(with = "String")]
        data: Vec<u8>,
    },
    Types(Vec<String>),
}

impl Clip {
    /// Bytes of content, as limited by [`MAX_CLIP_BYTES`].
    #[must_use]
    pub fn size(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Image { mime, data } => mime.len() + data.len(),
            Self::Types(types) => types.iter().map(String::len).sum(),
        }
    }
}

mod base64 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&data_encoding::BASE64.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        data_encoding::BASE64.decode(String::deserialize(deserializer)?.as_bytes()).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerData {
    SnapshotHead(Vec<u8>),
    SnapshotHistory(Vec<u8>),
    SnapshotDone,
    Output {
        offset: u64,
        bytes: Vec<u8>,
    },
    /// The PTY was resized at this point of the output stream.
    Resized {
        offset: u64,
        size: TermSize,
    },
    /// The program is gone; `code` is `None` when unknown (a block restored
    /// from disk). The stream stays open: the block remains readable.
    Exited {
        code: Option<i32>,
    },
    /// The effective terminal configuration from this point of the output
    /// stream on (sent after `Welcome` and whenever it changes).
    Configured(TerminalConfig),
    /// The first `count` `Input` frames of this stream were written to the
    /// PTY while the output ring ended at `offset`; their echo, if any, is
    /// at `>= offset`. Cumulative.
    InputAck {
        count: u64,
        offset: u64,
    },
    /// The PTY line discipline flags from this point of the output stream on
    /// (sent after `Welcome` and whenever they change).
    TtyMode {
        offset: u64,
        echo: bool,
        icanon: bool,
    },
    /// A program set the clipboard (OSC 52 or `silo clipboard copy`).
    ClipboardWrite {
        location: ClipboardLocation,
        text: String,
    },
    /// A program reads the clipboard; answer with
    /// [`ClientData::ClipboardContent`] for `request`.
    ClipboardRead {
        request: u64,
        kind: ClipKind,
    },
    /// A clipboard read of a kind this side does not know (a newer
    /// server's): answer `None` for `request`.
    ClipboardReadUnknown {
        request: u64,
        kind: u8,
    },
    /// A frame of a newer server; skipped.
    Unknown(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientData {
    Input(Vec<u8>),
    /// This client's desired size.
    Resize(TermSize),
    /// Every output byte below this offset has been applied. Required: the
    /// acked offset starts at `Welcome.offset`; the server ends the stream
    /// (forcing a reconnect with a fresh snapshot) when it falls behind the
    /// output ring or unacked output exceeds the server's budget. A
    /// backwards or not-yet-sent offset also ends the stream.
    Ack(u64),
    /// This client's desired terminal configuration; applied while it owns
    /// the block (same ownership as size).
    Config(TerminalConfig),
    /// Answer to [`ServerData::ClipboardRead`]; `None` when unavailable
    /// (or in a format this side does not know).
    ClipboardContent {
        request: u64,
        content: Option<Clip>,
    },
    /// Put `content` (text or image) on the host's system clipboard before
    /// the input that follows on this stream (a paste) is processed. Above
    /// [`MAX_CLIP_BYTES`], or on hosts without a system clipboard, ignored;
    /// content in a format this side does not know is [`Self::Unknown`].
    ClipboardPush {
        content: Clip,
    },
    /// A frame of a newer client; skipped.
    Unknown(u8),
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

fn location_byte(location: ClipboardLocation) -> u8 {
    match location {
        ClipboardLocation::Standard => 0,
        ClipboardLocation::Primary => 1,
    }
}

fn clip_kind_byte(kind: ClipKind) -> u8 {
    match kind {
        ClipKind::Text => 0,
        ClipKind::Image => 1,
        ClipKind::Types => 2,
    }
}

/// `tag: u8 | body`: 0 none, 1 text (UTF-8), 2 image (`mime_len: u16 LE |
/// mime | data`), 3 types (newline-separated). A newer tag reads as none.
fn clip_bytes(content: Option<&Clip>) -> Vec<u8> {
    match content {
        None => vec![0],
        Some(Clip::Text(text)) => [&[1u8][..], text.as_bytes()].concat(),
        Some(Clip::Image { mime, data }) => {
            let mime = &mime.as_bytes()[..mime.len().min(usize::from(u16::MAX))];
            let mut out = Vec::with_capacity(3 + mime.len() + data.len());
            out.push(2);
            out.extend_from_slice(&(mime.len() as u16).to_le_bytes());
            out.extend_from_slice(mime);
            out.extend_from_slice(data);
            out
        }
        Some(Clip::Types(types)) => [&[3u8][..], types.join("\n").as_bytes()].concat(),
    }
}

fn parse_clip(bytes: &[u8]) -> io::Result<Option<Clip>> {
    let utf8 = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| invalid("clipboard text is not UTF-8"));
    let Some((&tag, body)) = bytes.split_first() else { return Err(invalid("short clipboard content")) };
    Ok(match tag {
        0 => None,
        1 => Some(Clip::Text(utf8(body)?)),
        2 => {
            let len = body.get(..2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]]))).ok_or_else(|| invalid("short image"))?;
            let mime = body.get(2..2 + len).ok_or_else(|| invalid("short image mime"))?;
            Some(Clip::Image { mime: utf8(mime)?, data: body[2 + len..].to_vec() })
        }
        3 => {
            let text = utf8(body)?;
            Some(Clip::Types(text.split('\n').filter(|t| !t.is_empty()).map(str::to_owned).collect()))
        }
        _ => None,
    })
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
            Self::Exited { code } => Frame::new(kind::EXITED, code.map(|c| c.to_le_bytes().to_vec()).unwrap_or_default()),
            Self::Configured(config) => Frame::json(kind::CONFIGURED, config).expect("config serializes"),
            Self::InputAck { count, offset } => {
                let mut payload = count.to_le_bytes().to_vec();
                payload.extend_from_slice(&offset.to_le_bytes());
                Frame::new(kind::INPUT_ACK, payload)
            }
            Self::TtyMode { offset, echo, icanon } => {
                let mut payload = offset.to_le_bytes().to_vec();
                payload.push(u8::from(*echo) | (u8::from(*icanon) << 1));
                Frame::new(kind::TTY_MODE, payload)
            }
            Self::ClipboardWrite { location, text } => Frame::new(kind::CLIPBOARD_WRITE, [&[location_byte(*location)][..], text.as_bytes()].concat()),
            Self::ClipboardRead { request, kind: clip } => {
                let mut payload = request.to_le_bytes().to_vec();
                payload.push(clip_kind_byte(*clip));
                Frame::new(kind::CLIPBOARD_READ, payload)
            }
            Self::ClipboardReadUnknown { request, kind: clip } => Frame::new(kind::CLIPBOARD_READ, [&request.to_le_bytes()[..], &[*clip]].concat()),
            Self::Unknown(kind) => Frame::new(*kind, Vec::new()),
        }
    }

    /// # Errors
    /// Malformed payload (an unknown kind is [`Self::Unknown`]).
    pub fn from_frame(frame: Frame) -> io::Result<Self> {
        let p = frame.payload;
        Ok(match frame.kind {
            kind::SNAPSHOT_HEAD => Self::SnapshotHead(p),
            kind::SNAPSHOT_HISTORY => Self::SnapshotHistory(p),
            kind::SNAPSHOT_DONE => Self::SnapshotDone,
            kind::OUTPUT => Self::Output { offset: parse_u64(&p)?, bytes: p[8..].to_vec() },
            kind::RESIZED => Self::Resized { offset: parse_u64(&p)?, size: parse_size(&p[8..])? },
            kind::EXITED => match p.len() {
                0 => Self::Exited { code: None },
                4.. => Self::Exited { code: Some(i32::from_le_bytes(p[..4].try_into().unwrap())) },
                _ => return Err(invalid("short exit")),
            },
            kind::CONFIGURED => Self::Configured(parse_config(&p)?),
            kind::INPUT_ACK => Self::InputAck { count: parse_u64(&p)?, offset: parse_u64(p.get(8..).unwrap_or_default())? },
            kind::TTY_MODE => match p.get(8) {
                Some(flags) => Self::TtyMode { offset: parse_u64(&p)?, echo: flags & 1 != 0, icanon: flags & 2 != 0 },
                None => return Err(invalid("short tty mode")),
            },
            kind::CLIPBOARD_WRITE => {
                let location = match p.first() {
                    Some(0) => ClipboardLocation::Standard,
                    Some(1) => ClipboardLocation::Primary,
                    // A newer location: skipped.
                    Some(_) => return Ok(Self::Unknown(frame.kind)),
                    None => return Err(invalid("short clipboard write")),
                };
                Self::ClipboardWrite { location, text: String::from_utf8(p[1..].to_vec()).map_err(|_| invalid("clipboard text is not UTF-8"))? }
            }
            kind::CLIPBOARD_READ => {
                let request = parse_u64(&p)?;
                let clip = match p.get(8) {
                    Some(0) => ClipKind::Text,
                    Some(1) => ClipKind::Image,
                    Some(2) => ClipKind::Types,
                    Some(&other) => return Ok(Self::ClipboardReadUnknown { request, kind: other }),
                    None => return Err(invalid("short clipboard read")),
                };
                Self::ClipboardRead { request, kind: clip }
            }
            other => Self::Unknown(other),
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
            Self::ClipboardContent { request, content } => {
                Frame::new(kind::CLIPBOARD_CONTENT, [&request.to_le_bytes()[..], &clip_bytes(content.as_ref())].concat())
            }
            Self::ClipboardPush { content } => Frame::new(kind::CLIPBOARD_PUSH, clip_bytes(Some(content))),
            Self::Unknown(kind) => Frame::new(*kind, Vec::new()),
        }
    }

    /// # Errors
    /// Malformed payload (an unknown kind is [`Self::Unknown`]).
    pub fn from_frame(frame: Frame) -> io::Result<Self> {
        Ok(match frame.kind {
            kind::INPUT => Self::Input(frame.payload),
            kind::RESIZE => Self::Resize(parse_size(&frame.payload)?),
            kind::ACK => Self::Ack(parse_u64(&frame.payload)?),
            kind::CONFIG => Self::Config(parse_config(&frame.payload)?),
            kind::CLIPBOARD_CONTENT => {
                Self::ClipboardContent { request: parse_u64(&frame.payload)?, content: parse_clip(frame.payload.get(8..).unwrap_or_default())? }
            }
            kind::CLIPBOARD_PUSH => match parse_clip(&frame.payload)? {
                Some(content) => Self::ClipboardPush { content },
                None => Self::Unknown(frame.kind),
            },
            other => Self::Unknown(other),
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
            ServerData::Exited { code: Some(-1) },
            ServerData::Exited { code: None },
            ServerData::SnapshotDone,
            ServerData::Configured(TerminalConfig::default()),
            ServerData::InputAck { count: 3, offset: u64::MAX - 1 },
            ServerData::TtyMode { offset: 5, echo: false, icanon: true },
            ServerData::TtyMode { offset: 6, echo: true, icanon: false },
            ServerData::ClipboardWrite { location: ClipboardLocation::Primary, text: "héllo".into() },
            ServerData::ClipboardWrite { location: ClipboardLocation::Standard, text: String::new() },
            ServerData::ClipboardRead { request: u64::MAX, kind: ClipKind::Image },
            ServerData::ClipboardRead { request: 1, kind: ClipKind::Types },
            ServerData::ClipboardReadUnknown { request: 9, kind: 0x7f },
        ] {
            assert_eq!(ServerData::from_frame(message.to_frame()).unwrap(), message);
        }
        for content in [
            None,
            Some(Clip::Text("a\nb".into())),
            Some(Clip::Image { mime: "image/png".into(), data: vec![0x89, b'P', 0, 255] }),
            Some(Clip::Types(vec!["text/plain".into(), "image/png".into()])),
        ] {
            let message = ClientData::ClipboardContent { request: 7, content: content.clone() };
            assert_eq!(ClientData::from_frame(message.to_frame()).unwrap(), message);
            if let Some(content) = content.clone() {
                let message = ClientData::ClipboardPush { content };
                assert_eq!(ClientData::from_frame(message.to_frame()).unwrap(), message);
            }
            let json = serde_json::to_string(&content).unwrap();
            assert_eq!(serde_json::from_str::<Option<Clip>>(&json).unwrap(), content, "{json}");
        }
        assert!(ClientData::from_frame(Frame::new(kind::CLIPBOARD_CONTENT, vec![0; 8])).is_err());
        assert!(ClientData::from_frame(Frame::new(kind::CLIPBOARD_PUSH, vec![])).is_err());
        // Newer clipboard formats and locations are skipped, not errors.
        assert_eq!(ClientData::from_frame(Frame::new(kind::CLIPBOARD_PUSH, vec![0])).unwrap(), ClientData::Unknown(kind::CLIPBOARD_PUSH));
        assert_eq!(ClientData::from_frame(Frame::new(kind::CLIPBOARD_PUSH, vec![9, 1])).unwrap(), ClientData::Unknown(kind::CLIPBOARD_PUSH));
        let content = ClientData::from_frame(Frame::new(kind::CLIPBOARD_CONTENT, [&5u64.to_le_bytes()[..], &[9, 1]].concat())).unwrap();
        assert_eq!(content, ClientData::ClipboardContent { request: 5, content: None });
        assert_eq!(ServerData::from_frame(Frame::new(kind::CLIPBOARD_WRITE, vec![7, b'x'])).unwrap(), ServerData::Unknown(kind::CLIPBOARD_WRITE));
        let light = TerminalConfig { background: 0x00ff_ffff, foreground: 0, scrollback_lines: 500, ..TerminalConfig::default() };
        assert!(light.is_light() && !TerminalConfig::default().is_light());
        for message in [ClientData::Input(b"ls\r".to_vec()), ClientData::Resize(size), ClientData::Ack(99), ClientData::Config(light)] {
            assert_eq!(ClientData::from_frame(message.to_frame()).unwrap(), message);
        }
        // Out-of-range values are clamped, missing optional fields default.
        let frame = Frame::new(
            kind::CONFIG,
            br#"{"foreground":4294967295,"background":0,"cursor":0,"palette":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"scrollback_lines":99999999999}"#
                .to_vec(),
        );
        let ClientData::Config(config) = ClientData::from_frame(frame).unwrap() else { panic!() };
        assert_eq!(config.foreground, 0x00ff_ffff);
        assert_eq!(config.scrollback_lines, TerminalConfig::MAX_SCROLLBACK);
        assert!(config.cursor_blink);
    }
}
