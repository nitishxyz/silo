//! Shared durable agent runtime for local and server clients.
pub mod agents;
mod approvals;
mod compaction;
mod deltas;
mod hooks;
mod prepared;
mod prompt;
mod recovery;
mod runtime;
mod session;
pub mod skills;
mod spills;
mod step;
mod subagents;
mod tools;
pub use runtime::*;
pub use session::SessionHandle;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] otto_store::Error),
    #[error(transparent)]
    Model(#[from] otto_llm::LlmError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("session actor stopped")]
    Closed,
}
pub type Result<T> = std::result::Result<T, Error>;
pub(crate) fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
