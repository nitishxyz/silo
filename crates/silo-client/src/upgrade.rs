//! Upgrade streams, client side (see [`silo_proto::upgrade`]): ask a
//! server of any build what it runs, and send it a new binary.

use std::path::Path;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use silo_proto::{
    Frame, kind, read_frame,
    upgrade::{CHUNK, HostBuild, UpgradeBinary, UpgradeHello, UpgradeRejected, UpgradeStarted},
    write_frame,
};
use silo_transport::{Conn, Stream};
use tokio::io::AsyncWriteExt;

/// The server refused the upgrade; `code` is e.g.
/// [`silo_proto::upgrade::UP_TO_DATE`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub code: String,
    pub reason: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Refused {}

/// An open upgrade stream: the server's answer to the hello.
pub struct Upgrade {
    stream: Stream,
    pub host: HostBuild,
}

/// Open an upgrade stream on `conn`; it must be the connection's first
/// stream for a remote server.
///
/// # Errors
/// Connection failure or a refusal ([`Refused`]).
pub async fn open(conn: &Conn) -> anyhow::Result<Upgrade> {
    let mut stream = conn.open_stream().await?;
    write_frame(&mut stream, &Frame::json(kind::UPGRADE_HELLO, &UpgradeHello {})?).await?;
    stream.flush().await?;
    let host = match read_frame(&mut stream).await? {
        Some(frame) if frame.kind == kind::HOST_BUILD => frame.parse()?,
        Some(frame) => return Err(refusal(&frame)),
        None => bail!("the server closed the upgrade stream (it may predate remote upgrade)"),
    };
    Ok(Upgrade { stream, host })
}

impl Upgrade {
    /// Send the binary at `path`; returns once the server installed it
    /// and started handing over to it.
    ///
    /// # Errors
    /// Unreadable binary, connection failure or a refusal ([`Refused`]).
    pub async fn send(self, path: &Path, force: bool) -> anyhow::Result<Handover> {
        let bytes = tokio::fs::read(path).await.with_context(|| format!("read {}", path.display()))?;
        let sha256 = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        let header = UpgradeBinary { size: bytes.len() as u64, sha256, force, release: None };
        self.request(&header, &bytes).await
    }

    /// Have the server fetch release `version` for its platform itself;
    /// returns once it installed it and started handing over to it.
    ///
    /// # Errors
    /// Connection failure or a refusal ([`Refused`], e.g.
    /// [`silo_proto::upgrade::NOT_PUBLISHED`]).
    pub async fn fetch_release(self, version: &str, force: bool) -> anyhow::Result<Handover> {
        let header = UpgradeBinary { size: 0, sha256: String::new(), force, release: Some(version.to_owned()) };
        self.request(&header, &[]).await
    }

    async fn request(self, header: &UpgradeBinary, bytes: &[u8]) -> anyhow::Result<Handover> {
        let (mut reader, mut writer) = tokio::io::split(self.stream);
        let reply = {
            let send = async {
                write_frame(&mut writer, &Frame::json(kind::UPGRADE_BINARY, header)?).await?;
                for chunk in bytes.chunks(CHUNK) {
                    write_frame(&mut writer, &Frame::new(kind::UPGRADE_CHUNK, chunk.to_vec())).await?;
                }
                writer.flush().await?;
                anyhow::Ok(())
            };
            // The server may refuse before reading everything: its reply wins.
            let reply = read_frame(&mut reader);
            tokio::pin!(send, reply);
            tokio::select! {
                sent = &mut send => match (sent, reply.await) {
                    (_, Ok(Some(frame))) => frame,
                    (Err(error), _) => return Err(error),
                    (Ok(()), result) => {
                        result?;
                        bail!("the server closed the upgrade stream without answering");
                    }
                },
                reply = &mut reply => match reply? {
                    Some(frame) => frame,
                    None => bail!("the server closed the upgrade stream without answering"),
                },
            }
        };
        if reply.kind != kind::UPGRADE_STARTED {
            return Err(refusal(&reply));
        }
        Ok(Handover { started: reply.parse()?, reader, _writer: writer })
    }

    /// Close without upgrading.
    pub async fn close(mut self) {
        let _ = self.stream.shutdown().await;
    }
}

/// The new binary is installed and taking over.
pub struct Handover {
    pub started: UpgradeStarted,
    reader: tokio::io::ReadHalf<Stream>,
    _writer: tokio::io::WriteHalf<Stream>,
}

impl Handover {
    /// Resolves when the stream ends: the old server exited (the takeover
    /// committed) or the connection dropped.
    ///
    /// # Errors
    /// The takeover failed ([`Refused`] with code
    /// [`silo_proto::upgrade::HANDOFF`]); the old build keeps serving.
    pub async fn failure(mut self) -> anyhow::Result<()> {
        match read_frame(&mut self.reader).await {
            Ok(Some(frame)) => Err(refusal(&frame)),
            Ok(None) | Err(_) => Ok(()),
        }
    }
}

fn refusal(frame: &Frame) -> anyhow::Error {
    match frame.parse::<UpgradeRejected>() {
        Ok(rejected) if frame.kind == kind::UPGRADE_REJECTED => Refused { code: rejected.code, reason: rejected.reason }.into(),
        _ => anyhow::anyhow!("unexpected upgrade reply {:#x}", frame.kind),
    }
}
