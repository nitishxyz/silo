//! ⌘P's server side (docs/FINDER.md): the file and text searches the
//! Files and Grep chips ask for, and opening what the finder found (a file
//! in a Files block, an agent session, a changed file).

use gpui::{Context, Window};
use silo_proto::{
    changes::ChangesSetParams,
    control::{Created, FilesSetParams, Method, SearchFiles, SearchFilesParams, SearchGrep, SearchGrepParams, SearchOpenedParams, SpawnSpec},
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
    let control = control.ok_or_else(|| anyhow::anyhow!("not connected"))?;
    let limit = |n: usize| Some(u32::try_from(n).unwrap_or(u32::MAX));
    Ok(match request {
        Request::Files { query, limit: n } => {
            let method = Method::SearchFiles(SearchFilesParams { root: root.clone(), query: query.clone(), limit: limit(n) });
            Reply::Files(file_hits(root, query, control.call::<SearchFiles>(method).await?))
        }
        Request::Grep { query, limit: n } => {
            let method = Method::SearchGrep(SearchGrepParams { root: root.clone(), query: query.clone(), mode: None, limit: limit(n) });
            Reply::Grep(grep_hits(root, query, control.call::<SearchGrep>(method).await?))
        }
    })
}

/// `search.files`' answer as the finder shows it.
#[must_use]
pub fn file_hits(root: String, query: String, found: SearchFiles) -> Hits<FileHit> {
    let items = found.items.into_iter().map(|i| FileHit { git: i.git.map(finder::status_letter), path: i.path, name: i.name }).collect();
    Hits { root, query, items, total: usize::try_from(found.total).unwrap_or(usize::MAX), indexing: found.indexing }
}

/// `search.grep`'s answer as the finder shows it.
#[must_use]
pub fn grep_hits(root: String, query: String, found: SearchGrep) -> Hits<GrepHit> {
    let items = found
        .items
        .into_iter()
        .map(|i| GrepHit {
            path: i.path,
            line: u32::try_from(i.line).unwrap_or(u32::MAX),
            text: i.text,
            ranges: i.ranges.into_iter().map(|[a, b]| a as usize..b as usize).collect(),
            git: i.git.map(finder::status_letter),
        })
        .collect();
    Hits { root, query, items, total: usize::try_from(found.total).unwrap_or(usize::MAX), indexing: found.indexing }
}

/// The top line (0-based) that shows 1-based `line` with some lines above.
#[must_use]
pub fn top_for(line: u32) -> u64 {
    u64::from(line.saturating_sub(1 + CONTEXT_LINES))
}

impl Workspace {
    /// Open `path` (relative to `root`) found by ⌘P, at `line` when given.
    /// `how` ↩: the session's Files block whose root holds it (the deepest),
    /// else a new Files tab at `root`; ⌘↩: a Files block split beside the
    /// focused pane; ⌥⌘↩: a new Files tab. The server hears of the open, for
    /// frecency.
    pub(crate) fn open_found_file(
        &mut self,
        root: String,
        path: String,
        line: Option<u32>,
        how: finder::OpenIn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(control) = self.control() else { return };
        let Some(view) = &self.view else { return };
        let full = std::path::Path::new(&root).join(&path);
        let reuse = finder::files_block_for(&view.blocks, &full).filter(|_| how == finder::OpenIn::Place);
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
            let beside = (how == finder::OpenIn::Beside)
                .then(|| self.focused_block(window, cx).or_else(|| self.active_window().map(|w| w.focused.clone())))
                .flatten();
            let method = match beside {
                Some(block) => {
                    let axis = self.auto_axis(&block);
                    Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Files, url: None, agent: None }
                }
                None => Method::WindowCreate { session_id, spawn, kind: BlockKind::Files, url: None },
            };
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
        self.call(Method::SearchOpened(SearchOpenedParams { root, path }));
    }

    /// Show otto session `session` found by ⌘P: ↩ the block showing it,
    /// else (and ⌘↩, ⌥⌘↩ always) a new agent block beside the focused one.
    /// A new tab can't carry an agent's session yet (`window.create` has no
    /// `agent`), so ⌥⌘↩ splits too.
    pub(crate) fn open_found_agent(&mut self, session: String, title: String, how: finder::OpenIn, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let shown = view.blocks.iter().find(|b| b.agent.as_ref().and_then(|a| a.session_id.as_deref()) == Some(session.as_str()));
        if let Some(shown) = shown.filter(|_| how == finder::OpenIn::Place) {
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

#[cfg(test)]
mod tests {
    use silo_proto::{
        changes::FileStatus,
        control::{SearchFileItem, SearchGrepItem},
    };

    use super::*;

    #[test]
    fn server_answers_become_rows() {
        let files = SearchFiles {
            items: vec![
                SearchFileItem { path: "src/main.rs".into(), name: "main.rs".into(), git: Some(FileStatus::Modified), score: 9 },
                SearchFileItem { path: "new.rs".into(), name: "new.rs".into(), git: Some(FileStatus::Untracked), score: 3 },
                SearchFileItem { path: "lib.rs".into(), name: "lib.rs".into(), git: None, score: 1 },
            ],
            total: 7,
            indexing: true,
        };
        let hits = file_hits("/w".into(), "rs:3".into(), files);
        assert_eq!(hits.items.iter().map(|h| h.git).collect::<Vec<_>>(), [Some('M'), Some('?'), None]);
        assert_eq!((hits.total, hits.indexing, hits.query.as_str()), (7, true, "rs:3"));
        assert_eq!(hits.items[0].path, "src/main.rs", "the server's order is kept");
        let grep = SearchGrep {
            items: vec![SearchGrepItem { path: "a.rs".into(), line: 4, col: 5, text: "fn main() {}".into(), ranges: vec![[3, 7]], git: None }],
            total: 1,
            indexing: false,
            truncated: false,
        };
        let hits = grep_hits("/w".into(), "main".into(), grep);
        assert_eq!(hits.items[0].line, 4);
        assert_eq!(&hits.items[0].text[hits.items[0].ranges[0].clone()], "main");
    }

    #[test]
    fn a_line_opens_with_some_lines_above() {
        assert_eq!(top_for(1), 0);
        assert_eq!(top_for(6), 0);
        assert_eq!(top_for(42), 36);
    }
}
