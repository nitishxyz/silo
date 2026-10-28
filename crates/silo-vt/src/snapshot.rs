//! Framing helpers for Ghostty's snapshot record stream (`GHOSTSNP` v1).
//!
//! The codec itself is upstream's; this module only finds record boundaries
//! so a transport can deliver the renderable prefix (through READY) first and
//! the client can apply history one page at a time without the decoder ever
//! starving.

use crate::TerminalError;

pub const MAGIC: &[u8; 8] = b"GHOSTSNP";
pub const ENVELOPE_LEN: usize = 10;
pub const RECORD_HEADER_LEN: usize = 10;

pub mod tag {
    pub const TERMINAL: u16 = 1;
    pub const SCREEN: u16 = 2;
    pub const PAGE: u16 = 3;
    pub const HISTORY: u16 = 4;
    pub const READY: u16 = 5;
    pub const FINISH: u16 = 6;
    pub const CONTINUATION: u16 = 7;
    /// Optional Kitty graphics records. libghostty-vt is built without
    /// Kitty graphics, so its decoder skips them like any unknown tag.
    pub const KITTY_IMAGE_STORAGE: u16 = 8;
    pub const KITTY_IMAGE_PAYLOAD: u16 = 9;
    pub const KITTY_IMAGE_PAGE_PLACEMENTS: u16 = 10;

    /// Whether our decoder decodes `tag` rather than skipping it.
    #[must_use]
    pub const fn known(tag: u16) -> bool {
        matches!(tag, TERMINAL..=CONTINUATION)
    }
}

/// Snapshot format version from the envelope.
///
/// # Errors
/// Rejects data without the `GHOSTSNP` envelope.
pub fn version(bytes: &[u8]) -> Result<u16, TerminalError> {
    if bytes.len() < ENVELOPE_LEN || &bytes[..8] != MAGIC {
        return Err(TerminalError("not a GHOSTSNP snapshot".into()));
    }
    Ok(u16::from_le_bytes([bytes[8], bytes[9]]))
}

fn record_at(bytes: &[u8], offset: usize) -> Option<(u16, usize)> {
    let header = bytes.get(offset..offset + RECORD_HEADER_LEN)?;
    let tag = u16::from_le_bytes([header[0], header[1]]);
    let len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
    let end = offset.checked_add(RECORD_HEADER_LEN)?.checked_add(len)?;
    (end <= bytes.len()).then_some((tag, end))
}

/// Byte length of the renderable prefix: envelope plus every record through
/// READY.
///
/// # Errors
/// Rejects data without an envelope or without a complete READY record.
pub fn head_len(bytes: &[u8]) -> Result<usize, TerminalError> {
    version(bytes)?;
    let mut offset = ENVELOPE_LEN;
    while let Some((tag, end)) = record_at(bytes, offset) {
        offset = end;
        if tag == tag::READY {
            return Ok(offset);
        }
    }
    Err(TerminalError("snapshot has no complete READY record".into()))
}

/// Accumulates arbitrary chunks and yields whole records.
#[derive(Default)]
pub struct RecordSplitter {
    buf: Vec<u8>,
    pos: usize,
}

impl RecordSplitter {
    pub fn push(&mut self, bytes: &[u8]) {
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete record as `(tag, header + payload)`.
    pub fn next_record(&mut self) -> Option<(u16, &[u8])> {
        let (tag, end) = record_at(&self.buf, self.pos)?;
        let start = self.pos;
        self.pos = end;
        Some((tag, &self.buf[start..end]))
    }

    /// Bytes buffered but not yet returned as records.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buf.len() - self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tag: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_le_bytes().to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn splits_head_and_records_across_chunks() {
        let mut snap = MAGIC.to_vec();
        snap.extend_from_slice(&1u16.to_le_bytes());
        snap.extend(record(tag::TERMINAL, b"abc"));
        snap.extend(record(tag::READY, b""));
        let head = snap.len();
        snap.extend(record(tag::HISTORY, b"x"));
        snap.extend(record(tag::PAGE, b"yyyy"));
        snap.extend(record(tag::FINISH, b""));
        assert_eq!(head_len(&snap).unwrap(), head);
        let mut splitter = RecordSplitter::default();
        let mut tags = Vec::new();
        for chunk in snap[head..].chunks(3) {
            splitter.push(chunk);
            while let Some((tag, _)) = splitter.next_record() {
                tags.push(tag);
            }
        }
        assert_eq!(tags, [tag::HISTORY, tag::PAGE, tag::FINISH]);
        assert_eq!(splitter.pending(), 0);
    }
}
