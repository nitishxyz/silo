use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{KeyInit, OsRng};

use crate::error::{Error, Result};
use crate::fsio::sync_dir;

pub const KEY_FILE: &str = "key";
pub const KEY_LEN: usize = 32;

/// 32-byte host-local symmetric key.
#[derive(Clone, PartialEq, Eq)]
pub struct HostKey([u8; KEY_LEN]);

impl std::fmt::Debug for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostKey(<redacted>)")
    }
}

fn insecure(path: &Path, reason: impl Into<String>) -> Error {
    Error::InsecureKey {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

fn check_owner_mode(path: &Path, meta: &fs::Metadata) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(insecure(
            path,
            format!("owned by uid {}, expected {euid}", meta.uid()),
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(insecure(
            path,
            format!("permissions {mode:o} allow group/other access"),
        ));
    }
    Ok(())
}

impl HostKey {
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        HostKey(bytes)
    }

    pub fn generate() -> Self {
        let k = XChaCha20Poly1305::generate_key(&mut OsRng);
        let mut out = [0u8; KEY_LEN];
        out.copy_from_slice(&k);
        HostKey(out)
    }

    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Load `dir/key`, creating it (and `dir`, mode 0700) if absent.
    pub fn load_or_create(dir: impl AsRef<Path>) -> Result<HostKey> {
        let dir = dir.as_ref();
        ensure_private_dir(dir)?;
        let path = dir.join(KEY_FILE);
        for _ in 0..2 {
            match fs::symlink_metadata(&path) {
                Ok(_) => return load_existing(&path),
                Err(e) if e.kind() == ErrorKind::NotFound => match create_new(dir, &path) {
                    Ok(k) => return Ok(k),
                    // Lost a creation race: load the winner's key.
                    Err(Error::Io(e)) if e.kind() == ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                },
                Err(e) => return Err(e.into()),
            }
        }
        load_existing(&path)
    }
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(insecure(dir, "key directory is a symlink"));
            }
            if !meta.is_dir() {
                return Err(insecure(dir, "key directory is not a directory"));
            }
            check_owner_mode(dir, &meta)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            // Enforce 0700 regardless of umask.
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn load_existing(path: &PathBuf) -> Result<HostKey> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(insecure(path, "key file is a symlink"));
    }
    if !meta.is_file() {
        return Err(insecure(path, "key file is not a regular file"));
    }
    check_owner_mode(path, &meta)?;
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut buf = Vec::with_capacity(KEY_LEN + 1);
    (&mut f).take(KEY_LEN as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() != KEY_LEN {
        return Err(insecure(
            path,
            format!("wrong length {} (expected {KEY_LEN})", buf.len()),
        ));
    }
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&buf);
    Ok(HostKey(out))
}

fn create_new(dir: &Path, path: &Path) -> Result<HostKey> {
    let mut f: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let key = HostKey::generate();
    let res = (|| -> std::io::Result<()> {
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(&key.0)?;
        f.sync_all()?;
        sync_dir(dir)
    })();
    if let Err(e) = res {
        // Only remove the file we just created exclusively.
        drop(f);
        let _ = fs::remove_file(path);
        return Err(e.into());
    }
    Ok(key)
}
