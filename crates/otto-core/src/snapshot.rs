//! Work tree snapshots per turn (docs/CHANGES_BLOCK.md §4): at a turn's start and before it finishes, the
//! session's git work tree is written to a tree object through a copy of the index, so the user's index is
//! never touched, and the turn's [`TurnChanges`] come from the diff between the two. Outside git, the turn's
//! edit records (`undo_edit`'s) name the files. A failure is logged and never fails the turn.
use otto_proto::TurnChanges;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

/// Each git command's limit.
const TIMEOUT: Duration = Duration::from_secs(10);
/// No snapshot when the untracked files `git add -A` would add are larger.
pub(crate) const MAX_UNTRACKED: u64 = 50 * 1024 * 1024;

/// A turn's start: its snapshot, or the edit mark the fallback reads from.
#[derive(Debug)]
pub(crate) enum Start {
    /// Snapshots off: nothing is reported.
    Off,
    /// A git work tree; `base` is `None` when its snapshot failed.
    Git { base: Option<String> },
    /// Not in a git work tree (or no git): the session's edits after `mark`.
    Edits { mark: u64 },
}

/// Take the turn's base snapshot in `cwd`.
pub(crate) async fn start(cwd: &Path, enabled: bool) -> Start {
    if !enabled {
        return Start::Off;
    }
    let mark = otto_tools::builtin::undo_edit::mark();
    match repo(cwd).await {
        Some(repo) => Start::Git { base: snapshot(&repo).await },
        None => Start::Edits { mark },
    }
}

/// The turn's end: its end snapshot (`None` when it couldn't be taken) and what changed since `start`.
pub(crate) async fn finish(cwd: &Path, session: &str, start: &Start) -> (Option<String>, Option<TurnChanges>) {
    match start {
        Start::Off => (None, None),
        Start::Git { base } => {
            let Some(repo) = repo(cwd).await else { return (None, None) };
            let end = snapshot(&repo).await;
            let changes = match (base, &end) {
                (Some(base), Some(end)) => diff(&repo.top, base, end).await,
                _ => None,
            };
            (end, changes)
        }
        Start::Edits { mark } => (None, edited(cwd, session, *mark)),
    }
}

/// A git work tree: its top level and its git dir (a worktree's own, where its index is).
#[derive(Debug)]
pub(crate) struct Repo {
    pub top: PathBuf,
    pub git_dir: PathBuf,
}

/// The work tree `cwd` is in; `None` outside one, in a `.git` folder, or without a working git.
pub(crate) async fn repo(cwd: &Path) -> Option<Repo> {
    let out = git(cwd, &["rev-parse", "--is-inside-work-tree", "--show-toplevel", "--absolute-git-dir"], None).await.ok()?;
    let text = String::from_utf8(out).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "true" {
        return None;
    }
    Some(Repo { top: lines.next()?.into(), git_dir: lines.next()?.into() })
}

/// Write `repo`'s work tree (tracked and untracked files, ignored ones left out) to a tree object, through a
/// temporary copy of its index. `None` when untracked files exceed [`MAX_UNTRACKED`] or git fails (logged).
pub(crate) async fn snapshot(repo: &Repo) -> Option<String> {
    match try_snapshot(repo).await {
        Ok(tree) => tree,
        Err(error) => {
            tracing::warn!(top = %repo.top.display(), %error, "turn snapshot failed");
            None
        }
    }
}

async fn try_snapshot(repo: &Repo) -> Result<Option<String>, String> {
    let untracked = untracked_size(&repo.top).await?;
    if untracked > MAX_UNTRACKED {
        tracing::info!(top = %repo.top.display(), bytes = untracked, "turn snapshot skipped: untracked files over 50 MB");
        return Ok(None);
    }
    let index = TempIndex(std::env::temp_dir().join(format!("otto-index-{}", uuid::Uuid::new_v4())));
    match tokio::fs::copy(repo.git_dir.join("index"), &index.0).await {
        Ok(_) => {}
        // A repo with nothing staged yet has no index: git starts one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("copying the index: {e}")),
    }
    git(&repo.top, &["add", "-A", "--", "."], Some(&index.0)).await?;
    let tree = String::from_utf8(git(&repo.top, &["write-tree"], Some(&index.0)).await?).map_err(|e| e.to_string())?;
    let tree = tree.trim();
    if tree.is_empty() || !tree.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("write-tree printed {tree:?}"));
    }
    Ok(Some(tree.to_owned()))
}

/// The temporary index, removed when dropped (with a lock git may have left).
struct TempIndex(PathBuf);
impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// The bytes of the untracked, not ignored files under `top`.
async fn untracked_size(top: &Path) -> Result<u64, String> {
    let out = git(top, &["ls-files", "-z", "--others", "--exclude-standard"], None).await?;
    let mut total = 0;
    for path in out.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = top.join(String::from_utf8_lossy(path).as_ref());
        if let Ok(meta) = tokio::fs::symlink_metadata(&path).await {
            total += meta.len();
            if total > MAX_UNTRACKED {
                break;
            }
        }
    }
    Ok(total)
}

/// What changed from tree `base` to tree `end`; `None` when nothing did.
pub(crate) async fn diff(top: &Path, base: &str, end: &str) -> Option<TurnChanges> {
    if base == end {
        return None;
    }
    let out = match git(top, &["diff", "--numstat", "-z", "--no-color", "--no-ext-diff", "--no-textconv", base, end, "--"], None).await {
        Ok(out) => out,
        Err(error) => {
            tracing::warn!(top = %top.display(), %error, "turn diff failed");
            return None;
        }
    };
    let mut changes = numstat(&out);
    changes.base_tree = Some(base.into());
    changes.end_tree = Some(end.into());
    (changes.file_count() > 0).then_some(changes)
}

/// `git diff --numstat -z` output: `added\tremoved\tpath\0`, or for a rename `added\tremoved\t\0old\0new\0`
/// (`-` counts for a binary file).
pub(crate) fn numstat(out: &[u8]) -> TurnChanges {
    let mut changes = TurnChanges::default();
    let mut fields = out.split(|b| *b == 0);
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        let mut parts = text.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let path = if path.is_empty() {
            let _old = fields.next();
            fields.next().map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or_default()
        } else {
            path.to_owned()
        };
        changes.added += added.parse::<u64>().unwrap_or(0);
        changes.removed += removed.parse::<u64>().unwrap_or(0);
        push(&mut changes, path);
    }
    changes
}

fn push(changes: &mut TurnChanges, path: String) {
    if changes.files.len() < TurnChanges::MAX_FILES {
        changes.files.push(path);
    } else {
        changes.more += 1;
    }
}

/// Outside git: the files `session` edited after `mark`, relative to `cwd` where they are under it.
fn edited(cwd: &Path, session: &str, mark: u64) -> Option<TurnChanges> {
    let mut changes = TurnChanges::default();
    let real = std::fs::canonicalize(cwd).ok();
    for file in otto_tools::builtin::undo_edit::edited_since(session, mark) {
        changes.added += file.added;
        changes.removed += file.removed;
        let path = file.path.strip_prefix(cwd).ok().or_else(|| file.path.strip_prefix(real.as_ref()?).ok()).unwrap_or(&file.path);
        push(&mut changes, path.to_string_lossy().into_owned());
    }
    (changes.file_count() > 0).then_some(changes)
}

/// Run git in `dir` with optional locks off, the index `index` when given, no prompts, within [`TIMEOUT`];
/// its stdout, or why it failed.
async fn git(dir: &Path, args: &[&str], index: Option<&Path>) -> Result<Vec<u8>, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match index {
        Some(index) => command.env("GIT_INDEX_FILE", index),
        None => command.env_remove("GIT_INDEX_FILE"),
    };
    let output = tokio::time::timeout(TIMEOUT, command.output())
        .await
        .map_err(|_| format!("git {} timed out after {} s", args.join(" "), TIMEOUT.as_secs()))?
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_reads_plain_renamed_and_binary_records() {
        let out = b"3\t1\tsrc/a.rs\x002\t0\t\x00old name.txt\x00new name.txt\x00-\t-\timg.png\x00";
        let changes = numstat(out);
        assert_eq!(changes.files, ["src/a.rs", "new name.txt", "img.png"]);
        assert_eq!((changes.added, changes.removed, changes.more), (5, 1, 0));
    }

    #[test]
    fn numstat_lists_fifty_paths_and_counts_every_file() {
        let out: Vec<u8> = (0..60).flat_map(|i| format!("1\t2\tf{i}\0").into_bytes()).collect();
        let changes = numstat(&out);
        assert_eq!((changes.files.len(), changes.more, changes.file_count()), (50, 10, 60));
        assert_eq!((changes.added, changes.removed), (60, 120));
    }
}
