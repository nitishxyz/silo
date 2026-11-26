//! The watcher of a studio block: its root, recursively (one FSEvents
//! stream on macOS), as the Changes block's. Changes are debounced
//! ([`crate::files::watch::DEBOUNCE`], 150 ms), sorted into project, world
//! and canvas changes off the registry lock, and sent as one
//! [`Note::StudioChanged`].

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
};

use notify::{EventKind, RecursiveMode, Watcher};
use silo_proto::{model::BlockId, studio::StudioChange};
use tokio::sync::mpsc::UnboundedSender;

use super::{Changed, canvas, disk, project};
use crate::block::Note;

/// Dropping it stops the watch.
pub struct Watch {
    _stop: Sender<()>,
}

impl Watch {
    /// Watch canonical `root` for block `block_id`.
    ///
    /// # Errors
    /// A thread cannot start.
    pub fn start(block_id: BlockId, root: PathBuf, notes: UnboundedSender<Note>) -> std::io::Result<Self> {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let (changes, changed) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let worker_root = root.clone();
        std::thread::Builder::new().name("silo-studio-watch".into()).spawn(move || work(&worker_root, &stopped, changes))?;
        std::thread::Builder::new().name("silo-studio-debounce".into()).spawn(move || {
            crate::files::watch::debounce(&changed, crate::files::watch::DEBOUNCE, |paths| {
                let (changes, toml) = classify(&root, &paths);
                if changes.is_empty() {
                    return true;
                }
                notes.send(Note::StudioChanged { block_id: block_id.clone(), root: root.clone(), changes, toml }).is_ok()
            });
        })?;
        Ok(Self { _stop: stop })
    }
}

/// What changed paths under canonical `root` mean, and (when
/// `studio.toml` is among them) whether it exists now and the first canvas
/// in tab order.
pub fn classify(root: &Path, paths: &[PathBuf]) -> (Vec<Changed>, Option<(bool, Option<String>)>) {
    let mut out: Vec<Changed> = Vec::new();
    let mut toml = None;
    let push = |c: Changed, out: &mut Vec<Changed>| {
        if !out.iter().any(|o| o.what == c.what && o.name == c.name) {
            out.push(c);
        }
    };
    for path in paths {
        let Some(rel) = crate::files::paths::relative(root, path) else { continue };
        let parts: Vec<&str> = rel.split('/').collect();
        if parts.last().is_some_and(|n| disk::is_temp(n) || *n == ".DS_Store") {
            continue;
        }
        match parts.as_slice() {
            [project::TOML] => {
                let exists = root.join(project::TOML).is_file();
                toml = Some((exists, if exists { project::canvas_order(root).into_iter().next() } else { None }));
                push(Changed::project(), &mut out);
            }
            ["world", ..] => push(Changed { what: StudioChange::World, name: None, rev: None }, &mut out),
            ["canvases"] | ["canvases", _] => {
                push(Changed::project(), &mut out);
                if let ["canvases", name] = parts.as_slice() {
                    push(Changed::canvas(name, None), &mut out);
                }
            }
            ["canvases", name, canvas::FILE] => {
                if root.join("canvases").join(name).join(canvas::FILE).is_file() {
                    push(Changed::canvas(name, None), &mut out);
                } else {
                    push(Changed::project(), &mut out);
                }
            }
            // Its assets, and its documents (`script.md`: the cards wired from it go out of date).
            ["canvases", name, "assets", ..] => push(Changed::canvas(name, None), &mut out),
            // Its cuts (`studio.production`'s `cut`).
            ["canvases", name, "out", ..] => push(Changed::canvas(name, None), &mut out),
            ["canvases", name, file] if silo_proto::studio::is_document(file) => push(Changed::canvas(name, None), &mut out),
            _ => {}
        }
    }
    for c in &mut out {
        if c.what == StudioChange::Canvas
            && let Some(name) = &c.name
        {
            c.rev = canvas::current_rev(root, name);
        }
    }
    (out, toml)
}

fn work(root: &Path, stop: &Receiver<()>, changes: Sender<Vec<PathBuf>>) {
    let handler_root = root.to_owned();
    let sink = changes.clone();
    let handler = move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let paths: Vec<PathBuf> = event
            .paths
            .into_iter()
            .filter(|p| {
                p.strip_prefix(&handler_root)
                    .ok()
                    .and_then(|r| r.components().next())
                    .is_some_and(|c| matches!(c.as_os_str().to_str(), Some("studio.toml" | "world" | "canvases")))
            })
            .collect();
        if !paths.is_empty() {
            let _ = sink.send(paths);
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(watcher) => watcher,
        Err(error) => return tracing::warn!(root = %root.display(), "studio watch: {error}"),
    };
    // macOS: one FSEvents stream on the root. Elsewhere a watch per folder:
    // the root alone, and `world/` and `canvases/` whole once they exist (a
    // studio on a big folder doesn't watch all of it).
    let one_stream = cfg!(target_os = "macos");
    let mode = if one_stream { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
    if let Err(error) = watcher.watch(root, mode) {
        tracing::warn!(root = %root.display(), "studio watch: {error}");
    }
    let mut watched = [false, false];
    let mut first = true;
    loop {
        if !one_stream {
            for (i, dir) in ["world", "canvases"].iter().enumerate() {
                let path = root.join(dir);
                let exists = path.is_dir();
                if exists && !watched[i] {
                    watched[i] = watcher.watch(&path, RecursiveMode::Recursive).is_ok();
                    // What was made in it before the watch.
                    if !first {
                        let _ = changes.send(vec![path]);
                    }
                } else if !exists && watched[i] {
                    let _ = watcher.unwatch(&path);
                    watched[i] = false;
                }
            }
        }
        first = false;
        // Until the block's `Watch` is dropped.
        match stop.recv_timeout(std::time::Duration::from_secs(2)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            _ => return,
        }
    }
}
