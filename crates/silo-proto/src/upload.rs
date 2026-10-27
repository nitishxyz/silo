//! Upload streams: files and directories sent to a terminal block's host
//! (into a fresh staging folder) or into a files block's folder
//! ([`UploadHello::to_dir`], feature [`crate::compat::feature::FILES_OPS`]).
//!
//! The client obtains a ticket with `data.connect`, opens a stream and sends
//! [`UploadHello`], then for each entry an [`UploadEntry`] followed, for
//! files, by `UPLOAD_CHUNK` frames holding exactly `size` bytes, and finally
//! `UPLOAD_END`. The server answers [`Uploaded`] or, at any point, `REJECT`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Largest total size of one upload.
pub const MAX_UPLOAD: u64 = 2 * 1024 * 1024 * 1024;
/// Most entries (files and directories) in one upload.
pub const MAX_ENTRIES: usize = 10_000;
/// Largest `UPLOAD_CHUNK` payload.
pub const CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct UploadHello {
    /// The client's [`crate::PROTOCOL`] and [`crate::VT`]; must equal the
    /// server's.
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub vt: String,
    /// The client's [`crate::BUILD`], for display.
    pub build: String,
    pub ticket: String,
    /// Sum of every file's size; at most [`MAX_UPLOAD`].
    pub total: u64,
    /// Files blocks only (and required there): the folder the items land
    /// in, relative to the block's root (empty: the root). A taken name
    /// gets Finder's next free one (`name 2.ext`). Terminal blocks refuse
    /// it; older servers refuse uploads to files blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_dir: Option<String>,
    /// With `to_dir`: replace items with the same name (they go to the
    /// Trash first) instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace: bool,
}

impl UploadHello {
    /// This build's hello for `ticket`.
    #[must_use]
    pub fn ours(ticket: String, total: u64) -> Self {
        Self { protocol: crate::PROTOCOL, vt: crate::VT.to_owned(), build: crate::BUILD.to_owned(), ticket, total, to_dir: None, replace: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UploadEntry {
    /// Path components; the first names a top-level item, a one-component
    /// path starts the next one. Deeper entries belong to the current item.
    pub path: Vec<String>,
    pub dir: bool,
    /// File size; 0 for directories.
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Uploaded {
    /// Absolute path of every top-level item on the server's host; for a
    /// files block, relative to its root.
    pub paths: Vec<String>,
}
