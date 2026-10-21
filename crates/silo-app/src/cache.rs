//! The local host's client cache: the last session tree and block
//! checkpoints, painted at launch and while the server is unreachable.
//! Writes run on a worker thread.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use silo_client::cache::{Cache, CachedBlock, MirrorCheckpoint, server_dir};
use silo_proto::model::{SessionSummary, SessionView};

/// Minimum time between two checkpoints of one block.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(10);
/// At most one pane is checkpointed per tick: capture encodes on the UI
/// thread (~10 ms for 100k lines of scrollback).
pub const CHECKPOINT_TICK: Duration = Duration::from_secs(1);

/// What the cache remembers of a host: its sessions and the shown one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CachedTree {
    pub sessions: Vec<SessionSummary>,
    pub view: SessionView,
}

/// Cache root: `$SILO_HOME/cache` when `SILO_HOME` is set (tests, dev
/// instances), else `~/Library/Caches/silo`.
#[must_use]
pub fn root() -> PathBuf {
    root_for(std::env::var_os("SILO_HOME"), std::env::var_os("HOME"))
}

fn root_for(silo_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(dir) = silo_home.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("cache");
    }
    home.map_or_else(|| PathBuf::from("/tmp"), PathBuf::from).join("Library/Caches/silo")
}

/// Blocks of `old` that `new`, a later view of the same session, no longer
/// has: they were closed. A view of another session says nothing.
#[must_use]
pub fn closed_blocks(old: &SessionView, new: &SessionView) -> Vec<String> {
    if old.session.id != new.session.id {
        return Vec::new();
    }
    let live: BTreeSet<String> = live_blocks(new).into_iter().collect();
    live_blocks(old).into_iter().filter(|id| !live.contains(id)).collect()
}

/// Every block in `view`'s layouts.
#[must_use]
pub fn live_blocks(view: &SessionView) -> Vec<String> {
    view.session.windows.iter().flat_map(crate::floats::window_blocks).collect()
}

/// Open the cache for `identity`; a corrupt one is wiped and reopened.
fn open(root: &Path, identity: &str) -> anyhow::Result<(Cache, Option<CachedTree>, Vec<CachedBlock>)> {
    let opened = Cache::open(root, identity).and_then(|cache| {
        let loaded = cache.load::<CachedTree>()?;
        for e in &loaded.errors {
            tracing::warn!(block = %e.id, "cache: {:#}", e.error);
        }
        Ok((cache, loaded.tree, loaded.blocks))
    });
    match opened {
        Ok(opened) => Ok(opened),
        Err(error) => {
            tracing::warn!("cache unreadable, starting fresh: {error:#}");
            let _ = std::fs::remove_dir_all(server_dir(root, identity));
            Ok((Cache::open(root, identity)?, None, Vec::new()))
        }
    }
}

enum Job {
    Tree(Box<CachedTree>),
    ClearTree,
    Block(String, Box<MirrorCheckpoint>),
    Forget(Vec<String>),
    Gc(Vec<String>),
    Flush(mpsc::Sender<()>),
}

/// Handle to the cache worker.
#[derive(Clone)]
pub struct CacheWriter {
    jobs: mpsc::Sender<Job>,
}

/// Open the cache and start its worker; returns what was cached.
pub fn start(root: &Path, identity: &str) -> anyhow::Result<(CacheWriter, Option<CachedTree>, Vec<CachedBlock>)> {
    let (mut cache, tree, blocks) = open(root, identity)?;
    let (jobs, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new().name("silo-cache".into()).spawn(move || {
        for job in rx {
            let result = match job {
                Job::Tree(tree) => cache.save_tree(&*tree),
                Job::ClearTree => cache.clear_tree(),
                Job::Block(id, checkpoint) => cache.save_checkpoint(&id, &checkpoint),
                Job::Forget(ids) => ids.iter().try_for_each(|id| cache.forget_block(id)),
                Job::Gc(live) => cache.gc(live.iter().map(String::as_str)),
                Job::Flush(done) => {
                    let _ = done.send(());
                    Ok(())
                }
            };
            if let Err(error) = result {
                tracing::warn!("cache write: {error:#}");
            }
        }
    })?;
    Ok((CacheWriter { jobs }, tree, blocks))
}

/// One host's cache: its writer, the cached tree and restored mirrors
/// (waiting for their panes).
pub struct HostCache {
    pub writer: CacheWriter,
    pub tree: Option<CachedTree>,
    pub restored: std::collections::HashMap<String, silo_client::Mirror>,
}

/// Open the cache of the host keyed `identity`; `None` (logged) if it
/// can't be opened.
#[must_use]
pub fn open_host(root: &Path, identity: &str) -> Option<HostCache> {
    let (writer, tree, blocks) = start(root, identity).inspect_err(|e| tracing::warn!("client cache: {e:#}")).ok()?;
    let restored = blocks
        .into_iter()
        .filter_map(|b| b.checkpoint.restore().inspect_err(|e| tracing::warn!(block = %b.id, "cached block: {e:#}")).ok().map(|m| (b.id, m)))
        .collect();
    Some(HostCache { writer, tree, restored })
}

impl CacheWriter {
    pub fn save_tree(&self, tree: CachedTree) {
        let _ = self.jobs.send(Job::Tree(Box::new(tree)));
    }

    pub fn clear_tree(&self) {
        let _ = self.jobs.send(Job::ClearTree);
    }

    pub fn save_block(&self, id: String, checkpoint: MirrorCheckpoint) {
        let _ = self.jobs.send(Job::Block(id, Box::new(checkpoint)));
    }

    /// Drop the checkpoints of closed blocks.
    pub fn forget(&self, ids: Vec<String>) {
        if !ids.is_empty() {
            let _ = self.jobs.send(Job::Forget(ids));
        }
    }

    /// Keep only the checkpoints of `live` blocks: every block of every
    /// session on the server.
    pub fn gc(&self, live: Vec<String>) {
        let _ = self.jobs.send(Job::Gc(live));
    }

    /// Wait (blocking, up to `timeout`) until queued writes are done.
    pub fn flush(&self, timeout: Duration) {
        let (tx, rx) = mpsc::channel();
        if self.jobs.send(Job::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::model::{Axis, Layout, SessionInfo, WindowInfo};

    use super::*;

    fn view(blocks: &[&str]) -> SessionView {
        view_of("s", blocks)
    }

    fn view_of(session: &str, blocks: &[&str]) -> SessionView {
        let layout = blocks
            .iter()
            .map(|b| Layout::Block { id: (*b).to_owned() })
            .reduce(|a, b| Layout::Split { axis: Axis::Horizontal, ratio: 0.5, first: Box::new(a), second: Box::new(b) })
            .unwrap();
        let window = WindowInfo { id: "w".into(), name: None, layout, focused: blocks[0].into(), zoomed: false, floats: Vec::new() };
        SessionView {
            session: SessionInfo {
                id: session.into(),
                name: "main".into(),
                windows: vec![window],
                active_window: "w".into(),
                revision: 1,
                created_at: 0,
            },
            blocks: Vec::new(),
            status: Vec::new(),
            watchers: Vec::new(),
        }
    }

    #[test]
    fn root_follows_silo_home() {
        assert_eq!(root_for(Some("/tmp/x".into()), Some("/Users/u".into())), PathBuf::from("/tmp/x/cache"));
        assert_eq!(root_for(Some("".into()), Some("/Users/u".into())), PathBuf::from("/Users/u/Library/Caches/silo"));
        assert_eq!(root_for(None, Some("/Users/u".into())), PathBuf::from("/Users/u/Library/Caches/silo"));
    }

    #[test]
    fn reconcile_drops_closed_blocks() {
        assert_eq!(closed_blocks(&view(&["a", "b", "c"]), &view(&["b", "d"])), vec!["a".to_owned(), "c".to_owned()]);
        assert!(closed_blocks(&view(&["a"]), &view(&["a", "b"])).is_empty());
        // Switching sessions closes nothing.
        assert!(closed_blocks(&view_of("s", &["a"]), &view_of("t", &["x"])).is_empty());
    }

    #[test]
    fn floating_a_block_keeps_its_checkpoint() {
        let mut floated = view(&["a"]);
        let rect = silo_proto::model::FloatRect { x: 0.1, y: 0.1, w: 0.5, h: 0.5 };
        floated.session.windows[0].floats.push(silo_proto::model::Float { id: "b".into(), rect });
        assert_eq!(live_blocks(&floated), vec!["a".to_owned(), "b".to_owned()]);
        assert!(closed_blocks(&view(&["a", "b"]), &floated).is_empty());
    }

    fn checkpoint() -> MirrorCheckpoint {
        let grid = silo_vt::GridSize { columns: 20, rows: 5, cell_width: 8, cell_height: 16 };
        let snapshot = silo_vt::TerminalCore::new(grid).unwrap().snapshot().unwrap();
        MirrorCheckpoint {
            snapshot,
            resume: silo_proto::data::Resume { epoch: 1, offset: 0 },
            size: silo_proto::model::TermSize::new(20, 5),
            config: silo_proto::model::TerminalConfig::default(),
        }
    }

    fn cached_ids(root: &Path) -> Vec<String> {
        let (_, _, blocks) = start(root, "id").unwrap();
        let mut ids: Vec<String> = blocks.into_iter().map(|b| b.id).collect();
        ids.sort();
        ids
    }

    /// Checkpoint capture runs on the UI thread; report its cost with a
    /// full 100k-line scrollback (`--nocapture` shows the timing).
    #[test]
    fn capture_cost_with_large_scrollback() {
        use silo_proto::data::{ServerData, StartMode, Welcome};
        let mut mirror = silo_client::Mirror::new();
        mirror.begin(&Welcome { epoch: 1, mode: StartMode::Snapshot, offset: 0, size: silo_proto::model::TermSize::new(120, 40) }).unwrap();
        let config = silo_proto::model::TerminalConfig { scrollback_lines: 100_000, ..Default::default() };
        mirror.apply(ServerData::Configured(config)).unwrap();
        let grid = silo_vt::GridSize { columns: 120, rows: 40, cell_width: 8, cell_height: 16 };
        mirror.apply(ServerData::SnapshotHead(silo_vt::TerminalCore::new(grid).unwrap().snapshot().unwrap())).unwrap();
        mirror.apply(ServerData::SnapshotDone).unwrap();
        let mut offset = 0;
        for chunk in 0..100 {
            let bytes: Vec<u8> =
                (0..1000).flat_map(|i| format!("line {chunk}-{i} the quick brown fox jumps over the lazy dog\r\n").into_bytes()).collect();
            let len = bytes.len() as u64;
            mirror.apply(ServerData::Output { offset, bytes }).unwrap();
            offset += len;
        }
        let started = std::time::Instant::now();
        let checkpoint = MirrorCheckpoint::capture(&mut mirror).unwrap().unwrap();
        let elapsed = started.elapsed();
        eprintln!("capture: {elapsed:?} for {} snapshot bytes", checkpoint.snapshot.len());
    }

    #[test]
    fn only_closed_blocks_lose_checkpoints() {
        let root = temp("forget");
        let (writer, _, _) = start(&root, "id").unwrap();
        for id in ["a", "b", "x"] {
            writer.save_block(id.into(), checkpoint());
        }
        // "x" belongs to another session: a view switch keeps it.
        writer.forget(closed_blocks(&view_of("t", &["x"]), &view_of("s", &["a", "b"])));
        writer.forget(closed_blocks(&view_of("s", &["a", "b"]), &view_of("s", &["b"])));
        writer.flush(Duration::from_secs(5));
        drop(writer);
        assert_eq!(cached_ids(&root), vec!["b".to_owned(), "x".to_owned()]);
        let (writer, _, _) = start(&root, "id").unwrap();
        writer.gc(vec!["x".into()]);
        writer.flush(Duration::from_secs(5));
        drop(writer);
        assert_eq!(cached_ids(&root), vec!["x".to_owned()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("silo-app-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn tree_round_trips() {
        let root = temp("tree");
        let tree = CachedTree { sessions: Vec::new(), view: view(&["a", "b"]) };
        let (writer, cached, _) = start(&root, "/tmp/sock").unwrap();
        assert!(cached.is_none());
        writer.save_tree(tree.clone());
        writer.flush(Duration::from_secs(5));
        drop(writer);
        let (_, cached, _) = start(&root, "/tmp/sock").unwrap();
        assert_eq!(cached, Some(tree.clone()));
        let (writer, _, _) = start(&root, "/tmp/sock").unwrap();
        writer.save_block("a".into(), checkpoint());
        writer.clear_tree();
        writer.flush(Duration::from_secs(5));
        drop(writer);
        let (_, cached, blocks) = start(&root, "/tmp/sock").unwrap();
        assert!(cached.is_none(), "a closed session's tree is gone");
        assert_eq!(blocks.len(), 1, "clearing the tree keeps block checkpoints");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hosts_are_isolated_with_equal_block_ids() {
        let root = temp("isolated");
        let (local, remote) = ("/tmp/silo/server.sock", "iroh:abc");
        let a = open_host(&root, local).unwrap();
        let b = open_host(&root, remote).unwrap();
        a.writer.save_tree(CachedTree { sessions: Vec::new(), view: view_of("s-local", &["b1", "b2"]) });
        a.writer.save_block("b1".into(), checkpoint());
        a.writer.save_block("b2".into(), checkpoint());
        b.writer.save_tree(CachedTree { sessions: Vec::new(), view: view_of("s-remote", &["b1"]) });
        b.writer.save_block("b1".into(), checkpoint());
        // Closing b1 remotely leaves the local b1 alone.
        b.writer.gc(Vec::new());
        a.writer.flush(Duration::from_secs(5));
        b.writer.flush(Duration::from_secs(5));
        drop((a, b));
        let a = open_host(&root, local).unwrap();
        let b = open_host(&root, remote).unwrap();
        assert_eq!(a.tree.unwrap().view.session.id, "s-local");
        assert_eq!(b.tree.unwrap().view.session.id, "s-remote");
        let mut ids: Vec<_> = a.restored.keys().cloned().collect();
        ids.sort();
        assert_eq!(ids, ["b1", "b2"]);
        assert!(b.restored.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_manifest_starts_fresh() {
        let root = temp("corrupt");
        let (writer, _, _) = start(&root, "id").unwrap();
        writer.save_tree(CachedTree { sessions: Vec::new(), view: view(&["a"]) });
        writer.flush(Duration::from_secs(5));
        drop(writer);
        let dir = server_dir(&root, "id");
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            if entry.path().is_file() {
                std::fs::write(entry.path(), b"garbage").unwrap();
            }
        }
        let (_, cached, blocks) = start(&root, "id").unwrap();
        assert!(cached.is_none() && blocks.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
