//! Encrypted durable storage for silo: host key, AEAD envelope, atomic IO
//! and a generation-based snapshot store.

mod envelope;
mod error;
mod fsio;
mod key;
mod store;

pub use envelope::{MAGIC, open, seal};
pub use error::{Error, Result};
pub use fsio::{read_sealed, write_atomic, write_sealed};
pub use key::{HostKey, KEY_FILE, KEY_LEN};
pub use store::{LoadReport, LoadedSnapshot, MANIFEST_FILE, SNAPSHOTS_DIR, SnapshotError, Store};

#[cfg(test)]
mod tests;
