//! Search (docs/FINDER.md §2): fuzzy file names and file contents under a
//! folder on the server's machine, through [fff](https://github.com/dmtrKovalenko/fff).
//! Feature [`crate::compat::feature::SEARCH`]; older servers answer
//! `method_not_found`.
//!
//! `root` is an absolute directory (`~` is the home directory): the folder
//! of a block in a session the caller can see, or one under it. Every path
//! in a result is relative to it, `/`-separated.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::changes::FileStatus;

/// `search.files`: files under `root` whose path fuzzy-matches `query`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchFilesParams {
    pub root: String,
    /// fff's query syntax: fuzzy words, globs (`*.rs`, `src/**`),
    /// excludes (`!test`), `git:modified` (`staged`, `untracked`, …) and
    /// `path:line:col` (the location part is ignored for matching).
    #[serde(default)]
    pub query: String,
    /// Most items returned (default [`SearchFiles::DEFAULT_LIMIT`], at most
    /// [`SearchFiles::MAX_LIMIT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A file found by `search.files`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchFileItem {
    /// Relative to the root.
    pub path: String,
    /// The file name (the last part of `path`).
    pub name: String,
    /// Its git status when it differs from `HEAD` (or is untracked);
    /// `None` for a clean file or outside a repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<FileStatus>,
    /// fff's total score (fuzzy match plus frecency and git boosts);
    /// higher is better. Only the order means anything.
    #[serde(default)]
    pub score: i64,
}

/// `search.files`'s result: best match first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchFiles {
    pub items: Vec<SearchFileItem>,
    /// Files matching in all (`items` holds at most `limit` of them).
    #[serde(default)]
    pub total: u64,
    /// The folder's first scan is still running: files may be missing.
    #[serde(default)]
    pub indexing: bool,
}

impl SearchFiles {
    pub const DEFAULT_LIMIT: u32 = 50;
    pub const MAX_LIMIT: u32 = 200;
}

/// How `search.grep` reads its query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchGrepMode {
    /// The text as written (smart case: case-insensitive unless it has an
    /// uppercase letter).
    #[default]
    Plain,
    /// A regular expression (ripgrep's syntax).
    Regex,
    /// Fuzzy: tolerates a couple of typos or gaps.
    Fuzzy,
    /// A mode of a newer client; searched as [`Self::Plain`].
    #[serde(other)]
    Unknown,
}

/// `search.grep`: lines of the files under `root` matching `query`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchGrepParams {
    pub root: String,
    /// The text sought. Constraints of fff's query syntax (globs such as
    /// `*.rs`, `!excludes`, `git:modified`) narrow the files searched.
    pub query: String,
    /// Default [`SearchGrepMode::Plain`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SearchGrepMode>,
    /// Most matches returned (default [`SearchGrep::DEFAULT_LIMIT`], at most
    /// [`SearchGrep::MAX_LIMIT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A matching line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchGrepItem {
    /// Relative to the root.
    pub path: String,
    /// 1-based.
    pub line: u64,
    /// 1-based column of the first match, in characters.
    pub col: u32,
    /// The line, cut at [`SearchGrep::LINE_MAX`] bytes (on a character
    /// boundary).
    pub text: String,
    /// The matches in `text`: byte offsets `[start, end)`.
    #[serde(default)]
    pub ranges: Vec<[u32; 2]>,
}

/// `search.grep`'s result: files in fff's order (frecency first), lines in
/// file order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchGrep {
    pub items: Vec<SearchGrepItem>,
    /// Matches found (at least `items.len()`; more when cut at `limit`).
    #[serde(default)]
    pub total: u64,
    /// The folder's first scan is still running: files may be missing.
    #[serde(default)]
    pub indexing: bool,
    /// The search stopped early (its time budget ran out, or `limit` was
    /// reached before every file was searched): more matches may exist.
    #[serde(default)]
    pub truncated: bool,
}

impl SearchGrep {
    pub const DEFAULT_LIMIT: u32 = 100;
    pub const MAX_LIMIT: u32 = 500;
    /// Longest `text`.
    pub const LINE_MAX: usize = 512;
    /// How long one grep searches before it answers with what it found.
    pub const BUDGET_MS: u64 = 300;
}

/// `search.opened`: a file under `root` was opened (by ⌘P or a Files
/// block), for frecency: it ranks higher in later searches.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchOpenedParams {
    pub root: String,
    /// Relative to the root.
    pub path: String,
}
