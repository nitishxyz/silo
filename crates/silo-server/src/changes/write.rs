//! The writes of changes blocks: stage, unstage and revert a file or some
//! of its hunks, and commit. Hunks are named by the ids `changes.diff`
//! gave; each write looks them up in the diff it applies (stage: the work
//! tree against the index, unstage: the index against `HEAD`, revert: the
//! work tree against the index, or against the scope's base), so a hunk
//! that isn't there any more is `conflict` rather than a wrong patch.

use std::path::Path;

use silo_proto::{changes::ChangesCommitted, control::RpcError, model::ChangesScope};

use super::{
    diff::{self, Parsed},
    git::{COMMIT_TIMEOUT, Git},
    status::diff_args,
};

fn unborn(root: &Path) -> Result<bool, RpcError> {
    Ok(Git::read(root, ["rev-parse", "--verify", "-q", "HEAD"]).output()?.code != Some(0))
}

/// Apply `patch` with `git apply` and `flags` (`--cached`, `--reverse`).
fn apply(root: &Path, patch: Vec<u8>, flags: &[&str]) -> Result<(), RpcError> {
    let mut argv = vec!["apply", "--whitespace=nowarn", "--recount"];
    argv.extend_from_slice(flags);
    argv.push("-");
    let out = Git::write(root, argv).stdin(patch).output()?;
    if out.code == Some(0) {
        return Ok(());
    }
    Err(RpcError::new(RpcError::CONFLICT, format!("The file changed; review it again ({})", out.said())))
}

fn no_hunks(hunks: &[String]) -> Result<(), RpcError> {
    if hunks.is_empty() { Err(RpcError::bad_request("no hunks named")) } else { Ok(()) }
}

/// The work tree against the index for `rel` (untracked: against nothing).
fn unstaged(root: &Path, rel: &str, context: u32) -> Result<(Parsed, bool), RpcError> {
    if diff::is_untracked(root, rel)? {
        return Ok((diff::untracked(root, rel, context)?, true));
    }
    Ok((diff::tracked(root, &[], rel, None, context)?, false))
}

/// `changes.stage`.
///
/// # Errors
/// `conflict` for a hunk not in the file's unstaged changes; `git` when
/// git refuses.
pub fn stage(root: &Path, rel: &str, hunks: Option<&[String]>, context: u32) -> Result<(), RpcError> {
    let Some(hunks) = hunks else {
        Git::write(root, ["add", "-A", "--", rel]).run()?;
        return Ok(());
    };
    no_hunks(hunks)?;
    let (parsed, untracked) = unstaged(root, rel, context)?;
    let patch = parsed.patch(hunks)?;
    if untracked {
        // A new file is one hunk: staged whole.
        Git::write(root, ["add", "--", rel]).run()?;
        return Ok(());
    }
    apply(root, patch, &["--cached"])
}

/// `changes.unstage`.
///
/// # Errors
/// `conflict` for a hunk not in the file's staged changes; `git` when git
/// refuses.
pub fn unstage(root: &Path, rel: &str, hunks: Option<&[String]>, context: u32) -> Result<(), RpcError> {
    let unborn = unborn(root)?;
    let Some(hunks) = hunks else {
        if unborn {
            Git::write(root, ["rm", "--cached", "-q", "-r", "--ignore-unmatch", "--", rel]).run()?;
        } else {
            Git::write(root, ["restore", "--staged", "--", rel]).run()?;
        }
        return Ok(());
    };
    no_hunks(hunks)?;
    let parsed = diff::tracked(root, &["--cached".to_owned()], rel, None, context)?;
    let patch = parsed.patch(hunks)?;
    apply(root, patch, &["--cached", "--reverse"])
}

/// `changes.revert`: discard work tree changes; staged ones stay.
///
/// # Errors
/// `bad_request` in the staged scope; `conflict` for a hunk not in the
/// file's work tree changes; `git` when git refuses.
pub fn revert(root: &Path, scope: &ChangesScope, rel: &str, hunks: Option<&[String]>, context: u32) -> Result<(), RpcError> {
    if matches!(scope, ChangesScope::Staged) {
        return Err(RpcError::bad_request("Staged changes are not reverted; unstage them first"));
    }
    // Against a base other than the index (a turn, a branch): back to it.
    let base = match scope {
        ChangesScope::Tree { base, head: None, .. } => Some(base.clone()),
        ChangesScope::Range { base, head: None } => {
            let out = Git::read(root, ["merge-base", base.as_str(), "HEAD"]).run()?;
            Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        }
        _ => None,
    };
    let Some(hunks) = hunks else {
        if diff::is_untracked(root, rel)? {
            return trash(&root.join(rel));
        }
        match &base {
            Some(base) if Git::read(root, ["cat-file", "-e", &format!("{base}:{rel}")]).output()?.code == Some(0) => {
                Git::write(root, ["restore", &format!("--source={base}"), "--worktree", "--", rel]).run()?;
            }
            // New since the base: it goes.
            Some(_) if root.join(rel).symlink_metadata().is_ok() => return trash(&root.join(rel)),
            _ => drop(Git::write(root, ["restore", "--worktree", "--", rel]).run()?),
        }
        return Ok(());
    };
    no_hunks(hunks)?;
    let (parsed, untracked) = unstaged(root, rel, context)?;
    match parsed.patch(hunks) {
        Ok(_) if untracked => trash(&root.join(rel)),
        Ok(patch) => apply(root, patch, &["--reverse"]),
        Err(missing) => {
            let Some(_) = base else { return Err(missing) };
            let parsed = diff::tracked(root, &diff_args(root, scope)?, rel, None, context)?;
            apply(root, parsed.patch(hunks)?, &["--reverse"])
        }
    }
}

/// Move `path` to the Trash (macOS); delete it elsewhere.
fn trash(path: &Path) -> Result<(), RpcError> {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSFileManager, NSString, NSURL};
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, None)
            .map_err(|e| RpcError::new(RpcError::GIT, format!("could not move {} to the Trash: {}", path.display(), e.localizedDescription())))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let meta = path.symlink_metadata().map_err(|e| RpcError::not_found(format!("{} ({e})", path.display())))?;
        let removed = if meta.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
        removed.map_err(|e| RpcError::internal(format!("remove {}: {e}", path.display())))
    }
}

/// `changes.commit`: `git commit` with hooks; its output on failure is the
/// error's message.
///
/// # Errors
/// `bad_request` for an empty message; `git` when the commit fails.
pub fn commit(root: &Path, message: &str, amend: bool) -> Result<ChangesCommitted, RpcError> {
    if message.trim().is_empty() {
        return Err(RpcError::bad_request("the commit message is empty"));
    }
    let mut argv = vec!["commit", "-q", "-F", "-"];
    if amend {
        argv.push("--amend");
    }
    Git::write(root, argv).stdin(message.as_bytes().to_vec()).timeout(COMMIT_TIMEOUT).run()?;
    let out = Git::read(root, ["log", "-1", "--format=%H%x00%s"]).run()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (sha, summary) = text.trim_end_matches('\n').split_once('\0').unwrap_or((text.trim(), ""));
    Ok(ChangesCommitted { sha: sha.to_owned(), summary: summary.to_owned() })
}
