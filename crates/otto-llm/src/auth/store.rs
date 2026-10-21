use super::{Credential, auth_error};
use crate::LlmError;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct CredentialStore {
    dir: Arc<PathBuf>,
    flights: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}
pub(super) struct StoreLock(File);
impl Drop for StoreLock {
    fn drop(&mut self) {
        // The descriptor remains owned until after the lock is released.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl CredentialStore {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, LlmError> {
        let dir = dir.as_ref();
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(io_error)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
        let store = Self { dir: Arc::new(dir.to_owned()), flights: Arc::default() };
        let _lock = store.lock()?;
        if !dir.join("auth.json").exists() {
            store.write_locked(&BTreeMap::new())?;
        }
        fs::set_permissions(dir.join("auth.json"), fs::Permissions::from_mode(0o600)).map_err(io_error)?;
        Ok(store)
    }
    pub fn get(&self, provider: &str) -> Result<Option<Credential>, LlmError> {
        Ok(self.list()?.remove(provider))
    }
    pub fn list(&self) -> Result<BTreeMap<String, Credential>, LlmError> {
        let bytes = fs::read(self.dir.join("auth.json")).map_err(io_error)?;
        serde_json::from_slice(&bytes).map_err(|_| auth_error("invalid credential store JSON"))
    }
    pub fn set(&self, provider: &str, credential: Credential) -> Result<(), LlmError> {
        let _lock = self.lock()?;
        self.set_locked(provider, credential)
    }
    pub fn remove(&self, provider: &str) -> Result<(), LlmError> {
        let _lock = self.lock()?;
        let mut credentials = self.list()?;
        credentials.remove(provider);
        self.write_locked(&credentials)
    }
    pub(super) fn set_locked(&self, provider: &str, credential: Credential) -> Result<(), LlmError> {
        let mut credentials = self.list()?;
        credentials.insert(provider.into(), credential);
        self.write_locked(&credentials)
    }
    pub(super) fn lock(&self) -> Result<StoreLock, LlmError> {
        let file =
            OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(self.dir.join("auth.lock")).map_err(io_error)?;
        loop {
            // flock uses a stable sidecar inode, not the atomically replaced JSON file.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(StoreLock(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(io_error(error));
            }
        }
    }
    pub(super) fn provider_mutex(&self, provider: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.flights.lock().unwrap_or_else(|poison| poison.into_inner()).entry(provider.into()).or_default().clone()
    }
    fn write_locked(&self, credentials: &BTreeMap<String, Credential>) -> Result<(), LlmError> {
        let path = self.dir.join(format!(".auth-{:016x}.tmp", rand::random::<u64>()));
        let result = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(io_error)?;
            let bytes = serde_json::to_vec(credentials).map_err(|_| auth_error("cannot serialize credentials"))?;
            file.write_all(&bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            fs::rename(&path, self.dir.join("auth.json")).map_err(io_error)?;
            File::open(self.dir.as_ref()).and_then(|dir| dir.sync_all()).map_err(io_error)
        })();
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result
    }
}
fn io_error(error: std::io::Error) -> LlmError {
    auth_error(format!("credential store: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permissions_and_persistence() {
        let dir = std::env::temp_dir().join(format!("otto-store-{}", rand::random::<u64>()));
        let store = CredentialStore::open(&dir).unwrap();
        store.set("a", Credential::ApiKey { key: "sensitive".into() }).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(dir.join("auth.json")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(CredentialStore::open(&dir).unwrap().get("a").unwrap(), store.get("a").unwrap());
        fs::remove_dir_all(dir).unwrap();
    }
}
