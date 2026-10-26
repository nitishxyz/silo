//! The fff pickers in use, one per canonical root: created on first
//! search, dropped after [`Limits::idle`] unused, at most
//! [`Limits::pickers`] at once (the least recently used goes). Frecency
//! and query history are shared by all of them.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use fff_search as fff;
use silo_proto::control::RpcError;

/// How many pickers stay, for how long, and how long a first search waits
/// for its folder's scan.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub pickers: usize,
    pub idle: Duration,
    /// A new picker's first search waits this long for the scan, so small
    /// folders answer complete; larger ones answer with `indexing`.
    pub first_wait: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self { pickers: 8, idle: Duration::from_secs(600), first_wait: Duration::from_millis(100) }
    }
}

struct Entry {
    root: PathBuf,
    picker: fff::SharedFilePicker,
    used: Instant,
}

struct Dbs {
    frecency: fff::SharedFrecency,
    queries: fff::SharedQueryTracker,
}

struct Inner {
    /// `<home>/search`.
    dir: PathBuf,
    limits: Limits,
    dbs: OnceLock<Dbs>,
    pickers: Mutex<Vec<Entry>>,
}

/// The server's search state (cheap to clone).
#[derive(Clone)]
pub struct Search(Arc<Inner>);

impl Search {
    /// Search state keeping its databases in `dir` (opened on first use).
    pub fn new(dir: PathBuf, limits: Limits) -> Self {
        Self(Arc::new(Inner { dir, limits, dbs: OnceLock::new(), pickers: Mutex::new(Vec::new()) }))
    }

    pub fn limits(&self) -> Limits {
        self.0.limits
    }

    /// The databases, opened on first use. One that fails to open is left
    /// empty: searches work without it.
    fn dbs(&self) -> &Dbs {
        self.0.dbs.get_or_init(|| {
            let (frecency, queries) = (fff::SharedFrecency::default(), fff::SharedQueryTracker::default());
            let dir = &self.0.dir;
            if let Err(error) = std::fs::create_dir_all(dir) {
                tracing::warn!("search: create {}: {error}", dir.display());
            }
            match fff::FrecencyTracker::open(dir.join("frecency")).and_then(|t| frecency.init(t)) {
                Ok(()) => {}
                Err(error) => tracing::warn!("search: frecency database: {error}"),
            }
            match fff::QueryTracker::open(dir.join("queries")).and_then(|t| queries.init(t)) {
                Ok(()) => {}
                Err(error) => tracing::warn!("search: query history database: {error}"),
            }
            Dbs { frecency, queries }
        })
    }

    pub fn frecency(&self) -> fff::SharedFrecency {
        self.dbs().frecency.clone()
    }

    pub fn queries(&self) -> fff::SharedQueryTracker {
        self.dbs().queries.clone()
    }

    /// The picker of canonical `root`, if one is kept; marks it used.
    pub fn cached(&self, root: &Path) -> Option<fff::SharedFilePicker> {
        let mut pickers = self.0.pickers.lock().unwrap();
        let entry = pickers.iter_mut().find(|e| e.root == root)?;
        entry.used = Instant::now();
        Some(entry.picker.clone())
    }

    /// The picker of canonical `root`, created (and its scan started) when
    /// none is kept. Blocks for up to [`Limits::first_wait`] on a new one.
    ///
    /// # Errors
    /// `bad_request` for a folder fff refuses to index (the home folder or
    /// `/`).
    pub fn picker(&self, root: &Path) -> Result<fff::SharedFilePicker, RpcError> {
        if let Some(picker) = self.cached(root) {
            return Ok(picker);
        }
        let frecency = self.frecency();
        let (picker, evicted) = {
            let mut pickers = self.0.pickers.lock().unwrap();
            // Another caller may have made it meanwhile.
            if let Some(entry) = pickers.iter_mut().find(|e| e.root == root) {
                entry.used = Instant::now();
                return Ok(entry.picker.clone());
            }
            let picker = fff::SharedFilePicker::default();
            let options = fff::FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                mode: fff::FFFMode::Neovim,
                watch: true,
                ..Default::default()
            };
            fff::FilePicker::new_with_shared_state(picker.clone(), frecency, options).map_err(|error| match error {
                fff::Error::FilesystemRoot(_) => {
                    RpcError::bad_request(format!("{} is too broad to search (the home folder or /): pick a folder under it", root.display()))
                }
                other => RpcError::internal(format!("search: index {}: {other}", root.display())),
            })?;
            let mut evicted = Vec::new();
            while pickers.len() >= self.0.limits.pickers.max(1) {
                let oldest = pickers.iter().enumerate().min_by_key(|(_, e)| e.used).map(|(i, _)| i).expect("not empty");
                evicted.push(pickers.remove(oldest));
            }
            pickers.push(Entry { root: root.to_owned(), picker: picker.clone(), used: Instant::now() });
            (picker, evicted)
        };
        retire(evicted);
        picker.wait_for_scan(self.0.limits.first_wait);
        Ok(picker)
    }

    /// Drop the pickers unused for [`Limits::idle`].
    pub fn sweep(&self) {
        let idle = self.0.limits.idle;
        let gone = {
            let mut pickers = self.0.pickers.lock().unwrap();
            let (gone, kept) = std::mem::take(&mut *pickers).into_iter().partition(|e| e.used.elapsed() >= idle);
            *pickers = kept;
            gone
        };
        retire(gone);
    }

    /// The roots with a picker, most recently used first.
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut pickers: Vec<(Instant, PathBuf)> = self.0.pickers.lock().unwrap().iter().map(|e| (e.used, e.root.clone())).collect();
        pickers.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
        pickers.into_iter().map(|(_, root)| root).collect()
    }
}

/// Stop the pickers' scans and watchers and free their indexes.
fn retire(entries: Vec<Entry>) {
    for entry in entries {
        tracing::debug!(root = %entry.root.display(), "search: dropping picker");
        entry.picker.cancel();
        let taken = entry.picker.write().ok().and_then(|mut guard| guard.take());
        if let Some(mut picker) = taken {
            picker.stop_background_monitor();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        retire(std::mem::take(&mut *self.pickers.lock().unwrap_or_else(std::sync::PoisonError::into_inner)));
    }
}
