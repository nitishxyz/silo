//! Responses over WebSocket, with native mid-turn steering and
//! `previous_response_id` continuation (docs/OTTO.md §4.7).
//!
//! One connection per model instance (otto resolves a model per session), on
//! the implicit default lane. A connection is reused for the next call when
//! its last response ended cleanly. Our log stays the truth: a call continues
//! from the connection's last response (sending only new input) when every
//! other field is unchanged and its input extends the last input plus that
//! response's output; otherwise, or on `previous_response_not_found`, it
//! sends the full input.
use super::stream::State;
use crate::{ErrorClass, EventStream, FinishReason, LlmError, Part, StreamEvent as E};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tokio_websockets::{ClientBuilder, Connector, MaybeTlsStream, Message, WebSocketStream};

/// Connections last up to 60 minutes; never start a response close to that.
const MAX_AGE: Duration = Duration::from_secs(50 * 60);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type SteerTx = mpsc::UnboundedSender<(String, Value)>;
/// Input items an assistant item made of these parts becomes in the next request.
pub(super) type Replay = Box<dyn Fn(&[Part]) -> Vec<Value> + Send>;

pub(super) struct Lane {
    socket: Socket,
    opened: Instant,
    /// The connection-local cache entry the next call may continue from.
    last: Option<Last>,
}

/// What the server holds for the connection's last response.
struct Last {
    response_id: String,
    /// The request without `input`.
    rest: Value,
    /// Its input plus the response's output, as we would send them.
    baseline: Vec<Value>,
}

/// Split a body into everything but `input`, and `input`.
fn split(mut body: Value) -> (Value, Vec<Value>) {
    let input = body.as_object_mut().and_then(|o| o.remove("input")).and_then(|v| if let Value::Array(a) = v { Some(a) } else { None });
    (body, input.unwrap_or_default())
}

/// The `response.create` that continues `last`, when `body` extends it.
#[must_use]
fn continuation(last: Option<&Last>, rest: &Value, input: &[Value]) -> Option<String> {
    let last = last?;
    if &last.rest != rest || input.len() <= last.baseline.len() || input[..last.baseline.len()] != last.baseline[..] {
        return None;
    }
    let mut body = rest.clone();
    body["previous_response_id"] = json!(last.response_id);
    body["input"] = json!(&input[last.baseline.len()..]);
    Some(create_frame(body))
}

/// Per-model WebSocket state: the parked connection and the live stream's
/// steering inbox.
#[derive(Default)]
pub(super) struct Shared {
    idle: Mutex<Option<Lane>>,
    steer: Mutex<Option<SteerTx>>,
}
impl Shared {
    pub(super) fn take_idle(&self) -> Option<Lane> {
        self.idle.lock().unwrap_or_else(|e| e.into_inner()).take().filter(|lane| lane.opened.elapsed() < MAX_AGE)
    }
    fn park(&self, lane: Lane) {
        if lane.opened.elapsed() < MAX_AGE {
            *self.idle.lock().unwrap_or_else(|e| e.into_inner()) = Some(lane);
        }
    }
    /// Hand steering input to the live stream; false when none is listening.
    pub(super) fn steer(&self, id: &str, input: Value) -> bool {
        self.steer.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|tx| tx.send((id.into(), input)).is_ok())
    }
}

/// Where and how to open a connection.
#[derive(Clone)]
pub(super) struct Dial {
    pub url: String,
    pub headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
}
impl Dial {
    pub(super) async fn connect(&self, cancel: &CancellationToken, timeout: Duration) -> Result<Lane, LlmError> {
        let invalid = |e: &dyn std::fmt::Display| LlmError::new(ErrorClass::InvalidRequest, format!("WebSocket request: {e}"));
        let mut builder = ClientBuilder::new().uri(&self.url).map_err(|e| invalid(&e))?;
        if self.url.starts_with("wss://") {
            builder = builder.connector(tls()?);
        }
        for (name, value) in &self.headers {
            builder = builder.add_header(name.clone(), value.clone()).map_err(|e| invalid(&e))?;
        }
        let result = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled()),
            result = tokio::time::timeout(timeout, builder.connect()) => result,
        };
        match result {
            Err(_) => Err(LlmError::new(ErrorClass::Timeout, "WebSocket connect timed out")),
            Ok(Err(tokio_websockets::Error::Upgrade(tokio_websockets::upgrade::Error::DidNotSwitchProtocols(status)))) => {
                let status = reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::BAD_GATEWAY);
                Err(crate::http::classify_status(status, &reqwest::header::HeaderMap::new(), &format!("WebSocket upgrade rejected: HTTP {status}")))
            }
            Ok(Err(e)) => Err(LlmError::new(ErrorClass::Network, format!("WebSocket connect: {e}"))),
            Ok(Ok((socket, _))) => Ok(Lane { socket, opened: Instant::now(), last: None }),
        }
    }
}

fn tls() -> Result<&'static Connector, LlmError> {
    static TLS: OnceLock<Result<Connector, String>> = OnceLock::new();
    TLS.get_or_init(|| {
        use rustls_platform_verifier::BuilderVerifierExt;
        // Explicit provider: the process may link both ring and aws-lc-rs.
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .and_then(BuilderVerifierExt::with_platform_verifier)
            .map_err(|e| e.to_string())?
            .with_no_client_auth();
        Ok(Connector::Rustls(tokio_rustls::TlsConnector::from(Arc::new(config))))
    })
    .as_ref()
    .map_err(|e| LlmError::new(ErrorClass::Network, format!("TLS setup: {e}")))
}

fn cancelled() -> LlmError {
    LlmError::new(ErrorClass::Cancelled, "cancelled")
}

/// `response.create`: the HTTP body without transport fields.
#[must_use]
pub fn create_frame(mut body: Value) -> String {
    if let Some(object) = body.as_object_mut() {
        object.remove("stream");
        object.remove("background");
    }
    body["type"] = json!("response.create");
    body.to_string()
}

/// `response.steer` for the response that is generating.
#[must_use]
pub fn steer_frame(response_id: &str, input: &Value) -> String {
    json!({"type":"response.steer","previous_response_id":response_id,"input":input}).to_string()
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    /// No response to target yet (before `response.created` or between a
    /// steered response and its successor).
    Waiting,
    Sent,
    Accepted(String),
}
struct Steer {
    id: String,
    input: Value,
    status: Status,
}

/// One `response.create` and its steering successors, as a single model
/// call. Translates server events and decides what to send and when the call
/// is over. Pure, so it is tested without a socket.
pub struct Steering {
    parse: State,
    steers: Vec<Steer>,
    response: Option<String>,
    started: bool,
    active: bool,
    /// The server still holds steering input for our tool results; this
    /// connection must not be reused (queued input lives per connection).
    dirty: bool,
}
impl Steering {
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self { parse: State::new(model.into()), steers: vec![], response: None, started: false, active: false, dirty: false }
    }
    fn expecting_successor(&self) -> bool {
        self.steers.iter().any(|s| matches!(s.status, Status::Sent | Status::Accepted(_)))
    }
    /// Queue steering input. Returns the frame to send now, if any. Input that
    /// can no longer reach this call is dropped here and never reported, so
    /// the caller delivers it at the next step boundary.
    pub fn steer(&mut self, id: String, input: Value) -> Option<String> {
        if self.active
            && let Some(response) = &self.response
        {
            let frame = steer_frame(response, &input);
            self.steers.push(Steer { id, input, status: Status::Sent });
            return Some(frame);
        }
        if !self.started || self.expecting_successor() {
            self.steers.push(Steer { id, input, status: Status::Waiting });
        }
        None
    }
    /// Handle one server event: canonical events to emit and frames to send.
    ///
    /// # Errors
    /// Provider errors (`response.failed`, `error`) and malformed items.
    pub fn event(&mut self, v: Value) -> Result<(Vec<E>, Vec<String>), LlmError> {
        let mut events = Vec::new();
        let mut frames = Vec::new();
        let server_id = v["steer"]["id"].as_str().map(String::from);
        let position = |steers: &[Steer], status: &Status| steers.iter().position(|s| &s.status == status);
        match v["type"].as_str().unwrap_or_default() {
            "response.steer.accepted" => {
                if let Some(i) = position(&self.steers, &Status::Sent) {
                    self.steers[i].status = Status::Accepted(server_id.unwrap_or_default());
                }
            }
            "response.steer.failed" => {
                let i = server_id.map(Status::Accepted).and_then(|s| position(&self.steers, &s)).or_else(|| position(&self.steers, &Status::Sent));
                if let Some(i) = i {
                    let steer = self.steers.remove(i);
                    tracing::warn!(id = %steer.id, code = %v["error"]["code"], "steering not applied; delivering at the next step");
                }
            }
            // Accepted input that waits for our tool results. We deliver it
            // ourselves with them, so this connection's queue must go away.
            "response.steer.pending" => {
                if let Some(i) = server_id.map(Status::Accepted).and_then(|s| position(&self.steers, &s)) {
                    self.steers.remove(i);
                }
                self.dirty = true;
            }
            "response.created" => {
                // A successor's creation commits every accepted input, in order.
                if self.started {
                    let (committed, rest) = std::mem::take(&mut self.steers).into_iter().partition(|s| matches!(s.status, Status::Accepted(_)));
                    self.steers = rest;
                    let ids: Vec<String> = committed.into_iter().map(|s: Steer| s.id).collect();
                    if !ids.is_empty() {
                        events.push(E::Steered { ids });
                    }
                }
                self.started = true;
                self.active = true;
                self.response = v["response"]["id"].as_str().map(String::from);
                events.extend(self.parse.event(v)?);
                if let Some(response) = &self.response {
                    for steer in self.steers.iter_mut().filter(|s| s.status == Status::Waiting) {
                        steer.status = Status::Sent;
                        frames.push(steer_frame(response, &steer.input));
                    }
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" | "error" => {
                self.active = false;
                events.extend(self.parse.event(v)?);
            }
            _ => events.extend(self.parse.event(v)?),
        }
        Ok((events, frames))
    }
    /// The call is over: its last response ended and no steering input can
    /// still produce a successor.
    #[must_use]
    pub fn done(&self) -> bool {
        self.started && !self.active && !self.expecting_successor()
    }
}

pub(super) struct Call {
    pub shared: Arc<Shared>,
    pub dial: Dial,
    pub lane: Lane,
    pub reused: bool,
    pub body: Value,
    pub model: String,
    pub idle_timeout: Duration,
    pub cancel: CancellationToken,
    /// The input items an assistant item made of these parts becomes in the
    /// next request, for the continuation baseline.
    pub replay: Replay,
}

/// Start the call on its lane and stream its events. The connection goes back
/// to `shared` only when the call ends cleanly.
pub(super) fn stream(call: Call) -> EventStream {
    let (tx, rx) = mpsc::channel(256);
    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
    *call.shared.steer.lock().unwrap_or_else(|e| e.into_inner()) = Some(steer_tx);
    tokio::spawn(async move {
        let cancel = call.cancel.clone();
        let result = tokio::select! {
            () = cancel.cancelled() => Err(cancelled()),
            () = tx.closed() => return,
            result = drive(call, &tx, steer_rx) => result,
        };
        if let Err(error) = result {
            let _ = tx.send(Err(error)).await;
        }
    });
    Box::pin(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|event| (event, rx)) }))
}

async fn send(socket: &mut Socket, text: String) -> Result<(), LlmError> {
    socket.send(Message::text(text)).await.map_err(|e| LlmError::new(ErrorClass::Network, format!("WebSocket send: {e}")))
}

async fn drive(call: Call, out: &mpsc::Sender<Result<E, LlmError>>, mut steer_rx: mpsc::UnboundedReceiver<(String, Value)>) -> Result<(), LlmError> {
    let Call { shared, dial, mut lane, mut reused, body, model, idle_timeout, cancel, replay } = call;
    let full = create_frame(body.clone());
    let (rest, input) = split(body);
    let mut continued = continuation(lane.last.take().as_ref(), &rest, &input);
    let mut create = continued.clone().unwrap_or_else(|| full.clone());
    let mut steering = Steering::new(model);
    let mut seen = false;
    let mut pending_create = true;
    let mut parts: Vec<Part> = Vec::new();
    let mut clean = true;
    let mut response_id = None;
    loop {
        if pending_create {
            match send(&mut lane.socket, create.clone()).await {
                Ok(()) => pending_create = false,
                // A parked connection may have been closed by the server.
                Err(_) if reused => {
                    lane = dial.connect(&cancel, idle_timeout).await?;
                    reused = false;
                    continued = None;
                    create.clone_from(&full);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        let message = tokio::select! {
            Some((id, input)) = steer_rx.recv() => {
                if let Some(frame) = steering.steer(id, input) {
                    send(&mut lane.socket, frame).await?;
                }
                continue;
            }
            message = tokio::time::timeout(idle_timeout, lane.socket.next()) => message,
        };
        let message = match message {
            Err(_) => return Err(LlmError::new(ErrorClass::Timeout, "no Responses WebSocket event within the idle timeout")),
            Ok(Some(Ok(message))) if !message.is_close() => message,
            closed => {
                if reused && !seen {
                    lane = dial.connect(&cancel, idle_timeout).await?;
                    reused = false;
                    pending_create = true;
                    continued = None;
                    create.clone_from(&full);
                    continue;
                }
                let detail = match closed {
                    Ok(Some(Err(e))) => e.to_string(),
                    Ok(Some(Ok(message))) => message.as_close().map(|(code, reason)| format!("closed ({code:?}) {reason}")).unwrap_or_default(),
                    _ => "closed".into(),
                };
                return Err(LlmError::new(ErrorClass::Network, format!("Responses WebSocket ended before completion: {detail}")));
            }
        };
        if message.is_ping() {
            let _ = lane.socket.flush().await;
            continue;
        }
        let Some(text) = message.as_text() else { continue };
        seen = true;
        let value: Value = serde_json::from_str(text).map_err(|e| LlmError::new(ErrorClass::Other, e.to_string()))?;
        // The connection no longer holds the response we continued from: send everything.
        if continued.is_some() && value["type"] == "error" && value["error"]["code"] == "previous_response_not_found" {
            tracing::debug!("previous response not cached on this connection; resending the full input");
            continued = None;
            create.clone_from(&full);
            pending_create = true;
            continue;
        }
        let (events, frames) = steering.event(value)?;
        for frame in frames {
            send(&mut lane.socket, frame).await?;
        }
        for event in events {
            match &event {
                E::PartDone { part } => parts.push(part.clone()),
                // Steered successors interleave input we did not send in this call.
                E::Steered { .. } => clean = false,
                E::Finish { reason, response_id: id } => {
                    clean &= matches!(reason, FinishReason::Stop | FinishReason::ToolUse);
                    response_id.clone_from(id);
                }
                _ => {}
            }
            if out.send(Ok(event)).await.is_err() {
                return Ok(());
            }
        }
        if steering.done() {
            if !steering.dirty {
                lane.last = clean.then_some(()).and(response_id).map(|response_id| {
                    let mut baseline = input;
                    baseline.extend(replay(&parts));
                    Last { response_id, rest, baseline }
                });
                shared.park(lane);
            }
            return Ok(());
        }
    }
}
