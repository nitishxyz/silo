//! `@` mention completion (`files.complete`): a project's files and folders,
//! `.gitignore` respected and `.git` skipped, ranked by a fuzzy subsequence
//! match that weighs the basename over the rest of the path.

use std::path::Path;

/// Entries walked at most per query.
pub const MAX_WALK: usize = 20_000;

/// Matches for `query` under `root`, best first: `(path, dir)` with paths relative to `root`, `/`-separated,
/// folders ending in `/`. A query naming an existing folder (`src/ma`) matches the rest inside it.
#[must_use]
pub fn complete(root: &Path, query: &str, limit: usize) -> Vec<(String, bool)> {
    let query = query.trim().trim_start_matches("./");
    let (base, rest) = match query.rfind('/') {
        Some(i) if !query[..i].split('/').any(|c| c == "..") && root.join(&query[..i]).is_dir() => (&query[..=i], &query[i + 1..]),
        _ => ("", query),
    };
    let needle: Vec<char> = rest.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    let dir = root.join(base);
    let mut builder = ignore::WalkBuilder::new(&dir);
    builder.hidden(false).require_git(false).filter_entry(|e| e.file_name() != ".git");
    let mut hits: Vec<(i32, String, bool)> = Vec::new();
    for entry in builder.build().flatten().filter(|e| e.depth() > 0).take(MAX_WALK) {
        let Ok(relative) = entry.path().strip_prefix(&dir) else { continue };
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let mut path = relative.to_string_lossy().replace('\\', "/");
        if is_dir {
            path.push('/');
        }
        if let Some(score) = score(&needle, &path, entry.depth()) {
            hits.push((score, format!("{base}{path}"), is_dir));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    hits.into_iter().take(limit).map(|(_, path, dir)| (path, dir)).collect()
}

/// Higher is better; `None` unless `needle` is a subsequence of `path`. A match in the basename
/// counts most; deeper and longer paths a little less. An empty needle lists shallow entries first.
#[must_use]
pub fn score(needle: &[char], path: &str, depth: usize) -> Option<i32> {
    let depth = i32::try_from(depth).unwrap_or(1 << 16);
    let len = i32::try_from(path.len()).unwrap_or(1 << 16);
    if needle.is_empty() {
        return Some(-depth * 100 - len);
    }
    let name = path.trim_end_matches('/').rfind('/').map_or(0, |i| i + 1);
    let best = subsequence(needle, &path[name..]).map(|s| s + 100).or_else(|| subsequence(needle, path))?;
    Some(best - depth * 2 - len / 8)
}

/// A greedy subsequence match: a point per character, more at word starts, in runs and at the start.
fn subsequence(needle: &[char], text: &str) -> Option<i32> {
    let (mut score, mut at, mut prev, mut last) = (0, 0, None::<char>, None::<usize>);
    for (i, ch) in text.chars().enumerate() {
        if at < needle.len() && ch.to_lowercase().eq(std::iter::once(needle[at])) {
            score += 1;
            if prev.is_none_or(|p| !p.is_alphanumeric()) {
                score += 8;
            }
            if i == 0 {
                score += 10;
            }
            if last.is_some_and(|l| l + 1 == i) {
                score += 5;
            }
            last = Some(i);
            at += 1;
        }
        prev = Some(ch);
    }
    (at == needle.len()).then_some(score)
}
