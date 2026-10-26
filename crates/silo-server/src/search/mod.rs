//! Search (docs/FINDER.md §2): `search.files`, `search.grep` and
//! `search.opened` through [fff](https://github.com/dmtrKovalenko/fff).
//! One fff picker per folder in use ([`cache::Pickers`]); frecency and query
//! history in LMDB under `<home>/search/`.
//!
//! A root must be the folder of a block the caller can view, or under it
//! ([`allowed`]); every path in a result is relative to it.

pub mod cache;

use std::path::{Path, PathBuf};

use fff_search as fff;
use serde_json::Value;
use silo_proto::{
    access::Rights,
    changes::FileStatus,
    control::{Method, RpcError},
    search::{SearchFileItem, SearchFiles, SearchFilesParams, SearchGrep, SearchGrepItem, SearchGrepMode, SearchGrepParams, SearchOpenedParams},
};

pub use cache::{Limits, Search};

use crate::{
    Server,
    access::{On, Principal, rights_on},
    files::paths,
};

/// `root` as a canonical directory: absolute, or `~`-relative.
///
/// # Errors
/// `bad_request` for a relative or malformed path or a non-directory;
/// `not_found` when missing.
fn canonical_root(root: &str) -> Result<PathBuf, RpcError> {
    if !(root.starts_with('/') || root == "~" || root.starts_with("~/")) {
        return Err(RpcError::bad_request(format!("root {root} must be an absolute path")));
    }
    paths::new_root(None, root)
}

/// The folders `principal` may search under: those of the blocks it holds
/// `wanted` on (as the registry has them; canonicalized by [`allowed`]).
fn folders(server: &Server, principal: &Principal, wanted: Rights) -> Vec<String> {
    let reg = server.registry.lock().unwrap();
    reg.blocks
        .iter()
        .filter_map(|(id, block)| {
            let session = reg.block_session(id);
            let held = rights_on(principal, On::Block { block: id, session: session.as_deref() })?;
            (held >= wanted).then(|| block.info()?.cwd.filter(|c| !c.is_empty()))?
        })
        .collect()
}

/// Whether canonical `root` is one of `folders` (canonicalized here) or
/// under one.
fn under_any(root: &Path, folders: &[String]) -> bool {
    folders.iter().filter_map(|f| paths::new_root(None, f).ok()).any(|f| paths::inside(&f, root))
}

/// `root` canonical, when `principal` may search it with `wanted` rights.
///
/// # Errors
/// See [`canonical_root`]; `forbidden` outside every folder it can view.
pub async fn allowed(server: &Server, principal: &Principal, root: &str, wanted: Rights) -> Result<PathBuf, RpcError> {
    let folders = folders(server, principal, wanted);
    let root = root.to_owned();
    blocking(move || {
        let canonical = canonical_root(&root)?;
        if !under_any(&canonical, &folders) {
            return Err(RpcError::denied(format!("{root} is not under the folder of a block you can view")));
        }
        Ok(canonical)
    })
    .await
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, RpcError> + Send + 'static) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(f).await.map_err(RpcError::internal)?
}

fn fff_error(error: fff::Error) -> RpcError {
    RpcError::internal(format!("search: {error}"))
}

pub async fn handle(server: &Server, principal: &Principal, method: Method) -> Result<Value, RpcError> {
    let search = server.search.clone();
    match method {
        Method::SearchFiles(p) => {
            let root = allowed(server, principal, &p.root, Rights::View).await?;
            to_value(&blocking(move || files(&search, &root, &p)).await?)
        }
        Method::SearchGrep(p) => {
            let root = allowed(server, principal, &p.root, Rights::View).await?;
            to_value(&blocking(move || grep(&search, &root, &p)).await?)
        }
        Method::SearchOpened(p) => {
            let root = allowed(server, principal, &p.root, Rights::View).await?;
            blocking(move || opened(&search, &root, &p)).await?;
            Ok(serde_json::json!({}))
        }
        other => Err(RpcError::method_not_found(&silo_proto::api::method_name(&other))),
    }
}

fn to_value(value: &impl serde::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::internal)
}

/// A file's git status as the Changes block names it; `None` when clean.
fn git_status(status: Option<git2::Status>) -> Option<FileStatus> {
    use git2::Status as S;
    let s = status?;
    Some(if s.is_conflicted() {
        FileStatus::Conflicted
    } else if s.intersects(S::WT_NEW) {
        FileStatus::Untracked
    } else if s.intersects(S::INDEX_NEW) {
        FileStatus::Added
    } else if s.intersects(S::WT_DELETED | S::INDEX_DELETED) {
        FileStatus::Deleted
    } else if s.intersects(S::WT_RENAMED | S::INDEX_RENAMED) {
        FileStatus::Renamed
    } else if s.intersects(S::WT_TYPECHANGE | S::INDEX_TYPECHANGE) {
        FileStatus::TypeChanged
    } else if s.intersects(S::WT_MODIFIED | S::INDEX_MODIFIED) {
        FileStatus::Modified
    } else {
        return None;
    })
}

/// `search.files` on canonical `root`.
pub fn files(search: &Search, root: &Path, p: &SearchFilesParams) -> Result<SearchFiles, RpcError> {
    let limit = p.limit.unwrap_or(SearchFiles::DEFAULT_LIMIT).clamp(1, SearchFiles::MAX_LIMIT) as usize;
    let picker = search.picker(root)?;
    let queries = search.queries();
    let guard = picker.read().map_err(fff_error)?;
    let Some(picker) = guard.as_ref() else { return Ok(SearchFiles { indexing: true, ..SearchFiles::default() }) };
    let indexing = picker.get_scan_progress().is_scanning;
    let parser = fff::QueryParser::new(fff::FileSearchConfig);
    let query = parser.parse(&p.query);
    let tracker = queries.read().ok();
    let found = picker.fuzzy_search(
        &query,
        tracker.as_ref().and_then(|t| t.as_ref()),
        fff::FuzzySearchOptions {
            max_threads: 0,
            project_path: Some(root),
            combo_boost_score_multiplier: COMBO_BOOST,
            min_combo_count: COMBO_MIN,
            pagination: fff::PaginationArgs { offset: 0, limit },
            ..Default::default()
        },
    );
    let items = found
        .items
        .iter()
        .zip(&found.scores)
        .map(|(item, score)| SearchFileItem {
            path: item.relative_path(picker),
            name: item.file_name(picker),
            git: git_status(item.git_status),
            score: i64::from(score.total),
        })
        .collect();
    Ok(SearchFiles { items, total: found.total_matched as u64, indexing })
}

/// fff's boost for the file last opened from the same query (its Neovim
/// defaults).
const COMBO_BOOST: i32 = 100;
const COMBO_MIN: u32 = 3;

/// `text` cut at `max` bytes on a character boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `search.grep` on canonical `root`.
pub fn grep(search: &Search, root: &Path, p: &SearchGrepParams) -> Result<SearchGrep, RpcError> {
    let limit = p.limit.unwrap_or(SearchGrep::DEFAULT_LIMIT).clamp(1, SearchGrep::MAX_LIMIT) as usize;
    if p.query.trim().is_empty() {
        return Ok(SearchGrep::default());
    }
    let picker = search.picker(root)?;
    let guard = picker.read().map_err(fff_error)?;
    let Some(picker) = guard.as_ref() else { return Ok(SearchGrep { indexing: true, ..SearchGrep::default() }) };
    let indexing = picker.get_scan_progress().is_scanning;
    let mode = match p.mode.unwrap_or_default() {
        SearchGrepMode::Regex => fff::GrepMode::Regex,
        SearchGrepMode::Fuzzy => fff::GrepMode::Fuzzy,
        SearchGrepMode::Plain | SearchGrepMode::Unknown => fff::GrepMode::PlainText,
    };
    let parser = fff::QueryParser::new(fff::GrepConfig);
    let query = parser.parse(&p.query);
    let options =
        fff::GrepSearchOptions { page_limit: limit, mode, time_budget_ms: SearchGrep::BUDGET_MS, enforce_time_budget: true, ..Default::default() };
    let found = picker.grep(&query, &options);
    if let (fff::GrepMode::Regex, Some(error)) = (mode, &found.regex_fallback_error) {
        return Err(RpcError::bad_request(format!("not a valid regular expression: {error}")));
    }
    let mut items: Vec<SearchGrepItem> = found
        .matches
        .iter()
        .filter_map(|m| {
            let file = found.files.get(m.file_index)?;
            let text = cut(&m.line_content, SearchGrep::LINE_MAX);
            let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
            let ranges = m.match_byte_offsets.iter().filter(|(s, _)| *s < len).map(|&(s, e)| [s, e.min(len)]).collect();
            let col_bytes = m.col.min(m.line_content.len());
            let col = m.line_content.get(..col_bytes).map_or(col_bytes, |before| before.chars().count());
            Some(SearchGrepItem {
                path: file.relative_path(picker),
                line: m.line_number,
                col: u32::try_from(col).unwrap_or(u32::MAX).saturating_add(1),
                text: text.to_owned(),
                ranges,
                git: git_status(file.git_status),
            })
        })
        .collect();
    let total = items.len() as u64;
    let truncated = found.next_file_offset != 0 || items.len() >= limit;
    items.truncate(limit);
    if let Ok(mut queries) = search.queries().write()
        && let Some(queries) = queries.as_mut()
        && let Err(error) = queries.track_grep_query(&p.query, root)
    {
        tracing::debug!("search: grep history: {error}");
    }
    Ok(SearchGrep { items, total, indexing, truncated })
}

/// `search.opened` on canonical `root`: frecency for the file.
pub fn opened(search: &Search, root: &Path, p: &SearchOpenedParams) -> Result<(), RpcError> {
    let path = paths::resolve(root, &p.path)?;
    if !path.is_file() {
        return Err(RpcError::bad_request(format!("{} is not a file", p.path)));
    }
    let frecency = search.frecency();
    {
        let guard = frecency.read().map_err(fff_error)?;
        let Some(tracker) = guard.as_ref() else { return Ok(()) };
        tracker.track_access(&path).map_err(fff_error)?;
        if let Some(picker) = search.cached(root)
            && let Some(picker) = picker.write().map_err(fff_error)?.as_mut()
        {
            picker.update_single_file_frecency(&path, tracker).map_err(fff_error)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_keeps_characters_whole() {
        assert_eq!(cut("héllo", 2), "h");
        assert_eq!(cut("héllo", 3), "hé");
        assert_eq!(cut("abc", 10), "abc");
    }

    #[test]
    fn git_statuses_read_as_the_changes_block_names_them() {
        use git2::Status as S;
        assert_eq!(git_status(None), None);
        assert_eq!(git_status(Some(S::CURRENT)), None);
        assert_eq!(git_status(Some(S::WT_MODIFIED)), Some(FileStatus::Modified));
        assert_eq!(git_status(Some(S::INDEX_NEW)), Some(FileStatus::Added));
        assert_eq!(git_status(Some(S::WT_NEW)), Some(FileStatus::Untracked));
        assert_eq!(git_status(Some(S::CONFLICTED | S::WT_MODIFIED)), Some(FileStatus::Conflicted));
    }
}
