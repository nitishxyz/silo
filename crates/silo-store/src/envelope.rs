//! Sealed envelope format:
//!
//! ```text
//! "SILO" | nonce (24) | ciphertext+tag
//! ```

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::error::{Error, Result};
use crate::key::HostKey;

pub const MAGIC: &[u8; 4] = b"SILO";
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + NONCE_LEN;

fn cipher(key: &HostKey) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(key.as_bytes().into())
}

/// Encrypt `plaintext`, authenticating `aad`, with a fresh random nonce.
pub fn seal(key: &HostKey, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = cipher(key).encrypt(&nonce, Payload { msg: plaintext, aad }).expect("XChaCha20Poly1305 encryption cannot fail for in-memory buffers");
    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// Decrypt an envelope produced by [`seal`] with the same key and `aad`.
///
/// Returns [`Error::Corrupt`] for malformed or truncated input and
/// [`Error::AuthFailed`] when authentication fails (wrong key, wrong AAD or
/// tampered contents).
pub fn open(key: &HostKey, aad: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < HEADER_LEN + TAG_LEN || &bytes[..MAGIC.len()] != MAGIC {
        return Err(Error::Corrupt);
    }
    let nonce = XNonce::from_slice(&bytes[MAGIC.len()..HEADER_LEN]);
    cipher(key).decrypt(nonce, Payload { msg: &bytes[HEADER_LEN..], aad }).map_err(|_| Error::AuthFailed)
}
