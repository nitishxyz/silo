//! Native daemon connection and synchronous session projection.

pub mod autostart;
pub mod connection;
pub mod follower;
pub mod mirror;

pub use autostart::connect_or_start;
pub use connection::{ClientError, Connection, Events, Result, SessionCreateOptions, SessionUpdate};
pub use follower::{FollowerState, SessionFollower};
pub use mirror::{ApplyResult, SessionMirror, ToolState, ToolStatus};
