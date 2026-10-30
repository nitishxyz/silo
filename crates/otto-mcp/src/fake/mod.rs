//! An in-process streamable-HTTP MCP server for tests (feature `fake`): JSON and SSE
//! POST responses, a GET notification stream that replays after `Last-Event-ID`, server
//! requests, resources and prompts, and an OAuth authorization server (protected
//! resource metadata, RFC 8414 metadata, dynamic registration, PKCE S256, refresh).
mod mcp;
mod rpc;

use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{broadcast, watch},
};

/// What the fake saw and holds; tests read and change it.
#[derive(Default)]
pub struct FakeState {
    pub oauth: bool,
    pub grown: bool,
    /// Events for the GET stream: `(id, json)`.
    pub log: Vec<(u64, String)>,
    pub delivered: u64,
    /// `Last-Event-ID` of every GET stream opened.
    pub gets: Vec<Option<String>>,
    /// Requests and notifications received, by method.
    pub methods: Vec<String>,
    /// The client's replies to server requests, by request id.
    pub responses: HashMap<String, Value>,
    pub registered: Vec<Value>,
    pub authorize: Vec<BTreeMap<String, String>>,
    pub token_requests: Vec<BTreeMap<String, String>>,
    pub tokens: HashSet<String>,
    refresh: HashSet<String>,
    codes: HashMap<String, (String, String)>,
    issued: u64,
}

pub(crate) struct Inner {
    pub(crate) base: String,
    state: Mutex<FakeState>,
    live: broadcast::Sender<(u64, String)>,
    kick: watch::Sender<u64>,
}

pub struct Fake {
    pub url: String,
    pub base: String,
    inner: Arc<Inner>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) struct Req {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: BTreeMap<String, String>,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
}

async fn read(stream: &mut TcpStream) -> Option<Req> {
    let mut data = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        let n = stream.read(&mut buffer).await.ok().filter(|n| *n > 0)?;
        data.extend_from_slice(&buffer[..n]);
    };
    let head = String::from_utf8_lossy(&data[..end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let (method, target) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers: HashMap<String, String> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    let length: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
    let mut body = data[end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut buffer).await.ok().filter(|n| *n > 0)?;
        body.extend_from_slice(&buffer[..n]);
    }
    let url = reqwest::Url::parse(&format!("http://fake{target}")).ok()?;
    let query = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    Some(Req { method, path: url.path().to_owned(), query, headers, body })
}

pub(crate) async fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &str) {
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn form(body: &[u8]) -> BTreeMap<String, String> {
    reqwest::Url::parse(&format!("http://fake/?{}", String::from_utf8_lossy(body)))
        .map(|u| u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default()
}

impl Fake {
    /// Listening on a loopback port; with `oauth`, `/mcp` wants a bearer token.
    pub async fn start(oauth: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().expect("addr").port());
        let inner = Arc::new(Inner {
            base: base.clone(),
            state: Mutex::new(FakeState { oauth, ..Default::default() }),
            live: broadcast::channel(64).0,
            kick: watch::channel(0).0,
        });
        let task = tokio::spawn({
            let inner = inner.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(inner.clone().handle(stream));
                }
            }
        });
        Self { url: format!("{base}/mcp"), base, inner, task }
    }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.inner.state()
    }

    /// Send a message on the GET stream (logged for replay).
    pub fn push(&self, message: Value) {
        self.inner.push(message);
    }

    /// Drop every open GET stream.
    pub fn kick(&self) {
        self.inner.kick.send_modify(|n| *n += 1);
    }

    /// Invalidate the access tokens (refresh tokens stay valid).
    pub fn expire_tokens(&self) {
        self.state().tokens.clear();
    }

    /// Act as the browser: open `url` and follow its redirect to the loopback.
    pub async fn browse(url: &str) -> Result<String, String> {
        let response = reqwest::get(url).await.map_err(|e| e.to_string())?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() { Ok(body) } else { Err(format!("{status}: {body}")) }
    }
}
