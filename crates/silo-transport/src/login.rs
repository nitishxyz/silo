//! Login server tickets (`docs/LOGIN_SERVER.md` §4): how to reach a
//! machine's login server, with an invite's secret and the account it is
//! for; and the secret-less address of an account's server that the login
//! server hands back.

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use data_encoding::BASE32_NOPAD_NOCASE;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::ticket::INVITE_SECRET_LEN;

/// Prefix of a login ticket. `1` is not a base32 digit, so no other
/// ticket (`silo1`, `silos1`, `siloa1`) parses as this one.
pub const LOGIN_PREFIX: &str = "silol1";
/// Prefix of a server's address ticket: no secret, only where it is.
pub const ADDR_PREFIX: &str = "siloa1";
/// Longest unix user name accepted (glibc's `LOGIN_NAME_MAX` is 256, but
/// `useradd` refuses more than 32).
pub const MAX_ACCOUNT_LEN: usize = 32;

/// Whether `name` is a plain unix user name: a letter or `_`, then
/// letters, digits, `_`, `-` or `.`, at most [`MAX_ACCOUNT_LEN`]. Names
/// come from the network and end up in lookups and file keys, never a
/// shell; anything else is refused rather than escaped.
#[must_use]
pub fn valid_account(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    name.len() <= MAX_ACCOUNT_LEN
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A login server's ticket: its address, an invite's secret (redeemed
/// once) and the account to use. Encoded as `silol1` + lowercase unpadded
/// base32 of postcard bytes; `account@silol1…` picks the account in text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginTicket {
    pub addr: EndpointAddr,
    pub secret: Option<[u8; INVITE_SECRET_LEN]>,
    pub account: Option<String>,
}

impl fmt::Debug for LoginTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secret = self.secret.map(|_| "<redacted>");
        f.debug_struct("LoginTicket").field("addr", &self.addr).field("secret", &secret).field("account", &self.account).finish()
    }
}

fn decode_body(s: &str, prefix: &str, what: &str) -> Result<Vec<u8>> {
    let Some(body) = s.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &s[prefix.len()..]) else {
        bail!("invalid {what}: missing {prefix:?} prefix");
    };
    BASE32_NOPAD_NOCASE.decode(body.as_bytes()).with_context(|| format!("invalid {what}: bad base32"))
}

fn encode_body(prefix: &str, bytes: &[u8]) -> String {
    format!("{prefix}{}", BASE32_NOPAD_NOCASE.encode(bytes).to_ascii_lowercase())
}

impl LoginTicket {
    #[must_use]
    pub fn encode(&self) -> String {
        encode_body(LOGIN_PREFIX, &postcard::to_stdvec(self).expect("LoginTicket serializes"))
    }

    /// A bare `silol1…` ticket.
    ///
    /// # Errors
    /// Another prefix, bad base32 or payload, an invalid account.
    pub fn decode(s: &str) -> Result<LoginTicket> {
        let bytes = decode_body(s.trim(), LOGIN_PREFIX, "login ticket")?;
        let (ticket, rest): (LoginTicket, _) = postcard::take_from_bytes(&bytes).context("invalid login ticket: bad payload")?;
        if !rest.is_empty() {
            bail!("invalid login ticket: {} trailing bytes", rest.len());
        }
        if ticket.account.as_deref().is_some_and(|a| !valid_account(a)) {
            bail!("invalid login ticket: bad account name");
        }
        Ok(ticket)
    }

    /// `silol1…` or `account@silol1…`; the account in front replaces the
    /// ticket's.
    ///
    /// # Errors
    /// See [`LoginTicket::decode`]; an invalid account in front.
    pub fn parse(text: &str) -> Result<LoginTicket> {
        let text = text.trim();
        match text.split_once('@') {
            Some((account, ticket)) => {
                if !valid_account(account) {
                    bail!("invalid account {account:?} in front of the login ticket");
                }
                Ok(LoginTicket { account: Some(account.to_owned()), ..Self::decode(ticket)? })
            }
            None => Self::decode(text),
        }
    }

    /// Whether `text` looks like a login ticket (with or without
    /// `account@`), so callers can tell it from other tickets before
    /// decoding.
    #[must_use]
    pub fn is_login(text: &str) -> bool {
        let text = text.trim();
        let ticket = text.split_once('@').map_or(text, |(_, t)| t);
        ticket.get(..LOGIN_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(LOGIN_PREFIX))
    }
}

impl fmt::Display for LoginTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl FromStr for LoginTicket {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        LoginTicket::parse(s)
    }
}

/// A server's address without any secret (`siloa1…`): what `login.admit`
/// answers, so a device already admitted dials it as a paired device.
/// Encoded as `siloa1` + lowercase unpadded base32 of postcard bytes.
#[must_use]
pub fn encode_addr(addr: &EndpointAddr) -> String {
    encode_body(ADDR_PREFIX, &postcard::to_stdvec(addr).expect("EndpointAddr serializes"))
}

/// See [`encode_addr`].
///
/// # Errors
/// Another prefix, bad base32 or payload.
pub fn decode_addr(s: &str) -> Result<EndpointAddr> {
    let bytes = decode_body(s.trim(), ADDR_PREFIX, "server address")?;
    let (addr, rest): (EndpointAddr, _) = postcard::take_from_bytes(&bytes).context("invalid server address: bad payload")?;
    if !rest.is_empty() {
        bail!("invalid server address: {} trailing bytes", rest.len());
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;
    use crate::{PairTicket, ShareTicket};

    fn addr() -> EndpointAddr {
        let id = SecretKey::from_bytes(&[9; 32]).public();
        EndpointAddr::new(id).with_relay_url("https://relay.example.org".parse().unwrap()).with_ip_addr("192.168.1.9:4433".parse().unwrap())
    }

    #[test]
    fn login_tickets_round_trip_with_and_without_secret_and_account() {
        for (secret, account) in [(None, None), (Some([4; INVITE_SECRET_LEN]), None), (Some([5; INVITE_SECRET_LEN]), Some("alice".to_owned()))] {
            let ticket = LoginTicket { addr: addr(), secret, account };
            let text = ticket.encode();
            assert!(text.starts_with(LOGIN_PREFIX) && LoginTicket::is_login(&text), "{text}");
            assert_eq!(LoginTicket::decode(&text).unwrap(), ticket);
            assert_eq!(LoginTicket::decode(&text.to_ascii_uppercase()).unwrap(), ticket);
            assert_eq!(text.parse::<LoginTicket>().unwrap(), ticket);
        }
        let secret = LoginTicket { addr: addr(), secret: Some([4; INVITE_SECRET_LEN]), account: None };
        assert!(!format!("{secret:?}").contains("[4,"), "the secret is redacted");
    }

    #[test]
    fn an_account_in_front_picks_the_account() {
        let ticket = LoginTicket { addr: addr(), secret: None, account: Some("alice".into()) };
        let text = format!("  root@{}\n", ticket.encode());
        assert!(LoginTicket::is_login(&text));
        let parsed = LoginTicket::parse(&text).unwrap();
        assert_eq!((parsed.addr, parsed.account.as_deref()), (addr(), Some("root")));
        assert!(LoginTicket::parse(&format!("bad name@{}", ticket.encode())).is_err());
        assert!(LoginTicket::parse(&format!("$(id)@{}", ticket.encode())).is_err());
        assert!(LoginTicket::parse(&format!("@{}", ticket.encode())).is_err());
    }

    #[test]
    fn account_names_are_plain_unix_names() {
        for good in ["alice", "root", "_svc", "a.b-c_1", &"x".repeat(MAX_ACCOUNT_LEN)] {
            assert!(valid_account(good), "{good}");
        }
        for bad in ["", "*", "1abc", "-x", ".", "..", "a b", "a/b", "a;b", "ä", "a$", &"x".repeat(MAX_ACCOUNT_LEN + 1)] {
            assert!(!valid_account(bad), "{bad}");
        }
    }

    #[test]
    fn other_tickets_never_parse_as_login_or_address_tickets() {
        let login = LoginTicket { addr: addr(), secret: None, account: None }.encode();
        let pair = PairTicket { addr: addr(), secret: [1; INVITE_SECRET_LEN] }.encode();
        let share = ShareTicket::new(&addr(), [1; INVITE_SECRET_LEN], "vt1").encode();
        let address = encode_addr(&addr());
        assert_eq!(decode_addr(&address).unwrap(), addr());
        assert!(address.starts_with(ADDR_PREFIX));
        for other in [&pair, &share, &address] {
            assert!(LoginTicket::decode(other).is_err() && !LoginTicket::is_login(other), "{other}");
        }
        for other in [&pair, &share, &login] {
            assert!(decode_addr(other).is_err(), "{other}");
        }
        assert!(PairTicket::decode(&login).is_err() && ShareTicket::decode(&login).is_err());
        assert!(LoginTicket::decode("").is_err());
    }
}
