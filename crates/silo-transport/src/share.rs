use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use data_encoding::BASE32_NOPAD_NOCASE;
use iroh::{EndpointAddr, TransportAddr};
use serde::{Deserialize, Serialize};

use crate::ticket::INVITE_SECRET_LEN;

/// Prefix of a share ticket. `1` is not a base32 digit, so neither a
/// `silo1` pairing ticket nor this parses as the other.
pub const SHARE_PREFIX: &str = "silos1";

/// Where share links point: the ticket goes in the fragment, which never
/// reaches the web server.
pub const SHARE_ORIGIN: &str = "https://nitishxyz.github.io/silo/share/";

/// A share link: how to reach the server, the link grant's bearer secret
/// and the server's VT (a web client loads the matching libghostty before
/// it connects). `addr` holds the endpoint id and relay URLs only: a
/// browser can't use direct addresses and a shared link must not leak them.
/// Encoded as `silos1` + lowercase unpadded base32 of postcard bytes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTicket {
    pub addr: EndpointAddr,
    pub secret: [u8; INVITE_SECRET_LEN],
    pub vt: String,
}

impl fmt::Debug for ShareTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShareTicket").field("addr", &self.addr).field("secret", &"<redacted>").field("vt", &self.vt).finish()
    }
}

impl ShareTicket {
    /// A ticket for the server at `addr`, keeping only its relay URLs.
    #[must_use]
    pub fn new(addr: &EndpointAddr, secret: [u8; INVITE_SECRET_LEN], vt: &str) -> Self {
        let relays = addr.relay_urls().cloned().map(TransportAddr::Relay);
        Self { addr: EndpointAddr::from_parts(addr.id, relays), secret, vt: vt.to_owned() }
    }

    /// Whether a relay can reach the server (else only Silo on the same
    /// network can).
    #[must_use]
    pub fn has_relay(&self) -> bool {
        self.addr.relay_urls().next().is_some()
    }

    #[must_use]
    pub fn encode(&self) -> String {
        let bytes = postcard::to_stdvec(self).expect("ShareTicket serializes");
        let body = BASE32_NOPAD_NOCASE.encode(&bytes).to_ascii_lowercase();
        format!("{SHARE_PREFIX}{body}")
    }

    /// The link to hand out: [`SHARE_ORIGIN`] with the ticket as fragment.
    #[must_use]
    pub fn link(&self) -> String {
        format!("{SHARE_ORIGIN}#{}", self.encode())
    }

    /// A bare `silos1…` ticket.
    ///
    /// # Errors
    /// Another prefix, bad base32 or payload.
    pub fn decode(s: &str) -> Result<ShareTicket> {
        let s = s.trim();
        let Some(body) = s.get(..SHARE_PREFIX.len()).filter(|p| p.eq_ignore_ascii_case(SHARE_PREFIX)).map(|_| &s[SHARE_PREFIX.len()..]) else {
            bail!("invalid share link: missing {SHARE_PREFIX:?} prefix");
        };
        let bytes = BASE32_NOPAD_NOCASE.decode(body.as_bytes()).context("invalid share link: bad base32")?;
        let (ticket, rest): (ShareTicket, _) = postcard::take_from_bytes(&bytes).context("invalid share link: bad payload")?;
        if !rest.is_empty() {
            bail!("invalid share link: {} trailing bytes", rest.len());
        }
        Ok(ticket)
    }

    /// A full link (any origin, ticket in the fragment) or a bare ticket.
    ///
    /// # Errors
    /// No ticket in it, or an invalid one.
    pub fn parse(text: &str) -> Result<ShareTicket> {
        let text = text.trim();
        match text.rsplit_once('#') {
            Some((_, fragment)) => Self::decode(fragment),
            None => Self::decode(text),
        }
    }
}

impl fmt::Display for ShareTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl FromStr for ShareTicket {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        ShareTicket::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;
    use crate::PairTicket;

    fn addr() -> EndpointAddr {
        let id = SecretKey::from_bytes(&[7; 32]).public();
        EndpointAddr::new(id).with_relay_url("https://relay.example.org".parse().unwrap()).with_ip_addr("192.168.1.7:4433".parse().unwrap())
    }

    #[test]
    fn keeps_only_the_relay_and_round_trips() {
        let ticket = ShareTicket::new(&addr(), [3; INVITE_SECRET_LEN], "vt1");
        assert_eq!(ticket.addr.ip_addrs().count(), 0, "no direct addresses");
        assert!(ticket.has_relay());
        let text = ticket.encode();
        assert!(text.starts_with(SHARE_PREFIX), "{text}");
        assert_eq!(ShareTicket::decode(&text).unwrap(), ticket);
        assert_eq!(ShareTicket::decode(&text.to_ascii_uppercase()).unwrap(), ticket);
        assert_eq!(ShareTicket::parse(&ticket.link()).unwrap(), ticket);
        assert_eq!(ShareTicket::parse(&format!("  https://other.example/x?y=1#{text}\n")).unwrap(), ticket);
        assert!(!format!("{ticket:?}").contains("[3,"), "the secret is redacted");
        let offline = ShareTicket::new(&EndpointAddr::new(addr().id), [3; INVITE_SECRET_LEN], "vt1");
        assert!(!offline.has_relay());
    }

    #[test]
    fn share_and_pairing_tickets_never_parse_as_each_other() {
        let share = ShareTicket::new(&addr(), [1; INVITE_SECRET_LEN], "vt1").encode();
        let pair = PairTicket { addr: addr(), secret: [1; INVITE_SECRET_LEN] }.encode();
        assert!(PairTicket::decode(&share).is_err(), "a share ticket is no pairing ticket");
        assert!(ShareTicket::decode(&pair).is_err(), "a pairing ticket is no share ticket");
        assert!(ShareTicket::parse(&format!("{SHARE_ORIGIN}#{pair}")).is_err());
        // Even with the other prefix swapped in, the payload does not decode.
        assert!(ShareTicket::decode(&format!("{SHARE_PREFIX}{}", &pair["silo1".len()..])).is_err());
        assert!(PairTicket::decode(&format!("silo1{}", &share[SHARE_PREFIX.len()..])).is_err());
        assert!(ShareTicket::parse("https://silo.example/").is_err());
        assert!(ShareTicket::parse("").is_err());
    }
}
