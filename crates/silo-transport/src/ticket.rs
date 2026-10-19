use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use data_encoding::BASE32_NOPAD_NOCASE;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

pub const TICKET_PREFIX: &str = "silo1";
pub const INVITE_SECRET_LEN: usize = 32;

/// Pairing invite: how to reach the server plus a one-time secret.
/// Encoded as `silo1` + lowercase unpadded base32 of postcard bytes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairTicket {
    pub addr: EndpointAddr,
    pub secret: [u8; INVITE_SECRET_LEN],
}

impl fmt::Debug for PairTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairTicket").field("addr", &self.addr).field("secret", &"<redacted>").finish()
    }
}

impl PairTicket {
    pub fn encode(&self) -> String {
        let bytes = postcard::to_stdvec(self).expect("PairTicket serializes");
        let body = BASE32_NOPAD_NOCASE.encode(&bytes).to_ascii_lowercase();
        format!("{TICKET_PREFIX}{body}")
    }

    pub fn decode(s: &str) -> Result<PairTicket> {
        let s = s.trim();
        let Some(body) = s.get(..TICKET_PREFIX.len()).filter(|p| p.eq_ignore_ascii_case(TICKET_PREFIX)).map(|_| &s[TICKET_PREFIX.len()..]) else {
            bail!("invalid pairing ticket: missing {TICKET_PREFIX:?} prefix");
        };
        let bytes = BASE32_NOPAD_NOCASE.decode(body.as_bytes()).context("invalid pairing ticket: bad base32")?;
        let (ticket, rest): (PairTicket, _) = postcard::take_from_bytes(&bytes).context("invalid pairing ticket: bad payload")?;
        if !rest.is_empty() {
            bail!("invalid pairing ticket: {} trailing bytes", rest.len());
        }
        Ok(ticket)
    }
}

impl fmt::Display for PairTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl FromStr for PairTicket {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        PairTicket::decode(s)
    }
}
