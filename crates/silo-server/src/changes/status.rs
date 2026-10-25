//! `changes.status`: the changed files of a scope, from
//! `git status --porcelain=v2`, `git diff --name-status` and
//! `git diff --numstat`.

use std::{collections::HashMap, path::Path};

use silo_proto::{
    changes::{ChangedFile, ChangesStatus, ChangesTotals, FileStatus},
    control::RpcError,
    model::ChangesScope,
};

use super::git::{Git, fields};

/// Longest revision accepted in a scope.
pub const REV_MAX: usize = 256;

/// Whether `rev` may be passed to git as a revision: no option, space or
/// control character, no `..` range.
pub fn rev_ok(rev: &str) -> bool {
    !rev.is_empty()
        && rev.len() <= REV_MAX
        && !rev.starts_with('-')
        && !rev.contains("..")
        && !rev.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `bad_request` for a scope git can't be given.
///
/// # Errors
/// An unknown scope, or a revision [`rev_ok`] refuses.
pub fn check_scope(scope: &ChangesScope) -> Result<(), RpcError> {
    let revs: Vec<&str> = match scope {
        ChangesScope::Working | ChangesScope::Staged | ChangesScope::Unstaged => Vec::new(),
        ChangesScope::Tree { base, head, label } => {
            if label.chars().count() > 200 {
                return Err(RpcError::bad_request("scope label is too long"));
            }
            std::iter::once(base.as_str()).chain(head.as_deref()).collect()
        }
        ChangesScope::Range { base, head } => std::iter::once(base.as_str()).chain(head.as_deref()).collect(),
        ChangesScope::Unknown => return Err(RpcError::bad_request("unsupported scope")),
    };
    match revs.iter().find(|r| !rev_ok(r)) {
        Some(bad) => Err(RpcError::bad_request(format!("bad revision {bad:?}"))),
        None => Ok(()),
    }
}

/// The scope compares against the work tree (its untracked files count).
pub fn has_work_tree(scope: &ChangesScope) -> bool {
    match scope {
        ChangesScope::Working | ChangesScope::Unstaged => true,
        ChangesScope::Tree { head, .. } | ChangesScope::Range { head, .. } => head.is_none(),
        ChangesScope::Staged | ChangesScope::Unknown => false,
    }
}

/// The revision arguments of `git diff` for `scope`.
///
/// # Errors
/// See [`check_scope`]; an unborn `HEAD` needs the empty tree, which git
/// is asked for.
pub fn diff_args(root: &Path, scope: &ChangesScope) -> Result<Vec<String>, RpcError> {
    check_scope(scope)?;
    Ok(match scope {
        ChangesScope::Working => vec![head_or_empty(root)?],
        ChangesScope::Staged => vec!["--cached".into()],
        ChangesScope::Unstaged => Vec::new(),
        ChangesScope::Tree { base, head, .. } => std::iter::once(base.clone()).chain(head.clone()).collect(),
        ChangesScope::Range { base, head: Some(head) } => vec![format!("{base}...{head}")],
        ChangesScope::Range { base, head: None } => vec!["--merge-base".into(), base.clone()],
        ChangesScope::Unknown => return Err(RpcError::bad_request("unsupported scope")),
    })
}

/// `HEAD`, or the empty tree before the first commit.
fn head_or_empty(root: &Path) -> Result<String, RpcError> {
    if Git::read(root, ["rev-parse", "--verify", "-q", "HEAD"]).output()?.code == Some(0) {
        return Ok("HEAD".into());
    }
    let out = Git::read(root, ["hash-object", "-t", "tree", "/dev/null"]).run()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The flags every `git diff` here runs with.
pub const DIFF_FLAGS: &[&str] = &["--no-color", "--no-ext-diff", "--no-relative", "-M", "--src-prefix=a/", "--dst-prefix=b/"];

/// One `git status --porcelain=v2` entry.
#[derive(Default, Clone)]
pub struct Entry {
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
    pub conflicted: bool,
}

/// What `git status --porcelain=v2 --branch` says.
#[derive(Default)]
pub struct Porcelain {
    pub branch: Option<String>,
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: HashMap<String, Entry>,
    /// Untracked files, in git's order.
    pub untracked: Vec<String>,
}

/// `None` outside a git work tree.
///
/// # Errors
/// git failed otherwise.
pub fn porcelain(root: &Path, untracked: bool) -> Result<Option<Porcelain>, RpcError> {
    let mode = if untracked { "--untracked-files=all" } else { "--untracked-files=no" };
    let out = Git::read(root, ["status", "--porcelain=v2", "--branch", "-z", mode]).output()?;
    if out.code != Some(0) {
        if out.stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(RpcError::new(RpcError::GIT, out.said()));
    }
    Ok(Some(parse_porcelain(&fields(&out.stdout))))
}

fn parse_porcelain(fields: &[String]) -> Porcelain {
    let mut p = Porcelain::default();
    let mut it = fields.iter();
    while let Some(field) = it.next() {
        if let Some(header) = field.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => p.oid = Some(value.to_owned()),
                "branch.head" if value != "(detached)" => p.branch = Some(value.to_owned()),
                "branch.upstream" => p.upstream = Some(value.to_owned()),
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            p.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            p.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let (kind, rest) = field.split_once(' ').unwrap_or((field.as_str(), ""));
        let xy = |rest: &str| {
            let b = rest.as_bytes();
            (b.first().copied().unwrap_or(b'.'), b.get(1).copied().unwrap_or(b'.'))
        };
        match kind {
            "1" | "2" => {
                let (x, y) = xy(rest);
                let skip = if kind == "1" { 7 } else { 8 };
                let path = rest.splitn(skip + 1, ' ').nth(skip).unwrap_or_default().to_owned();
                if kind == "2" {
                    it.next();
                }
                p.entries.insert(path, Entry { staged: x != b'.', unstaged: y != b'.', ..Entry::default() });
            }
            "u" => {
                let path = rest.splitn(10, ' ').nth(9).unwrap_or_default().to_owned();
                p.entries.insert(path, Entry { staged: true, unstaged: true, conflicted: true, ..Entry::default() });
            }
            "?" => {
                p.entries.insert(rest.to_owned(), Entry { unstaged: true, untracked: true, ..Entry::default() });
                p.untracked.push(rest.to_owned());
            }
            _ => {}
        }
    }
    p
}

/// One file of `git diff --name-status`, with its `--numstat` counts.
#[derive(Clone)]
pub struct Named {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
}

fn status_of(letter: &str) -> FileStatus {
    match letter.chars().next() {
        Some('A') => FileStatus::Added,
        Some('M') => FileStatus::Modified,
        Some('D') => FileStatus::Deleted,
        Some('R') => FileStatus::Renamed,
        Some('C') => FileStatus::Copied,
        Some('T') => FileStatus::TypeChanged,
        Some('U') => FileStatus::Conflicted,
        _ => FileStatus::Unknown,
    }
}

/// The files `git diff <args>` changes, by `--name-status`.
///
/// # Errors
/// git failed (a bad revision, say).
pub fn names(root: &Path, args: &[String], paths: &[&str]) -> Result<Vec<Named>, RpcError> {
    let mut argv: Vec<String> = vec!["diff".into(), "--name-status".into(), "-z".into()];
    argv.extend(DIFF_FLAGS.iter().map(|s| (*s).to_owned()));
    argv.extend(args.iter().cloned());
    argv.push("--".into());
    argv.extend(paths.iter().map(|p| (*p).to_owned()));
    let out = Git::read(root, &argv).run()?;
    let fields = fields(&out.stdout);
    let mut it = fields.into_iter();
    let mut named = Vec::new();
    while let Some(letter) = it.next() {
        let status = status_of(&letter);
        let Some(first) = it.next() else { break };
        if matches!(status, FileStatus::Renamed | FileStatus::Copied) {
            let Some(second) = it.next() else { break };
            named.push(Named { path: second, old_path: Some(first), status });
        } else {
            named.push(Named { path: first, old_path: None, status });
        }
    }
    Ok(named)
}

/// Lines added and removed per (new) path, `None` for binaries.
fn numstat(root: &Path, args: &[String]) -> Result<HashMap<String, Option<(u32, u32)>>, RpcError> {
    let mut argv: Vec<String> = vec!["diff".into(), "--numstat".into(), "-z".into()];
    argv.extend(DIFF_FLAGS.iter().map(|s| (*s).to_owned()));
    argv.extend(args.iter().cloned());
    argv.push("--".into());
    let out = Git::read(root, &argv).run()?;
    let fields = fields(&out.stdout);
    let mut it = fields.into_iter();
    let mut counts = HashMap::new();
    while let Some(field) = it.next() {
        let mut parts = field.splitn(3, '\t');
        let (added, removed, path) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let path = if path.is_empty() {
            // A rename: the old path, then the new.
            it.next();
            it.next().unwrap_or_default()
        } else {
            path.to_owned()
        };
        let count = added.parse::<u32>().ok().zip(removed.parse::<u32>().ok());
        counts.insert(path, count);
    }
    Ok(counts)
}

/// Lines of untracked `rel` and whether it is binary (a NUL in its first
/// 8000 bytes); counts stop at [`UNTRACKED_READ`].
pub fn untracked_lines(root: &Path, rel: &str) -> (u32, bool) {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(root.join(rel)) else { return (0, false) };
    let mut bytes = Vec::new();
    let _ = file.take(UNTRACKED_READ).read_to_end(&mut bytes);
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return (0, true);
    }
    let lines = bytes.iter().filter(|b| **b == b'\n').count() + usize::from(bytes.last().is_some_and(|b| *b != b'\n'));
    (u32::try_from(lines).unwrap_or(u32::MAX), false)
}

/// Most bytes of an untracked file read to count its lines.
const UNTRACKED_READ: u64 = 8 * 1024 * 1024;

/// The status of `scope` in the repo at `root`.
///
/// # Errors
/// git failed (a revision that doesn't exist, say).
pub fn status(root: &Path, scope: &ChangesScope) -> Result<ChangesStatus, RpcError> {
    check_scope(scope)?;
    let with_untracked = has_work_tree(scope);
    let Some(porcelain) = porcelain(root, with_untracked)? else {
        return Ok(ChangesStatus {
            repo: false,
            branch: None,
            head: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            files: Vec::new(),
            totals: ChangesTotals::default(),
            truncated: false,
        });
    };
    let args = diff_args(root, scope)?;
    let names = names(root, &args, &[])?;
    let counts = numstat(root, &args)?;
    let flags = matches!(scope, ChangesScope::Working | ChangesScope::Staged | ChangesScope::Unstaged);
    let mut seen = std::collections::HashSet::new();
    let mut files: Vec<ChangedFile> = names
        .into_iter()
        // An unmerged path may be listed twice.
        .filter(|n| seen.insert(n.path.clone()))
        .map(|n| {
            let count = counts.get(&n.path).copied().unwrap_or(Some((0, 0)));
            let entry = porcelain.entries.get(&n.path).cloned().unwrap_or_default();
            let status = if entry.conflicted { FileStatus::Conflicted } else { n.status };
            let (staged, unstaged) = if flags { (entry.staged, entry.unstaged) } else { (false, false) };
            let (added, removed) = count.unwrap_or_default();
            ChangedFile { path: n.path, old_path: n.old_path, status, staged, unstaged, added, removed, binary: count.is_none() }
        })
        .collect();
    if with_untracked {
        let listed: std::collections::HashSet<String> = files.iter().map(|f| f.path.clone()).collect();
        for path in porcelain.untracked.iter().filter(|p| !listed.contains(*p)) {
            let (added, binary) = untracked_lines(root, path);
            files.push(ChangedFile {
                path: path.clone(),
                old_path: None,
                status: FileStatus::Untracked,
                staged: false,
                unstaged: true,
                added,
                removed: 0,
                binary,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let totals = ChangesTotals {
        files: u32::try_from(files.len()).unwrap_or(u32::MAX),
        added: files.iter().map(|f| f.added).fold(0, u32::saturating_add),
        removed: files.iter().map(|f| f.removed).fold(0, u32::saturating_add),
    };
    let truncated = files.len() > ChangesStatus::MAX_FILES;
    files.truncate(ChangesStatus::MAX_FILES);
    let head = match &porcelain.oid {
        Some(_) => Git::read(root, ["rev-parse", "--short", "HEAD"]).run().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()),
        None => None,
    };
    Ok(ChangesStatus {
        repo: true,
        branch: porcelain.branch,
        head,
        upstream: porcelain.upstream,
        ahead: porcelain.ahead,
        behind: porcelain.behind,
        files,
        totals,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_v2_parses() {
        let raw = "# branch.oid 1234567890abcdef\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
                   1 M. N... 100644 100644 100644 aaa bbb src/a b.rs\0\
                   2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
                   u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
                   ? new file.txt\0";
        let p = parse_porcelain(&fields(raw.as_bytes()));
        assert_eq!((p.branch.as_deref(), p.upstream.as_deref(), p.ahead, p.behind), (Some("main"), Some("origin/main"), 2, 1));
        assert!(p.entries["src/a b.rs"].staged && !p.entries["src/a b.rs"].unstaged);
        assert!(p.entries["new.rs"].staged && !p.entries.contains_key("old.rs"));
        assert!(p.entries["conflict.rs"].conflicted);
        assert_eq!(p.untracked, ["new file.txt"]);
    }

    #[test]
    fn revisions_are_checked() {
        for good in ["main", "HEAD~2", "4b825dc642cb6eb9a060e54bf8d69288fbee4904", "origin/feature-x"] {
            assert!(rev_ok(good), "{good}");
        }
        for bad in ["", "-c", "--output=/tmp/x", "a b", "main..otto", "a\nb"] {
            assert!(!rev_ok(bad), "{bad}");
        }
    }
}
