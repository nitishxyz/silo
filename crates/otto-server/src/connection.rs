use crate::{State, stopped};
use otto_proto::*;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    net::UnixStream,
    sync::{Mutex, broadcast, mpsc, watch},
    task::{JoinHandle, JoinSet},
};

const OUTBOX: usize = 256;
pub(crate) struct Outgoing {
    frame: Frame,
    shutdown: bool,
}
#[derive(Clone)]
pub(crate) struct Sink {
    tx: mpsc::Sender<Outgoing>,
    close: watch::Sender<bool>,
}
impl Sink {
    #[cfg(test)]
    pub fn test_sink(tx: mpsc::Sender<Outgoing>, close: watch::Sender<bool>) -> Self {
        Self { tx, close }
    }
    /// Queue `message`. One that would not fit a frame never reaches the
    /// writer: a response becomes a `too_large` error for its request, a
    /// journal event a malformed-event notice (the client fetches the view
    /// again), a live overlay is dropped; the connection stays open.
    pub fn send(&self, message: &ServerMessage, shutdown: bool) -> bool {
        let Ok(mut frame) = Frame::json(kind::CONTROL, message) else {
            self.close.send_replace(true);
            return false;
        };
        if !frame.fits() {
            let Some(replacement) = oversized(message, frame.payload.len()) else { return true };
            match Frame::json(kind::CONTROL, &replacement) {
                Ok(small) if small.fits() => frame = small,
                _ => {
                    self.close.send_replace(true);
                    return false;
                }
            }
        }
        if self.tx.try_send(Outgoing { frame, shutdown }).is_err() {
            self.close.send_replace(true);
            return false;
        }
        true
    }
    /// A journal event, as clients get it: clipped (`otto_proto::clip`).
    pub fn event(&self, id: Option<String>, seq: i64, mut event: Event) -> bool {
        otto_proto::clip::clip_event(&mut event);
        self.send(&ServerMessage::Event { seq, session_id: id, event: Box::new(event) }, false)
    }
    pub fn close(&self) {
        self.close.send_replace(true);
    }
}
/// What is sent instead of `message`, `len` bytes, which would not fit a frame.
fn oversized(message: &ServerMessage, len: usize) -> Option<ServerMessage> {
    let why = format!("{} is larger than a frame ({})", otto_proto::clip::size_label(len as u64), otto_proto::clip::size_label(MAX_FRAME as u64));
    tracing::warn!(len, "oversized message not sent: {why}");
    match message {
        ServerMessage::Response { id, .. } => Some(ServerMessage::Response {
            id: *id,
            result: None,
            error: Some(RpcError::too_large(format!("The reply ({why})")).with_data(serde_json::json!({"bytes": len, "max": MAX_FRAME}))),
        }),
        ServerMessage::Event { seq, session_id, event } => Some(ServerMessage::MalformedEvent {
            seq: *seq,
            session_id: session_id.clone(),
            event: serde_json::to_value(event).ok().and_then(|v| v["event"].as_str().map(str::to_owned)).unwrap_or_default(),
            error: format!("{}: the event ({why})", RpcError::TOO_LARGE),
        }),
        _ => None,
    }
}
pub(crate) struct Subscriptions {
    pub sessions: HashMap<String, JoinHandle<()>>,
}
impl Drop for Subscriptions {
    fn drop(&mut self) {
        for task in self.sessions.values() {
            task.abort();
        }
    }
}
pub(crate) type Subs = Arc<Mutex<Subscriptions>>;

pub(crate) async fn serve(state: Arc<State>, mut stream: UnixStream, mut index: broadcast::Receiver<(i64, Event)>) {
    let mut stop = state.stop.subscribe();
    let hello = async {
        let frame = read_frame(&mut stream).await?.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
        if frame.kind != kind::HELLO {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "expected HELLO"));
        }
        let hello: ClientHello = frame.parse()?;
        if hello.protocol != PROTOCOL {
            let message =
                ServerMessage::Response { id: 0, result: None, error: Some(RpcError::incompatible(format!("expected protocol {PROTOCOL}"))) };
            write_frame(&mut stream, &Frame::json(kind::CONTROL, &message)?).await?;
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "incompatible protocol"));
        }
        let info = &state.info;
        let hello = ServerHello {
            protocol: PROTOCOL,
            version: info.version.clone(),
            server_id: info.server_id.clone(),
            pid: info.pid,
            started_at: info.started_at,
            client_id: uuid::Uuid::new_v4().to_string(),
            home: crate::dispatch::user_home(),
            silo_home: info.silo_home.clone(),
            build: info.build.clone(),
        };
        write_frame(&mut stream, &Frame::json(kind::HELLO, &hello)?).await
    };
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(5), hello) => if !matches!(result, Ok(Ok(()))) { return; },
        () = stopped(&mut stop) => return,
    }
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Outgoing>(OUTBOX);
    let (close, mut closed) = watch::channel(false);
    let sink = Sink { tx, close };
    let subs = Arc::new(Mutex::new(Subscriptions { sessions: HashMap::new() }));
    let mut requests = JoinSet::new();
    let permits = Arc::new(tokio::sync::Semaphore::new(64));
    let read = async {
        loop {
            let Ok(local) = permits.clone().acquire_owned().await else {
                break;
            };
            let Ok(Some(frame)) = read_frame(&mut reader).await else {
                break;
            };
            if frame.kind != kind::CONTROL {
                break;
            }
            match frame.parse::<Request>() {
                Ok(request) => {
                    if matches!(request.method, Method::Ping {}) {
                        if !sink.send(&ServerMessage::Response { id: request.id, result: Some(serde_json::json!({})), error: None }, false) {
                            break;
                        }
                        continue;
                    }
                    let Ok(global) = state.requests.clone().try_acquire_owned() else {
                        if !sink.send(
                            &ServerMessage::Response { id: request.id, result: None, error: Some(RpcError::busy("server request limit reached")) },
                            false,
                        ) {
                            break;
                        }
                        continue;
                    };
                    let state = state.clone();
                    let sink = sink.clone();
                    let subs = subs.clone();
                    requests.spawn(async move {
                        let (_local, _global) = (local, global);
                        let shutdown = matches!(request.method, Method::ServerShutdown {});
                        let result = crate::dispatch::dispatch(&state, request.method, &sink, &subs).await;
                        let shutdown = shutdown && result.is_ok();
                        let message = match result {
                            Ok(result) => ServerMessage::Response { id: request.id, result: Some(result), error: None },
                            Err(error) => ServerMessage::Response { id: request.id, result: None, error: Some(error) },
                        };
                        if !sink.send(&message, shutdown) && shutdown {
                            state.stop.send_replace(true);
                        }
                    });
                    while requests.try_join_next().is_some() {}
                }
                Err(error) => {
                    let value = frame.parse::<serde_json::Value>().unwrap_or_default();
                    let id = value["id"].as_u64().unwrap_or(0);
                    let error = if let Some(method) = value["method"].as_str()
                        && !METHODS.iter().any(|(name, _)| *name == method)
                    {
                        RpcError::method_not_found(method)
                    } else {
                        RpcError::bad_request(error.to_string())
                    };
                    if !sink.send(&ServerMessage::Response { id, result: None, error: Some(error) }, false) {
                        break;
                    }
                }
            }
        }
    };
    let write = async {
        while let Some(outgoing) = rx.recv().await {
            // `Sink::send` replaces what would not fit; never end the connection over one frame.
            if !outgoing.frame.fits() {
                tracing::warn!(len = outgoing.frame.payload.len(), "oversized frame dropped");
                continue;
            }
            let result = write_frame(&mut writer, &outgoing.frame).await;
            if outgoing.shutdown {
                state.stop.send_replace(true);
            }
            if result.is_err() {
                break;
            }
        }
    };
    let global = async {
        loop {
            match index.recv().await {
                Ok((seq, event)) => {
                    if !sink.event(None, seq, event) {
                        break;
                    }
                }
                Err(_) => {
                    sink.close();
                    break;
                }
            }
        }
    };
    tokio::select! {
        () = read => {},
        () = write => {},
        () = global => {},
        () = stopped(&mut closed) => {},
        () = stopped(&mut stop) => {},
    }
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    let mut subs = subs.lock().await;
    for (_, task) in subs.sessions.drain() {
        task.abort();
        let _ = task.await;
    }
}

pub(crate) async fn subscribe(state: &State, id: String, after: i64, sink: Sink, subs: &Subs) -> Result<Subscribed, RpcError> {
    if after < 0 {
        return Err(RpcError::bad_request("after_seq must be nonnegative"));
    }
    let mut subs = subs.lock().await;
    if let Some(old) = subs.sessions.remove(&id) {
        old.abort();
        let _ = old.await;
    }
    let (replay, mut live) = state.config.runtime.subscribe(&id, after).await.map_err(crate::dispatch::core_error)?;
    let mut deltas = state.config.runtime.session(&id).await.map_err(crate::dispatch::core_error)?.deltas();
    let seq = replay.last().map_or(after, |event| event.0);
    let subscribed = Subscribed { session_id: id.clone(), seq };
    let task_id = id.clone();
    let task = tokio::spawn(async move {
        let mut boundary = seq;
        for (seq, event) in replay {
            if !sink.event(Some(id.clone()), seq, event) {
                return;
            }
            tokio::task::yield_now().await;
        }
        loop {
            tokio::select! {
                biased;
                persisted = live.recv() => match persisted {
                    Ok((seq, event)) => {
                        if event.is_live_delta() || matches!(event, Event::ItemDone { .. }) {
                            drain_live(&sink, &id, boundary, &mut deltas);
                        }
                        if !sink.event(Some(id.clone()), seq, event) { return; }
                        boundary = seq;
                    }
                    Err(_) => { sink.close(); return; }
                },
                delta = deltas.recv() => match delta {
                    Ok(event) => send_live(&sink, &id, boundary, event),
                    Err(broadcast::error::RecvError::Lagged(_)) => {},
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    });
    subs.sessions.insert(task_id, task);
    Ok(subscribed)
}

fn send_live(sink: &Sink, id: &str, after_seq: i64, mut event: Event) {
    if !event.is_live_delta() {
        return;
    }
    otto_proto::clip::clip_event(&mut event);
    let message = ServerMessage::Live { live: true, id: 0, session_id: id.into(), after_seq, event: Box::new(event) };
    // Ephemeral output may be dropped; reserve queue capacity for durable events.
    if sink.tx.capacity() > OUTBOX / 2
        && let Ok(frame) = Frame::json(kind::CONTROL, &message)
        && frame.fits()
    {
        let _ = sink.tx.try_send(Outgoing { frame, shutdown: false });
    }
}

fn drain_live(sink: &Sink, id: &str, after_seq: i64, deltas: &mut broadcast::Receiver<Event>) {
    loop {
        match deltas.try_recv() {
            Ok(event) => send_live(sink, id, after_seq, event),
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[tokio::test]
    async fn huge_events_reach_clients_clipped_and_what_cannot_fit_never_closes() {
        let (tx, mut outbox) = mpsc::channel(OUTBOX);
        let (close, closed) = watch::channel(false);
        let sink = Sink { tx, close };
        let result = otto_llm::Part::ToolResult {
            call_id: "c".into(),
            content: vec![otto_llm::ResultPart::text("r".repeat(20 * 1024 * 1024))],
            is_error: false,
        };
        let item = Item::new("i", otto_llm::Role::Tool, vec![result]);
        assert!(sink.event(Some("s".into()), 1, Event::ItemDone { item }));
        let output = serde_json::json!({"content": [{"type": "text", "text": "o".repeat(20 * 1024 * 1024)}]});
        assert!(sink.event(Some("s".into()), 2, Event::ToolResult { call_id: "c".into(), is_error: false, output, item_id: "i".into() }));
        // Nothing clips a session's info: it cannot fit, so the client is told to fetch the view again.
        let mut info: SessionInfo = serde_json::from_value(serde_json::json!({
            "id": "s", "project_id": "p", "parent_id": null, "agent": "a", "model": "m", "provider": "p", "cwd": "/", "status": "idle",
            "created_at": 0, "updated_at": 0, "last_seq": 3, "usage": {"usage": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "reasoning": 0}, "cost_usd": 0.0},
            "title": ""
        }))
        .unwrap();
        info.title = "t".repeat(MAX_FRAME);
        assert!(sink.event(Some("s".into()), 3, Event::SessionChanged { info: info.clone() }));
        let response = ServerMessage::Response { id: 7, result: Some(serde_json::json!(info)), error: None };
        assert!(sink.send(&response, false));
        let mut got = Vec::new();
        while let Ok(outgoing) = outbox.try_recv() {
            assert!(outgoing.frame.fits());
            got.push(outgoing.frame.parse::<ServerMessage>().unwrap());
        }
        assert!(
            matches!(&got[0], ServerMessage::Event { seq: 1, event, .. } if matches!(&**event, Event::ItemDone { item } if item.clipped.is_some()))
        );
        assert!(
            matches!(&got[1], ServerMessage::Event { seq: 2, event, .. } if matches!(&**event, Event::ToolResult { output, .. } if output["clipped"]["bytes"].as_u64() > Some(20 << 20)))
        );
        // Clients read it as a malformed event (its data is missing) and fetch the view again.
        assert!(matches!(&got[2], ServerMessage::MalformedEvent { seq: 3, event, .. } if event == "session.changed"));
        assert!(matches!(&got[3], ServerMessage::Response { id: 7, error: Some(e), .. } if e.code == RpcError::TOO_LARGE));
        assert!(!*closed.borrow());
    }

    #[tokio::test]
    async fn lagging_live_stream_and_slow_outbox_preserve_durable_events() {
        let (tx, mut outbox) = mpsc::channel(OUTBOX);
        let (close, closed) = watch::channel(false);
        let sink = Sink { tx, close };
        let (deltas, mut receiver) = broadcast::channel(2);
        let event = Event::TextDelta { item_id: "i".into(), text: "x".into() };
        for _ in 0..100 {
            deltas.send(event.clone()).unwrap();
        }
        drain_live(&sink, "s", 1, &mut receiver);
        for _ in 0..OUTBOX * 2 {
            send_live(&sink, "s", 1, event.clone());
        }
        assert_eq!(sink.tx.capacity(), OUTBOX / 2);
        assert!(!*closed.borrow());
        let durable = Event::TextDelta { item_id: "i".into(), text: "x".repeat(100) };
        assert!(sink.event(Some("s".into()), 2, durable));
        let mut count = 0;
        let mut persisted = false;
        while let Ok(outgoing) = outbox.try_recv() {
            match outgoing.frame.parse::<ServerMessage>().unwrap() {
                ServerMessage::Live { after_seq: 1, .. } => count += 1,
                ServerMessage::Event { seq: 2, event, .. } => {
                    assert!(matches!(*event, Event::TextDelta { text, .. } if text.len() == 100));
                    persisted = true;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(persisted);
        assert_eq!(count, OUTBOX / 2);
        assert!(!*closed.borrow());
    }
}
