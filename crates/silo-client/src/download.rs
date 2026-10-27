//! Download streams (`silo_proto::download`): a files block's items copied
//! to a local folder.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use silo_proto::{
    Frame,
    control::{DataTicket, Method},
    data::Reject,
    download::{DownloadHello, DownloadPlan, DownloadRequest},
    kind, spawn_reader,
    upload::UploadEntry,
    write_frame,
};
use tokio::io::AsyncWriteExt;

use crate::Control;

/// A name the server sent, safe to create here: one component.
fn component(name: &str) -> anyhow::Result<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        bail!("the server sent a bad name: {name:?}");
    }
    Ok(name)
}

/// `name`, else Finder's `name 2.ext`, `name 3.ext`…, free in `dir`.
#[must_use]
pub fn free_name(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if path.symlink_metadata().is_err() {
        return path;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(at) if at > 0 => name.split_at(at),
        _ => (name, ""),
    };
    (2..).map(|n| dir.join(format!("{stem} {n}{ext}"))).find(|p| p.symlink_metadata().is_err()).expect("a free name")
}

fn rejected(frame: &Frame) -> anyhow::Error {
    match frame.parse::<Reject>() {
        Ok(reject) => anyhow!("{}", reject.reason),
        Err(_) => anyhow!("unexpected download frame {:#x}", frame.kind),
    }
}

/// Download `paths` (relative to files block `block_id`'s root; folders
/// recursively) into local folder `dest_dir`, each under its own name or
/// Finder's next free one; returns where each top-level item landed.
/// `progress(received, total)` follows the bytes. Dropping the future
/// cancels; on failure or cancel what was written so far is removed.
///
/// # Errors
/// Connection failures, a server refusal, or local write errors.
pub async fn download(
    control: &Control,
    block_id: &str,
    paths: Vec<String>,
    dest_dir: &Path,
    mut progress: impl FnMut(u64, u64) + Send,
) -> anyhow::Result<Vec<PathBuf>> {
    let ticket: DataTicket = control.call(Method::DataConnect { block_id: block_id.to_owned() }).await?;
    let stream = control.conn().open_stream().await.context("open download stream")?;
    let (reader, mut writer) = tokio::io::split(stream);
    write_frame(&mut writer, &Frame::json(kind::DOWNLOAD_HELLO, &DownloadHello::ours(ticket.ticket))?).await?;
    write_frame(&mut writer, &Frame::json(kind::DOWNLOAD_REQUEST, &DownloadRequest { paths })?).await?;
    writer.flush().await?;
    let mut frames = spawn_reader(reader, 16);
    let mut partial = Partial(Vec::new());
    receive(&mut frames, dest_dir, &mut partial.0, &mut progress).await?;
    drop(writer);
    Ok(std::mem::take(&mut partial.0))
}

/// What a download made so far: removed unless it completes (an error, or
/// the future dropped).
struct Partial(Vec<PathBuf>);

impl Drop for Partial {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = if path.symlink_metadata().is_ok_and(|m| m.is_dir()) { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
        }
    }
}

async fn next(frames: &mut tokio::sync::mpsc::Receiver<std::io::Result<Frame>>) -> anyhow::Result<Frame> {
    match frames.recv().await {
        Some(Ok(frame)) if frame.kind == kind::REJECT => Err(rejected(&frame)),
        Some(Ok(frame)) => Ok(frame),
        Some(Err(error)) => Err(error.into()),
        None => Err(anyhow!("the download stream closed early")),
    }
}

async fn receive(
    frames: &mut tokio::sync::mpsc::Receiver<std::io::Result<Frame>>,
    dest_dir: &Path,
    created: &mut Vec<PathBuf>,
    progress: &mut (impl FnMut(u64, u64) + Send),
) -> anyhow::Result<()> {
    let plan = next(frames).await?;
    if plan.kind != kind::DOWNLOAD_PLAN {
        return Err(rejected(&plan));
    }
    let plan: DownloadPlan = plan.parse()?;
    let mut received = 0u64;
    progress(0, plan.total);
    // The current top-level item: its name as sent and where it landed.
    let mut top: Option<(String, PathBuf)> = None;
    loop {
        let frame = next(frames).await?;
        match frame.kind {
            kind::DOWNLOAD_END => return Ok(()),
            kind::DOWNLOAD_ENTRY => {}
            _ => return Err(rejected(&frame)),
        }
        let entry: UploadEntry = frame.parse()?;
        let target = match entry.path.as_slice() {
            [] => bail!("the server sent an empty path"),
            [name] => {
                let path = free_name(dest_dir, component(name)?);
                created.push(path.clone());
                top = Some((name.clone(), path.clone()));
                path
            }
            [first, rest @ ..] => match &top {
                Some((name, root)) if name == first => {
                    let mut path = root.clone();
                    for part in rest {
                        path.push(component(part)?);
                    }
                    path
                }
                _ => bail!("the server sent an entry outside the current folder"),
            },
        };
        if entry.dir {
            tokio::fs::create_dir_all(&target).await.with_context(|| format!("create {}", target.display()))?;
            continue;
        }
        let mut file =
            tokio::fs::OpenOptions::new().write(true).create_new(true).open(&target).await.with_context(|| format!("create {}", target.display()))?;
        let mut left = entry.size;
        while left > 0 {
            let chunk = next(frames).await?;
            if chunk.kind != kind::DOWNLOAD_CHUNK {
                return Err(rejected(&chunk));
            }
            if chunk.payload.len() as u64 > left {
                bail!("the server sent more than the file's size");
            }
            file.write_all(&chunk.payload).await.with_context(|| format!("write {}", target.display()))?;
            left -= chunk.payload.len() as u64;
            received += chunk.payload.len() as u64;
            progress(received, plan.total);
        }
        file.flush().await?;
    }
}
