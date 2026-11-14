//! Logging in through a machine's login server (`docs/LOGIN_SERVER.md`
//! §5): one `silo/login/1` exchange that has the account's server admit
//! this device and says where that server is. The device then connects to
//! it as a paired device.
//!
//! The exchange must come from the device key's own endpoint: the login
//! server knows a device by the endpoint that dials it.

use std::time::Duration;

use anyhow::Context;
use data_encoding::BASE32_NOPAD_NOCASE;
use silo_proto::{
    Frame, kind,
    login::{LOGIN_PROTOCOL, LoginAnswer, LoginReply, LoginRequest},
    write_frame,
};
use silo_transport::{EndpointAddr, LoginTicket, Net};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::hosts::{Host, LoginVia};

/// Largest answer read.
const MAX_ANSWER: usize = 64 * 1024;
/// A login may start the account's server first.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(60);

/// A stored host did not answer its dial: a host added through a login
/// server can be started by logging in again ([`relogin`]).
#[derive(Debug)]
pub struct NotAnswering(pub String);

impl std::fmt::Display for NotAnswering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotAnswering {}

/// Whether `error` is a host not answering: [`NotAnswering`], or a dial
/// nobody answered ([`silo_transport::Unreachable`]).
#[must_use]
pub fn not_answering(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NotAnswering>().is_some() || error.downcast_ref::<silo_transport::Unreachable>().is_some()
}

/// Whether logging in again may bring `host` up after `error`: it did not
/// answer ([`not_answering`]) and it was added through a login server.
#[must_use]
pub fn may_start(error: &anyhow::Error, host: &Host) -> bool {
    host.login.is_some() && not_answering(error)
}

/// The login server said no.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginRefused {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for LoginRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the login server refused: {}", self.message)
    }
}

impl std::error::Error for LoginRefused {}

/// Log in with `ticket` (its invite redeemed, if it has one) as `account`
/// (else the ticket's, else the device's default) to `server_of`'s server
/// (else the account's own), under the device name `name`.
///
/// # Errors
/// Dial or stream failure, a malformed answer, or [`LoginRefused`].
pub async fn login(net: &Net, ticket: &LoginTicket, account: Option<&str>, server_of: Option<&str>, name: &str) -> anyhow::Result<LoginReply> {
    let rc = net.dial_login(ticket.addr.clone()).await.context("dial the login server")?;
    let exchange = async {
        let mut stream = rc.open_stream().await?;
        let request = LoginRequest {
            protocol: LOGIN_PROTOCOL,
            account: account.map(str::to_owned).or_else(|| ticket.account.clone()),
            server_of: server_of.map(str::to_owned),
            // The broker reads it as lowercase unpadded base32.
            invite: ticket.secret.map(|s| BASE32_NOPAD_NOCASE.encode(&s).to_ascii_lowercase()),
            name: name.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        write_frame(&mut stream, &Frame::json(kind::CONTROL_HELLO, &request)?).await?;
        stream.flush().await?;
        let mut len = [0u8; 4];
        stream.read_exact(&mut len).await.context("the login server hung up")?;
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > MAX_ANSWER {
            anyhow::bail!("bad answer from the login server");
        }
        let mut rest = vec![0; len];
        stream.read_exact(&mut rest).await?;
        let frame = Frame { kind: rest[0], payload: rest.split_off(1) };
        let answer: LoginAnswer = frame.parse().context("bad answer from the login server")?;
        let _ = stream.shutdown().await;
        anyhow::Ok(answer)
    };
    let answer = tokio::time::timeout(LOGIN_TIMEOUT, exchange).await.unwrap_or_else(|_| Err(anyhow::anyhow!("the login server did not answer")));
    rc.close(b"done");
    match answer? {
        LoginAnswer::Ok(reply) => Ok(reply),
        LoginAnswer::Error(error) => Err(LoginRefused { code: error.code, message: error.message }.into()),
    }
}

/// The address of the server a [`LoginReply`] names.
///
/// # Errors
/// Not a server address ticket.
pub fn server_addr(reply: &LoginReply) -> anyhow::Result<EndpointAddr> {
    silo_transport::decode_addr(&reply.ticket)
}

/// Where to log in again for a host added through a login server.
#[must_use]
pub fn via(ticket: &LoginTicket, reply: &LoginReply, server_of: Option<&str>) -> LoginVia {
    LoginVia { addr: ticket.addr.clone(), account: Some(reply.account.clone()), server_of: server_of.map(str::to_owned) }
}

/// Log in again through `host`'s login server, which starts the
/// account's server if it is not running: the server's address now.
/// `net` must be bound with the device key.
///
/// # Errors
/// The host has no login server, or [`login`] failed.
pub async fn relogin(net: &Net, host: &Host, name: &str) -> anyhow::Result<EndpointAddr> {
    let via = host.login.as_ref().with_context(|| format!("{} was not added through a login server", host.name))?;
    let ticket = LoginTicket { addr: via.addr.clone(), secret: None, account: via.account.clone() };
    let reply = login(net, &ticket, None, via.server_of.as_deref(), name).await?;
    let addr = server_addr(&reply)?;
    if addr.id.to_string() != host.id {
        anyhow::bail!("the login server now names another server for {}", host.name);
    }
    Ok(addr)
}
