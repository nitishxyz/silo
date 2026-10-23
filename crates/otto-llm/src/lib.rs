//! Provider clients and the generic model interface (docs/OTTO.md §4).
//!
//! Two layers: native clients per provider ([`anthropic`], [`openai`] (also
//! xAI), [`google`], [`compat`]) that mirror each API in full, and the
//! [`Model`] trait the harness calls,
//! implemented by each provider on top of its native client. The canonical
//! transcript ([`types`]) is lossless per provider: opaque items are kept and
//! replayed byte-exact to the provider that produced them.

#[cfg(feature = "client")]
pub mod auth;
#[cfg(feature = "client")]
pub mod http;
#[cfg(feature = "client")]
pub mod providers;
#[cfg(feature = "client")]
pub mod sse;
#[cfg(feature = "client")]
#[doc(hidden)]
pub mod testing;

pub mod anthropic;
pub mod catalog;
pub mod compat;
pub mod error;
pub mod event;
pub mod google;
pub mod model;
pub mod openai;
pub mod request;
pub mod types;

pub use error::{ErrorClass, LlmError};
#[cfg(feature = "client")]
pub use event::EventStream;
pub use event::{FinishReason, StreamEvent, Usage};
#[cfg(feature = "client")]
pub use model::{CallCx, Model, ModelSession};
pub use model::{Caps, ModelInfo, Pricing, ToolFamily};
#[cfg(feature = "client")]
pub use providers::Providers;
pub use request::{CacheOptions, Compaction, Effort, ProviderOptions, Reasoning, Request, ToolChoice, ToolDef, ToolKind};
pub use types::{CallKind, Citation, Item, ItemId, Media, MediaSource, Mention, MentionKind, Opaque, Origin, Part, ProviderId, ResultPart, Role};
