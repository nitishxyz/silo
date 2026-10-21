//! Device proofs: how a client process with an iroh endpoint of its own
//! authenticates as the paired device whose key it holds. Each process
//! (the app, every CLI run) needs its own endpoint id, since a relay keeps
//! one connection per endpoint id; pairing grants belong to the device
//! key. Carried by the control and upgrade hellos, so frozen like
//! [`crate::upgrade`]: it only ever gains optional fields.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The paired device `device` states, with its key's signature, that the
/// connecting endpoint speaks for it to this server (see
/// `silo_transport::Identity::prove`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeviceProof {
    /// The paired device's endpoint id.
    pub device: String,
    /// Lowercase hex Ed25519 signature by the device key over the server's
    /// and the connecting endpoint's ids.
    pub signature: String,
}
