//! Files blocks (docs/FILES_BLOCK.md): a read-only view of a directory on
//! the server's machine. Every path is relative to the block's root
//! ([`crate::model::FilesState::root`]); the empty path is the root.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::BlockId;

/// `files.list`: one directory's entries.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FilesListParams {
    pub block_id: BlockId,
    /// The directory; empty (default) for the root.
    #[serde(default)]
    pub path: String,
}

/// `files.read`: a file's contents as text, or what it is when binary.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FilesReadParams {
    pub block_id: BlockId,
    pub path: String,
    /// Read at most this many bytes (default [`FileRead::DEFAULT_BYTES`],
    /// at most [`FileRead::MAX_BYTES`]).
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// `files.read_image`: an image's bytes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FilesReadImageParams {
    pub block_id: BlockId,
    pub path: String,
}

/// `files.find`: files under the root whose path fuzzy-matches `query`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FilesFindParams {
    pub block_id: BlockId,
    #[serde(default)]
    pub query: String,
    /// Most paths returned (default [`FileFound::DEFAULT_LIMIT`], at most
    /// [`FileFound::MAX_LIMIT`]).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `files.set`: change the block's view state; omitted fields stay.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct FilesSetParams {
    pub block_id: BlockId,
    /// Open tabs, in order (replaces the list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<Vec<String>>,
    /// The tab shown; empty for none. A path not open is opened (appended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Whether the tree is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<bool>,
    /// Expanded directories (replaces the list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<Vec<String>>,
    /// Top line per tab, merged into the stored map (tabs not open are
    /// dropped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<BTreeMap<String, u64>>,
    /// Marked lines per tab, replacing that tab's (an empty list clears
    /// them). Applied after `open` and `active`; paths still not open are
    /// dropped. Older servers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlights: Option<BTreeMap<String, Vec<crate::model::FileHighlight>>>,
}

/// `files.root`: show another directory; the view state starts afresh.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FilesRootParams {
    pub block_id: BlockId,
    /// An absolute path (`~` is the home directory), or one relative to
    /// the current root.
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    /// Sockets, fifos, devices; and kinds of a newer server.
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileEntry {
    pub name: String,
    pub kind: FileKind,
    /// Symlinks: what they point to when that is inside the root (`file`
    /// or `dir`); `None` for a dangling link or one leaving the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<FileKind>,
    /// Bytes (of the target, for symlinks).
    #[serde(default)]
    pub size: u64,
    /// Last modification, unix ms.
    #[serde(default)]
    pub mtime: u64,
}

/// `files.list`'s result: directories first, then by name.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FileList {
    pub path: String,
    pub entries: Vec<FileEntry>,
    /// More than [`FileList::MAX_ENTRIES`] entries; the rest are left out.
    #[serde(default)]
    pub truncated: bool,
}

impl FileList {
    pub const MAX_ENTRIES: usize = 10_000;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ContentKind {
    /// UTF-8 text, in `text`.
    Text,
    /// Not text: only `mime` and `size`.
    Binary,
    #[serde(other)]
    Unknown,
}

/// `files.read`'s result.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FileRead {
    pub path: String,
    pub kind: ContentKind,
    /// The contents (`kind: text`), cut at `max_bytes` on a character
    /// boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Guessed from the name (and contents); `text/plain` for other text,
    /// `application/octet-stream` for other binaries.
    pub mime: String,
    /// The file's whole size in bytes.
    pub size: u64,
    /// Last modification, unix ms.
    pub mtime: u64,
    /// `text` holds less than the whole file.
    #[serde(default)]
    pub truncated: bool,
}

impl FileRead {
    pub const DEFAULT_BYTES: u64 = 2 * 1024 * 1024;
    /// Most bytes one read returns: a reply must fit in a frame
    /// ([`crate::MAX_FRAME`]).
    pub const MAX_BYTES: u64 = 12 * 1024 * 1024;
}

/// `files.read_image`'s result.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FileImage {
    pub path: String,
    /// `image/png`, `image/jpeg`, `image/gif`, `image/webp` or
    /// `image/svg+xml`.
    pub mime: String,
    pub size: u64,
    pub mtime: u64,
    /// The bytes, base64 (standard, padded).
    pub data_base64: String,
}

impl FileImage {
    /// Largest image sent: its base64 must fit in a frame
    /// ([`crate::MAX_FRAME`]).
    pub const MAX_BYTES: u64 = 11 * 1024 * 1024;
}

/// `files.find`'s result: best match first.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FileFound {
    pub paths: Vec<String>,
    /// The walk stopped early (too many files, or out of time): matches
    /// may be missing.
    #[serde(default)]
    pub truncated: bool,
}

impl FileFound {
    pub const DEFAULT_LIMIT: u32 = 50;
    pub const MAX_LIMIT: u32 = 1000;
}
