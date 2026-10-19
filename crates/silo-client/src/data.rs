use anyhow::{Context, anyhow, bail};
use silo_proto::{
    BUILD, Frame,
    control::{DataTicket, Method},
    data::{ClientData, DataHello, Reject, Resume, ServerData, Welcome},
    kind, read_frame, spawn_reader, write_frame,
};
use tokio::{net::UnixStream, sync::mpsc};

use crate::{Control, Mirror};

/// An attached block data stream.
pub struct DataStream {
    pub welcome: Welcome,
    /// Server frames in order; `None` once the stream closed.
    pub rx: mpsc::UnboundedReceiver<ServerData>,
    tx: mpsc::UnboundedSender<Frame>,
}

/// Cloneable sender half of a [`DataStream`].
#[derive(Clone)]
pub struct DataSender(mpsc::UnboundedSender<Frame>);

impl DataSender {
    pub fn send(&self, data: &ClientData) -> bool {
        self.0.send(data.to_frame()).is_ok()
    }

    /// Send the mirror's pending acknowledgement, if one is due.
    pub fn ack(&self, mirror: &mut Mirror, idle: bool) -> bool {
        mirror.take_ack(idle).is_none_or(|offset| self.send(&ClientData::Ack(offset)))
    }
}

impl DataStream {
    /// Obtain a ticket over `control` and attach to `block_id`.
    ///
    /// # Errors
    /// Ticket, connection or handshake failure (including `Reject`).
    pub async fn open(control: &Control, block_id: &str, resume: Option<Resume>) -> anyhow::Result<Self> {
        let ticket: DataTicket = control.call(Method::DataConnect { block_id: block_id.to_owned() }).await?;
        let stream = UnixStream::connect(control.socket()).await.context("connect data stream")?;
        let (mut reader, mut writer) = stream.into_split();
        let hello = DataHello {
            build: BUILD.to_owned(),
            ticket: ticket.ticket,
            resume,
        };
        write_frame(&mut writer, &Frame::json(kind::DATA_HELLO, &hello)?).await?;
        let first = read_frame(&mut reader).await?.ok_or_else(|| anyhow!("data stream closed during hello"))?;
        let welcome: Welcome = match first.kind {
            kind::WELCOME => first.parse()?,
            kind::REJECT => {
                let reject: Reject = first.parse()?;
                bail!("attach rejected ({}): {}", reject.code, reject.reason);
            }
            other => bail!("unexpected data frame {other:#x}"),
        };
        let (tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if write_frame(&mut writer, &frame).await.is_err() {
                    break;
                }
            }
        });
        let (in_tx, rx) = mpsc::unbounded_channel();
        let mut frames = spawn_reader(reader, 256);
        tokio::spawn(async move {
            while let Some(Ok(frame)) = frames.recv().await {
                match ServerData::from_frame(frame) {
                    Ok(data) => {
                        if in_tx.send(data).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("bad data frame: {e}");
                        break;
                    }
                }
            }
        });
        Ok(Self { welcome, rx, tx })
    }

    pub fn send(&self, data: &ClientData) -> bool {
        self.tx.send(data.to_frame()).is_ok()
    }

    /// Send the mirror's pending acknowledgement, if one is due; `idle`
    /// when every received frame has been applied.
    pub fn ack(&self, mirror: &mut Mirror, idle: bool) -> bool {
        mirror.take_ack(idle).is_none_or(|offset| self.send(&ClientData::Ack(offset)))
    }

    #[must_use]
    pub fn sender(&self) -> DataSender {
        DataSender(self.tx.clone())
    }
}
