use std::path::PathBuf;

use anyhow::{Context, anyhow, bail};
use silo_proto::{
    BUILD, Frame,
    control::{DataTicket, Method},
    data::Reject,
    drops::human_size,
    kind, spawn_reader,
    upload::{CHUNK, MAX_ENTRIES, MAX_UPLOAD, UploadEntry, UploadHello, Uploaded},
    write_frame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::Control;

/// A top-level item to upload, stored on the server's host as `name`.
pub struct Upload {
    pub name: String,
    pub source: UploadSource,
}

pub enum UploadSource {
    /// A file, or a directory uploaded recursively (symlinks inside skipped).
    Path(PathBuf),
    Bytes(Vec<u8>),
}

enum Body {
    Dir,
    File(PathBuf),
    Bytes(Vec<u8>),
}

struct Planned {
    path: Vec<String>,
    size: u64,
    body: Body,
}

fn plan(files: Vec<Upload>) -> anyhow::Result<Vec<Planned>> {
    let mut out = Vec::new();
    for upload in files {
        let top = vec![upload.name];
        match upload.source {
            UploadSource::Bytes(bytes) => out.push(Planned { path: top, size: bytes.len() as u64, body: Body::Bytes(bytes) }),
            UploadSource::Path(path) => {
                let meta = std::fs::metadata(&path).with_context(|| format!("read {}", path.display()))?;
                walk(&path, top, &meta, &mut out)?;
            }
        }
    }
    Ok(out)
}

fn walk(path: &std::path::Path, at: Vec<String>, meta: &std::fs::Metadata, out: &mut Vec<Planned>) -> anyhow::Result<()> {
    if out.len() >= MAX_ENTRIES {
        bail!("Too many files to upload (max {MAX_ENTRIES})");
    }
    if meta.is_file() {
        out.push(Planned { path: at, size: meta.len(), body: Body::File(path.to_owned()) });
        return Ok(());
    }
    if !meta.is_dir() {
        return Ok(());
    }
    out.push(Planned { path: at.clone(), size: 0, body: Body::Dir });
    let mut children = std::fs::read_dir(path)
        .with_context(|| format!("read {}", path.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("read {}", path.display()))?;
    children.sort();
    for child in children {
        let meta = std::fs::symlink_metadata(&child).with_context(|| format!("read {}", child.display()))?;
        let name = child.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut sub = at.clone();
        sub.push(name);
        walk(&child, sub, &meta, out)?;
    }
    Ok(())
}

fn rejected(frame: &Frame) -> anyhow::Error {
    match frame.parse::<Reject>() {
        Ok(reject) => anyhow!("{}", reject.reason),
        Err(_) => anyhow!("unexpected upload reply {:#x}", frame.kind),
    }
}

/// Upload `files` to the host of terminal block `block_id`; returns each
/// top-level item's absolute path there. `progress(sent, total)` follows
/// the bytes sent.
///
/// # Errors
/// Unreadable sources, more than [`MAX_UPLOAD`] bytes or [`MAX_ENTRIES`]
/// entries, connection failures or a server rejection.
pub async fn upload(control: &Control, block_id: &str, files: Vec<Upload>, mut progress: impl FnMut(u64, u64) + Send) -> anyhow::Result<Vec<String>> {
    let planned = tokio::task::spawn_blocking(move || plan(files)).await??;
    let total: u64 = planned.iter().map(|p| p.size).sum();
    if total > MAX_UPLOAD {
        bail!("Too large to upload ({}, max {})", human_size(total), human_size(MAX_UPLOAD));
    }
    let ticket: DataTicket = control.call(Method::DataConnect { block_id: block_id.to_owned() }).await?;
    let stream = control.conn().open_stream().await.context("open upload stream")?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut replies = spawn_reader(reader, 4);
    let send = async {
        write_frame(&mut writer, &Frame::json(kind::UPLOAD_HELLO, &UploadHello { build: BUILD.to_owned(), ticket: ticket.ticket, total })?).await?;
        let mut sent = 0;
        progress(sent, total);
        let mut buf = vec![0; CHUNK];
        for item in planned {
            let entry = UploadEntry { path: item.path, dir: matches!(item.body, Body::Dir), size: item.size };
            write_frame(&mut writer, &Frame::json(kind::UPLOAD_ENTRY, &entry)?).await?;
            match item.body {
                Body::Dir => {}
                Body::Bytes(bytes) => {
                    for chunk in bytes.chunks(CHUNK) {
                        write_frame(&mut writer, &Frame::new(kind::UPLOAD_CHUNK, chunk.to_vec())).await?;
                        sent += chunk.len() as u64;
                        progress(sent, total);
                    }
                }
                Body::File(path) => {
                    let file = tokio::fs::File::open(&path).await.with_context(|| format!("open {}", path.display()))?;
                    let mut file = file.take(item.size);
                    let mut left = item.size;
                    while left > 0 {
                        let n = file.read(&mut buf).await.with_context(|| format!("read {}", path.display()))?;
                        if n == 0 {
                            bail!("{} shrank during the upload", path.display());
                        }
                        write_frame(&mut writer, &Frame::new(kind::UPLOAD_CHUNK, buf[..n].to_vec())).await?;
                        left -= n as u64;
                        sent += n as u64;
                        progress(sent, total);
                    }
                }
            }
        }
        write_frame(&mut writer, &Frame::new(kind::UPLOAD_END, Vec::new())).await?;
        writer.flush().await?;
        anyhow::Ok(())
    };
    tokio::pin!(send);
    // The server may refuse before reading everything: its reply wins.
    let reply = tokio::select! {
        sent = &mut send => {
            let reply = replies.recv().await;
            if let (Err(error), None | Some(Err(_))) = (&sent, &reply) {
                bail!("{error:#}");
            }
            reply
        }
        reply = replies.recv() => reply,
    };
    match reply {
        Some(Ok(frame)) if frame.kind == kind::UPLOADED => Ok(frame.parse::<Uploaded>()?.paths),
        Some(Ok(frame)) => Err(rejected(&frame)),
        Some(Err(error)) => Err(error.into()),
        None => Err(anyhow!("upload stream closed before the server answered")),
    }
}
