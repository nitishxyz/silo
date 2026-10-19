//! Wire protocol shared by the silo server and its clients.
//!
//! Every stream (unix socket or QUIC stream) carries frames:
//! `u32 len (LE, kind + payload) | u8 kind | payload`.
//! The first frame of a stream decides its role: [`kind::CONTROL_HELLO`]
//! opens a JSON control stream, [`kind::DATA_HELLO`] attaches a binary data
//! stream to one block using a ticket obtained over control.

pub mod control;
pub mod data;
pub mod frame;
pub mod model;
pub mod paths;

#[cfg(test)]
mod build_id;

pub use frame::{Frame, read_frame, spawn_reader, write_frame};

/// Build id: a hash of every workspace source (see `build.rs`). Server and
/// client hellos carry it and must match exactly; there is no other
/// protocol versioning.
pub const BUILD: &str = env!("SILO_BUILD_ID");

/// Upper bound for any single frame.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

pub mod kind {
    /// JSON [`crate::control::ClientHello`].
    pub const CONTROL_HELLO: u8 = 0x01;
    /// JSON control message (request, response or event).
    pub const CONTROL: u8 = 0x02;
    /// JSON [`crate::data::DataHello`].
    pub const DATA_HELLO: u8 = 0x10;

    // Data stream, client -> server.
    pub const INPUT: u8 = 0x11;
    pub const RESIZE: u8 = 0x12;
    pub const ACK: u8 = 0x13;
    /// JSON [`crate::model::TerminalConfig`]: this client's desired config.
    pub const CONFIG: u8 = 0x14;

    // Data stream, server -> client.
    pub const WELCOME: u8 = 0x81;
    pub const REJECT: u8 = 0x82;
    pub const SNAPSHOT_HEAD: u8 = 0x83;
    pub const SNAPSHOT_HISTORY: u8 = 0x84;
    pub const OUTPUT: u8 = 0x85;
    pub const RESIZED: u8 = 0x86;
    pub const EXITED: u8 = 0x87;
    pub const SNAPSHOT_DONE: u8 = 0x88;
    /// JSON [`crate::model::TerminalConfig`]: the effective config from
    /// this point of the output stream on.
    pub const CONFIGURED: u8 = 0x89;
    /// `count: u64 LE | offset: u64 LE`, see [`crate::data::ServerData::InputAck`].
    pub const INPUT_ACK: u8 = 0x8a;
    /// `offset: u64 LE | flags: u8` (bit 0 echo, bit 1 icanon).
    pub const TTY_MODE: u8 = 0x8b;
}
