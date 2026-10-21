//! Client side of the silo protocol: control connection, daemon discovery
//! and spawn, per-block data streams and the local terminal mirror.

pub mod cache;
mod control;
mod data;
pub mod hosts;
mod mirror;
pub mod mismatch;
mod predict;
mod remote;
pub mod runtime;
pub mod service;
mod spawn;
pub mod tunnel;
pub mod update;
pub mod upgrade;
mod upload;

pub use control::{
    Closed, Control, Events, IncompatibleProtocol, NotSupported, Offline, PING_EVERY, PING_TIMEOUT_IROH, PING_TIMEOUT_LOCAL, TimedOut, Unauthorized,
    Upgrading, advertise, set_role, set_token,
};
pub use data::{DataSender, DataStream};
pub use mirror::{Applied, Mirror, engine_config};
pub use mismatch::IncompatibleServer;
pub use predict::{PredCell, Predictor, RTT_OFF, RTT_ON, Rtt};
pub use remote::{client_identity, device_proof, offer_host, pair};
pub use spawn::{connect_or_spawn, server_running};
pub use upload::{Upload, UploadSource, upload};

pub use silo_proto as proto;
pub use silo_transport as transport;
pub use silo_vt as vt;
