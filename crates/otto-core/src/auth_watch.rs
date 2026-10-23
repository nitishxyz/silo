//! Notices sign-ins changed by another process (the CLI, another daemon's refresh) and
//! announces them as a global `auth.changed`, so clients reread `auth.status`.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use otto_proto::Event;

use crate::Runtime;

const POLL: Duration = Duration::from_secs(2);

/// What identifies one version of the auth file: written atomically, so a new inode each time.
type Stamp = Option<(SystemTime, u64, u64)>;

fn stamp(file: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(file).ok()?;
    Some((meta.modified().ok()?, meta.len(), meta.ino()))
}

/// The last version seen of the auth file.
pub(crate) struct AuthWatch {
    file: PathBuf,
    last: Stamp,
}

impl AuthWatch {
    pub(crate) fn new(file: PathBuf) -> Self {
        let last = stamp(&file);
        Self { file, last }
    }

    /// Whether the file changed (written, replaced or removed) since the last check.
    pub(crate) fn changed(&mut self) -> bool {
        let now = stamp(&self.file);
        let changed = now != self.last;
        self.last = now;
        changed
    }
}

impl Runtime {
    /// Poll `file` (the credential store's) and send `auth.changed` when it changes; stops with the runtime.
    pub fn watch_auth(&self, file: PathBuf) {
        let inner = Arc::downgrade(&self.0);
        let mut watch = AuthWatch::new(file);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(inner) = inner.upgrade() else { return };
                if watch.changed() {
                    let _ = inner.global.send(Event::AuthChanged {});
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_writes_replacements_and_removal_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        let mut watch = AuthWatch::new(file.clone());
        assert!(!watch.changed(), "nothing yet");
        std::fs::write(&file, "{}").unwrap();
        assert!(watch.changed(), "created");
        assert!(!watch.changed(), "reported once");
        // Another process signs in: a new file renamed over the old one.
        let tmp = dir.path().join(".auth-1.tmp");
        std::fs::write(&tmp, "{}").unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        assert!(watch.changed(), "replaced, same size");
        std::fs::remove_file(&file).unwrap();
        assert!(watch.changed(), "removed");
        assert!(!watch.changed());
    }
}
