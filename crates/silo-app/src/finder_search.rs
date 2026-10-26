//! ⌘P's server side (docs/FINDER.md): the file and text searches the
//! Files and Grep chips ask for, and opening what the finder found (a file
//! in a Files block, an agent session, a changed file).

use gpui::{Context, Window};
use silo_proto::{
    changes::ChangesSetParams,
    control::{Created, FilesSetParams, Method, SpawnSpec},
    model::{AgentState, BlockKind},
};

use crate::{
    finder::{self, FileHit, GrepHit, Hits, Request},
    workspace::Workspace,
};

/// Lines kept above a line opened at (as `files_show` does).
const CONTEXT_LINES: u32 = 5;

/// A search's answer.
#[derive(Debug)]
pub enum Reply {
    Files(Hits<FileHit>),
    Grep(Hits<GrepHit>),
}

/// Run `request` under `root` on the host's server.
pub async fn run(control: Option<silo_client::Control>, root: String, request: Request) -> anyhow::Result<Reply> {
    let _ = control;
    Ok(fake(&root, &request))
}

/// A stand-in until the server answers `search.files` / `search.grep`.
fn fake(root: &str, request: &Request) -> Reply {
    let files = [("Cargo.toml", None), ("src/main.rs", Some('M')), ("src/finder.rs", None), ("README.md", Some('?'))];
    match request {
        Request::Files { query, limit } => {
            let q = query.split(':').next().unwrap_or_default();
            let items: Vec<FileHit> = files
                .iter()
                .filter(|(p, _)| crate::commands::fuzzy(&q.to_lowercase(), p).is_some())
                .map(|(p, g)| FileHit { path: (*p).into(), name: p.rsplit('/').next().unwrap_or(p).into(), git: *g })
                .take(*limit)
                .collect();
            Reply::Files(Hits { root: root.into(), query: query.clone(), total: items.len(), items, indexing: false })
        }
        Request::Grep { query, .. } => {
            let text = format!("    let {query} = 1;");
            let start = 8;
            let items = vec![GrepHit { path: "src/main.rs".into(), line: 12, text, ranges: std::iter::once(start..start + query.len()).collect() }];
            Reply::Grep(Hits { root: root.into(), query: query.clone(), total: 1, items, indexing: false })
        }
    }
}

/// The top line (0-based) that shows 1-based `line` with some lines above.
#[must_use]
pub fn top_for(line: u32) -> u64 {
    u64::from(line.saturating_sub(1 + CONTEXT_LINES))
}

impl Workspace {
    /// Open `path` (relative to `root`) found by ⌘P: in the session's Files
    /// block whose root holds it (the deepest), else in a new Files tab at
    /// `root`; at `line` when given. The server hears of the open, for
    /// frecency.
    pub(crate) fn open_found_file(&mut self, root: String, path: String, line: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(control) = self.control() else { return };
        let Some(view) = &self.view else { return };
        let full = std::path::Path::new(&root).join(&path);
        let reuse = finder::files_block_for(&view.blocks, &full);
        let session_id = view.session.id.clone();
        let opened = (root.clone(), path.clone());
        let set = move |block_id: String, rel: String| FilesSetParams {
            block_id,
            active: Some(rel.clone()),
            scroll: line.map(|l| [(rel, top_for(l))].into()),
            ..FilesSetParams::default()
        };
        if let Some((block, rel)) = reuse.clone() {
            self.call(Method::FilesSet(set(block.clone(), rel)));
            self.reveal_agent_target(block, window, cx);
        } else {
            let spawn = SpawnSpec { cwd: Some(root.clone()), command: Vec::new(), size: None };
            let method = Method::WindowCreate { session_id, spawn, kind: BlockKind::Files, url: None };
            let control = control.clone();
            let task = self.rt.spawn(async move {
                let created: Created = control.call(method).await?;
                control.call_value(Method::FilesSet(set(created.block_id, path))).await?;
                anyhow::Ok(())
            });
            cx.spawn(async move |this, cx| {
                if let Ok(Err(error)) | Err(error) = task.await.map_err(anyhow::Error::from) {
                    tracing::warn!("open found file: {error:#}");
                    let _ = this.update(cx, |this, cx| this.flash(this.failed(&error, "Couldn't open the file"), cx));
                }
            })
            .detach();
        }
        self.send_opened(opened.0, opened.1);
    }

    /// Tell the server a file was opened, for frecency (`search.opened`).
    pub(crate) fn send_opened(&mut self, root: String, path: String) {
        if !self.host_supports(finder::SEARCH_FEATURE) {
            return;
        }
        tracing::debug!(root, path, "search.opened");
    }

    /// Show otto session `session` found by ⌘P: the block showing it, else
    /// a new agent block beside the focused one, showing it.
    pub(crate) fn open_found_agent(&mut self, session: String, title: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        if let Some(shown) = view.blocks.iter().find(|b| b.agent.as_ref().and_then(|a| a.session_id.as_deref()) == Some(session.as_str())) {
            let block = shown.id.clone();
            return self.reveal_agent_target(block, window, cx);
        }
        let Some(block) = self.focused_block(window, cx).or_else(|| self.active_window().map(|w| w.focused.clone())) else { return };
        let spawn = SpawnSpec { cwd: self.cwd_of(&block, cx), command: Vec::new(), size: None };
        let axis = self.auto_axis(&block);
        let agent = Some(AgentState { session_id: Some(session), title });
        self.create_agent(Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Agent, url: None, agent }, cx);
    }

    /// Show changed file `path` in Changes block `block`.
    pub(crate) fn open_found_change(&mut self, block: String, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.call(Method::ChangesSet(ChangesSetParams { block_id: block.clone(), active: Some(path), ..ChangesSetParams::default() }));
        self.reveal_agent_target(block, window, cx);
    }
}
