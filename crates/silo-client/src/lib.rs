//! Client side of the silo protocol: control connection, daemon discovery
//! and spawn, per-block data streams and the local terminal mirror.

pub mod cache;
mod control;
mod data;
mod mirror;
pub mod mismatch;
mod predict;
mod remote;
pub mod runtime;
pub mod service;
mod spawn;
pub mod tunnel;

pub use control::{
    BuildMismatch, Closed, Control, Events, Offline, PING_EVERY, PING_TIMEOUT_IROH, PING_TIMEOUT_LOCAL, TimedOut, Unauthorized, Upgrading,
};
pub use data::{DataSender, DataStream};
pub use mirror::{Applied, Mirror, engine_config};
pub use mismatch::DifferentBuild;
pub use predict::{PredCell, Predictor, RTT_OFF, RTT_ON, Rtt};
pub use remote::{client_identity, offer_host, pair};
pub use spawn::{connect_or_spawn, server_running};

pub use silo_proto as proto;
pub use silo_transport as transport;
pub use silo_vt as vt;
