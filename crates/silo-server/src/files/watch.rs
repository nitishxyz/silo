//! Folder watches for files blocks: the root, expanded folders and the
//! folders of open files (docs/FILES_BLOCK.md §7). Linux watches each such
//! folder (one inotify watch apiece, none for the rest of the tree); macOS
//! has one FSEvents stream on the root, whose restarts take seconds, and
//! keeps the events in those folders. A worker thread owns the platform
//! watcher, so starting and stopping it never holds the registry lock.
//! Changes are debounced and reported as a [`Note`].

use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use notify::{EventKind, RecursiveMode, Watcher};
use silo_proto::model::BlockId;
use tokio::sync::mpsc::UnboundedSender;

use crate::block::Note;

/// How long changes gather before one note goes out.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// One stream on the root (FSEvents) rather than a watch per folder.
const ONE_STREAM: bool = cfg!(target_os = "macos");

/// A block's watcher; dropping it stops the watches and, once the last
/// change is out, the worker and debounce threads.
pub struct Watch {
    /// Folders whose changes are reported (canonical).
    shown: Arc<Mutex<HashSet<PathBuf>>>,
    /// To the worker: the folders to watch.
    folders: Sender<HashSet<PathBuf>>,
}

impl Watch {
    /// Report changes under canonical `root` as block `block_id`'s.
    ///
    /// # Errors
    /// A thread cannot start.
    pub fn start(block_id: BlockId, root: PathBuf, notes: UnboundedSender<Note>) -> std::io::Result<Self> {
        let shown = Arc::new(Mutex::new(HashSet::new()));
        let (changes, changed) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let (folders, wanted) = std::sync::mpsc::channel();
        let filter = shown.clone();
        let worker_root = root.clone();
        std::thread::Builder::new().name("silo-files-watch".into()).spawn(move || work(&worker_root, &wanted, filter, changes))?;
        std::thread::Builder::new().name("silo-files-debounce".into()).spawn(move || debounce(&changed, &block_id, &root, &notes))?;
        Ok(Self { shown, folders })
    }

    /// Report changes in exactly the folders in `want` (canonical paths).
    pub fn sync(&mut self, want: HashSet<PathBuf>) {
        want.clone_into(&mut self.shown.lock().unwrap());
        let _ = self.folders.send(want);
    }

    /// The folders whose changes are reported.
    pub fn watched(&self) -> HashSet<PathBuf> {
        self.shown.lock().unwrap().clone()
    }
}

/// Own the platform watcher until the [`Watch`] is dropped.
fn work(root: &Path, wanted: &Receiver<HashSet<PathBuf>>, shown: Arc<Mutex<HashSet<PathBuf>>>, changes: Sender<Vec<PathBuf>>) {
    let handler = move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let shown = shown.lock().unwrap();
        let paths: Vec<PathBuf> = event.paths.into_iter().filter(|p| shown.contains(p) || p.parent().is_some_and(|d| shown.contains(d))).collect();
        drop(shown);
        if !paths.is_empty() {
            let _ = changes.send(paths);
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(watcher) => watcher,
        Err(error) => return tracing::warn!(root = %root.display(), "files watch: {error}"),
    };
    if ONE_STREAM && let Err(error) = watcher.watch(root, RecursiveMode::Recursive) {
        tracing::warn!(root = %root.display(), "files watch: {error}");
    }
    let mut watched: HashSet<PathBuf> = HashSet::new();
    while let Ok(mut want) = wanted.recv() {
        // Only the latest set matters.
        while let Ok(newer) = wanted.try_recv() {
            want = newer;
        }
        if ONE_STREAM {
            continue;
        }
        for path in watched.difference(&want) {
            let _ = watcher.unwatch(path);
        }
        watched.retain(|p| want.contains(p));
        for path in want {
            if watched.contains(&path) {
                continue;
            }
            match watcher.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => drop(watched.insert(path)),
                Err(error) => tracing::debug!(path = %path.display(), "files watch: {error}"),
            }
        }
    }
}

/// Gather changes for [`DEBOUNCE`] after the first, then send them; ends
/// when the watcher is gone.
fn debounce(rx: &Receiver<Vec<PathBuf>>, block_id: &str, root: &Path, notes: &UnboundedSender<Note>) {
    while let Ok(first) = rx.recv() {
        let mut paths: BTreeSet<PathBuf> = first.into_iter().collect();
        let deadline = Instant::now() + DEBOUNCE;
        let mut open = true;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(more) => paths.extend(more),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    open = false;
                    break;
                }
            }
        }
        let note = Note::FilesChanged { block_id: block_id.to_owned(), root: root.to_owned(), paths: paths.into_iter().collect() };
        if notes.send(note).is_err() || !open {
            return;
        }
    }
}
