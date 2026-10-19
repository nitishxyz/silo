use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::envelope;
use crate::error::{Error, Result};
use crate::fsio::{TMP_PREFIX, read_sealed, write_atomic, write_sealed};
use crate::key::HostKey;

pub const SNAPSHOTS_DIR: &str = "snapshots";
pub const MANIFEST_FILE: &str = "tree.json.sealed";
const SNAP_EXT: &str = "snap";
const MANIFEST_AAD: &[u8] = b"silo-store manifest";

/// On-disk manifest wrapper (sealed JSON).
#[derive(Serialize, Deserialize)]
struct ManifestFile<T> {
    /// Monotonic commit counter.
    commit: u64,
    /// Snapshot generations referenced by this commit.
    refs: Vec<(String, u64)>,
    /// Snapshot generations referenced by the previous commit (kept by gc).
    prev_refs: Vec<(String, u64)>,
    data: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSnapshot {
    pub id: String,
    pub generation: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct SnapshotError {
    pub id: String,
    pub generation: u64,
    pub error: Error,
}

#[derive(Debug)]
pub struct LoadReport<T> {
    /// `None` when no manifest has been committed yet.
    pub manifest: Option<T>,
    pub commit: u64,
    pub snapshots: Vec<LoadedSnapshot>,
    /// Per-item failures; they do not fail the whole load.
    pub errors: Vec<SnapshotError>,
}

/// Generation-based encrypted store rooted at a directory:
///
/// ```text
/// <root>/tree.json.sealed
/// <root>/snapshots/<id>/<generation>.snap
/// ```
pub struct Store {
    root: PathBuf,
    key: HostKey,
}

fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidId(id.to_string()))
    }
}

fn snapshot_aad(id: &str, generation: u64) -> Vec<u8> {
    format!("silo-store snapshot\0{id}\0{generation}").into_bytes()
}

fn private_dir(dir: &Path) -> Result<()> {
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(Error::Io(std::io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        ))));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

impl Store {
    /// Open (creating if needed) a store at `root` with directories 0700.
    pub fn open(root: impl AsRef<Path>, key: HostKey) -> Result<Store> {
        let root = root.as_ref().to_path_buf();
        private_dir(&root)?;
        private_dir(&root.join(SNAPSHOTS_DIR))?;
        Ok(Store { root, key })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_FILE)
    }

    pub fn snapshot_path(&self, id: &str, generation: u64) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self
            .root
            .join(SNAPSHOTS_DIR)
            .join(id)
            .join(format!("{generation}.{SNAP_EXT}")))
    }

    /// Seal and atomically write one snapshot generation. It only becomes
    /// visible to `load` once a manifest referencing it is committed.
    pub fn write_snapshot(&self, id: &str, generation: u64, bytes: &[u8]) -> Result<()> {
        let path = self.snapshot_path(id, generation)?;
        if let Some(dir) = path.parent() {
            private_dir(dir)?;
        }
        write_sealed(&path, &self.key, &snapshot_aad(id, generation), bytes)
    }

    pub fn read_snapshot(&self, id: &str, generation: u64) -> Result<Vec<u8>> {
        let path = self.snapshot_path(id, generation)?;
        read_sealed(&path, &self.key, &snapshot_aad(id, generation))
    }

    fn read_manifest<T: DeserializeOwned>(&self) -> Result<Option<ManifestFile<T>>> {
        let bytes = match fs::read(self.manifest_path()) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let plain = envelope::open(&self.key, MANIFEST_AAD, &bytes)?;
        Ok(Some(serde_json::from_slice(&plain)?))
    }

    /// Commit: write every snapshot in `snapshots` first, then atomically
    /// replace the manifest. `refs` lists every (id, generation) the new
    /// manifest references (including unchanged ones not rewritten here).
    pub fn commit<T: Serialize>(
        &self,
        snapshots: &[(&str, u64, &[u8])],
        refs: &[(&str, u64)],
        data: &T,
    ) -> Result<u64> {
        for (id, generation, _) in snapshots {
            validate_id(id)?;
            if !refs.iter().any(|(r, g)| r == id && g == generation) {
                return Err(Error::InvalidId(format!(
                    "{id} gen {generation} not in refs"
                )));
            }
        }
        for (id, _) in refs {
            validate_id(id)?;
        }
        for (id, generation, bytes) in snapshots {
            self.write_snapshot(id, *generation, bytes)?;
        }
        let (commit, prev_refs) = match self.read_manifest::<serde_json::Value>()? {
            Some(m) => (m.commit + 1, m.refs),
            None => (1, Vec::new()),
        };
        let file = ManifestFile {
            commit,
            refs: refs.iter().map(|(i, g)| (i.to_string(), *g)).collect(),
            prev_refs,
            data,
        };
        let plain = serde_json::to_vec(&file)?;
        write_atomic(
            self.manifest_path(),
            &envelope::seal(&self.key, MANIFEST_AAD, &plain),
        )?;
        Ok(commit)
    }
}

impl Store {
    /// Load the committed manifest and every snapshot it references.
    /// A missing manifest yields an empty report. An unreadable or corrupt
    /// manifest is an error; unreadable or corrupt snapshots are reported per
    /// item in `errors` without failing the whole load.
    pub fn load<T: DeserializeOwned>(&self) -> Result<LoadReport<T>> {
        let Some(m) = self.read_manifest::<T>()? else {
            return Ok(LoadReport {
                manifest: None,
                commit: 0,
                snapshots: Vec::new(),
                errors: Vec::new(),
            });
        };
        let mut snapshots = Vec::new();
        let mut errors = Vec::new();
        for (id, generation) in m.refs {
            match self.read_snapshot(&id, generation) {
                Ok(bytes) => snapshots.push(LoadedSnapshot {
                    id,
                    generation,
                    bytes,
                }),
                Err(error) => errors.push(SnapshotError {
                    id,
                    generation,
                    error,
                }),
            }
        }
        Ok(LoadReport {
            manifest: Some(m.data),
            commit: m.commit,
            snapshots,
            errors,
        })
    }

    /// Remove snapshot files not in `referenced`, plus stale `write_atomic`
    /// temp files (`.tmp-*`) in the store root, `snapshots/` and every
    /// snapshot dir. The generations referenced by the current and previous
    /// committed manifests are always kept. Must not run concurrently with
    /// writes to this store. Returns the number of files removed.
    pub fn gc<'a>(&self, referenced: impl IntoIterator<Item = (&'a str, u64)>) -> Result<usize> {
        let mut keep: BTreeSet<(String, u64)> = referenced
            .into_iter()
            .map(|(i, g)| (i.to_string(), g))
            .collect();
        match self.read_manifest::<serde_json::Value>() {
            Ok(Some(m)) => keep.extend(m.refs.into_iter().chain(m.prev_refs)),
            Ok(None) => {}
            // Can't tell what the manifest references: delete nothing.
            Err(e) => return Err(e),
        }
        let mut removed = 0;
        removed += remove_tmp_files(&self.root)?;
        let snaps = self.root.join(SNAPSHOTS_DIR);
        removed += remove_tmp_files(&snaps)?;
        for entry in fs::read_dir(&snaps)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if validate_id(&id).is_err() {
                continue;
            }
            let mut remaining = 0;
            for file in fs::read_dir(entry.path())? {
                let file = file?;
                let name = file.file_name().to_string_lossy().into_owned();
                let generation = name
                    .strip_suffix(&format!(".{SNAP_EXT}"))
                    .and_then(|g| g.parse::<u64>().ok());
                let stale = match generation {
                    Some(g) => !keep.contains(&(id.clone(), g)),
                    None => name.starts_with(TMP_PREFIX),
                };
                if stale && file.file_type()?.is_file() {
                    fs::remove_file(file.path())?;
                    removed += 1;
                } else {
                    remaining += 1;
                }
            }
            if remaining == 0 {
                let _ = fs::remove_dir(entry.path());
            }
        }
        Ok(removed)
    }
}

fn remove_tmp_files(dir: &Path) -> Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(TMP_PREFIX)
            && entry.file_type()?.is_file()
        {
            fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}
