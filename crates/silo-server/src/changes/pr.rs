//! Pull requests through GitHub's `gh`: the branch's pull request
//! (`gh pr view`), what a new one starts with (from the branch's commits),
//! and `gh pr create`. `gh` never prompts here (`GH_PROMPT_DISABLED`); a
//! missing `gh`, a signed-out one or a repo not on GitHub is a state the
//! app shows, not an error.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use silo_proto::{
    changes::{ChangesPr, ChangesPrCreateParams, GhState, PullRequest},
    control::RpcError,
};

use super::{
    git::{Git, fields},
    status::{porcelain, rev_ok},
    sync::{self, Progress, Remotes},
};

const GH_TIMEOUT: Duration = Duration::from_secs(30);
const CREATE_TIMEOUT: Duration = Duration::from_secs(120);
const BASES_MAX: usize = 50;

/// `gh`'s path: on `PATH`, else where Homebrew puts it (a server started
/// by launchd has a short `PATH`).
pub fn gh_path() -> Option<PathBuf> {
    let dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    dirs.into_iter().chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from)).map(|d| d.join("gh")).find(|p| p.is_file())
}

fn gh<'a>(root: &'a Path, gh: &'a str, args: &[&str]) -> Git<'a> {
    Git::read(root, args.iter().copied())
        .program(gh)
        .timeout(GH_TIMEOUT)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("NO_COLOR", "1")
}

/// What a failing `gh` said means.
pub fn gh_state(said: &str) -> Option<GhState> {
    let s = said.to_lowercase();
    if ["gh auth login", "not logged in", "authentication", "bad credentials", "http 401"].iter().any(|n| s.contains(n)) {
        return Some(GhState::SignedOut);
    }
    if ["none of the git remotes", "no git remotes", "not a github", "unable to determine"].iter().any(|n| s.contains(n)) {
        return Some(GhState::NotGithub);
    }
    None
}

/// A `gh pr view --json` answer.
pub fn parse_pr(json: &Value) -> Option<PullRequest> {
    Some(PullRequest {
        number: json["number"].as_u64()?,
        url: json["url"].as_str()?.to_owned(),
        title: json["title"].as_str().unwrap_or_default().to_owned(),
        state: json["state"].as_str().unwrap_or("open").to_lowercase(),
        draft: json["isDraft"].as_bool().unwrap_or(false),
        base: json["baseRefName"].as_str().unwrap_or_default().to_owned(),
    })
}

/// "fix-login_bug" → "Fix login bug".
pub fn branch_title(branch: &str) -> String {
    let words = branch.rsplit('/').next().unwrap_or(branch).replace(['-', '_'], " ");
    let mut chars = words.trim().chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// A title and body from `(subject, body)` of each commit, newest first:
/// one commit's own, else the branch's name and a list of subjects.
pub fn prefill(branch: &str, commits: &[(String, String)]) -> (String, String) {
    match commits {
        [(subject, body)] => (subject.clone(), body.trim().to_owned()),
        [] => (branch_title(branch), String::new()),
        many => (branch_title(branch), many.iter().rev().map(|(s, _)| format!("- {s}")).collect::<Vec<_>>().join("\n")),
    }
}

/// The remote's default branch, then its other branches (not `current`).
fn bases(root: &Path, remote: &str, current: &str) -> Vec<String> {
    let head = Git::read(root, ["symbolic-ref", "--short", "-q", &format!("refs/remotes/{remote}/HEAD")]).run().ok();
    let prefix = format!("{remote}/");
    let default = head.map(|o| String::from_utf8_lossy(&o.stdout).trim().trim_start_matches(&prefix).to_owned()).filter(|d| !d.is_empty());
    let refs = Git::read(root, ["for-each-ref", "--format=%(refname:lstrip=3)", &format!("refs/remotes/{remote}")]).run().ok();
    let mut all: Vec<String> = refs
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_owned).filter(|b| b != "HEAD" && b != current).collect())
        .unwrap_or_default();
    let default = default.or_else(|| ["main", "master"].into_iter().find(|b| all.iter().any(|a| a == b)).map(str::to_owned));
    if let Some(d) = default {
        all.retain(|b| *b != d);
        all.insert(0, d);
    }
    all.truncate(BASES_MAX);
    all
}

/// `(subject, body)` of the commits on `HEAD` not on `base`, newest first.
fn commits(root: &Path, base: &str) -> Vec<(String, String)> {
    let Ok(out) = Git::read(root, ["log", "-z", "--max-count=50", "--format=%s%x1f%b", &format!("{base}..HEAD"), "--"]).run() else {
        return Vec::new();
    };
    fields(&out.stdout).into_iter().map(|f| f.split_once('\u{1f}').map(|(s, b)| (s.to_owned(), b.to_owned())).unwrap_or((f, String::new()))).collect()
}

/// `changes.pr`.
///
/// # Errors
/// git failed reading the repo; `gh` failing otherwise is `git` with what
/// it said.
pub fn info(root: &Path) -> Result<ChangesPr, RpcError> {
    let mut pr = ChangesPr { gh: GhState::Ready, pr: None, bases: Vec::new(), title: String::new(), body: String::new() };
    let Some(path) = gh_path() else { return Ok(ChangesPr { gh: GhState::Missing, ..pr }) };
    let p = porcelain(root, false)?.ok_or_else(|| RpcError::bad_request("Not a git repository"))?;
    let Some(branch) = p.branch else { return Ok(pr) };
    let remotes = Remotes::read(root);
    let Some(remote) = remotes.push_remote(Some(&branch)) else { return Ok(ChangesPr { gh: GhState::NotGithub, ..pr }) };
    let path = path.to_string_lossy().into_owned();
    let out = gh(root, &path, &["pr", "view", "--json", "number,url,title,state,isDraft,baseRefName"]).output()?;
    if out.code == Some(0) {
        let json: Value = serde_json::from_slice(&out.stdout).map_err(|e| RpcError::new(RpcError::GIT, format!("gh: {e}")))?;
        pr.pr = parse_pr(&json);
    } else if let Some(state) = gh_state(&out.said()) {
        return Ok(ChangesPr { gh: state, ..pr });
    } else if !out.said().to_lowercase().contains("no pull requests found") {
        return Err(RpcError::new(RpcError::GIT, out.said()));
    }
    pr.bases = bases(root, &remote, &branch);
    if let Some(base) = pr.bases.first() {
        (pr.title, pr.body) = prefill(&branch, &commits(root, &format!("{remote}/{base}")));
    } else {
        pr.title = branch_title(&branch);
    }
    Ok(pr)
}

/// `changes.pr_create`: pushes first when the remote lacks the branch or
/// some of its commits.
///
/// # Errors
/// `bad_request` without `gh`, a title or a branch, or for a bad base;
/// `git` with an explanation when the push or `gh` fails.
pub fn create(root: &Path, p: &ChangesPrCreateParams, progress: Option<Progress>) -> Result<PullRequest, RpcError> {
    let path = gh_path().ok_or_else(|| RpcError::bad_request("Install gh to open pull requests"))?;
    let title = p.title.trim();
    if title.is_empty() {
        return Err(RpcError::bad_request("A pull request needs a title"));
    }
    if p.base.as_deref().is_some_and(|b| !rev_ok(b)) {
        return Err(RpcError::bad_request("bad base branch"));
    }
    let status = porcelain(root, false)?.ok_or_else(|| RpcError::bad_request("Not a git repository"))?;
    let branch = status.branch.ok_or_else(|| RpcError::bad_request("Check out a branch to open a pull request"))?;
    if status.upstream.is_none() || status.ahead > 0 {
        sync::push(root, progress)?;
    }
    let path = path.to_string_lossy().into_owned();
    let mut args = vec!["pr", "create", "--title", title, "--body-file", "-", "--head", &branch];
    if let Some(base) = &p.base {
        args.extend(["--base", base]);
    }
    if p.draft {
        args.push("--draft");
    }
    let out = gh(root, &path, &args).timeout(CREATE_TIMEOUT).stdin(p.body.clone().into_bytes()).output()?;
    if out.code != Some(0) {
        let said = out.said();
        let text = match gh_state(&said) {
            Some(GhState::SignedOut) => "Sign in to GitHub first: run gh auth login in a terminal".to_owned(),
            Some(GhState::NotGithub) => "No remote of this repo is on GitHub".to_owned(),
            _ => said.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("gh pr create failed").trim().to_owned(),
        };
        return Err(RpcError::new(RpcError::GIT, text).with_data(serde_json::json!({ "output": said })));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let url = stdout.lines().map(str::trim).rfind(|l| l.starts_with("https://")).ok_or_else(|| RpcError::new(RpcError::GIT, "gh gave no link"))?;
    let number = url.trim_end_matches('/').rsplit('/').next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let base = p.base.clone().unwrap_or_default();
    Ok(PullRequest { number, url: url.to_owned(), title: title.to_owned(), state: "open".into(), draft: p.draft, base })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_pull_request_starts_from_the_commits() {
        assert_eq!(branch_title("nitish/fix-login_bug"), "Fix login bug");
        let one = [("Fix it".to_owned(), "Because.\n".to_owned())];
        assert_eq!(prefill("x", &one), ("Fix it".into(), "Because.".into()));
        let two = [("Second".to_owned(), String::new()), ("First".to_owned(), String::new())];
        assert_eq!(prefill("add-thing", &two), ("Add thing".into(), "- First\n- Second".into()));
        assert_eq!(prefill("add-thing", &[]).0, "Add thing");
    }

    #[test]
    fn gh_answers_are_read() {
        let json = serde_json::json!({"number": 12, "url": "https://github.com/o/r/pull/12", "title": "T", "state": "MERGED", "isDraft": false, "baseRefName": "main"});
        assert_eq!(parse_pr(&json).map(|p| (p.number, p.state)), Some((12, "merged".into())));
        assert_eq!(gh_state("To get started with GitHub CLI, please run:  gh auth login"), Some(GhState::SignedOut));
        assert_eq!(gh_state("none of the git remotes configured for this repository point to a known GitHub host"), Some(GhState::NotGithub));
        assert_eq!(gh_state("no pull requests found for branch \"x\""), None);
    }
}
