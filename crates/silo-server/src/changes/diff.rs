//! `changes.diff`: one file's hunks in a scope, parsed from
//! `git diff -U<context>`; untracked files against `/dev/null`
//! (`git diff --no-index`). Hunk ids hash what the hunk shows.

use std::path::Path;

use sha2::{Digest, Sha256};
use silo_proto::{
    changes::{DiffLine, FileDiff, FileStatus, Hunk, LineKind},
    control::RpcError,
    model::ChangesScope,
};

use super::{
    git::{Git, fields},
    status::{DIFF_FLAGS, diff_args, has_work_tree, names},
};

/// A hunk and the bytes it came as (its `@@` line included), to apply it.
pub struct RawHunk {
    pub hunk: Hunk,
    pub raw: Vec<u8>,
}

/// One file's diff as git printed it.
#[derive(Default)]
pub struct Parsed {
    /// Everything before the first hunk (`diff --git`, modes, `---`/`+++`).
    pub header: Vec<u8>,
    pub hunks: Vec<RawHunk>,
    pub binary: bool,
    pub truncated: bool,
}

impl Parsed {
    /// A patch of the hunks `ids` names, in the file's order.
    ///
    /// # Errors
    /// `conflict` when one of them isn't in the file (any more).
    pub fn patch(&self, ids: &[String]) -> Result<Vec<u8>, RpcError> {
        if let Some(missing) = ids.iter().find(|id| !self.hunks.iter().any(|h| &h.hunk.id == *id)) {
            return Err(RpcError::new(RpcError::CONFLICT, "The file changed; review it again").with_data(serde_json::json!({ "hunk": missing })));
        }
        let mut patch = self.header.clone();
        for raw in self.hunks.iter().filter(|h| ids.contains(&h.hunk.id)) {
            patch.extend_from_slice(&raw.raw);
        }
        Ok(patch)
    }
}

/// `@@ -a,b +c,d @@ section`: the numbers and the section.
fn hunk_header(line: &str) -> Option<(u32, u32, u32, u32, String)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, section) = rest.split_once(" @@").unwrap_or((rest, ""));
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((start, lines)) => Some((start.parse().ok()?, lines.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let ((os, ol), (ns, nl)) = (range(old)?, range(new)?);
    Some((os, ol, ns, nl, section.strip_prefix(' ').unwrap_or(section).to_owned()))
}

/// Parse the first file of `git diff` output; `truncated`: the output was
/// cut, so a last hunk not whole is dropped.
#[must_use]
pub fn parse(bytes: &[u8], truncated: bool) -> Parsed {
    let mut parsed = Parsed { truncated, ..Parsed::default() };
    let mut current: Option<(RawHunk, u32, u32)> = None;
    let mut sections = 0;
    let finish = |parsed: &mut Parsed, current: &mut Option<(RawHunk, u32, u32)>| {
        if let Some((raw, _, _)) = current.take() {
            parsed.hunks.push(raw);
        }
    };
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(b"diff --git ") || body.starts_with(b"diff --cc ") {
            sections += 1;
            if sections > 1 {
                break;
            }
        }
        if body.starts_with(b"@@ ") {
            finish(&mut parsed, &mut current);
            let text = String::from_utf8_lossy(body);
            let Some((old_start, old_lines, new_start, new_lines, header)) = hunk_header(&text) else { break };
            let hunk = Hunk { id: String::new(), old_start, old_lines, new_start, new_lines, header, lines: Vec::new() };
            current = Some((RawHunk { hunk, raw: line.to_vec() }, old_start, new_start));
            continue;
        }
        let Some((raw, old, new)) = current.as_mut() else {
            if body.starts_with(b"Binary files ") || body.starts_with(b"GIT binary patch") {
                parsed.binary = true;
            }
            parsed.header.extend_from_slice(line);
            continue;
        };
        let text = |rest: &[u8]| String::from_utf8_lossy(rest.strip_suffix(b"\r").unwrap_or(rest)).into_owned();
        let (kind, rest) = (body.first().copied(), body.get(1..).unwrap_or_default());
        let line_of = match kind {
            Some(b' ') => {
                let l = DiffLine { kind: LineKind::Context, old: Some(*old), new: Some(*new), text: text(rest) };
                (*old, *new) = (*old + 1, *new + 1);
                l
            }
            Some(b'-') => {
                let l = DiffLine { kind: LineKind::Removed, old: Some(*old), new: None, text: text(rest) };
                *old += 1;
                l
            }
            Some(b'+') => {
                let l = DiffLine { kind: LineKind::Added, old: None, new: Some(*new), text: text(rest) };
                *new += 1;
                l
            }
            Some(b'\\') => DiffLine { kind: LineKind::NoNewline, old: None, new: None, text: text(body.get(2..).unwrap_or_default()) },
            _ => break,
        };
        raw.hunk.lines.push(line_of);
        raw.raw.extend_from_slice(line);
    }
    if let Some((raw, old, new)) = current.take() {
        let whole = old == raw.hunk.old_start + raw.hunk.old_lines && new == raw.hunk.new_start + raw.hunk.new_lines;
        if whole || !truncated {
            parsed.hunks.push(raw);
        }
    }
    assign_ids(&mut parsed);
    parsed
}

/// Ids: a hash of each hunk's section and lines (not its position, so
/// edits above it keep it), with the occurrence for identical hunks.
fn assign_ids(parsed: &mut Parsed) {
    let mut seen: Vec<String> = Vec::new();
    for raw in &mut parsed.hunks {
        let body = raw.raw.splitn(2, |b| *b == b'\n').nth(1).unwrap_or_default();
        let digest = |n: usize| {
            let mut hash = Sha256::new();
            hash.update(raw.hunk.header.as_bytes());
            hash.update([0]);
            hash.update(body);
            hash.update(n.to_le_bytes());
            hash.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect::<String>()
        };
        let base = digest(0);
        let n = seen.iter().filter(|s| **s == base).count();
        seen.push(base.clone());
        raw.hunk.id = if n == 0 { base } else { digest(n) };
    }
}

/// Whether `rel` is untracked (and not ignored).
///
/// # Errors
/// git failed.
pub fn is_untracked(root: &Path, rel: &str) -> Result<bool, RpcError> {
    let out = Git::read(root, ["ls-files", "--others", "--exclude-standard", "-z", "--", rel]).run()?;
    Ok(fields(&out.stdout).iter().any(|p| p == rel))
}

/// Untracked `rel` against `/dev/null`.
///
/// # Errors
/// git failed.
pub fn untracked(root: &Path, rel: &str, context: u32) -> Result<Parsed, RpcError> {
    let mut argv: Vec<String> = vec!["diff".into(), "--no-index".into(), format!("-U{context}")];
    argv.extend(DIFF_FLAGS.iter().filter(|f| **f != "--no-relative").map(|s| (*s).to_owned()));
    argv.extend(["--".into(), "/dev/null".into(), rel.to_owned()]);
    let out = Git::read(root, &argv).stdout_max(FileDiff::MAX_BYTES).run_ok(&[1])?;
    Ok(parse(&out.stdout, out.truncated))
}

/// `rel`'s diff by `git diff <args>` (`old` first for a rename).
///
/// # Errors
/// git failed.
pub fn tracked(root: &Path, args: &[String], rel: &str, old: Option<&str>, context: u32) -> Result<Parsed, RpcError> {
    let mut argv: Vec<String> = vec!["diff".into(), format!("-U{context}")];
    argv.extend(DIFF_FLAGS.iter().map(|s| (*s).to_owned()));
    argv.extend(args.iter().cloned());
    argv.push("--".into());
    argv.extend(old.map(str::to_owned));
    argv.push(rel.to_owned());
    let out = Git::read(root, &argv).stdout_max(FileDiff::MAX_BYTES).run()?;
    Ok(parse(&out.stdout, out.truncated))
}

/// `rel` in `scope`: its diff, with its status and old path.
///
/// # Errors
/// git failed.
pub fn diff(root: &Path, scope: &ChangesScope, rel: &str, context: u32) -> Result<(FileDiff, Parsed), RpcError> {
    let file = |status, old_path, parsed: &Parsed| FileDiff {
        path: rel.to_owned(),
        old_path,
        status,
        binary: parsed.binary,
        hunks: parsed.hunks.iter().map(|h| h.hunk.clone()).collect(),
        truncated: parsed.truncated,
    };
    if has_work_tree(scope) && is_untracked(root, rel)? {
        let parsed = untracked(root, rel, context)?;
        return Ok((file(FileStatus::Untracked, None, &parsed), parsed));
    }
    let args = diff_args(root, scope)?;
    let mut named = names(root, &args, &[rel])?.into_iter().find(|n| n.path == rel);
    // A rename shows as an addition when the pathspec leaves its source out.
    if named.as_ref().is_none_or(|n| n.status == FileStatus::Added)
        && let Some(renamed) = names(root, &args, &[])?.into_iter().find(|n| n.path == rel && n.old_path.is_some())
    {
        named = Some(renamed);
    }
    let (status, old_path) = named.map_or((FileStatus::Modified, None), |n| (n.status, n.old_path));
    let parsed = tracked(root, &args, rel, old_path.as_deref(), context)?;
    Ok((file(status, old_path.clone(), &parsed), parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n\
                        @@ -1,3 +1,3 @@ fn main\n a\n-b\n+B\n c\n\
                        @@ -10,2 +10,3 @@\n x\n+y\n z\n\\ No newline at end of file\n";

    #[test]
    fn hunks_parse_with_line_numbers() {
        let p = parse(DIFF.as_bytes(), false);
        assert!(!p.binary);
        assert!(String::from_utf8_lossy(&p.header).ends_with("+++ b/a.rs\n"));
        assert_eq!(p.hunks.len(), 2);
        let h = &p.hunks[0].hunk;
        assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines, h.header.as_str()), (1, 3, 1, 3, "fn main"));
        let numbers: Vec<_> = h.lines.iter().map(|l| (l.kind, l.old, l.new, l.text.as_str())).collect();
        assert_eq!(
            numbers,
            [
                (LineKind::Context, Some(1), Some(1), "a"),
                (LineKind::Removed, Some(2), None, "b"),
                (LineKind::Added, None, Some(2), "B"),
                (LineKind::Context, Some(3), Some(3), "c")
            ]
        );
        let last = &p.hunks[1].hunk;
        assert_eq!(last.lines.last().map(|l| (l.kind, l.text.as_str())), Some((LineKind::NoNewline, "No newline at end of file")));
        assert_eq!(last.lines[1].new, Some(11));
        // A patch of the second hunk only: the header, then that hunk as sent.
        let patch = String::from_utf8(p.patch(std::slice::from_ref(&last.id)).unwrap()).unwrap();
        assert!(patch.starts_with("diff --git") && patch.contains("@@ -10,2 +10,3 @@") && !patch.contains("fn main"));
        assert_eq!(p.patch(&["nope".into()]).unwrap_err().code, RpcError::CONFLICT);
    }

    #[test]
    fn ids_follow_content_not_position() {
        let moved = DIFF.replace("@@ -10,2 +10,3 @@", "@@ -20,2 +21,3 @@");
        let (a, b) = (parse(DIFF.as_bytes(), false), parse(moved.as_bytes(), false));
        assert_eq!(a.hunks[1].hunk.id, b.hunks[1].hunk.id);
        assert_ne!(a.hunks[0].hunk.id, a.hunks[1].hunk.id);
        let twice = format!("{DIFF}@@ -30,2 +31,3 @@\n x\n+y\n z\n\\ No newline at end of file\n");
        let t = parse(twice.as_bytes(), false);
        assert_eq!(t.hunks.len(), 3);
        assert_ne!(t.hunks[1].hunk.id, t.hunks[2].hunk.id, "identical hunks get distinct ids");
    }

    #[test]
    fn a_cut_diff_keeps_whole_hunks_and_binaries_have_none() {
        let cut = &DIFF[..DIFF.find("+y").unwrap()];
        let p = parse(cut.as_bytes(), true);
        assert_eq!((p.hunks.len(), p.truncated), (1, true));
        let bin = parse(b"diff --git a/x b/x\nindex 1..2 100644\nBinary files a/x and b/x differ\n", false);
        assert!(bin.binary && bin.hunks.is_empty());
    }
}
