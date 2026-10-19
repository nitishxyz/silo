//! Upload streams: files and directories sent to a terminal block's host.
//!
//! The client obtains a ticket with `data.connect`, opens a stream and sends
//! [`UploadHello`], then for each entry an [`UploadEntry`] followed, for
//! files, by `UPLOAD_CHUNK` frames holding exactly `size` bytes, and finally
//! `UPLOAD_END`. The server answers [`Uploaded`] or, at any point, `REJECT`.

use serde::{Deserialize, Serialize};

/// Largest total size of one upload.
pub const MAX_UPLOAD: u64 = 2 * 1024 * 1024 * 1024;
/// Most entries (files and directories) in one upload.
pub const MAX_ENTRIES: usize = 10_000;
/// Largest `UPLOAD_CHUNK` payload.
pub const CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadHello {
    /// The client's [`crate::BUILD`]; must equal the server's.
    pub build: String,
    pub ticket: String,
    /// Sum of every file's size; at most [`MAX_UPLOAD`].
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadEntry {
    /// Path components; the first names a top-level item, a one-component
    /// path starts the next one. Deeper entries belong to the current item.
    pub path: Vec<String>,
    pub dir: bool,
    /// File size; 0 for directories.
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uploaded {
    /// Absolute path of every top-level item on the server's host.
    pub paths: Vec<String>,
}
