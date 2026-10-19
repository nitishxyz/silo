use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicU64},
};

use anyhow::{Context, anyhow, bail};
use serde::de::DeserializeOwned;
use silo_proto::{
    BUILD, Frame,
    control::{ClientHello, Event, Method, Request, RpcError, ServerHello, ServerMessage},
    kind, read_frame, spawn_reader, write_frame,
};
use silo_transport::Conn;
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot},
};

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
    /// The server rejected our hello because it is a different build.
    BuildMismatch;
    /// The server refused our device: not paired, revoked or a bad invite.
    Unauthorized;
    /// The server is handing over to a new build; retry shortly.
    Upgrading;
    /// The server has no reachable address yet (`pair.create`); retry.
    Offline;
}

const UPGRADING: &str = "upgrading";
const OFFLINE: &str = "offline";

/// A server error as its typed client error where one exists.
fn typed(error: RpcError) -> anyhow::Error {
    let message = error.message.clone();
    match error.code.as_str() {
        RpcError::DIFFERENT_BUILD => BuildMismatch { message }.into(),
        RpcError::UNAUTHORIZED => Unauthorized { message }.into(),
        UPGRADING => Upgrading { message }.into(),
        OFFLINE => Offline { message }.into(),
        _ => error.into(),
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
    closed: tokio::sync::watch::Receiver<bool>,
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

    /// Hello over `conn` announcing `build`. Only for best-effort
    /// `session.list` / `server.shutdown` on a server of another build,
    /// whose shapes may differ.
    ///
    /// # Errors
    /// Connection or handshake failure; a build rejection is a
    /// [`BuildMismatch`], a refused device [`Unauthorized`].
    pub async fn connect_build(conn: Conn, client_name: &str, passive: bool, build: &str) -> anyhow::Result<(Self, Events)> {
        Self::handshake(conn, client_name, passive, build, None).await
    }

    /// Hello over a fresh stream on `conn`, then start the reader/writer.
    pub(crate) async fn handshake(conn: Conn, client_name: &str, passive: bool, build: &str, invite: Option<String>) -> anyhow::Result<(Self, Events)> {
        let mut stream = conn.open_stream().await?;
        let hello = ClientHello { build: build.to_owned(), client_name: client_name.to_owned(), token: None, passive, invite };
        write_frame(&mut stream, &Frame::json(kind::CONTROL_HELLO, &hello)?).await?;
        stream.flush().await?;
        let reply = read_frame(&mut stream).await?.ok_or_else(|| anyhow!("server closed during hello"))?;
        if reply.kind != kind::CONTROL_HELLO {
            if let Ok(ServerMessage::Response { error: Some(error), .. }) = reply.parse::<ServerMessage>() {
                return Err(typed(error));
            }
            bail!("unexpected hello reply");
        }
        let hello: ServerHello = reply.parse()?;
        let (reader, mut writer) = tokio::io::split(stream);

        let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_frame(&mut writer, &frame).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
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
                };
                let Some(Ok(frame)) = frame else { break };
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
                        let _ = events_tx.send((seq, event));
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
        };
        Ok((Self { inner: Arc::new(inner) }, events_rx))
    }

    #[must_use]
    pub fn hello(&self) -> &ServerHello {
        &self.inner.hello
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

    /// Call a method and return its raw JSON result.
    ///
    /// # Errors
    /// Server error responses or a closed connection.
    pub async fn call_value(&self, method: Method) -> anyhow::Result<serde_json::Value> {
        let id = self.inner.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        match self.inner.pending.lock().unwrap().as_mut() {
            Some(pending) => pending.insert(id, reply),
            None => bail!("connection closed"),
        };
        let frame = Frame::json(kind::CONTROL, &Request { id, method })?;
        if self.inner.tx.send(frame).is_err() {
            if let Some(pending) = self.inner.pending.lock().unwrap().as_mut() {
                pending.remove(&id);
            }
            bail!("connection closed");
        }
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(typed(error)),
            Err(_) => bail!("connection closed"),
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

    #[test]
    fn server_errors_are_typed() {
        assert_eq!(code(RpcError::DIFFERENT_BUILD).downcast_ref::<BuildMismatch>().unwrap().message, "why");
        assert!(code(RpcError::UNAUTHORIZED).downcast_ref::<Unauthorized>().is_some());
        assert!(code("upgrading").downcast_ref::<Upgrading>().is_some());
        assert!(code("offline").downcast_ref::<Offline>().is_some());
        assert_eq!(code(RpcError::FORBIDDEN).downcast_ref::<RpcError>().unwrap().code, RpcError::FORBIDDEN);
    }
}
