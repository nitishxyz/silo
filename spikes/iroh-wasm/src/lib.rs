//! P7-8 spike: a browser (wasm32-unknown-unknown) client that dials a native
//! silo server over iroh relays and speaks the real silo wire protocol:
//! `silo/ctl/1`, `ClientHello` with a `silo1…` invite, `session.list`,
//! `data.connect`, and a second bi stream with `DataHello` → `Welcome` →
//! snapshot frames. The same code also builds natively (`src/main.rs`).
//!
//! The protocol types are mirrored as JSON here instead of depending on
//! `silo-proto`, which pulls tokio `process` and rustix and doesn't build for
//! wasm.

use anyhow::{Context, Result, anyhow, bail};
use data_encoding::BASE32_NOPAD_NOCASE;
use iroh::endpoint::{RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, TransportAddr};
use n0_future::time::Instant;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CTL_ALPN: &[u8] = b"silo/ctl/1";
pub const TICKET_PREFIX: &str = "silo1";
pub const SHARE_PREFIX: &str = "silos1";
pub const PROTOCOL: u32 = 1;
const MAX_FRAME: usize = 16 * 1024 * 1024;

mod kind {
    pub const CONTROL_HELLO: u8 = 0x01;
    pub const CONTROL: u8 = 0x02;
    pub const DATA_HELLO: u8 = 0x10;
    pub const WELCOME: u8 = 0x81;
    pub const REJECT: u8 = 0x82;
    pub const SNAPSHOT_HEAD: u8 = 0x83;
    pub const SNAPSHOT_HISTORY: u8 = 0x84;
    pub const OUTPUT: u8 = 0x85;
    pub const SNAPSHOT_DONE: u8 = 0x88;
}

/// `silo-transport::PairTicket`, byte-for-byte.
#[derive(Clone, Serialize, Deserialize)]
pub struct PairTicket {
    pub addr: EndpointAddr,
    pub secret: [u8; 32],
}

/// The planned `silos1` share ticket (PHASE7 §8).
#[derive(Clone, Serialize, Deserialize)]
pub struct ShareTicket {
    pub addr: EndpointAddr,
    pub secret: [u8; 32],
    pub vt: String,
}

fn decode_prefixed<T: for<'de> Deserialize<'de>>(s: &str, prefix: &str) -> Result<T> {
    let s = s.trim();
    let body = s
        .get(..prefix.len())
        .filter(|p| p.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
        .ok_or_else(|| anyhow!("missing {prefix:?} prefix"))?;
    let bytes = BASE32_NOPAD_NOCASE.decode(body.as_bytes()).context("bad base32")?;
    let (value, rest) = postcard::take_from_bytes(&bytes).context("bad payload")?;
    if !rest.is_empty() {
        bail!("{} trailing bytes", rest.len());
    }
    Ok(value)
}

fn encode_prefixed<T: Serialize>(value: &T, prefix: &str) -> String {
    let bytes = postcard::to_stdvec(value).expect("serializes");
    format!("{prefix}{}", BASE32_NOPAD_NOCASE.encode(&bytes).to_ascii_lowercase())
}

impl PairTicket {
    pub fn decode(s: &str) -> Result<Self> {
        decode_prefixed(s, TICKET_PREFIX).context("invalid pairing ticket")
    }
    pub fn encode(&self) -> String {
        encode_prefixed(self, TICKET_PREFIX)
    }
}

impl ShareTicket {
    pub fn decode(s: &str) -> Result<Self> {
        decode_prefixed(s, SHARE_PREFIX).context("invalid share ticket")
    }
    pub fn encode(&self) -> String {
        encode_prefixed(self, SHARE_PREFIX)
    }
}

/// The ticket part of a link `https://<origin>/#silo1…` (or `#silos1…`), or
/// a bare ticket.
pub fn ticket_from_link(link: &str) -> &str {
    let link = link.trim();
    match link.split_once('#') {
        Some((_, frag)) => frag,
        None => link,
    }
}

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&((payload.len() + 1) as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

async fn write_frame(send: &mut SendStream, kind: u8, payload: &[u8]) -> Result<()> {
    send.write_all(&frame(kind, payload)).await.context("write frame")
}

async fn read_frame(recv: &mut RecvStream) -> Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.context("read frame length")?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        bail!("bad frame length {len}");
    }
    let mut rest = vec![0u8; len];
    recv.read_exact(&mut rest).await.context("read frame body")?;
    let payload = rest.split_off(1);
    Ok((rest[0], payload))
}

/// What the spike learned, reported to the page / stdout.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub server_id: String,
    pub relay: Option<String>,
    pub bind_ms: f64,
    pub online_ms: f64,
    pub connect_ms: f64,
    pub hello_ms: f64,
    pub list_ms: f64,
    pub snapshot_ms: f64,
    pub total_ms: f64,
    pub path: Option<String>,
    pub rtt_ms: Option<f64>,
    pub server_hello: Value,
    pub sessions: Value,
    pub block_id: Option<String>,
    pub welcome: Value,
    pub snapshot_head_len: usize,
    pub snapshot_history_frames: usize,
    pub snapshot_history_len: usize,
    pub output_len: usize,
    pub snapshot_magic: String,
    #[serde(skip)]
    pub snapshot: Vec<u8>,
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

/// Request/response on the control stream, skipping events.
async fn call(send: &mut SendStream, recv: &mut RecvStream, id: u64, method: &str, params: Value) -> Result<Value> {
    let req = json!({ "id": id, "method": method, "params": params });
    write_frame(send, kind::CONTROL, &serde_json::to_vec(&req)?).await?;
    loop {
        let (k, payload) = read_frame(recv).await?;
        if k != kind::CONTROL {
            continue;
        }
        let msg: Value = serde_json::from_slice(&payload)?;
        if msg.get("seq").is_some() {
            continue;
        }
        if msg.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(err) = msg.get("error") {
            bail!("{method}: {err}");
        }
        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// Dial the server named by `ticket` relay-only, redeem the invite, list
/// sessions, and attach to the first terminal block of the first session.
///
/// `wait_online`: wait for the browser's own home relay before dialing (the
/// dial itself only needs the server's relay).
/// `relay_only`: drop the ticket's IP addresses before dialing (browsers
/// can't use them anyway).
pub async fn run(ticket: &str, vt: &str, wait_online: bool, relay_only: bool, log: &dyn Fn(&str)) -> Result<Report> {
    let t0 = Instant::now();
    let mut report = Report::default();
    let ticket_str = ticket_from_link(ticket).to_owned();
    let pair = PairTicket::decode(&ticket_str)?;
    report.server_id = pair.addr.id.to_string();
    // A browser can't send UDP; optionally keep only relay addresses.
    let dial = EndpointAddr {
        id: pair.addr.id,
        addrs: pair.addr.addrs.iter().filter(|a| !relay_only || matches!(a, TransportAddr::Relay(_))).cloned().collect(),
    };
    report.relay = dial.addrs.iter().find_map(|a| match a {
        TransportAddr::Relay(url) => Some(url.to_string()),
        _ => None,
    });
    log(&format!("ticket ok: server {} relay {:?} ({} addrs, {} kept)", report.server_id, report.relay, pair.addr.addrs.len(), dial.addrs.len()));

    let t = Instant::now();
    let endpoint = Endpoint::builder(presets::N0).bind().await.context("bind endpoint")?;
    report.bind_ms = ms(t);
    log(&format!("bound endpoint {} in {:.0} ms", endpoint.id(), report.bind_ms));
    if wait_online {
        let t = Instant::now();
        endpoint.online().await;
        report.online_ms = ms(t);
        log(&format!("home relay online in {:.0} ms: {:?}", report.online_ms, endpoint.addr().addrs));
    }

    let t = Instant::now();
    let conn = endpoint.connect(dial, CTL_ALPN).await.context("connect")?;
    report.connect_ms = ms(t);
    log(&format!("connected (silo/ctl/1) in {:.0} ms", report.connect_ms));

    // Control stream: ClientHello -> ServerHello.
    let t = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;
    let hello = json!({
        "protocol": PROTOCOL,
        "vt": vt,
        "version": "0.0.0",
        "build": "iroh-wasm-spike",
        "features": [],
        "client_name": "silo web spike",
        "passive": true,
        "invite": ticket_str,
    });
    write_frame(&mut send, kind::CONTROL_HELLO, &serde_json::to_vec(&hello)?).await?;
    let (k, payload) = read_frame(&mut recv).await?;
    let reply: Value = serde_json::from_slice(&payload)?;
    if k != kind::CONTROL_HELLO {
        bail!("hello rejected: {reply}");
    }
    report.hello_ms = ms(t);
    log(&format!("ServerHello in {:.0} ms: client_id {}", report.hello_ms, reply["client_id"]));
    report.server_hello = reply;

    let t = Instant::now();
    let sessions = call(&mut send, &mut recv, 1, "session.list", json!({})).await?;
    report.list_ms = ms(t);
    log(&format!("session.list in {:.0} ms: {sessions}", report.list_ms));
    report.sessions = sessions.clone();
    let session_id = sessions.get(0).and_then(|s| s["id"].as_str()).ok_or_else(|| anyhow!("no sessions"))?.to_owned();
    let view = call(&mut send, &mut recv, 2, "session.view", json!({ "session_id": session_id })).await?;
    let block_id = view["blocks"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["kind"].as_str().is_none_or(|k| k == "terminal")))
        .and_then(|b| b["id"].as_str())
        .ok_or_else(|| anyhow!("no terminal block in {view}"))?
        .to_owned();
    report.block_id = Some(block_id.clone());
    let data_ticket = call(&mut send, &mut recv, 3, "data.connect", json!({ "block_id": block_id })).await?;
    let data_ticket = data_ticket["ticket"].as_str().ok_or_else(|| anyhow!("no ticket"))?.to_owned();
    log(&format!("data.connect ticket for block {block_id}"));

    // Second bi stream: DataHello -> Welcome -> snapshot.
    let t = Instant::now();
    let (mut dsend, mut drecv) = conn.open_bi().await.context("open data stream")?;
    let dhello = json!({ "protocol": PROTOCOL, "vt": vt, "build": "iroh-wasm-spike", "ticket": data_ticket });
    write_frame(&mut dsend, kind::DATA_HELLO, &serde_json::to_vec(&dhello)?).await?;
    loop {
        let (k, payload) = read_frame(&mut drecv).await?;
        match k {
            kind::WELCOME => {
                report.welcome = serde_json::from_slice(&payload)?;
                log(&format!("Welcome: {}", report.welcome));
            }
            kind::REJECT => bail!("data rejected: {}", String::from_utf8_lossy(&payload)),
            kind::SNAPSHOT_HEAD => {
                report.snapshot_head_len = payload.len();
                report.snapshot_magic = String::from_utf8_lossy(&payload[..payload.len().min(8)]).into_owned();
                report.snapshot.extend_from_slice(&payload);
            }
            kind::SNAPSHOT_HISTORY => {
                report.snapshot_history_frames += 1;
                report.snapshot_history_len += payload.len();
                report.snapshot.extend_from_slice(&payload);
            }
            kind::OUTPUT => report.output_len += payload.len(),
            kind::SNAPSHOT_DONE => break,
            _ => {}
        }
    }
    report.snapshot_ms = ms(t);
    log(&format!(
        "snapshot in {:.0} ms: head {} B ({:?}), history {} B in {} frames",
        report.snapshot_ms, report.snapshot_head_len, report.snapshot_magic, report.snapshot_history_len, report.snapshot_history_frames
    ));
    {
        let paths = conn.paths();
        if let Some(p) = paths.iter().find(|p| p.is_selected()) {
            report.path = Some(format!("{} {:?}", if p.is_relay() { "relay" } else { "direct" }, p.remote_addr()));
            report.rtt_ms = Some(p.rtt().as_secs_f64() * 1e3);
        }
    }
    report.total_ms = ms(t0);
    conn.close(0u32.into(), b"spike done");
    endpoint.close().await;
    Ok(report)
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod web {
    use wasm_bindgen::prelude::*;

    fn console(s: &str) {
        web_sys::console::log_1(&s.into());
    }

    /// Returns `{report, snapshot}`; `log` gets progress lines.
    #[wasm_bindgen]
    pub async fn run(ticket: String, vt: String, wait_online: bool, relay_only: bool, log: js_sys::Function) -> Result<JsValue, JsValue> {
        console_error_panic_hook::set_once();
        let log_fn = |s: &str| {
            console(s);
            let _ = log.call1(&JsValue::NULL, &s.into());
        };
        match super::run(&ticket, &vt, wait_online, relay_only, &log_fn).await {
            Ok(report) => {
                let out = js_sys::Object::new();
                let json = serde_json::to_string(&report).unwrap_or_default();
                js_sys::Reflect::set(&out, &"report".into(), &js_sys::JSON::parse(&json)?)?;
                js_sys::Reflect::set(&out, &"snapshot".into(), &js_sys::Uint8Array::from(report.snapshot.as_slice()))?;
                Ok(out.into())
            }
            Err(e) => Err(JsValue::from_str(&format!("{e:#}"))),
        }
    }

    /// Round-trips a `silos1` ShareTicket for the link-format check.
    #[wasm_bindgen]
    pub fn share_ticket_from_pair(pair: String, vt: String) -> Result<String, JsValue> {
        let p = super::PairTicket::decode(super::ticket_from_link(&pair)).map_err(|e| JsValue::from_str(&format!("{e:#}")))?;
        let s = super::ShareTicket { addr: p.addr, secret: p.secret, vt }.encode();
        let back = super::ShareTicket::decode(&s).map_err(|e| JsValue::from_str(&format!("{e:#}")))?;
        assert_eq!(back.secret, p.secret);
        Ok(s)
    }
}
