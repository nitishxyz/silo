use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicU64},
};

use anyhow::{Context, anyhow, bail};
use serde::de::DeserializeOwned;
use silo_proto::{
    Frame, PROTOCOL_VERSION,
    control::{ClientHello, Event, Method, Request, RpcError, ServerHello, ServerMessage},
    kind, read_frame, spawn_reader, write_frame,
};
use tokio::{
    net::UnixStream,
    sync::{mpsc, oneshot},
};

/// Waiting calls; `None` once the connection closed so late calls fail
/// instead of waiting forever.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<serde_json::Value, RpcError>>>>>>;

/// Sequenced server events; `None` once the connection closed.
pub type Events = mpsc::UnboundedReceiver<(u64, Event)>;

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
    socket: PathBuf,
    client_name: String,
    closed: tokio::sync::watch::Receiver<bool>,
}

impl Control {
    /// Connect to the server socket and complete the hello exchange.
    /// Returns the connection and its event stream (events flow after
    /// [`Control::subscribe`]).
    ///
    /// # Errors
    /// Connection or handshake failure.
    pub async fn connect(socket: &Path, client_name: &str) -> anyhow::Result<(Self, Events)> {
        Self::connect_with(socket, client_name, false).await
    }

    /// Like [`Control::connect`]; a `passive` client does not keep an idle
    /// auto-started server alive.
    ///
    /// # Errors
    /// Connection or handshake failure.
    pub async fn connect_with(socket: &Path, client_name: &str, passive: bool) -> anyhow::Result<(Self, Events)> {
        let stream = UnixStream::connect(socket).await.with_context(|| format!("connect {}", socket.display()))?;
        let (mut reader, mut writer) = stream.into_split();
        let hello = ClientHello { protocol: PROTOCOL_VERSION, client_name: client_name.to_owned(), token: None, passive };
        write_frame(&mut writer, &Frame::json(kind::CONTROL_HELLO, &hello)?).await?;
        let reply = read_frame(&mut reader).await?.ok_or_else(|| anyhow!("server closed during hello"))?;
        if reply.kind != kind::CONTROL_HELLO {
            if let Ok(ServerMessage::Response { error: Some(error), .. }) = reply.parse::<ServerMessage>() {
                bail!("server rejected client: {error}");
            }
            bail!("unexpected hello reply");
        }
        let hello: ServerHello = reply.parse()?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_frame(&mut writer, &frame).await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        let reader_pending = pending.clone();
        let mut frames = spawn_reader(reader, 256);
        tokio::spawn(async move {
            while let Some(Ok(frame)) = frames.recv().await {
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
            socket: socket.to_owned(),
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

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.inner.socket
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
            Ok(Err(error)) => Err(error.into()),
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
