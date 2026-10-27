//! Files blocks' data streams (feature `files_ops`): uploads into a folder
//! under the root (`UploadHello::to_dir`) and downloads of items under it
//! (docs/FILES_BLOCK.md §8).

use std::path::{Path, PathBuf};

use silo_proto::{
    Frame,
    download::{CHUNK, DownloadPlan, DownloadRequest, MAX_ENTRIES, MAX_PATHS},
    drops::human_size,
    kind,
    upload::{MAX_UPLOAD, UploadEntry, Uploaded},
    write_frame,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    sync::mpsc::Receiver,
};

use super::paths::{canonical_root, normalize, relative, resolve};
use crate::drops::{Refusal, io, refuse, sanitize};

fn rpc(error: silo_proto::control::RpcError) -> Refusal {
    let code = match error.code.as_str() {
        "forbidden" => "forbidden",
        "not_found" => "not_found",
        _ => "bad_request",
    };
    (code, error.message)
}

/// Receive an upload of `total` bytes into folder `to_dir` under `root`:
/// taken names get Finder's next free one, or (`replace`) the old item goes
/// to the Trash. On failure what this upload made is removed. The result's
/// paths are relative to the root.
pub async fn receive(
    root: &Path,
    to_dir: &str,
    replace: bool,
    trash: Option<PathBuf>,
    total: u64,
    frames: &mut Receiver<std::io::Result<Frame>>,
) -> Result<Uploaded, Refusal> {
    if total > MAX_UPLOAD {
        return Err(refuse("too_large", format!("Too large to upload ({}, max {})", human_size(total), human_size(MAX_UPLOAD))));
    }
    let canonical = canonical_root(root).map_err(rpc)?;
    let dir = resolve(root, to_dir).map_err(rpc)?;
    if !dir.is_dir() {
        return Err(refuse("bad_request", format!("{} is not a folder", normalize(to_dir).unwrap_or_default())));
    }
    let place = |dir: &Path, name: &str| -> Result<PathBuf, Refusal> {
        let name = sanitize(name);
        let path = dir.join(&name);
        if path.symlink_metadata().is_err() {
            return Ok(path);
        }
        if replace {
            crate::trash::trash(&path, trash.as_deref()).map_err(|e| refuse("read_only", format!("{name} can't be replaced: {e}")))?;
            return Ok(path);
        }
        Ok(crate::trash::free(dir, &name))
    };
    let mut created = Vec::new();
    let result = crate::drops::receive_into(&dir, total, frames, &mut created, place).await;
    if result.is_err() {
        for path in &created {
            let gone = if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                tokio::fs::remove_dir_all(path).await
            } else {
                tokio::fs::remove_file(path).await
            };
            drop(gone);
        }
    }
    result.map(|()| Uploaded { paths: created.iter().filter_map(|p| relative(&canonical, p)).collect() })
}

/// One entry to send: where it is (files only) and how it's announced.
struct Planned {
    entry: UploadEntry,
    file: Option<PathBuf>,
}

/// Walk `paths` under `root`: each top-level item resolved (and so
/// confined), folders recursively without following symlinks.
fn plan(root: &Path, paths: &[String]) -> Result<Vec<Planned>, Refusal> {
    if paths.len() > MAX_PATHS {
        return Err(refuse("too_many", format!("at most {MAX_PATHS} items at once")));
    }
    let mut out = Vec::new();
    for rel in paths {
        let path = resolve(root, rel).map_err(rpc)?;
        let rel = normalize(rel).map_err(rpc)?;
        let name = rel.rsplit('/').next().filter(|n| !n.is_empty()).map_or_else(
            || canonical_root(root).ok().and_then(|r| r.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "root".into()),
            str::to_owned,
        );
        let meta = path.metadata().map_err(io)?;
        walk(&path, vec![name], &meta, &mut out)?;
    }
    Ok(out)
}

fn walk(path: &Path, at: Vec<String>, meta: &std::fs::Metadata, out: &mut Vec<Planned>) -> Result<(), Refusal> {
    if out.len() >= MAX_ENTRIES {
        return Err(refuse("too_many", format!("Too many files to download (max {MAX_ENTRIES})")));
    }
    if meta.is_file() {
        out.push(Planned { entry: UploadEntry { path: at, dir: false, size: meta.len() }, file: Some(path.to_owned()) });
        return Ok(());
    }
    if !meta.is_dir() {
        return Ok(());
    }
    out.push(Planned { entry: UploadEntry { path: at.clone(), dir: true, size: 0 }, file: None });
    let mut children: Vec<(String, PathBuf)> =
        std::fs::read_dir(path).map_err(io)?.filter_map(Result::ok).filter_map(|e| Some((e.file_name().into_string().ok()?, e.path()))).collect();
    children.sort();
    for (name, child) in children {
        let Ok(meta) = child.symlink_metadata() else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        let mut sub = at.clone();
        sub.push(name);
        walk(&child, sub, &meta, out)?;
    }
    Ok(())
}

/// Serve a download: wait for the [`DownloadRequest`], then send the plan,
/// entries and bytes to `writer`. An error before anything is sent is the
/// caller's to reject; once the plan is out, a refusal follows as
/// `REJECT` too (the client stops at it).
pub async fn send<W: AsyncWrite + Unpin>(root: PathBuf, frames: &mut Receiver<std::io::Result<Frame>>, writer: &mut W) -> Result<(), Refusal> {
    let request: DownloadRequest = match frames.recv().await {
        Some(Ok(frame)) if frame.kind == kind::DOWNLOAD_REQUEST => frame.parse().map_err(|e| refuse("protocol", e.to_string()))?,
        Some(Ok(frame)) => return Err(refuse("protocol", format!("expected a download request, got {:#x}", frame.kind))),
        Some(Err(e)) => return Err(io(e)),
        None => return Err(refuse("closed", "download ended early")),
    };
    let planned = tokio::task::spawn_blocking(move || plan(&root, &request.paths)).await.map_err(|e| refuse("internal", e.to_string()))??;
    let total = planned.iter().map(|p| p.entry.size).sum();
    let sent = |e: std::io::Error| refuse("closed", e.to_string());
    let plan = Frame::json(kind::DOWNLOAD_PLAN, &DownloadPlan { total, entries: planned.len() as u64 }).map_err(io)?;
    write_frame(writer, &plan).await.map_err(sent)?;
    let mut buf = vec![0; CHUNK];
    for item in planned {
        write_frame(writer, &Frame::json(kind::DOWNLOAD_ENTRY, &item.entry).map_err(io)?).await.map_err(sent)?;
        let Some(path) = item.file else { continue };
        let file = tokio::fs::File::open(&path).await.map_err(io)?;
        let mut file = file.take(item.entry.size);
        let mut left = item.entry.size;
        while left > 0 {
            let n = file.read(&mut buf).await.map_err(io)?;
            if n == 0 {
                return Err(refuse("changed", format!("{} shrank during the download", item.entry.path.join("/"))));
            }
            write_frame(writer, &Frame::new(kind::DOWNLOAD_CHUNK, buf[..n].to_vec())).await.map_err(sent)?;
            left -= n as u64;
        }
    }
    write_frame(writer, &Frame::new(kind::DOWNLOAD_END, Vec::new())).await.map_err(sent)?;
    writer.flush().await.map_err(sent)
}
