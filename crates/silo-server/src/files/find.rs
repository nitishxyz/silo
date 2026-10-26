//! `files.find`: in a git repo, the root's fff picker (the one `search.files`
//! uses, docs/FINDER.md §2); elsewhere, or when fff refuses the folder, a
//! `.gitignore`-aware walk of the root, fuzzy-scored. fff skips hidden files
//! outside repos (and reads no `.gitignore` there), so the walk keeps
//! finding them.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use silo_proto::{control::RpcError, files::FileFound};

use super::paths::{canonical_root, relative};
use crate::search::Search;

/// Directories never walked.
const SKIP: &[&str] = &[".git", "target", "node_modules"];
/// Most files one find looks at.
const WALK_MAX: usize = 200_000;
/// Longest one find walks.
const WALK_TIME: Duration = Duration::from_secs(2);

/// Files under `root` matching `query`, best first.
pub fn find(search: &Search, root: &Path, query: &str, limit: Option<u32>) -> Result<FileFound, RpcError> {
    let root = canonical_root(root)?;
    let limit = limit.unwrap_or(FileFound::DEFAULT_LIMIT).clamp(1, FileFound::MAX_LIMIT) as usize;
    if in_repo(&root) {
        match picked(search, &root, query, limit) {
            Ok(found) => return Ok(found),
            Err(error) => tracing::debug!(root = %root.display(), "files.find: walking instead of fff: {}", error.message),
        }
    }
    walk(&root, query, limit)
}

/// Whether `root` is in a git work tree (as fff finds it: then it walks
/// hidden files and reads `.gitignore`).
fn in_repo(root: &Path) -> bool {
    git2::Repository::discover(root).is_ok_and(|repo| repo.workdir().is_some())
}

/// [`find`] through the root's fff picker, waiting up to [`WALK_TIME`] for
/// its scan (`truncated` when it is still running).
fn picked(search: &Search, root: &Path, query: &str, limit: usize) -> Result<FileFound, RpcError> {
    let picker = search.picker(root)?;
    let truncated = !picker.wait_for_scan(WALK_TIME);
    let guard = picker.read().map_err(|e| RpcError::internal(format!("search: {e}")))?;
    let picker = guard.as_ref().ok_or_else(|| RpcError::internal("search: no picker"))?;
    let query = fff_search::QueryParser::new(fff_search::FileSearchConfig).parse(query);
    let options = fff_search::FuzzySearchOptions {
        max_threads: 0,
        project_path: Some(root),
        pagination: fff_search::PaginationArgs { offset: 0, limit },
        ..Default::default()
    };
    let found = picker.fuzzy_search(&query, None, options);
    let paths = found.items.iter().map(|item| item.relative_path(picker)).filter(|path| !path.split('/').any(|part| SKIP.contains(&part))).collect();
    Ok(FileFound { paths, truncated })
}

/// [`find`] by walking `root`.
fn walk(root: &Path, query: &str, limit: usize) -> Result<FileFound, RpcError> {
    let root = root.to_owned();
    let query: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    let walk = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_str().is_some_and(|n| SKIP.contains(&n)))
        .build();
    let started = Instant::now();
    let (mut seen, mut truncated) = (0, false);
    let mut found: Vec<(i64, String)> = Vec::new();
    for entry in walk {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        seen += 1;
        if seen > WALK_MAX || (seen % 256 == 0 && started.elapsed() > WALK_TIME) {
            truncated = true;
            break;
        }
        let Some(rel) = relative(&root, entry.path()) else { continue };
        if let Some(score) = score(&query, &rel) {
            found.push((score, rel));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())).then_with(|| a.1.cmp(&b.1)));
    found.truncate(limit);
    Ok(FileFound { paths: found.into_iter().map(|(_, p)| p).collect(), truncated })
}

/// How well `path` matches `query` (lowercase, no spaces), or `None` when
/// the query is not a subsequence of it. Matches in the file name,
/// consecutive matches and matches at word starts score higher; longer
/// paths score a little lower.
pub fn score(query: &[char], path: &str) -> Option<i64> {
    let chars: Vec<char> = path.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let name_start = chars.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
    let penalty = (chars.len() / 4) as i64;
    if query.is_empty() {
        return Some(-penalty);
    }
    // Forwards (earliest matches) and backwards (latest, so in the name).
    let forward = positions(query, &lower, false)?;
    let backward = positions(query, &lower, true)?;
    let rate = |at: &[usize]| -> i64 {
        let mut total = 0;
        for (i, &p) in at.iter().enumerate() {
            total += 1;
            if i > 0 && at[i - 1] + 1 == p {
                total += 5;
            }
            let prev = p.checked_sub(1).map(|q| chars[q]);
            let boundary =
                prev.is_none_or(|c| matches!(c, '/' | '_' | '-' | '.' | ' ')) || (prev.is_some_and(char::is_lowercase) && chars[p].is_uppercase());
            if boundary {
                total += 8;
            }
            if p >= name_start {
                total += 10;
            }
        }
        total
    };
    Some(rate(&forward).max(rate(&backward)) - penalty)
}

fn positions(query: &[char], lower: &[char], backwards: bool) -> Option<Vec<usize>> {
    let mut at = Vec::with_capacity(query.len());
    if backwards {
        let mut end = lower.len();
        for q in query.iter().rev() {
            end = lower[..end].iter().rposition(|c| c == q)?;
            at.push(end);
        }
        at.reverse();
    } else {
        let mut start = 0;
        for q in query {
            let p = start + lower[start..].iter().position(|c| c == q)?;
            at.push(p);
            start = p + 1;
        }
    }
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn subsequences_match_and_names_win() {
        assert!(score(&q("xyz"), "src/main.rs").is_none());
        let name = score(&q("main"), "src/main.rs").unwrap();
        let dir = score(&q("main"), "main/src/lib.rs").unwrap();
        assert!(name > dir, "{name} {dir}");
        let tight = score(&q("mod"), "src/model.rs").unwrap();
        let loose = score(&q("mod"), "src/my/other/dir.rs").unwrap();
        assert!(tight > loose, "{tight} {loose}");
        assert!(score(&q("mr"), "src/main.rs").is_some());
    }
}
