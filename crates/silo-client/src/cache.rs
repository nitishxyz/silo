//! Per-server encrypted client cache: the last session tree and per-block
//! terminal checkpoints, so the UI can paint instantly (and offline) and
//! then resume each block from where the checkpoint left off.
//!
//! ```text
//! <cache root>/key                                  client-local key
//! <cache root>/servers/<sha256(identity)>/...       silo-store layout
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use silo_proto::{
    data::{CommandAnchor, Resume, ServerData, StartMode, Welcome},
    model::{TermSize, TerminalConfig},
};
use silo_store::{HostKey, Store};

use crate::mirror::Mirror;

pub const SERVERS_DIR: &str = "servers";

/// A coherent mirror state: the encoded terminal snapshot plus the stream
/// point and configuration it corresponds to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorCheckpoint {
    pub snapshot: Vec<u8>,
    pub resume: Resume,
    pub size: TermSize,
    pub config: TerminalConfig,
    /// The finished commands' rows the mirror tracked.
    pub commands: Vec<CommandAnchor>,
}

impl MirrorCheckpoint {
    /// Capture `mirror`; `None` unless it is coherent and configured.
    ///
    /// # Errors
    /// The terminal snapshot could not be encoded.
    pub fn capture(mirror: &mut Mirror) -> anyhow::Result<Option<Self>> {
        let (Some(resume), Some(config)) = (mirror.checkpointable(), mirror.config()) else {
            return Ok(None);
        };
        let size = mirror.size();
        let commands = mirror.command_anchors();
        let vt = mirror.terminal().ok_or_else(|| anyhow!("coherent mirror without terminal"))?;
        let snapshot = vt.snapshot().map_err(|e| anyhow!("{e}"))?;
        Ok(Some(Self { snapshot, resume, size, config, commands }))
    }

    /// Rebuild a coherent mirror (screen, history and configuration) that
    /// resumes at [`resume`](Self::resume).
    ///
    /// # Errors
    /// Malformed snapshot data.
    pub fn restore(&self) -> anyhow::Result<Mirror> {
        let mut mirror = Mirror::new();
        let welcome = Welcome { epoch: self.resume.epoch, mode: StartMode::Snapshot, offset: self.resume.offset, size: self.size };
        mirror.begin(&welcome)?;
        mirror.apply(ServerData::Configured(self.config))?;
        mirror.apply(ServerData::SnapshotHead(self.snapshot.clone()))?;
        mirror.apply(ServerData::SnapshotDone)?;
        mirror.restore_command_anchors(&self.commands);
        if mirror.checkpointable() != Some(self.resume) {
            anyhow::bail!("restored mirror is not coherent at {:?}", self.resume);
        }
        Ok(mirror)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BlockMeta {
    generation: u64,
    resume: Resume,
    size: TermSize,
    config: TerminalConfig,
    #[serde(default)]
    commands: Vec<CommandAnchor>,
}

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    tree: Option<serde_json::Value>,
    blocks: BTreeMap<String, BlockMeta>,
}

#[derive(Debug)]
pub struct CachedBlock {
    pub id: String,
    pub checkpoint: MirrorCheckpoint,
}

#[derive(Debug)]
pub struct BlockError {
    pub id: String,
    pub error: anyhow::Error,
}

#[derive(Debug)]
pub struct Cached<T> {
    pub tree: Option<T>,
    pub blocks: Vec<CachedBlock>,
    /// Blocks whose checkpoint could not be read; the rest still load.
    pub errors: Vec<BlockError>,
}

pub struct Cache {
    store: Store,
    manifest: Manifest,
}

impl Cache {
    /// Open (creating if needed) the cache for `server_identity` under
    /// `root`.
    ///
    /// # Errors
    /// Key or directory failures, or an unreadable manifest.
    pub fn open(root: impl AsRef<Path>, server_identity: &str) -> anyhow::Result<Self> {
        let root = root.as_ref();
        let key = HostKey::load_or_create(root).with_context(|| format!("cache key in {}", root.display()))?;
        let store = Store::open(server_dir(root, server_identity), key)?;
        let manifest = store.load::<Manifest>()?.manifest.unwrap_or_default();
        Ok(Self { store, manifest })
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        self.store.root()
    }

    /// Replace the cached session tree.
    ///
    /// # Errors
    /// Serialization or write failure.
    pub fn save_tree<T: Serialize>(&mut self, tree: &T) -> anyhow::Result<()> {
        let previous = self.manifest.tree.replace(serde_json::to_value(tree)?);
        self.commit(&[]).inspect_err(|_| self.manifest.tree = previous)?;
        self.collect()
    }

    /// Drop the cached session tree (its session is gone).
    ///
    /// # Errors
    /// Write failure.
    pub fn clear_tree(&mut self) -> anyhow::Result<()> {
        let Some(previous) = self.manifest.tree.take() else {
            return Ok(());
        };
        self.commit(&[]).inspect_err(|_| self.manifest.tree = Some(previous))
    }

    /// Checkpoint `mirror` as block `id`. Returns `false` (and keeps any
    /// previous checkpoint) when the mirror is not coherent.
    ///
    /// # Errors
    /// Snapshot encoding or write failure.
    pub fn save_block(&mut self, id: &str, mirror: &mut Mirror) -> anyhow::Result<bool> {
        let Some(checkpoint) = MirrorCheckpoint::capture(mirror)? else {
            return Ok(false);
        };
        self.save_checkpoint(id, &checkpoint)?;
        Ok(true)
    }

    /// Store a checkpoint captured earlier with [`MirrorCheckpoint::capture`]
    /// as block `id`, replacing any previous one.
    ///
    /// # Errors
    /// Write failure.
    pub fn save_checkpoint(&mut self, id: &str, checkpoint: &MirrorCheckpoint) -> anyhow::Result<()> {
        let generation = self.manifest.blocks.get(id).map_or(1, |m| m.generation + 1);
        let meta = BlockMeta {
            generation,
            resume: checkpoint.resume,
            size: checkpoint.size,
            config: checkpoint.config,
            commands: checkpoint.commands.clone(),
        };
        let previous = self.manifest.blocks.insert(id.to_owned(), meta);
        self.commit(&[(id, generation, &checkpoint.snapshot)]).inspect_err(|_| self.restore_meta(id, previous))?;
        self.collect()
    }

    /// Drop the checkpoint of block `id`.
    ///
    /// # Errors
    /// Write failure.
    pub fn forget_block(&mut self, id: &str) -> anyhow::Result<()> {
        let Some(previous) = self.manifest.blocks.remove(id) else {
            return Ok(());
        };
        self.commit(&[]).inspect_err(|_| self.restore_meta(id, Some(previous)))?;
        self.collect()
    }

    /// Keep only the checkpoints of `live` blocks and delete stale files.
    ///
    /// # Errors
    /// Write or filesystem failure.
    pub fn gc<'a>(&mut self, live: impl IntoIterator<Item = &'a str>) -> anyhow::Result<()> {
        let live: BTreeSet<&str> = live.into_iter().collect();
        let stale: Vec<String> = self.manifest.blocks.keys().filter(|id| !live.contains(id.as_str())).cloned().collect();
        if !stale.is_empty() {
            let previous = std::mem::take(&mut self.manifest.blocks);
            self.manifest.blocks = previous.iter().filter(|(id, _)| live.contains(id.as_str())).map(|(id, m)| (id.clone(), m.clone())).collect();
            self.commit(&[]).inspect_err(|_| self.manifest.blocks = previous)?;
        }
        self.collect()
    }

    /// Load the cached tree and every block checkpoint.
    ///
    /// # Errors
    /// An unreadable manifest or a tree that does not decode as `T`;
    /// unreadable checkpoints are reported per block in [`Cached::errors`].
    pub fn load<T: DeserializeOwned>(&self) -> anyhow::Result<Cached<T>> {
        let report = self.store.load::<Manifest>()?;
        let manifest = report.manifest.unwrap_or_default();
        let tree = manifest.tree.map(serde_json::from_value).transpose().context("cached tree")?;
        let mut errors: Vec<BlockError> = report
            .errors
            .into_iter()
            .map(|e| BlockError { id: e.id, error: anyhow::Error::new(e.error).context("cached block checkpoint") })
            .collect();
        let mut blocks = Vec::new();
        for snap in report.snapshots {
            match manifest.blocks.get(&snap.id) {
                Some(meta) if meta.generation == snap.generation => blocks.push(CachedBlock {
                    checkpoint: MirrorCheckpoint {
                        snapshot: snap.bytes,
                        resume: meta.resume,
                        size: meta.size,
                        config: meta.config,
                        commands: meta.commands.clone(),
                    },
                    id: snap.id,
                }),
                _ => errors.push(BlockError { error: anyhow!("checkpoint {} gen {} has no metadata", snap.id, snap.generation), id: snap.id }),
            }
        }
        Ok(Cached { tree, blocks, errors })
    }

    fn refs(&self) -> Vec<(&str, u64)> {
        self.manifest.blocks.iter().map(|(id, m)| (id.as_str(), m.generation)).collect()
    }

    fn commit(&self, snapshots: &[(&str, u64, &[u8])]) -> anyhow::Result<()> {
        self.store.commit(snapshots, &self.refs(), &self.manifest)?;
        Ok(())
    }

    /// Delete files no longer referenced. silo-store also keeps the
    /// previous commit's generations, so at most two per block remain.
    fn collect(&self) -> anyhow::Result<()> {
        self.store.gc(self.refs())?;
        Ok(())
    }

    fn restore_meta(&mut self, id: &str, previous: Option<BlockMeta>) {
        match previous {
            Some(meta) => self.manifest.blocks.insert(id.to_owned(), meta),
            None => self.manifest.blocks.remove(id),
        };
    }
}

/// The cache directory for one server identity.
#[must_use]
pub fn server_dir(root: &Path, server_identity: &str) -> PathBuf {
    let digest = Sha256::digest(server_identity.as_bytes());
    let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    root.join(SERVERS_DIR).join(name)
}
