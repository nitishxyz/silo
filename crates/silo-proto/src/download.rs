//! Download streams: a files block's items, sent from the server's host
//! (feature [`crate::compat::feature::FILES_OPS`]).
//!
//! The client obtains a ticket for the files block with `data.connect`,
//! opens a stream and sends [`DownloadHello`] then [`DownloadRequest`]
//! (apart, so a remote hello stays small). The server walks the items
//! (each relative to the block's root and confined to it; symlinks inside
//! folders are skipped) and answers [`DownloadPlan`], then mirrors an
//! upload: for each entry an [`crate::upload::UploadEntry`] (`DOWNLOAD_ENTRY`)
//! followed, for files, by `DOWNLOAD_CHUNK` frames holding exactly `size`
//! bytes, and finally `DOWNLOAD_END`; or, at any point, `REJECT`. The
//! client cancels by closing the stream.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Most entries (files and folders) in one download.
pub const MAX_ENTRIES: usize = 100_000;
/// Most top-level items one request names.
pub const MAX_PATHS: usize = 10_000;
/// Largest `DOWNLOAD_CHUNK` payload.
pub const CHUNK: usize = crate::upload::CHUNK;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DownloadHello {
    /// The client's [`crate::PROTOCOL`] and [`crate::VT`]; must equal the
    /// server's.
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub vt: String,
    /// The client's [`crate::BUILD`], for display.
    pub build: String,
    pub ticket: String,
}

impl DownloadHello {
    /// This build's hello for `ticket`.
    #[must_use]
    pub fn ours(ticket: String) -> Self {
        Self { protocol: crate::PROTOCOL, vt: crate::VT.to_owned(), build: crate::BUILD.to_owned(), ticket }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DownloadRequest {
    /// Items relative to the block's root; each is a top-level entry named
    /// by its last component.
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DownloadPlan {
    /// Sum of every file's size.
    pub total: u64,
    /// Files and folders to come.
    pub entries: u64,
}
