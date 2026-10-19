//! Client side of the silo protocol: control connection, daemon discovery
//! and spawn, per-block data streams and the local terminal mirror.

pub mod cache;
mod control;
mod data;
mod mirror;
pub mod mismatch;
mod remote;
mod spawn;

pub use control::{BuildMismatch, Control, Events, Offline, Unauthorized, Upgrading};
pub use remote::{client_identity, pair};
pub use mismatch::DifferentBuild;
pub use data::{DataSender, DataStream};
pub use mirror::{Applied, Mirror, engine_config};
pub use spawn::{connect_or_spawn, server_running};

pub use silo_proto as proto;
pub use silo_transport as transport;
pub use silo_vt as vt;
