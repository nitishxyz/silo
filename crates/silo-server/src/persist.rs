//! Durable server state under `<home>/state/`: the session tree as the sealed
//! manifest and one checkpoint per block as snapshot generations.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use silo_proto::model::{BlockId, BlockInfo, Lifecycle, TermSize, TerminalConfig};
use silo_store::{HostKey, Store};
use silo_vt::{GridSize, TerminalCore};
use tokio::sync::{Notify, mpsc::UnboundedSender};

use crate::{
    Server,
    block::{self, BlockHandle, Checkpoint, Note},
    registry::Session,
};

pub const STATE_DIR: &str = "state";
/// Tree mutations are coalesced for this long before committing.
const DEBOUNCE: Duration = Duration::from_millis(200);
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(5);

pub fn state_dir(home: &Path) -> PathBuf {
    home.join(STATE_DIR)
}

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub server_id: String,
    pub sessions: Vec<Session>,
    pub blocks: Vec<BlockRecord>,
}

#[derive(Serialize, Deserialize)]
pub struct BlockRecord {
    pub info: BlockInfo,
    /// Committed checkpoint generation; `None` if never checkpointed.
    pub generation: Option<u64>,
}

/// What a checkpoint reflects; an unchanged stamp is not rewritten.
#[derive(Clone, Copy, PartialEq)]
struct Stamp {
    epoch: u64,
    offset: u64,
    size: TermSize,
    lifecycle: Lifecycle,
    config: TerminalConfig,
}

impl Stamp {
    fn of(cp: &Checkpoint) -> Self {
        Self { epoch: cp.epoch, offset: cp.offset, size: cp.size, lifecycle: cp.lifecycle, config: cp.config }
    }
}

/// Snapshot file payload: `u32 LE header length`, JSON [`Checkpoint`] with
/// an empty `snapshot`, then the raw ghostty snapshot.
pub fn encode(mut cp: Checkpoint) -> anyhow::Result<Vec<u8>> {
    let snapshot = std::mem::take(&mut cp.snapshot);
    let header = serde_json::to_vec(&cp)?;
    let mut out = Vec::with_capacity(4 + header.len() + snapshot.len());
    out.extend_from_slice(&u32::try_from(header.len())?.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&snapshot);
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> anyhow::Result<Checkpoint> {
    let len = bytes.get(..4).context("truncated checkpoint")?;
    let len = u32::from_le_bytes(len.try_into()?) as usize;
    let header = bytes.get(4..4 + len).context("truncated checkpoint")?;
    let mut cp: Checkpoint = serde_json::from_slice(header)?;
    cp.snapshot = bytes[4 + len..].to_vec();
    Ok(cp)
}

/// The live view of what has been committed.
pub struct Persist {
    store: Arc<Store>,
    server_id: String,
    /// Committed generation per block, with the stamp it reflects (`None`
    /// for blocks restored at startup).
    generations: HashMap<BlockId, (u64, Option<Stamp>)>,
}

pub struct Restored {
    pub sessions: Vec<Session>,
    pub blocks: HashMap<BlockId, BlockHandle>,
}

/// Open `<home>/state` and rebuild the last committed tree. Blocks come back
/// process-less as [`Lifecycle::Restored`]; one whose checkpoint is missing or
/// unreadable comes back with an empty screen.
///
/// # Errors
/// The key or store cannot be opened, or the manifest is unreadable.
pub fn open(home: &Path, notes: &UnboundedSender<Note>) -> anyhow::Result<(Persist, Restored)> {
    let dir = state_dir(home);
    let key = HostKey::load_or_create(&dir).with_context(|| format!("host key in {}", dir.display()))?;
    let store = Store::open(&dir, key).with_context(|| format!("open {}", dir.display()))?;
    let report = store.load::<Manifest>().with_context(|| format!("load {}", store.manifest_path().display()))?;
    for error in &report.errors {
        tracing::warn!(block = %error.id, generation = error.generation, "checkpoint unreadable: {}", error.error);
    }
    let mut snapshots: HashMap<(String, u64), Vec<u8>> =
        report.snapshots.into_iter().map(|s| ((s.id, s.generation), s.bytes)).collect();
    let manifest = report.manifest.unwrap_or_else(|| Manifest {
        server_id: format!("{:016x}", rand::random::<u64>()),
        sessions: Vec::new(),
        blocks: Vec::new(),
    });
    let mut generations = HashMap::new();
    let mut blocks = HashMap::new();
    for record in manifest.blocks {
        let id = record.info.id.clone();
        let restored = record
            .generation
            .and_then(|g| snapshots.remove(&(id.clone(), g)).map(|bytes| (g, bytes)))
            .context("no readable checkpoint")
            .and_then(|(g, bytes)| Ok((g, block::spawn_restored(record.info.clone(), decode(&bytes)?, notes.clone())?)));
        let handle = match restored {
            Ok((generation, handle)) => {
                generations.insert(id.clone(), (generation, None));
                handle
            }
            Err(error) => {
                tracing::warn!(block = %id, "restoring block with an empty screen: {error:#}");
                let size = record.info.size;
                block::spawn_restored(record.info, empty_checkpoint(size)?, notes.clone())?
            }
        };
        blocks.insert(id, handle);
    }
    for session in &manifest.sessions {
        if let Some(missing) = session.blocks().into_iter().find(|b| !blocks.contains_key(b)) {
            anyhow::bail!("manifest: session {} references unknown block {missing}", session.id);
        }
    }
    let persist = Persist { store: Arc::new(store), server_id: manifest.server_id, generations };
    Ok((persist, Restored { sessions: manifest.sessions, blocks }))
}

pub(crate) fn empty_checkpoint(size: TermSize) -> anyhow::Result<Checkpoint> {
    let grid = GridSize { columns: size.columns.clamp(1, 512), rows: size.rows.clamp(1, 256), cell_width: 8, cell_height: 16 };
    let mut vt = TerminalCore::new(grid).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Checkpoint {
        snapshot: vt.snapshot().map_err(|e| anyhow::anyhow!("{e}"))?,
        size,
        config: TerminalConfig::default(),
        epoch: 0,
        offset: 0,
        lifecycle: Lifecycle::Restored,
        exit_code: None,
        osc_title: String::new(),
        osc_pwd: None,
    })
}

impl Persist {
    /// Open `<home>/state` for a server that took its tree over live:
    /// nothing is restored, but committed generations carry on.
    ///
    /// # Errors
    /// The key or store cannot be opened, or the manifest is unreadable.
    pub fn attach(home: &Path, server_id: String) -> anyhow::Result<Self> {
        let dir = state_dir(home);
        let key = HostKey::load_or_create(&dir).with_context(|| format!("host key in {}", dir.display()))?;
        let store = Store::open(&dir, key).with_context(|| format!("open {}", dir.display()))?;
        let report = store.load::<Manifest>().with_context(|| format!("load {}", store.manifest_path().display()))?;
        let generations = report
            .manifest
            .map(|m| m.blocks.into_iter().filter_map(|b| Some((b.info.id, (b.generation?, None)))).collect())
            .unwrap_or_default();
        Ok(Self { store: Arc::new(store), server_id, generations })
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// Checkpoint changed blocks and commit them with the current tree.
    /// Dirty blocks are only taken when `blocks_due` or the tree changed;
    /// `full` also checkpoints every running block. Skipped when nothing
    /// changed.
    async fn commit(&mut self, server: &Server, blocks_due: bool, full: bool) {
        let (tree_dirty, sessions, infos, targets) = {
            let mut reg = server.registry.lock().unwrap();
            let (dirty, tree_dirty) = reg.take_dirty(blocks_due || full);
            let sessions = reg.sessions.clone();
            let ids: Vec<BlockId> = sessions.iter().flat_map(Session::blocks).collect();
            let mut targets = Vec::new();
            for id in &ids {
                let Some(handle) = reg.blocks.get(id) else { continue };
                let running = handle.info.lock().is_ok_and(|i| i.lifecycle.is_running());
                if dirty.contains(id) || !self.generations.contains_key(id) || (full && running) {
                    targets.push((id.clone(), BlockHandle { tx: handle.tx.clone(), info: handle.info.clone() }));
                }
            }
            let infos: Vec<BlockInfo> = ids.iter().filter_map(|id| reg.block_info(id)).collect();
            (tree_dirty, sessions, infos, targets)
        };
        let mut writes = Vec::new();
        // Blocks to retry on the next commit.
        let mut failed = Vec::new();
        let mut next = self.generations.clone();
        for (id, handle) in targets {
            let cp = match tokio::time::timeout(CHECKPOINT_TIMEOUT, handle.checkpoint()).await {
                Ok(Ok(cp)) => cp,
                Ok(Err(error)) => {
                    tracing::warn!(block = %id, "checkpoint failed: {error:#}");
                    failed.push(id);
                    continue;
                }
                Err(_) => {
                    tracing::warn!(block = %id, "checkpoint timed out");
                    failed.push(id);
                    continue;
                }
            };
            let stamp = Stamp::of(&cp);
            let prev = self.generations.get(&id).copied();
            if prev.is_some_and(|(_, s)| s == Some(stamp)) {
                continue;
            }
            let generation = prev.map_or(1, |(g, _)| g + 1);
            match encode(cp) {
                Ok(bytes) => {
                    next.insert(id.clone(), (generation, Some(stamp)));
                    writes.push((id, generation, bytes));
                }
                Err(error) => {
                    tracing::warn!(block = %id, "encode checkpoint: {error:#}");
                    failed.push(id);
                }
            }
        }
        if !failed.is_empty() {
            server.registry.lock().unwrap().restore_dirty(failed, false);
        }
        let written: Vec<BlockId> = writes.iter().map(|(id, _, _)| id.clone()).collect();
        let live: HashSet<&str> = infos.iter().map(|i| i.id.as_str()).collect();
        let pruned = next.len();
        next.retain(|id, _| live.contains(id.as_str()));
        if !tree_dirty && writes.is_empty() && pruned == next.len() {
            return;
        }
        let manifest = Manifest {
            server_id: self.server_id.clone(),
            sessions,
            blocks: infos.into_iter().map(|info| BlockRecord { generation: next.get(&info.id).map(|g| g.0), info }).collect(),
        };
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let refs: Vec<(&str, u64)> =
                manifest.blocks.iter().filter_map(|b| b.generation.map(|g| (b.info.id.as_str(), g))).collect();
            let snaps: Vec<(&str, u64, &[u8])> = writes.iter().map(|(i, g, b)| (i.as_str(), *g, b.as_slice())).collect();
            store.commit(&snaps, &refs, &manifest)?;
            store.gc(refs.iter().copied())?;
            Ok(())
        })
        .await;
        match result {
            Ok(Ok(())) => self.generations = next,
            Ok(Err(error)) => {
                tracing::error!("commit state: {error:#}");
                server.registry.lock().unwrap().restore_dirty(written, tree_dirty);
            }
            Err(error) => {
                tracing::error!("commit state: {error}");
                server.registry.lock().unwrap().restore_dirty(written, tree_dirty);
            }
        }
    }
}

/// Background committer; dropping it (a crash) aborts without a final commit.
pub struct Worker {
    stop: Arc<Notify>,
    task: Option<tokio::task::JoinHandle<Persist>>,
}

impl Worker {
    pub fn spawn(persist: Persist, server: Arc<Server>, kick: Arc<Notify>, interval: Duration) -> Self {
        let stop = Arc::new(Notify::new());
        let task = tokio::spawn(run(persist, server, kick, stop.clone(), interval));
        Self { stop, task: Some(task) }
    }

    /// Commit everything once more and stop, returning the store so a
    /// worker can be spawned again (an aborted upgrade).
    pub async fn finish(mut self) -> Option<Persist> {
        self.stop.notify_one();
        self.task.take()?.await.ok()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn run(mut persist: Persist, server: Arc<Server>, kick: Arc<Notify>, stop: Arc<Notify>, interval: Duration) -> Persist {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Block-only changes are written at most once per interval.
    let mut last_blocks = tokio::time::Instant::now();
    loop {
        let blocks_due = tokio::select! {
            biased;
            () = stop.notified() => break,
            _ = tick.tick() => true,
            () = kick.notified() => {
                tokio::time::sleep(DEBOUNCE).await;
                last_blocks.elapsed() >= interval
            }
        };
        if blocks_due {
            last_blocks = tokio::time::Instant::now();
        }
        persist.commit(&server, blocks_due, false).await;
    }
    persist.commit(&server, true, true).await;
    persist
}
