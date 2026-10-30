//! `files.git` and `files.diff`: git's view of a Files block's root, read
//! with the Changes block's status and diff code (`crate::changes`) in the
//! repo holding the root, with paths made relative to the root.

use std::path::Path;

use silo_proto::{
    changes::{FileDiff, FileStatus},
    control::RpcError,
    files::{FileGitMark, FilesGit},
    model::ChangesScope,
};

use super::paths::{canonical_root, normalize};
use crate::changes::{self, git::Git, status};

/// The repo holding the root, and the root's path in it (`sub/dir/`, empty
/// at the top); `None` outside a work tree.
fn repo_of(root: &Path) -> Result<Option<(std::path::PathBuf, String)>, RpcError> {
    let canonical = canonical_root(root)?;
    let repo = changes::find_root(&canonical);
    if !repo.join(".git").exists() {
        return Ok(None);
    }
    let prefix = canonical.strip_prefix(&repo).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(Some((repo, if prefix.is_empty() { prefix } else { format!("{prefix}/") })))
}

/// The changed, untracked and ignored paths under the root.
pub fn git(root: &Path) -> Result<FilesGit, RpcError> {
    let Some((repo, prefix)) = repo_of(root)? else { return Ok(FilesGit::default()) };
    let Some(porcelain) = status::porcelain(&repo, true)? else { return Ok(FilesGit::default()) };
    let args = status::diff_args(&repo, &ChangesScope::Working)?;
    let spec: Vec<&str> = if prefix.is_empty() { Vec::new() } else { vec![prefix.as_str()] };
    let under = |path: &str| path.strip_prefix(prefix.as_str()).filter(|p| !p.is_empty()).map(str::to_owned);
    let mut files: Vec<FileGitMark> = status::names(&repo, &args, &spec)?
        .into_iter()
        .filter_map(|n| {
            let conflicted = porcelain.entries.get(&n.path).is_some_and(|e| e.conflicted);
            Some(FileGitMark { path: under(&n.path)?, status: if conflicted { FileStatus::Conflicted } else { n.status } })
        })
        .collect();
    files.extend(porcelain.untracked.iter().filter_map(|p| Some(FileGitMark { path: under(p)?, status: FileStatus::Untracked })));
    let mut argv = vec!["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z", "--"];
    argv.extend(spec.iter().copied());
    let out = Git::read(&repo, &argv).run()?;
    let ignored: Vec<String> = changes::git::fields(&out.stdout).iter().filter_map(|p| under(p.trim_end_matches('/'))).collect();
    let truncated = files.len() + ignored.len() > FilesGit::MAX_PATHS;
    files.truncate(FilesGit::MAX_PATHS);
    let mut ignored = ignored;
    ignored.truncate(FilesGit::MAX_PATHS - files.len());
    Ok(FilesGit { repo: true, files, ignored, truncated })
}

/// `rel` (under the root) against `HEAD`, as the Changes block diffs it.
pub fn diff(root: &Path, rel: &str, context: Option<u32>) -> Result<FileDiff, RpcError> {
    let rel = normalize(rel)?;
    let Some((repo, prefix)) = repo_of(root)? else { return Err(RpcError::new(RpcError::GIT, "not in a git repository")) };
    let in_repo = changes::check_path(&repo, &format!("{prefix}{rel}"))?;
    let (mut diff, _) = changes::diff::diff(&repo, &ChangesScope::Working, &in_repo, context.unwrap_or(3))?;
    diff.path = rel;
    if let Some(old) = diff.old_path.take() {
        diff.old_path = Some(old.strip_prefix(prefix.as_str()).unwrap_or(&old).to_owned());
    }
    Ok(diff)
}
