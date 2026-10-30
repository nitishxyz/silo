//! Git in the Files block (`files.git`, `files.diff`; host feature
//! `files_git`): marks in the tree, folders taking their strongest
//! descendant's, ignored paths dimmed; and the shown tab's diff against
//! `HEAD` (drawn in `diff.rs`).

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use gpui::{AppContext as _, Context};
use silo_proto::{
    changes::FileStatus,
    compat::feature,
    control::{FileDiff, FilesDiffParams, FilesGit, FilesGitParams, Method},
};

use super::{FilesPane, describe, model};

/// A path's mark, weakest first: a folder shows its strongest descendant's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mark {
    Ignored,
    Untracked,
    Added,
    Renamed,
    Modified,
    Conflicted,
}

impl Mark {
    fn of(status: FileStatus) -> Self {
        match status {
            FileStatus::Untracked => Self::Untracked,
            FileStatus::Added | FileStatus::Copied => Self::Added,
            FileStatus::Renamed => Self::Renamed,
            FileStatus::Conflicted => Self::Conflicted,
            _ => Self::Modified,
        }
    }

    /// The letter after a file's name.
    #[must_use]
    pub fn letter(self) -> &'static str {
        match self {
            Self::Ignored => "",
            Self::Untracked => "U",
            Self::Added => "A",
            Self::Renamed => "R",
            Self::Modified => "M",
            Self::Conflicted => "!",
        }
    }
}

/// `files.git`'s answer, by path.
#[derive(Clone, Debug, Default)]
pub struct Marks {
    files: HashMap<String, Mark>,
    dirs: HashMap<String, Mark>,
    ignored: HashSet<String>,
}

impl Marks {
    #[must_use]
    pub fn new(git: &FilesGit) -> Self {
        let mut marks = Self { ignored: git.ignored.iter().cloned().collect(), ..Self::default() };
        for file in &git.files {
            let mark = Mark::of(file.status);
            marks.files.insert(file.path.clone(), mark);
            let mut dir = model::parent(&file.path);
            while !dir.is_empty() {
                let strongest = marks.dirs.entry(dir.to_owned()).or_insert(mark);
                *strongest = (*strongest).max(mark);
                dir = model::parent(dir);
            }
        }
        marks
    }

    /// `path`'s mark: its own (a folder's: its strongest descendant's), or
    /// ignored when it or a folder above it is.
    #[must_use]
    pub fn of(&self, path: &str, dir: bool) -> Option<Mark> {
        let own = if dir { self.dirs.get(path) } else { self.files.get(path) };
        if let Some(mark) = own {
            return Some(*mark);
        }
        let mut at = path;
        while !at.is_empty() {
            if self.ignored.contains(at) {
                return Some(Mark::Ignored);
            }
            at = model::parent(at);
        }
        None
    }
}

/// How long changes settle before git is asked again.
const SETTLE: Duration = Duration::from_millis(400);

impl FilesPane {
    /// The host answers `files.git` and `files.diff`.
    pub(crate) fn has_git(&self) -> bool {
        self.control.as_ref().is_some_and(|c| c.peer_supports(feature::FILES_GIT))
    }

    /// Ask git for the marks again once changes settle.
    pub(super) fn refresh_git(&mut self, cx: &mut Context<Self>) {
        if !self.has_git() || self.state.root.is_empty() {
            return;
        }
        self.git_refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            let _ = this.update(cx, |this, cx| {
                let root = this.state.root.clone();
                let method = Method::FilesGit(FilesGitParams { block_id: this.block_id.clone() });
                this.request(method, cx, move |this, result: anyhow::Result<FilesGit>, cx| {
                    if this.state.root == root {
                        this.git = result.map(|g| Marks::new(&g)).unwrap_or_default();
                        cx.notify();
                    }
                });
                let shown: Vec<String> = this.diffs.keys().cloned().collect();
                for path in shown {
                    this.load_diff(path, cx);
                }
            });
        }));
    }

    /// Read `path`'s diff against `HEAD`.
    pub(super) fn load_diff(&mut self, path: String, cx: &mut Context<Self>) {
        let method = Method::FilesDiff(FilesDiffParams { block_id: self.block_id.clone(), path: path.clone(), context: None });
        self.request(method, cx, move |this, result: anyhow::Result<FileDiff>, cx| {
            let Some(view) = this.diffs.get_mut(&path) else { return };
            let diff = result.as_ref().ok().cloned();
            let job = view.set(result.map_err(|e| describe(&e)));
            cx.notify();
            let (Some(job), Some(diff)) = (job, diff) else { return };
            // Syntax colours off the UI thread, then into the view if its diff is still this one.
            let task = cx.background_spawn(async move { job.run() });
            cx.spawn(async move |this, cx| {
                let colours = task.await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(view) = this.diffs.get_mut(&path) {
                        view.coloured(&diff, colours.into_shared());
                        cx.notify();
                    }
                });
            })
            .detach();
        });
    }

    /// Show the shown tab's diff, or its text again.
    pub(crate) fn toggle_diff(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.state.active.clone() else { return };
        if self.diffs.remove(&path).is_none() {
            self.diffs.insert(path.clone(), super::diff::DiffView::default());
            self.load_diff(path, cx);
        }
        cx.emit(super::FilesPaneEvent::Header);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::files::FileGitMark;

    use super::*;

    fn git(files: &[(&str, FileStatus)], ignored: &[&str]) -> FilesGit {
        FilesGit {
            repo: true,
            files: files.iter().map(|(p, s)| FileGitMark { path: (*p).into(), status: *s }).collect(),
            ignored: ignored.iter().map(|p| (*p).into()).collect(),
            truncated: false,
        }
    }

    #[test]
    fn folders_take_their_strongest_descendant() {
        let marks = Marks::new(&git(
            &[
                ("src/a.rs", FileStatus::Untracked),
                ("src/files/b.rs", FileStatus::Modified),
                ("src/c.rs", FileStatus::Added),
                ("docs/x.md", FileStatus::Copied),
            ],
            &[],
        ));
        assert_eq!(marks.of("src/a.rs", false), Some(Mark::Untracked));
        assert_eq!(marks.of("src/files", true), Some(Mark::Modified));
        assert_eq!(marks.of("src", true), Some(Mark::Modified), "modified beats added and untracked");
        assert_eq!(marks.of("docs", true), Some(Mark::Added));
        assert_eq!(marks.of("src/d.rs", false), None);
        assert_eq!(marks.of("src", false), None, "a folder's mark is not a file's");
        let conflict = Marks::new(&git(&[("a/b.rs", FileStatus::Modified), ("a/c.rs", FileStatus::Conflicted)], &[]));
        assert_eq!(conflict.of("a", true), Some(Mark::Conflicted));
    }

    #[test]
    fn ignored_folders_cover_what_is_in_them() {
        let marks = Marks::new(&git(&[("src/a.rs", FileStatus::Modified)], &["target", "debug.log", "web/node_modules"]));
        assert_eq!(marks.of("target", true), Some(Mark::Ignored));
        assert_eq!(marks.of("target/debug/x", false), Some(Mark::Ignored));
        assert_eq!(marks.of("debug.log", false), Some(Mark::Ignored));
        assert_eq!(marks.of("web/node_modules/p/i.js", false), Some(Mark::Ignored));
        assert_eq!(marks.of("web", true), None, "a folder holding an ignored one isn't");
        assert_eq!(marks.of("src", true), Some(Mark::Modified));
        assert_eq!(Mark::Modified.letter(), "M");
    }
}
