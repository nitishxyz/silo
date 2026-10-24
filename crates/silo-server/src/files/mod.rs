//! Files blocks (docs/FILES_BLOCK.md): a read-only view of a directory,
//! with no process behind it. The server holds the view state (tabs, tree,
//! scroll), answers `files.*` reads under the block's root and watches the
//! folders shown.

pub mod find;
pub mod paths;
pub mod read;
pub mod watch;

use std::{collections::HashSet, path::PathBuf};

use serde_json::Value;
use silo_proto::{
    control::{FilesSetParams, Method, RpcError},
    model::{BlockInfo, BlockKind, FileHighlight, FilesState, TermSize},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{Server, block::Note, browser::Block};

pub struct FilesBlock {
    /// Block metadata; `files` is `None` here and filled from `state`.
    base: BlockInfo,
    state: FilesState,
    watch: Option<watch::Watch>,
}

impl FilesBlock {
    /// A files block showing `state`; it watches nothing until [`Self::watch`].
    pub fn new(mut base: BlockInfo, state: FilesState) -> Self {
        base.kind = BlockKind::Files;
        (base.browser, base.agent, base.files) = (None, None, None);
        base.cwd = Some(state.root.clone());
        base.size = TermSize::default();
        Self { base, state, watch: None }
    }

    /// A files block rebuilt from saved or transferred metadata.
    ///
    /// # Errors
    /// `info` carries no files state.
    pub fn restore(mut info: BlockInfo) -> anyhow::Result<Self> {
        let state = *info.files.take().ok_or_else(|| anyhow::anyhow!("files block {} has no state", info.id))?;
        info.owner = None;
        info.owner_name = None;
        Ok(Self::new(info, state))
    }

    pub fn info(&self) -> BlockInfo {
        let root = std::path::Path::new(&self.state.root);
        let title = root.file_name().map_or_else(|| self.state.root.clone(), |n| n.to_string_lossy().into_owned());
        BlockInfo { title, files: Some(Box::new(self.state.clone())), ..self.base.clone() }
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.state.root)
    }

    pub fn state(&self) -> &FilesState {
        &self.state
    }

    /// The folders watched (for tests and diagnostics).
    pub fn watched(&self) -> HashSet<PathBuf> {
        self.watch.as_ref().map(watch::Watch::watched).unwrap_or_default()
    }

    /// (Re)start watching the root, expanded folders and open files'
    /// folders, reporting to `notes`.
    pub fn watch(&mut self, notes: UnboundedSender<Note>) {
        self.watch = None;
        match watch::Watch::start(self.base.id.clone(), self.root(), notes) {
            Ok(w) => {
                self.watch = Some(w);
                self.sync();
            }
            Err(error) => tracing::warn!(block = %self.base.id, "files watch: {error}"),
        }
    }

    fn sync(&mut self) {
        let Some(watch) = &mut self.watch else { return };
        let root = PathBuf::from(&self.state.root);
        let parent = |p: &str| p.rsplit_once('/').map_or(String::new(), |(dir, _)| dir.to_owned());
        let dirs = std::iter::once(String::new()).chain(self.state.expanded.iter().cloned()).chain(self.state.open.iter().map(|p| parent(p)));
        let want = dirs.filter_map(|d| paths::resolve(&root, &d).ok()).filter(|p| p.is_dir()).collect();
        watch.sync(want);
    }

    /// Apply `files.set`; true if the state changed.
    ///
    /// # Errors
    /// `bad_request` for malformed paths or too many of them; `forbidden`
    /// for paths with `..`.
    pub fn set(&mut self, params: FilesSetParams) -> Result<bool, RpcError> {
        let mut state = self.state.clone();
        let clean = |list: Vec<String>, max: usize, what: &str| -> Result<Vec<String>, RpcError> {
            let mut out: Vec<String> = Vec::new();
            for path in list {
                let path = paths::normalize(&path)?;
                if !path.is_empty() && !out.contains(&path) {
                    out.push(path);
                }
            }
            if out.len() > max {
                return Err(RpcError::bad_request(format!("at most {max} {what}")));
            }
            Ok(out)
        };
        if let Some(open) = params.open {
            state.open = clean(open, FilesState::OPEN_MAX, "open files")?;
        }
        if let Some(active) = params.active {
            let active = paths::normalize(&active)?;
            if !active.is_empty() && !state.open.contains(&active) {
                state.open.push(active.clone());
            }
            state.active = Some(active).filter(|a| !a.is_empty());
        }
        if state.open.len() > FilesState::OPEN_MAX {
            return Err(RpcError::bad_request(format!("at most {} open files", FilesState::OPEN_MAX)));
        }
        if let Some(tree) = params.tree {
            state.tree = tree;
        }
        if let Some(expanded) = params.expanded {
            state.expanded = clean(expanded, FilesState::EXPANDED_MAX, "expanded folders")?;
        }
        for (path, line) in params.scroll.unwrap_or_default() {
            state.scroll.insert(paths::normalize(&path)?, line);
        }
        if state.active.as_ref().is_some_and(|a| !state.open.contains(a)) {
            state.active = state.open.last().cloned();
        }
        for (path, marks) in params.highlights.unwrap_or_default() {
            let path = paths::normalize(&path)?;
            let marks = clean_highlights(marks)?;
            if marks.is_empty() {
                state.highlights.remove(&path);
            } else {
                state.highlights.insert(path, marks);
            }
        }
        let open = state.open.clone();
        state.scroll.retain(|path, _| open.contains(path));
        state.highlights.retain(|path, _| open.contains(path));
        let resync = (&state.open, &state.expanded) != (&self.state.open, &self.state.expanded);
        let changed = state != self.state;
        self.state = state;
        if resync {
            self.sync();
        }
        Ok(changed)
    }

    /// Show canonical directory `root` with a fresh view state.
    pub fn reroot(&mut self, root: &std::path::Path, notes: UnboundedSender<Note>) {
        self.state = FilesState::new(root.to_string_lossy().into_owned());
        self.base.cwd = Some(self.state.root.clone());
        self.watch(notes);
    }
}

/// Most paths one `files.changed` lists.
pub const CHANGED_MAX: usize = 1000;

/// Highlights as stored: 1-based ranges with `start <= end` (a reversed
/// one is turned round), notes trimmed and cut to
/// [`FileHighlight::NOTE_MAX`] characters, empty notes dropped.
///
/// # Errors
/// `bad_request` for line 0 or more than [`FileHighlight::PER_FILE`].
pub fn clean_highlights(marks: Vec<FileHighlight>) -> Result<Vec<FileHighlight>, RpcError> {
    if marks.len() > FileHighlight::PER_FILE {
        return Err(RpcError::bad_request(format!("at most {} highlights per file", FileHighlight::PER_FILE)));
    }
    marks
        .into_iter()
        .map(|m| {
            if m.start == 0 || m.end == 0 {
                return Err(RpcError::bad_request("highlight lines start at 1"));
            }
            let note = m.note.map(|n| n.trim().chars().take(FileHighlight::NOTE_MAX).collect::<String>()).filter(|n| !n.is_empty());
            Ok(FileHighlight { start: m.start.min(m.end), end: m.start.max(m.end), note })
        })
        .collect()
}

/// Changed absolute `paths` relative to canonical `root`, sorted and
/// deduplicated, and whether some were left out ([`CHANGED_MAX`]).
pub fn changed(root: &std::path::Path, paths: Vec<PathBuf>) -> (Vec<String>, bool) {
    let mut rel: Vec<String> = paths.iter().filter_map(|p| paths::relative(root, p)).collect();
    rel.sort();
    rel.dedup();
    let truncated = rel.len() > CHANGED_MAX;
    rel.truncate(CHANGED_MAX);
    (rel, truncated)
}

/// The root of files block `block_id`.
fn root_of(server: &Server, block_id: &str) -> Result<PathBuf, RpcError> {
    match server.registry.lock().unwrap().blocks.get(block_id) {
        Some(Block::Files(files)) => Ok(files.root()),
        Some(_) => Err(RpcError::bad_request(format!("block {block_id} is not a files block"))),
        None => Err(RpcError::not_found(format!("block {block_id}"))),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, RpcError> + Send + 'static) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(f).await.map_err(RpcError::internal)?
}

/// A reply that fits in a frame.
fn reply(value: &impl serde::Serialize) -> Result<Value, RpcError> {
    let bytes = serde_json::to_vec(value).map_err(RpcError::internal)?;
    if bytes.len() > silo_proto::MAX_FRAME - 64 * 1024 {
        return Err(RpcError::new("too_large", "the reply would not fit in a frame; read fewer bytes"));
    }
    serde_json::from_slice(&bytes).map_err(RpcError::internal)
}

pub async fn handle(server: &Server, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::FilesList(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || read::list(&root, &p.path)).await?)
        }
        Method::FilesRead(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || read::read(&root, &p.path, p.max_bytes)).await?)
        }
        Method::FilesReadImage(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || read::read_image(&root, &p.path)).await?)
        }
        Method::FilesFind(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || find::find(&root, &p.query, p.limit)).await?)
        }
        Method::FilesSet(p) => reply(&server.registry.lock().unwrap().files_set(p)?),
        Method::FilesRoot(p) => {
            let current = root_of(server, &p.block_id)?;
            let path = p.path.clone();
            let root = blocking(move || paths::new_root(Some(&current), &path)).await?;
            reply(&server.registry.lock().unwrap().files_root(&p.block_id, &root)?)
        }
        other => Err(RpcError::method_not_found(&silo_proto::api::method_name(&other))),
    }
}
