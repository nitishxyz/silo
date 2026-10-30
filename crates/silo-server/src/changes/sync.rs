//! Push, pull and fetch for changes blocks, and the merge, rebase,
//! cherry-pick or revert git stopped in: `changes.continue` and
//! `changes.abort`. Nothing here may wait on a prompt: git's terminal
//! prompts are off, ssh runs in batch mode (unless the user set its
//! command) and Git Credential Manager is non-interactive, so a remote
//! that needs a sign-in fails at once with a message saying so.

use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};

use silo_proto::{changes::ChangesPushed, changes::RepoOp, control::RpcError};

use super::{
    git::{COMMIT_TIMEOUT, Git, NETWORK_TIMEOUT, fields},
    status::porcelain,
    watch::git_dir,
};

/// Where progress lines go (to `changes.progress`).
pub type Progress = tokio::sync::mpsc::UnboundedSender<String>;

/// At most one progress line per this.
const THROTTLE: Duration = Duration::from_millis(120);

/// What git is in the middle of, from the files in its git dir.
pub fn repo_op(git_dir: &Path) -> Option<RepoOp> {
    if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
        return Some(RepoOp::Rebase);
    }
    [("MERGE_HEAD", RepoOp::Merge), ("CHERRY_PICK_HEAD", RepoOp::CherryPick), ("REVERT_HEAD", RepoOp::Revert)]
        .into_iter()
        .find(|(file, _)| git_dir.join(file).is_file())
        .map(|(_, op)| op)
}

/// The op `root`'s repo is in, if any.
pub fn op_of(root: &Path) -> Option<RepoOp> {
    git_dir(root).and_then(|d| repo_op(&d))
}

/// The remotes and branch settings of a repo, from one `git config`.
#[derive(Default)]
pub struct Remotes {
    pub names: Vec<String>,
    /// Lowercased keys (`branch.<name>.remote` keeps the branch's case).
    config: HashMap<String, String>,
}

impl Remotes {
    pub fn read(root: &Path) -> Self {
        let Ok(out) = Git::read(root, ["config", "-z", "--get-regexp", r"^(remote|branch)\."]).run_ok(&[1]) else { return Self::default() };
        let mut remotes = Self::default();
        for field in fields(&out.stdout) {
            let (key, value) = field.split_once('\n').unwrap_or((field.as_str(), ""));
            if let Some(name) = key.strip_prefix("remote.").and_then(|k| k.strip_suffix(".url"))
                && !remotes.names.iter().any(|n| n == name)
            {
                remotes.names.push(name.to_owned());
            }
            remotes.config.insert(key.to_owned(), value.to_owned());
        }
        remotes
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.config.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// The branch's upstream: its remote and the ref there (`refs/heads/x`).
    pub fn upstream(&self, branch: &str) -> Option<(&str, &str)> {
        let remote = self.get(&format!("branch.{branch}.remote")).filter(|r| self.names.iter().any(|n| n == r))?;
        Some((remote, self.get(&format!("branch.{branch}.merge"))?))
    }

    /// Where `branch` is pushed: its push remote, the default one, its
    /// upstream's, else the only remote or `origin`.
    pub fn push_remote(&self, branch: Option<&str>) -> Option<String> {
        let known = |r: &&str| self.names.iter().any(|n| n == r);
        let configured = branch
            .and_then(|b| self.get(&format!("branch.{b}.pushremote")))
            .filter(known)
            .or_else(|| self.get("remote.pushdefault").filter(known))
            .or_else(|| branch.and_then(|b| self.upstream(b)).map(|(r, _)| r));
        let fallback = || match self.names.as_slice() {
            [only] => Some(only.clone()),
            names => names.iter().find(|n| *n == "origin").cloned(),
        };
        configured.map(str::to_owned).or_else(fallback)
    }
}

/// `git` set up for the network: a long timeout and no prompts.
fn network<'a>(root: &'a Path, args: Vec<String>, progress: Option<Progress>) -> Git<'a> {
    let mut git = Git::write(root, args).timeout(NETWORK_TIMEOUT).env("GCM_INTERACTIVE", "never");
    let ssh_set = ["GIT_SSH_COMMAND", "GIT_SSH"].iter().any(|k| std::env::var_os(k).is_some())
        || Git::read(root, ["config", "--get", "core.sshCommand"]).output().is_ok_and(|o| o.code == Some(0));
    if !ssh_set {
        git = git.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    if let Some(tx) = progress {
        let mut last: Option<Instant> = None;
        git = git.on_line(move |line| {
            let now = Instant::now();
            if last.is_some_and(|t| now.duration_since(t) < THROTTLE) {
                return;
            }
            last = Some(now);
            let _ = tx.send(line.strip_prefix("remote: ").unwrap_or(line).chars().take(200).collect());
        });
    }
    git
}

/// What a failed push, pull or fetch to `remote` said, for people.
pub fn explain(said: &str, remote: &str, op: &str) -> String {
    let s = said.to_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| s.contains(n));
    if any(&[
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "authentication failed",
        "permission denied (publickey",
        "host key verification failed",
        "invalid username or password",
    ]) {
        return format!("Couldn't sign in to {remote}: run git {op} once in a terminal to set up credentials");
    }
    if any(&["fetch first", "non-fast-forward", "updates were rejected because"]) {
        return "The remote has commits this branch lacks: pull first".into();
    }
    if any(&[
        "could not resolve host",
        "connection refused",
        "failed to connect",
        "couldn't connect to server",
        "connection timed out",
        "network is unreachable",
        "operation timed out",
    ]) {
        return format!("Couldn't reach {remote}: check the network");
    }
    if any(&["repository not found", "does not appear to be a git repository"]) {
        return format!("{remote} wasn't found, or this account can't reach it");
    }
    if any(&["would be overwritten by merge", "your local changes to the following files would be overwritten"]) {
        return format!("Commit or stash your changes first: the {op} would overwrite them");
    }
    if any(&["you have unstaged changes", "your index contains uncommitted changes"]) {
        return format!("Commit or stash your changes first: the {op} needs a clean work tree");
    }
    // The last lines git said that aren't hints or progress.
    let lines: Vec<String> = said
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("hint:") && !l.starts_with("To ") && *l != "Done" && !l.contains('%'))
        .map(|l| match l.strip_prefix("!\t") {
            // A ref `--porcelain` reports refused: `[remote rejected] (why)`.
            Some(rest) => {
                let verdict = rest.rsplit('\t').next().unwrap_or(rest);
                let why = verdict.split_once("] ").map_or(verdict, |(_, w)| w).trim_matches(|c| c == '(' || c == ')');
                format!("Refused: {why}")
            }
            None => l.strip_prefix("remote: ").unwrap_or(l).to_owned(),
        })
        .collect();
    lines[lines.len().saturating_sub(3)..].join("\n")
}

fn failed(out: &super::git::Output, remote: &str, op: &str) -> RpcError {
    let said = out.said();
    // A push's verdict per ref (`--porcelain`) is on stdout.
    let all = format!("{}\n{}", out.stderr, String::from_utf8_lossy(&out.stdout));
    let text = explain(&all, remote, op);
    let text = if text.is_empty() { explain(&said, remote, op) } else { text };
    RpcError::new(RpcError::GIT, text).with_data(serde_json::json!({ "output": said }))
}

/// The checked-out branch, refusing a detached or unborn `HEAD`.
fn branch(root: &Path, what: &str) -> Result<(String, Option<String>, u32), RpcError> {
    let p = porcelain(root, false)?.ok_or_else(|| RpcError::bad_request("Not a git repository"))?;
    let branch = p.branch.ok_or_else(|| RpcError::bad_request(format!("Check out a branch to {what}: HEAD is detached")))?;
    if p.oid.is_none() {
        return Err(RpcError::bad_request(format!("Commit something first: there's nothing to {what}")));
    }
    Ok((branch, p.upstream, p.ahead))
}

/// `changes.push`.
///
/// # Errors
/// `bad_request` for a detached or unborn `HEAD` or no remote; `git` with
/// an explanation when the push fails.
pub fn push(root: &Path, progress: Option<Progress>) -> Result<ChangesPushed, RpcError> {
    let (branch, upstream, _) = branch(root, "push")?;
    let remotes = Remotes::read(root);
    let remote = remotes.push_remote(Some(&branch)).ok_or_else(|| RpcError::bad_request("This repo has no remote to push to"))?;
    let dest = match remotes.upstream(&branch) {
        Some((r, merge)) if r == remote => merge.to_owned(),
        _ => format!("refs/heads/{branch}"),
    };
    let set_upstream = upstream.is_none();
    let mut args: Vec<String> = ["push", "--progress", "--porcelain"].map(str::to_owned).to_vec();
    if set_upstream {
        args.push("--set-upstream".into());
    }
    args.extend([remote.clone(), format!("refs/heads/{branch}:{dest}")]);
    let out = network(root, args, progress).output()?;
    if out.code != Some(0) {
        return Err(failed(&out, &remote, "push"));
    }
    let up_to_date = String::from_utf8_lossy(&out.stdout).lines().any(|l| l.starts_with('=') || l.ends_with("[up to date]"));
    Ok(ChangesPushed { remote, branch, set_upstream, up_to_date })
}

/// `changes.pull`: a merge unless `pull.rebase` or `pull.ff` says how.
/// Stopping on conflicts is no error: the status shows them and the op.
///
/// # Errors
/// `bad_request` without an upstream; `git` with an explanation.
pub fn pull(root: &Path, progress: Option<Progress>) -> Result<(), RpcError> {
    let (branch, upstream, _) = branch(root, "pull")?;
    if upstream.is_none() {
        return Err(RpcError::bad_request("This branch has no upstream: push it first"));
    }
    if op_of(root).is_some() {
        return Err(RpcError::bad_request("Continue or abort the stopped merge or rebase first"));
    }
    let remotes = Remotes::read(root);
    let remote = remotes.upstream(&branch).map_or_else(|| "the remote".to_owned(), |(r, _)| r.to_owned());
    let mut args: Vec<String> = ["pull", "--progress", "--no-edit"].map(str::to_owned).to_vec();
    let set = |key: &str| Git::read(root, ["config", "--get", key]).output().is_ok_and(|o| o.code == Some(0));
    if !set("pull.rebase") && !set("pull.ff") {
        args.push("--no-rebase".into());
    }
    let out = network(root, args, progress).output()?;
    if out.code == Some(0) || op_of(root).is_some() {
        return Ok(());
    }
    Err(failed(&out, &remote, "pull"))
}

/// `changes.fetch`: the push remote (or upstream's).
///
/// # Errors
/// `bad_request` without a remote; `git` with an explanation.
pub fn fetch(root: &Path, progress: Option<Progress>) -> Result<(), RpcError> {
    let p = porcelain(root, false)?.ok_or_else(|| RpcError::bad_request("Not a git repository"))?;
    let remotes = Remotes::read(root);
    let branch = p.branch.as_deref();
    let remote = branch
        .and_then(|b| remotes.upstream(b))
        .map(|(r, _)| r.to_owned())
        .or_else(|| remotes.push_remote(branch))
        .ok_or_else(|| RpcError::bad_request("This repo has no remote to fetch from"))?;
    let out = network(root, vec!["fetch".into(), "--progress".into(), remote.clone()], progress).output()?;
    if out.code == Some(0) { Ok(()) } else { Err(failed(&out, &remote, "fetch")) }
}

fn op_name(op: RepoOp) -> Option<&'static str> {
    match op {
        RepoOp::Merge => Some("merge"),
        RepoOp::Rebase => Some("rebase"),
        RepoOp::CherryPick => Some("cherry-pick"),
        RepoOp::Revert => Some("revert"),
        RepoOp::Unknown => None,
    }
}

/// Files still conflicted (unmerged in the index).
pub fn unmerged(root: &Path) -> Result<usize, RpcError> {
    let out = Git::read(root, ["diff", "--name-only", "--diff-filter=U", "-z"]).run()?;
    Ok(fields(&out.stdout).len())
}

/// `changes.continue`: the op goes on with the resolved files (its message
/// as git wrote it; hooks run). Stopping on the next conflicts is no error.
///
/// # Errors
/// `bad_request` with nothing stopped or conflicts left; `git` otherwise.
pub fn resume(root: &Path) -> Result<(), RpcError> {
    let name = op_of(root).and_then(op_name).ok_or_else(|| RpcError::bad_request("Nothing to continue"))?;
    match unmerged(root)? {
        0 => {}
        1 => return Err(RpcError::bad_request("Resolve the conflicted file first")),
        n => return Err(RpcError::bad_request(format!("Resolve the {n} conflicted files first"))),
    }
    let out = Git::write(root, [name, "--continue"]).env("GIT_EDITOR", "true").timeout(COMMIT_TIMEOUT).output()?;
    if out.code == Some(0) || (op_of(root).is_some() && unmerged(root)? > 0) {
        return Ok(());
    }
    Err(RpcError::new(RpcError::GIT, out.said()))
}

/// `changes.abort`: back to before the op.
///
/// # Errors
/// `bad_request` with nothing stopped; `git` when git refuses.
pub fn abort(root: &Path) -> Result<(), RpcError> {
    let name = op_of(root).and_then(op_name).ok_or_else(|| RpcError::bad_request("Nothing to abort"))?;
    Git::write(root, [name, "--abort"]).run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_explained() {
        let auth = "fatal: could not read Username for 'https://github.com': terminal prompts disabled";
        assert_eq!(explain(auth, "origin", "push"), "Couldn't sign in to origin: run git push once in a terminal to set up credentials");
        assert_eq!(
            explain("git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.", "up", "fetch"),
            "Couldn't sign in to up: run git fetch once in a terminal to set up credentials"
        );
        let rejected = "To /tmp/r.git\n!\trefs/heads/main:refs/heads/main\t[rejected] (fetch first)\nDone\nerror: failed to push some refs";
        assert_eq!(explain(rejected, "origin", "push"), "The remote has commits this branch lacks: pull first");
        let hook = "remote: no pushing on fridays\nTo /tmp/r.git\n!\trefs/heads/main:refs/heads/main\t[remote rejected] (pre-receive hook declined)\nDone\nhint: see above";
        assert_eq!(explain(hook, "origin", "push"), "no pushing on fridays\nRefused: pre-receive hook declined");
    }

    #[test]
    fn the_push_remote_follows_the_config() {
        let mut r = Remotes { names: vec!["fork".into(), "origin".into()], config: HashMap::new() };
        assert_eq!(r.push_remote(Some("main")).as_deref(), Some("origin"));
        r.config.insert("branch.main.remote".into(), "fork".into());
        r.config.insert("branch.main.merge".into(), "refs/heads/trunk".into());
        assert_eq!((r.push_remote(Some("main")).as_deref(), r.upstream("main")), (Some("fork"), Some(("fork", "refs/heads/trunk"))));
        r.config.insert("remote.pushdefault".into(), "origin".into());
        assert_eq!(r.push_remote(Some("main")).as_deref(), Some("origin"));
        r.names = vec!["only".into()];
        assert_eq!(r.push_remote(None).as_deref(), Some("only"));
    }
}
