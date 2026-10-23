//! Native daemon connection and synchronous session projection.

pub mod autostart;
pub mod connection;
pub mod follower;
pub mod mirror;

pub use autostart::{connect_or_start, ensure_started};
pub use connection::{ClientError, Connection, Dial, Events, Result, SessionCreateOptions, SessionUpdate, Stream};
pub use follower::{FollowerState, SessionFollower};
pub use mirror::{ApplyResult, SessionMirror, ToolState, ToolStatus};
