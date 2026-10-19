use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::envelope;
use crate::error::Result;
use crate::key::HostKey;

pub(crate) const TMP_PREFIX: &str = ".tmp-";

pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Atomically replace `path` with `bytes`: private temp file in the same
/// directory (0600), `sync_all`, rename, then fsync the directory.
pub fn write_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = path.as_ref();
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = dir.join(format!(
        "{TMP_PREFIX}{}.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let res = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        sync_dir(dir)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Seal `plaintext` with `aad` and write it atomically to `path`.
pub fn write_sealed(
    path: impl AsRef<Path>,
    key: &HostKey,
    aad: &[u8],
    plaintext: &[u8],
) -> Result<()> {
    write_atomic(path, &envelope::seal(key, aad, plaintext))?;
    Ok(())
}

/// Read `path` and open it with `key` and `aad`.
pub fn read_sealed(path: impl AsRef<Path>, key: &HostKey, aad: &[u8]) -> Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    envelope::open(key, aad, &bytes)
}
