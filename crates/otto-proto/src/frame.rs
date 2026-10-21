//! Length-prefixed frames. Partial headers are errors, not clean EOF.

use std::io;

use crate::MAX_FRAME;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    #[must_use]
    pub fn new(kind: u8, payload: Vec<u8>) -> Self {
        Self { kind, payload }
    }

    /// # Errors
    /// JSON serialization fails.
    pub fn json(kind: u8, value: &impl serde::Serialize) -> io::Result<Self> {
        Ok(Self::new(kind, serde_json::to_vec(value)?))
    }

    /// Parse the JSON payload.
    /// # Errors
    /// The payload is not valid JSON for `T`.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> io::Result<T> {
        serde_json::from_slice(&self.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Encode `len | kind | payload`.
    /// # Panics
    /// The frame exceeds [`MAX_FRAME`].
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.payload.len() < MAX_FRAME, "frame exceeds MAX_FRAME");
        let mut bytes = Vec::with_capacity(5 + self.payload.len());
        bytes.extend_from_slice(&((self.payload.len() + 1) as u32).to_le_bytes());
        bytes.push(self.kind);
        bytes.extend_from_slice(&self.payload);
        bytes
    }

    /// Decode one complete frame and return its consumed byte count.
    /// `None` means more bytes are needed.
    /// # Errors
    /// A zero or oversized frame length is declared.
    pub fn decode(bytes: &[u8]) -> io::Result<Option<(Self, usize)>> {
        let Some(header) = bytes.get(..4) else { return Ok(None) };
        let len = u32::from_le_bytes(header.try_into().expect("four bytes")) as usize;
        check_len(len)?;
        if bytes.len() < len + 4 {
            return Ok(None);
        }
        Ok(Some((Self::new(bytes[4], bytes[5..len + 4].to_vec()), len + 4)))
    }
}

fn check_len(len: usize) -> io::Result<()> {
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad frame length {len}")));
    }
    Ok(())
}

#[cfg(feature = "io")]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one frame; `None` only for EOF at a frame boundary.
/// # Errors
/// I/O failure, truncated header/payload, or invalid length.
#[cfg(feature = "io")]
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut header = [0; 4];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let len = u32::from_le_bytes(header) as usize;
    check_len(len)?;
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(Frame::new(body[0], body[1..].to_vec())))
}

/// # Errors
/// I/O failure or a frame over [`MAX_FRAME`].
#[cfg(feature = "io")]
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> io::Result<()> {
    if frame.payload.len() >= MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame exceeds MAX_FRAME"));
    }
    writer.write_all(&frame.encode()).await
}

/// Dedicated, bounded reader task: consumers may cancel channel receives without
/// losing partial frames. Dropping the receiver releases the stream.
#[cfg(feature = "io")]
pub fn spawn_reader<R>(mut reader: R, buffer: usize) -> tokio::sync::mpsc::Receiver<io::Result<Frame>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(buffer.max(1));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                read = read_frame(&mut reader) => match read {
                    Ok(Some(frame)) => { if tx.send(Ok(frame)).await.is_err() { return; } }
                    Ok(None) => return,
                    Err(error) => { let _ = tx.send(Err(error)).await; return; }
                },
                () = tx.closed() => return,
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_and_json() {
        let frame = Frame::json(crate::kind::CONTROL, &serde_json::json!({"x": 1})).unwrap();
        let bytes = frame.encode();
        assert_eq!(&bytes[..5], &[8, 0, 0, 0, 2]);
        for end in 0..bytes.len() {
            assert!(Frame::decode(&bytes[..end]).unwrap().is_none());
        }
        assert_eq!(Frame::decode(&bytes).unwrap(), Some((frame.clone(), bytes.len())));
        assert_eq!(frame.parse::<serde_json::Value>().unwrap()["x"], 1);
        assert!(Frame::decode(&[0; 4]).is_err());
        assert!(Frame::decode(&((MAX_FRAME + 1) as u32).to_le_bytes()).is_err());
        assert!(Frame::new(2, b"!".to_vec()).parse::<serde_json::Value>().is_err());
    }

    #[cfg(feature = "io")]
    #[tokio::test(flavor = "current_thread")]
    async fn async_framing_and_reader() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let mut frames = spawn_reader(reader, 1);
        let frame = Frame::new(1, b"hello".to_vec());
        write_frame(&mut writer, &frame).await.unwrap();
        assert_eq!(frames.recv().await.unwrap().unwrap(), frame);
        writer.write_all(&[2, 0]).await.unwrap();
        drop(writer);
        assert_eq!(frames.recv().await.unwrap().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(frames.recv().await.is_none());
        assert!(read_frame(&mut &[][..]).await.unwrap().is_none());
        assert!(read_frame(&mut &[0, 0, 0, 0][..]).await.is_err());
        assert!(read_frame(&mut &[2, 0, 0, 0, 1][..]).await.is_err());
    }
}
