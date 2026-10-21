//! Wire protocol shared by the silo server and its clients.
//!
//! Every stream (unix socket or QUIC stream) carries frames:
//! `u32 len (LE, kind + payload) | u8 kind | payload`.
//! The first frame of a stream decides its role: [`kind::CONTROL_HELLO`]
//! opens a JSON control stream, [`kind::DATA_HELLO`] attaches a binary data
//! stream to one block using a ticket obtained over control,
//! [`kind::UPLOAD_HELLO`] uploads files for one block with such a ticket.

pub mod access;
pub mod api;
pub mod compat;
pub mod control;
pub mod data;
pub mod device;
pub mod drops;
pub mod frame;
pub mod model;
pub mod offer;
pub mod paths;
pub mod release;
pub mod text;
pub mod upgrade;
pub mod upload;

#[cfg(test)]
mod build_id;

pub use frame::{Frame, read_frame, spawn_reader, write_frame};

/// Build id: a hash of every workspace source (see `build.rs`). Hellos
/// carry it for display and upgrade decisions; peers of any build work
/// together when they speak the same [`PROTOCOL`] and [`VT`].
pub const BUILD: &str = env!("SILO_BUILD_ID");

/// Wire compatibility number, bumped only for incompatible changes (see
/// [`compat`] for the additive rules and `protocol.json`).
pub const PROTOCOL: u32 = 1;

/// Terminal snapshot format: libghostty's GHOSTSNP records of the pinned
/// ghostty revision, which carry no compatibility guarantee across
/// revisions, so peers must agree on it exactly.
pub const VT: &str = env!("SILO_VT_FORMAT");

/// The install script (`curl -fsSL … | sh`) for the CLI and server.
pub const INSTALL_URL: &str = "https://nitishxyz.github.io/silo/install.sh";

/// Exit status of `silo server --takeover` when the running server closed
/// on the upgrade request: it predates live upgrade and keeps serving.
pub const EXIT_NOT_UPGRADABLE: i32 = 3;

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
    /// `request: u64 LE | content`, see [`crate::data::ClientData::ClipboardContent`].
    pub const CLIPBOARD_CONTENT: u8 = 0x15;
    /// `content`, see [`crate::data::ClientData::ClipboardPush`].
    pub const CLIPBOARD_PUSH: u8 = 0x16;

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
    /// `location: u8 | text`, see [`crate::data::ServerData::ClipboardWrite`].
    pub const CLIPBOARD_WRITE: u8 = 0x8c;
    /// `request: u64 LE | kind: u8`, see [`crate::data::ServerData::ClipboardRead`].
    pub const CLIPBOARD_READ: u8 = 0x8d;
    /// JSON [`crate::data::CommandEnd`]: a command finished in the output
    /// just sent.
    pub const COMMAND_END: u8 = 0x8f;
    /// JSON `[`[`crate::data::CommandAnchor`]`]`: where the snapshot's
    /// commands ended (sent right after `SNAPSHOT_HEAD`).
    pub const COMMANDS: u8 = 0x90;

    /// JSON [`crate::upload::UploadHello`].
    pub const UPLOAD_HELLO: u8 = 0x20;
    // Upload stream, client -> server.
    /// JSON [`crate::upload::UploadEntry`].
    pub const UPLOAD_ENTRY: u8 = 0x21;
    /// Bytes of the current file.
    pub const UPLOAD_CHUNK: u8 = 0x22;
    /// Every entry was sent.
    pub const UPLOAD_END: u8 = 0x23;
    // Upload stream, server -> client (or `REJECT`).
    /// JSON [`crate::upload::Uploaded`].
    pub const UPLOADED: u8 = 0x8e;

    // Upgrade stream (stable across builds, see `crate::upgrade`).
    /// JSON [`crate::upgrade::UpgradeHello`].
    pub const UPGRADE_HELLO: u8 = 0x30;
    /// JSON [`crate::upgrade::UpgradeBinary`].
    pub const UPGRADE_BINARY: u8 = 0x31;
    /// Bytes of the binary.
    pub const UPGRADE_CHUNK: u8 = 0x32;
    /// JSON [`crate::upgrade::HostBuild`].
    pub const HOST_BUILD: u8 = 0xb0;
    /// JSON [`crate::upgrade::UpgradeStarted`].
    pub const UPGRADE_STARTED: u8 = 0xb1;
    /// JSON [`crate::upgrade::UpgradeRejected`].
    pub const UPGRADE_REJECTED: u8 = 0xb2;
}
