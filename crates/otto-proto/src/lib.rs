//! Shared, wasm-safe contract for the otto server and its clients.
//!
//! Streams carry `u32 len (LE, kind + payload) | u8 kind | payload`.
//! A HELLO frame establishes identity, then CONTROL frames carry JSON.
//! Async framing is available through the non-default `io` feature.

pub mod api;
pub mod clip;
pub mod control;
pub mod frame;
pub mod mcp;
pub mod model;
pub mod paths;
pub mod timing;

pub use control::*;
pub use frame::Frame;
#[cfg(feature = "io")]
pub use frame::{read_frame, spawn_reader, write_frame};
pub use mcp::*;
pub use model::*;
pub use otto_llm::{Effort, Item, Mention, MentionKind, ModelInfo, Part, Role, StreamEvent, Usage};
pub use timing::PartTime;

/// Bump for incompatible wire changes; additive changes update `protocol.json`.
pub const PROTOCOL: u32 = 1;
/// Maximum frame length, including the kind byte.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Largest attachment (`attachment.put`, all chunks together).
pub const MAX_ATTACHMENT: usize = 25 * 1024 * 1024;
/// Largest decoded chunk a client should send in one `attachment.put`.
pub const ATTACHMENT_CHUNK: usize = 8 * 1024 * 1024;
pub const BINARY: &str = "otto-silo";
pub const ENV_PREFIX: &str = "OTTO_SILO";
pub const PROJECT_STATE_DIR: &str = ".otto-silo";
pub const PROJECT_SHARED_DIR: &str = ".otto";

pub mod kind {
    /// JSON client or server hello.
    pub const HELLO: u8 = 0x01;
    /// JSON request, response or event.
    pub const CONTROL: u8 = 0x02;
}
