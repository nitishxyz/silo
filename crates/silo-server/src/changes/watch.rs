//! The watcher of a changes block: the work tree (recursively, without
//! `.gitignore`d paths) and the repo's `index`, `HEAD` and refs, so the view
//! follows edits, staging, commits and branch switches. Changes are
//! debounced ([`DEBOUNCE`]) into one [`Note::ChangesChanged`].

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
    time::Duration,
};

use notify::{EventKind, RecursiveMode, Watcher};
use silo_proto::model::BlockId;
use tokio::sync::mpsc::UnboundedSender;

use super::git::Git;
use crate::block::Note;

pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Dropping it stops the watch.
pub struct Watch {
    _stop: Sender<()>,
}

impl Watch {
    /// Watch canonical `root` for block `block_id`.
    ///
    /// # Errors
    /// A thread cannot start.
    pub fn start(block_id: BlockId, root: PathBuf, notes: UnboundedSender<Note>) -> std::io::Result<Self> {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let (changes, changed) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let worker_root = root.clone();
        std::thread::Builder::new().name("silo-changes-watch".into()).spawn(move || work(&worker_root, &stopped, changes))?;
        std::thread::Builder::new().name("silo-changes-debounce".into()).spawn(move || {
            crate::files::watch::debounce(&changed, DEBOUNCE, |_| {
                notes.send(Note::ChangesChanged { block_id: block_id.clone(), root: root.clone() }).is_ok()
            });
        })?;
        Ok(Self { _stop: stop })
    }
}

/// The repo's git directory (canonical), if `root` is in one.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let out = Git::read(root, ["rev-parse", "--absolute-git-dir"]).run().ok()?;
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).canonicalize().ok()
}

/// Whether a change at `rel` inside the git directory is one a review
/// shows: the index, `HEAD`, refs (commits, branch switches), merges.
fn repo_state(rel: &Path) -> bool {
    let name = rel.to_string_lossy();
    if name.ends_with(".lock") {
        return false;
    }
    matches!(name.as_ref(), "index" | "HEAD" | "packed-refs" | "MERGE_HEAD" | "ORIG_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD")
        || name.starts_with("refs/")
}

fn ignores(root: &Path, git_dir: Option<&Path>) -> ignore::gitignore::Gitignore {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let _ = builder.add(root.join(".gitignore"));
    if let Some(dir) = git_dir {
        let _ = builder.add(dir.join("info/exclude"));
    }
    builder.build().unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
}

fn work(root: &Path, stop: &Receiver<()>, changes: Sender<Vec<PathBuf>>) {
    let git = git_dir(root);
    let ignored = ignores(root, git.as_deref());
    let (handler_root, handler_git) = (root.to_owned(), git.clone());
    let handler = move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let paths: Vec<PathBuf> = event
            .paths
            .into_iter()
            .filter(|p| {
                if let Some(rel) = handler_git.as_deref().and_then(|g| p.strip_prefix(g).ok()) {
                    return repo_state(rel);
                }
                p.starts_with(&handler_root) && !ignored.matched_path_or_any_parents(p, p.is_dir()).is_ignore()
            })
            .collect();
        if !paths.is_empty() {
            let _ = changes.send(paths);
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(watcher) => watcher,
        Err(error) => return tracing::warn!(root = %root.display(), "changes watch: {error}"),
    };
    if let Err(error) = watcher.watch(root, RecursiveMode::Recursive) {
        tracing::warn!(root = %root.display(), "changes watch: {error}");
    }
    if let Some(git) = git.filter(|g| !g.starts_with(root))
        && let Err(error) = watcher.watch(&git, RecursiveMode::Recursive)
    {
        tracing::warn!(git = %git.display(), "changes watch: {error}");
    }
    // Until the block's `Watch` is dropped.
    let _ = stop.recv();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_repo_state_in_the_git_dir_counts() {
        for yes in ["index", "HEAD", "refs/heads/main", "packed-refs"] {
            assert!(repo_state(Path::new(yes)), "{yes}");
        }
        for no in ["index.lock", "objects/ab/cdef", "logs/HEAD", "refs/heads/main.lock", "FETCH_HEAD"] {
            assert!(!repo_state(Path::new(no)), "{no}");
        }
    }
}
