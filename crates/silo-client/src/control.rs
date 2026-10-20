use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicU64},
    time::Duration,
};

use anyhow::Context;
use serde::de::DeserializeOwned;
use silo_proto::{
    BUILD, Frame,
    control::{ClientHello, Event, Method, Request, RpcError, ServerHello, ServerMessage},
    kind, read_frame, spawn_reader, write_frame,
};
use silo_transport::Conn;
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot, watch},
};

/// How often [`Control::heartbeat`] pings an answering server.
pub const PING_EVERY: Duration = Duration::from_secs(5);
/// A ping unanswered this long is a miss on a local socket; two in a row
/// and the server is not responding.
pub const PING_TIMEOUT_LOCAL: Duration = Duration::from_secs(3);
/// As [`PING_TIMEOUT_LOCAL`], over iroh.
pub const PING_TIMEOUT_IROH: Duration = Duration::from_secs(10);

/// A [`Control::call_timeout`] got no reply in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("server did not reply in time")
    }
}

impl std::error::Error for TimedOut {}

/// A call on a closed connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection closed")
    }
}

impl std::error::Error for Closed {}

/// Waiting calls; `None` once the connection closed so late calls fail
/// instead of waiting forever.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<serde_json::Value, RpcError>>>>>>;

/// Sequenced server events; `None` once the connection closed.
pub type Events = mpsc::UnboundedReceiver<(u64, Event)>;

macro_rules! server_error {
    ($(#[$doc:meta] $name:ident;)*) => {$(
        #[$doc]
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name {
            /// The server's message.
            pub message: String,
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.message)
            }
        }

        impl std::error::Error for $name {}
    )*};
}

server_error! {
    /// The server speaks an incompatible protocol (or predates protocol numbers).
    IncompatibleProtocol;
    /// The server refused our device: not paired, revoked or a bad invite.
    Unauthorized;
    /// The server is handing over to a new build; retry shortly.
    Upgrading;
    /// The server has no reachable address yet (`pair.create`); retry.
    Offline;
    /// The server does not know the method: it is older than this client.
    NotSupported;
}

const UPGRADING: &str = "upgrading";
const OFFLINE: &str = "offline";

/// A server error as its typed client error where one exists.
fn typed(error: RpcError) -> anyhow::Error {
    let message = error.message.clone();
    match error.code.as_str() {
        RpcError::INCOMPATIBLE => IncompatibleProtocol { message }.into(),
        RpcError::METHOD_NOT_FOUND => NotSupported { message }.into(),
        RpcError::UNAUTHORIZED => Unauthorized { message }.into(),
        UPGRADING => Upgrading { message }.into(),
        OFFLINE => Offline { message }.into(),
        _ => error.into(),
    }
}

/// The server does not speak this protocol's hello.
fn incompatible_hello() -> anyhow::Error {
    IncompatibleProtocol { message: "the server did not complete this protocol's hello".into() }.into()
}

/// The hello reply of a compatible server ([`silo_proto::compat::check`]),
/// or, when speaking as another `build` (best-effort inspection of an old
/// server), of that build. Any other shape or an unknown rejection is
/// [`IncompatibleProtocol`].
fn server_hello(reply: &Frame, build: &str) -> anyhow::Result<ServerHello> {
    if reply.kind == kind::CONTROL_HELLO {
        let hello = reply.parse::<ServerHello>().map_err(|_| incompatible_hello())?;
        if build != BUILD {
            return if hello.build == build { Ok(hello) } else { Err(incompatible_hello()) };
        }
        silo_proto::compat::check(hello.protocol, &hello.vt).map_err(|e| IncompatibleProtocol { message: e.message })?;
        return Ok(hello);
    }
    match reply.parse::<ServerMessage>() {
        Ok(ServerMessage::Response { error: Some(error), .. }) => match typed(error) {
            e if e.is::<RpcError>() => Err(incompatible_hello()),
            e => Err(e),
        },
        _ => Err(incompatible_hello()),
    }
}

/// A control connection. Cheap to clone; all clones share one stream.
#[derive(Clone)]
pub struct Control {
    inner: Arc<Inner>,
}

struct Inner {
    tx: mpsc::UnboundedSender<Frame>,
    pending: Pending,
    next_id: AtomicU64,
    hello: ServerHello,
    conn: Conn,
    client_name: String,
    closed: watch::Receiver<bool>,
    /// Bumped when a known event failed to parse (see [`Control::stale`]).
    stale: watch::Receiver<u64>,
    /// Set to stop the reader and writer: the connection closes locally.
    shutdown: watch::Sender<bool>,
}

impl Drop for Inner {
    /// The connection belongs to this control: its data streams end with it.
    fn drop(&mut self) {
        if let Conn::Iroh(rc) = &self.conn {
            rc.close(b"client closed");
        }
    }
}

impl Control {
    /// Connect to the server socket and complete the hello exchange.
    /// Returns the connection and its event stream (events flow after
    /// [`Control::subscribe`]).
    ///
    /// # Errors
    /// Connection or handshake failure.
    pub async fn connect(socket: &Path, client_name: &str) -> anyhow::Result<(Self, Events)> {
        Self::connect_local(socket, client_name, false).await
    }

    /// Connect to the local server socket; a `passive` client does not keep
    /// an idle auto-started server alive.
    ///
    /// # Errors
    /// Connection or handshake failure.
    pub async fn connect_local(socket: &Path, client_name: &str, passive: bool) -> anyhow::Result<(Self, Events)> {
        Self::connect_build(Conn::Unix(socket.to_owned()), client_name, passive, BUILD).await
    }

    /// Hello over `conn` announcing `build`: [`BUILD`] for any compatible
    /// server; another build only for best-effort `session.list` /
    /// `server.shutdown` on an incompatible server of that build.
    ///
    /// # Errors
    /// Connection or handshake failure; a build rejection is a
    /// [`IncompatibleProtocol`], a refused device [`Unauthorized`].
    pub async fn connect_build(conn: Conn, client_name: &str, passive: bool, build: &str) -> anyhow::Result<(Self, Events)> {
        Self::handshake(conn, client_name, passive, build, None).await
    }

    /// Hello over a fresh stream on `conn`, then start the reader/writer.
    pub(crate) async fn handshake(
        conn: Conn,
        client_name: &str,
        passive: bool,
        build: &str,
        invite: Option<String>,
    ) -> anyhow::Result<(Self, Events)> {
        let mut stream = conn.open_stream().await?;
        let hello = ClientHello { build: build.to_owned(), ..ClientHello::ours(client_name, passive, invite) };
        write_frame(&mut stream, &Frame::json(kind::CONTROL_HELLO, &hello)?).await?;
        stream.flush().await?;
        let hello = match read_frame(&mut stream).await {
            Ok(Some(reply)) => server_hello(&reply, build)?,
            Ok(None) => return Err(incompatible_hello()),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Err(incompatible_hello()),
            Err(e) => return Err(e.into()),
        };
        let (reader, mut writer) = tokio::io::split(stream);

        let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
        let (shutdown, mut writer_stop) = watch::channel(false);
        let mut reader_stop = shutdown.subscribe();
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    frame = rx.recv() => frame,
                    _ = writer_stop.wait_for(|stop| *stop) => None,
                };
                let Some(frame) = frame else { break };
                if write_frame(&mut writer, &frame).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (closed_tx, closed_rx) = watch::channel(false);
        let (stale_tx, stale_rx) = watch::channel(0u64);
        let reader_pending = pending.clone();
        let mut frames = spawn_reader(reader, 256);
        let remote = match &conn {
            Conn::Iroh(rc) => Some(rc.clone()),
            Conn::Unix(_) => None,
        };
        tokio::spawn(async move {
            let conn_closed = async {
                match remote {
                    Some(rc) => drop(rc.closed().await),
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(conn_closed);
            loop {
                let frame = tokio::select! {
                    frame = frames.recv() => frame,
                    () = &mut conn_closed => break,
                    _ = reader_stop.wait_for(|stop| *stop) => break,
                };
                let Some(Ok(frame)) = frame else { break };
                // A newer server's frame kinds are skipped.
                if frame.kind != kind::CONTROL {
                    continue;
                }
                match frame.parse::<ServerMessage>() {
                    Ok(ServerMessage::Response { id, result, error }) => {
                        let waiter = reader_pending.lock().unwrap().as_mut().and_then(|p| p.remove(&id));
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(match error {
                                Some(error) => Err(error),
                                None => Ok(result.unwrap_or(serde_json::Value::Null)),
                            });
                        }
                    }
                    Ok(ServerMessage::Event { seq, event }) => {
                        let _ = events_tx.send((seq, *event));
                    }
                    Ok(ServerMessage::UnknownEvent { seq }) => tracing::debug!(seq, "skipped an unknown event"),
                    Ok(ServerMessage::MalformedEvent { seq, event, error }) => {
                        tracing::warn!(seq, event, "unreadable event, resyncing: {error}");
                        stale_tx.send_modify(|n| *n += 1);
                    }
                    Err(e) => tracing::warn!("bad control message: {e}"),
                }
            }
            reader_pending.lock().unwrap().take();
            let _ = closed_tx.send(true);
        });
        let inner = Inner {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            hello,
            conn,
            client_name: client_name.to_owned(),
            closed: closed_rx,
            stale: stale_rx,
            shutdown,
        };
        Ok((Self { inner: Arc::new(inner) }, events_rx))
    }

    #[must_use]
    pub fn hello(&self) -> &ServerHello {
        &self.inner.hello
    }

    /// Changes whenever an event this build knows arrived unreadable (a
    /// newer server's data inside it): state built from events is stale,
    /// re-fetch it.
    #[must_use]
    pub fn stale(&self) -> watch::Receiver<u64> {
        self.inner.stale.clone()
    }

    /// Whether the server advertises `feature` (see
    /// [`silo_proto::compat::feature`]).
    #[must_use]
    pub fn peer_supports(&self, feature: &str) -> bool {
        self.inner.hello.features.iter().any(|f| f == feature)
    }

    #[must_use]
    pub fn client_id(&self) -> u64 {
        self.inner.hello.client_id
    }

    /// The route to the server; data streams open through it.
    #[must_use]
    pub fn conn(&self) -> &Conn {
        &self.inner.conn
    }

    #[must_use]
    pub fn client_name(&self) -> &str {
        &self.inner.client_name
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
    }

    /// Resolves when the connection closes.
    pub async fn closed(&self) {
        let mut rx = self.inner.closed.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// How long a ping may go unanswered on this route.
    #[must_use]
    pub fn ping_timeout(&self) -> Duration {
        match self.inner.conn {
            Conn::Unix(_) => PING_TIMEOUT_LOCAL,
            Conn::Iroh(_) => PING_TIMEOUT_IROH,
        }
    }

    /// Pings every [`PING_EVERY`]; resolves once two pings in a row go
    /// unanswered for [`Control::ping_timeout`], having started closing
    /// this connection (calls then fail and [`Control::closed`] resolves).
    /// Polled in the same `select!` as the event stream it wins over the
    /// stream ending. Never resolves if the connection closes another way.
    pub async fn heartbeat(self) {
        let timeout = self.ping_timeout();
        let mut missed = 0;
        loop {
            match self.call_timeout(Method::Ping {}, timeout).await {
                Err(e) if e.is::<TimedOut>() => missed += 1,
                Err(e) if e.is::<Closed>() => return std::future::pending().await,
                _ => missed = 0,
            }
            if missed >= 2 {
                break;
            }
            if missed == 0 {
                tokio::time::sleep(PING_EVERY).await;
            }
        }
        self.inner.shutdown.send_replace(true);
        if let Conn::Iroh(rc) = &self.inner.conn {
            rc.close(b"server not responding");
        }
    }

    /// Call a method and return its raw JSON result.
    ///
    /// # Errors
    /// Server error responses or a closed connection.
    pub async fn call_value(&self, method: Method) -> anyhow::Result<serde_json::Value> {
        let (_, rx) = self.send(method)?;
        Self::reply(rx).await
    }

    /// [`Control::call_value`] giving up after `timeout` with [`TimedOut`];
    /// the abandoned call is forgotten.
    ///
    /// # Errors
    /// [`TimedOut`], server error responses or a closed connection.
    pub async fn call_timeout(&self, method: Method, timeout: Duration) -> anyhow::Result<serde_json::Value> {
        let (id, rx) = self.send(method)?;
        if let Ok(reply) = tokio::time::timeout(timeout, Self::reply(rx)).await {
            return reply;
        }
        if let Some(pending) = self.inner.pending.lock().unwrap().as_mut() {
            pending.remove(&id);
        }
        Err(TimedOut.into())
    }

    /// Register a call and queue its request.
    fn send(&self, method: Method) -> anyhow::Result<(u64, oneshot::Receiver<Result<serde_json::Value, RpcError>>)> {
        let id = self.inner.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        match self.inner.pending.lock().unwrap().as_mut() {
            Some(pending) => pending.insert(id, reply),
            None => return Err(Closed.into()),
        };
        let frame = Frame::json(kind::CONTROL, &Request { id, method })?;
        if self.inner.tx.send(frame).is_err() {
            if let Some(pending) = self.inner.pending.lock().unwrap().as_mut() {
                pending.remove(&id);
            }
            return Err(Closed.into());
        }
        Ok((id, rx))
    }

    async fn reply(rx: oneshot::Receiver<Result<serde_json::Value, RpcError>>) -> anyhow::Result<serde_json::Value> {
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(typed(error)),
            Err(_) => Err(Closed.into()),
        }
    }

    /// Call a method and decode its result.
    ///
    /// # Errors
    /// Server error responses, a closed connection or an unexpected result.
    pub async fn call<T: DeserializeOwned>(&self, method: Method) -> anyhow::Result<T> {
        let value = self.call_value(method).await?;
        serde_json::from_value(value).context("decode result")
    }

    /// Start receiving events on this connection's event stream.
    ///
    /// # Errors
    /// See [`Control::call`].
    pub async fn subscribe(&self) -> anyhow::Result<()> {
        self.call_value(Method::EventsSubscribe {}).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(code: &str) -> anyhow::Error {
        typed(RpcError::new(code, "why"))
    }

    #[tokio::test]
    async fn call_timeout_forgets_the_call() {
        let dir = std::env::temp_dir().join(format!("silo-ct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_frame(&mut stream).await.unwrap();
            let info = silo_proto::control::ServerInfo {
                version: String::new(),
                pid: 0,
                server_id: String::new(),
                started_at: 0,
                endpoint_id: String::new(),
                hostname: String::new(),
                remote_error: None,
                endpoint_addr: None,
                idle_exit_secs: 0,
            };
            let hello = ServerHello::ours(1, info);
            write_frame(&mut stream, &Frame::json(kind::CONTROL_HELLO, &hello).unwrap()).await.unwrap();
            std::future::pending::<()>().await;
        });
        let (control, _events) = Control::connect(&socket, "test").await.unwrap();
        let err = control.call_timeout(Method::Ping {}, Duration::from_millis(50)).await.unwrap_err();
        assert!(err.is::<TimedOut>());
        assert!(control.inner.pending.lock().unwrap().as_ref().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn foreign_hellos_are_build_mismatches() {
        let mismatch = |frame: Frame| server_hello(&frame, BUILD).unwrap_err().is::<IncompatibleProtocol>();
        let rejected =
            |code: &str| Frame::json(kind::CONTROL, &ServerMessage::Response { id: 0, result: None, error: Some(RpcError::new(code, "x")) }).unwrap();
        assert!(mismatch(rejected("protocol")));
        assert!(mismatch(Frame::json(kind::CONTROL_HELLO, &serde_json::json!({"protocol": 1, "client_id": 1})).unwrap()));
        let other_protocol = serde_json::json!({
            "protocol": silo_proto::PROTOCOL + 1, "vt": silo_proto::VT, "build": "b", "client_id": 1,
            "server": {"version": "9", "pid": 1, "server_id": "s", "started_at": 0, "endpoint_id": "e", "idle_exit_secs": 0}
        });
        let error = server_hello(&Frame::json(kind::CONTROL_HELLO, &other_protocol).unwrap(), BUILD).unwrap_err();
        assert!(error.downcast_ref::<IncompatibleProtocol>().unwrap().message.contains("protocol"), "{error:#}");
        let other_build = serde_json::json!({
            "protocol": silo_proto::PROTOCOL, "vt": silo_proto::VT, "build": "another", "client_id": 1,
            "server": {"version": "9", "pid": 1, "server_id": "s", "started_at": 0, "endpoint_id": "e", "idle_exit_secs": 0}
        });
        assert_eq!(server_hello(&Frame::json(kind::CONTROL_HELLO, &other_build).unwrap(), BUILD).unwrap().build, "another");
        assert!(mismatch(Frame::new(kind::CONTROL_HELLO, b"garbage".to_vec())));
        assert!(mismatch(Frame::new(kind::CONTROL, b"garbage".to_vec())));
        assert!(server_hello(&rejected(RpcError::UNAUTHORIZED), BUILD).unwrap_err().is::<Unauthorized>());
        assert!(server_hello(&rejected("upgrading"), BUILD).unwrap_err().is::<Upgrading>());
    }

    #[test]
    fn server_errors_are_typed() {
        assert_eq!(code(RpcError::INCOMPATIBLE).downcast_ref::<IncompatibleProtocol>().unwrap().message, "why");
        assert!(code(RpcError::METHOD_NOT_FOUND).downcast_ref::<NotSupported>().is_some());
        assert!(code(RpcError::UNAUTHORIZED).downcast_ref::<Unauthorized>().is_some());
        assert!(code("upgrading").downcast_ref::<Upgrading>().is_some());
        assert!(code("offline").downcast_ref::<Offline>().is_some());
        assert_eq!(code(RpcError::FORBIDDEN).downcast_ref::<RpcError>().unwrap().code, RpcError::FORBIDDEN);
    }
}
