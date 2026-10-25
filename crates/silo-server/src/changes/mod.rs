//! Changes blocks (docs/CHANGES_BLOCK.md): a review of a git repo's
//! changes, with no process behind it. The server holds the view state
//! (scope, tabs, comments, marks), answers `changes.*` through the `git`
//! command line in the block's root (the repo's top level) and watches the
//! repo, sending `changes.changed`.

pub mod diff;
pub mod git;
pub mod status;
pub mod watch;
pub mod write;

use std::path::{Path, PathBuf};

use serde_json::Value;
use silo_proto::{
    control::{ChangesSetParams, Method, RpcError},
    model::{AgentLink, BlockInfo, BlockKind, ChangeMark, ChangesScope, ChangesState, ReviewComment, TermSize},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{Server, block::Note, browser::Block, files::paths};

pub struct ChangesBlock {
    /// Block metadata; `changes` is `None` here and filled from `state`.
    base: BlockInfo,
    state: ChangesState,
    watch: Option<watch::Watch>,
}

impl ChangesBlock {
    /// A changes block showing `state`; it watches nothing until
    /// [`Self::watch`].
    pub fn new(mut base: BlockInfo, state: ChangesState) -> Self {
        base.kind = BlockKind::Changes;
        (base.browser, base.agent, base.files, base.changes) = (None, None, None, None);
        base.cwd = Some(state.root.clone());
        base.size = TermSize::default();
        Self { base, state, watch: None }
    }

    /// A changes block rebuilt from saved or transferred metadata.
    ///
    /// # Errors
    /// `info` carries no changes state.
    pub fn restore(mut info: BlockInfo) -> anyhow::Result<Self> {
        let state = *info.changes.take().ok_or_else(|| anyhow::anyhow!("changes block {} has no state", info.id))?;
        info.owner = None;
        info.owner_name = None;
        Ok(Self::new(info, state))
    }

    pub fn info(&self) -> BlockInfo {
        let root = Path::new(&self.state.root);
        let title = root.file_name().map_or_else(|| self.state.root.clone(), |n| n.to_string_lossy().into_owned());
        BlockInfo { title, changes: Some(Box::new(self.state.clone())), ..self.base.clone() }
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.state.root)
    }

    pub fn state(&self) -> &ChangesState {
        &self.state
    }

    /// (Re)start watching the repo, reporting to `notes`.
    pub fn watch(&mut self, notes: UnboundedSender<Note>) {
        self.watch = None;
        match watch::Watch::start(self.base.id.clone(), self.root(), notes) {
            Ok(w) => self.watch = Some(w),
            Err(error) => tracing::warn!(block = %self.base.id, "changes watch: {error}"),
        }
    }

    /// Apply `changes.set`; true if the state changed.
    ///
    /// # Errors
    /// `bad_request` for malformed paths, scopes, comments or marks, or too
    /// many of them; `forbidden` for paths with `..`.
    pub fn set(&mut self, params: ChangesSetParams) -> Result<bool, RpcError> {
        let mut state = self.state.clone();
        if let Some(scope) = params.scope {
            status::check_scope(&scope)?;
            state.scope = scope;
        }
        if let Some(open) = params.open {
            let mut out: Vec<String> = Vec::new();
            for path in open {
                let path = paths::normalize(&path)?;
                if !path.is_empty() && !out.contains(&path) {
                    out.push(path);
                }
            }
            state.open = out;
        }
        if let Some(active) = params.active {
            let active = paths::normalize(&active)?;
            if !active.is_empty() && !state.open.contains(&active) {
                state.open.push(active.clone());
            }
            state.active = Some(active).filter(|a| !a.is_empty());
        }
        if state.open.len() > ChangesState::OPEN_MAX {
            return Err(RpcError::bad_request(format!("at most {} open files", ChangesState::OPEN_MAX)));
        }
        if state.active.as_ref().is_some_and(|a| !state.open.contains(a)) {
            state.active = state.open.last().cloned();
        }
        if let Some(list) = params.list {
            state.list = list;
        }
        if let Some(split) = params.split {
            state.split = split;
        }
        if let Some(context) = params.context {
            if context > ChangesState::CONTEXT_MAX {
                return Err(RpcError::bad_request(format!("at most {} context lines", ChangesState::CONTEXT_MAX)));
            }
            state.context = context;
        }
        if let Some(comments) = params.comments {
            state.comments = clean_comments(comments)?;
        }
        if let Some(agent) = params.agent {
            state.agent = agent.map(clean_agent).transpose()?;
        }
        if let Some(marks) = params.marks {
            state.marks = clean_marks(marks)?;
        }
        let changed = state != self.state;
        self.state = state;
        Ok(changed)
    }

    /// Link the agent the review goes back to (a block split off it).
    pub fn link(&mut self, agent: AgentLink) {
        self.state.agent = Some(agent);
    }

    /// Show the repo at canonical `root` with a fresh view state (the
    /// linked agent stays).
    pub fn reroot(&mut self, root: &Path, notes: UnboundedSender<Note>) {
        let agent = self.state.agent.take();
        self.state = ChangesState { agent, ..ChangesState::new(root.to_string_lossy().into_owned()) };
        self.base.cwd = Some(self.state.root.clone());
        self.watch(notes);
    }
}

fn short(text: &str, max: usize) -> String {
    text.trim().chars().take(max).collect()
}

/// Comments as stored: paths normalized, lines from 1 and a range turned
/// round, text trimmed and cut, ids unique.
///
/// # Errors
/// `bad_request` for too many, an empty id or text, or line 0.
pub fn clean_comments(comments: Vec<ReviewComment>) -> Result<Vec<ReviewComment>, RpcError> {
    if comments.len() > ChangesState::COMMENTS_MAX {
        return Err(RpcError::bad_request(format!("at most {} comments", ChangesState::COMMENTS_MAX)));
    }
    let mut out: Vec<ReviewComment> = Vec::new();
    for c in comments {
        let path = paths::normalize(&c.path)?;
        let text = short(&c.text, ChangesState::COMMENT_MAX);
        if c.id.is_empty() || c.id.len() > 64 || path.is_empty() || text.is_empty() || c.line == 0 {
            return Err(RpcError::bad_request("a comment needs an id, a path, a line from 1 and text"));
        }
        if out.iter().any(|o| o.id == c.id) {
            return Err(RpcError::bad_request(format!("comment id {} is used twice", c.id)));
        }
        let end = c.end_line.filter(|e| *e != c.line);
        let (line, end_line) = match end {
            Some(e) if e < c.line => (e, Some(c.line)),
            other => (c.line, other),
        };
        out.push(ReviewComment { path, text, line, end_line, ..c });
    }
    Ok(out)
}

fn clean_agent(agent: AgentLink) -> Result<AgentLink, RpcError> {
    if agent.otto_session.is_empty() || agent.otto_session.len() > 256 || agent.otto_session.chars().any(char::is_control) {
        return Err(RpcError::bad_request("agent.otto_session must be 1 to 256 printable bytes"));
    }
    Ok(agent)
}

/// Marks as stored: paths normalized, notes trimmed and cut to
/// [`ChangeMark::NOTE_MAX`] characters, empty notes dropped.
///
/// # Errors
/// `bad_request` for line 0 or more than [`ChangeMark::MAX`].
pub fn clean_marks(marks: Vec<ChangeMark>) -> Result<Vec<ChangeMark>, RpcError> {
    if marks.len() > ChangeMark::MAX {
        return Err(RpcError::bad_request(format!("at most {} marks", ChangeMark::MAX)));
    }
    marks
        .into_iter()
        .map(|m| {
            let path = paths::normalize(&m.path)?;
            if path.is_empty() || m.line == 0 {
                return Err(RpcError::bad_request("a mark needs a path and a line from 1"));
            }
            let note = m.note.map(|n| short(&n, ChangeMark::NOTE_MAX)).filter(|n| !n.is_empty());
            Ok(ChangeMark { path, line: m.line, note })
        })
        .collect()
}

/// The repo holding canonical folder `dir`: the nearest folder up from it
/// with a `.git` (a folder, or a worktree's file); `dir` itself when none.
/// Read from the file system (no git), as it runs under the registry lock.
pub fn find_root(dir: &Path) -> PathBuf {
    dir.ancestors().find(|a| a.join(".git").symlink_metadata().is_ok()).unwrap_or(dir).to_owned()
}

/// `rel` as git is given it: normalized, not empty, and its folder (with
/// symlinks resolved) under the root; the file itself may be gone (a
/// deletion) or be a link, which git reads as a link.
///
/// # Errors
/// See [`paths::normalize`]; `forbidden` when its folder leaves the root.
pub fn check_path(root: &Path, rel: &str) -> Result<String, RpcError> {
    let rel = paths::normalize(rel)?;
    if rel.is_empty() {
        return Err(RpcError::bad_request("a file path is needed"));
    }
    let root = paths::canonical_root(root)?;
    let parent = Path::new(&rel).parent().map(|p| root.join(p)).unwrap_or_else(|| root.clone());
    let existing = parent.ancestors().find(|a| a.exists()).unwrap_or(&root);
    let canonical = existing.canonicalize().map_err(|e| RpcError::not_found(format!("{rel} ({e})")))?;
    if !paths::inside(&root, &canonical) {
        return Err(RpcError::denied(format!("path {rel} is outside the block's root")));
    }
    Ok(rel)
}

/// The root, scope and context of changes block `block_id`.
fn view_of(server: &Server, block_id: &str) -> Result<(PathBuf, ChangesScope, u32), RpcError> {
    match server.registry.lock().unwrap().blocks.get(block_id) {
        Some(Block::Changes(c)) => Ok((c.root(), c.state().scope.clone(), c.state().context)),
        Some(_) => Err(RpcError::bad_request(format!("block {block_id} is not a changes block"))),
        None => Err(RpcError::not_found(format!("block {block_id}"))),
    }
}

/// Like [`view_of`]; `bad_request` when the scope is read-only.
fn writable(server: &Server, block_id: &str) -> Result<(PathBuf, ChangesScope, u32), RpcError> {
    let view = view_of(server, block_id)?;
    if view.1.read_only() {
        return Err(RpcError::bad_request("this scope is read-only: it compares two snapshots or revisions"));
    }
    Ok(view)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, RpcError> + Send + 'static) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(f).await.map_err(RpcError::internal)?
}

fn reply(value: &impl serde::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::internal)
}

pub async fn handle(server: &Server, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::ChangesStatus(p) => {
            let (root, scope, _) = view_of(server, &p.block_id)?;
            reply(&blocking(move || status::status(&root, &scope)).await?)
        }
        Method::ChangesDiff(p) => {
            let (root, scope, context) = view_of(server, &p.block_id)?;
            let context = p.context.unwrap_or(context).min(ChangesState::CONTEXT_MAX);
            reply(&blocking(move || diff::diff(&root, &scope, &check_path(&root, &p.path)?, context).map(|(file, _)| file)).await?)
        }
        Method::ChangesSet(p) => reply(&server.registry.lock().unwrap().changes_set(p)?),
        Method::ChangesStage(p) => {
            let (root, scope, context) = writable(server, &p.block_id)?;
            let run = move || {
                write::stage(&root, &check_path(&root, &p.path)?, p.hunks.as_deref(), context)?;
                status::status(&root, &scope)
            };
            reply(&blocking(run).await?)
        }
        Method::ChangesUnstage(p) => {
            let (root, scope, context) = writable(server, &p.block_id)?;
            let run = move || {
                write::unstage(&root, &check_path(&root, &p.path)?, p.hunks.as_deref(), context)?;
                status::status(&root, &scope)
            };
            reply(&blocking(run).await?)
        }
        Method::ChangesRevert(p) => {
            let (root, scope, context) = writable(server, &p.block_id)?;
            let run = move || {
                write::revert(&root, &scope, &check_path(&root, &p.path)?, p.hunks.as_deref(), context)?;
                status::status(&root, &scope)
            };
            reply(&blocking(run).await?)
        }
        Method::ChangesCommit(p) => {
            let (root, _, _) = view_of(server, &p.block_id)?;
            reply(&blocking(move || write::commit(&root, &p.message, p.amend)).await?)
        }
        Method::ChangesRoot(p) => {
            let (current, _, _) = view_of(server, &p.block_id)?;
            let path = p.path.clone();
            let root = blocking(move || paths::new_root(Some(&current), &path).map(|dir| find_root(&dir))).await?;
            reply(&server.registry.lock().unwrap().changes_root(&p.block_id, &root)?)
        }
        other => Err(RpcError::method_not_found(&silo_proto::api::method_name(&other))),
    }
}
