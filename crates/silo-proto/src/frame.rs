use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
    /// Serialization failure.
    pub fn json(kind: u8, value: &impl serde::Serialize) -> io::Result<Self> {
        Ok(Self::new(kind, serde_json::to_vec(value)?))
    }

    /// # Errors
    /// Invalid JSON for `T`.
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> io::Result<T> {
        serde_json::from_slice(&self.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Encode to `len | kind | payload`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + self.payload.len());
        out.extend_from_slice(&((self.payload.len() + 1) as u32).to_le_bytes());
        out.push(self.kind);
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Read one frame; `Ok(None)` on clean end-of-stream at a frame boundary.
///
/// # Errors
/// I/O errors, truncated frames and frames over [`MAX_FRAME`].
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad frame length {len}")));
    }
    let mut kind = [0u8; 1];
    reader.read_exact(&mut kind).await?;
    let mut payload = vec![0; len - 1];
    reader.read_exact(&mut payload).await?;
    Ok(Some(Frame { kind: kind[0], payload }))
}

/// # Errors
/// I/O errors.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> io::Result<()> {
    writer.write_all(&frame.encode()).await
}

/// Read frames on a dedicated task so consumers can `select!` on the
/// returned channel without losing partially read frames (`read_frame` is
/// not cancel-safe). The channel closes on end-of-stream; an I/O or framing
/// error is delivered as the last item. Bounded for backpressure.
pub fn spawn_reader<R>(mut reader: R, buffer: usize) -> tokio::sync::mpsc::Receiver<io::Result<Frame>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(buffer.max(1));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                read = read_frame(&mut reader) => match read {
                    Ok(Some(frame)) => {
                        if tx.send(Ok(frame)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                },
                // Stop reading (and release the stream) once nobody listens.
                () = tx.closed() => return,
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_frames() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let sent = Frame::new(7, b"hello world".to_vec());
        let writer = tokio::spawn(async move {
            write_frame(&mut a, &sent).await.unwrap();
            write_frame(&mut a, &Frame::new(1, vec![])).await.unwrap();
        });
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap().payload, b"hello world");
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap().kind, 1);
        writer.await.unwrap();
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }
}
