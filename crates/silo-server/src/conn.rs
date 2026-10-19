//! Control and data stream handling, transport-independent.

use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use silo_proto::{
    BUILD, Frame,
    control::{ClientHello, METHODS, Method, PairInvite, PairedDevices, Request, RpcError, ServerHello, ServerMessage},
    data::{ClientData, DataHello, Reject},
    kind, read_frame, spawn_reader, write_frame,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

use crate::{
    Server,
    auth::Peer,
    block::{Attach, BlockMsg, Sink},
    handoff,
};

/// A remote peer must send its first frame within this time.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `pair.create` waits for the endpoint to learn an address.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(4);
/// Largest first frame (a hello) accepted from a remote peer.
pub const REMOTE_HELLO_MAX: usize = 8 * 1024;

/// [`read_frame`] limited to `max` bytes.
async fn read_frame_max<R: AsyncRead + Unpin>(reader: &mut R, max: usize) -> std::io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad hello length {len}")));
    }
    let mut rest = vec![0; len];
    reader.read_exact(&mut rest).await?;
    Ok(Some(Frame { kind: rest[0], payload: rest.split_off(1) }))
}

/// Which first frames a stream may start with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Any,
    Control,
    Data,
}

/// Serve one accepted stream; its first frame selects control or data.
/// `admitted` fires once a control hello is accepted.
pub async fn serve<S>(server: Arc<Server>, stream: S, peer: Peer, expect: Expect, admitted: Option<tokio::sync::oneshot::Sender<()>>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, writer) = tokio::io::split(stream);
    let first = match &peer {
        Peer::Local => read_frame(&mut reader).await,
        Peer::Remote { .. } => tokio::time::timeout(HELLO_TIMEOUT, read_frame_max(&mut reader, REMOTE_HELLO_MAX)).await.unwrap_or(Ok(None)),
    };
    let first = match first {
        Ok(Some(frame)) => frame,
        _ => return,
    };
    let result = match (first.kind, expect) {
        (kind::CONTROL_HELLO, Expect::Any | Expect::Control) => control(server, first, reader, writer, peer, admitted).await,
        (kind::DATA_HELLO, Expect::Any | Expect::Data) => data(server, first, reader, writer, peer).await,
        other => {
            tracing::debug!("unexpected first frame {:#x}", other.0);
            Ok(())
        }
    };
    if let Err(error) = result {
        tracing::debug!("connection ended: {error}");
    }
}

/// Serve one accepted unix stream, or pass it on as an upgrade request when
/// it starts with [`handoff::MAGIC`].
pub async fn accept(server: Arc<Server>, stream: tokio::net::UnixStream, upgrades: mpsc::UnboundedSender<std::os::unix::net::UnixStream>) {
    match handoff::peek_magic(&stream).await {
        Ok(true) => {}
        Ok(false) => return serve(server, stream, Peer::Local, Expect::Any, None).await,
        Err(error) => return tracing::debug!("peek: {error}"),
    }
    let stream = match stream.into_std() {
        Ok(stream) => stream,
        Err(error) => return tracing::warn!("upgrade stream: {error}"),
    };
    if let Err(error) = silo_pty::handoff::check_peer_uid(&stream) {
        return tracing::warn!("upgrade refused: {error}");
    }
    tracing::info!("successor requested an upgrade");
    let _ = upgrades.send(stream);
}

/// Writer task draining a channel. Every frame must go through the returned
/// [`Sink`] so the unsent-bytes counter stays balanced.
fn spawn_writer<W>(mut writer: W) -> (Sink, tokio::task::JoinHandle<()>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    let sink = Sink::new(tx);
    let counter = sink.queued.clone();
    let cancelled = sink.cancelled();
    let task = tokio::spawn(async move {
        let pump = async {
            while let Some(frame) = rx.recv().await {
                let len = frame.payload.len() + 5;
                let ok = write_frame(&mut writer, &frame).await.is_ok();
                counter.fetch_sub(len, Ordering::AcqRel);
                if !ok {
                    return;
                }
                // Coalesce small frames before flushing.
                if rx.is_empty() && writer.flush().await.is_err() {
                    return;
                }
            }
            let _ = writer.shutdown().await;
        };
        // A cancelled stream stops even mid-write to a peer that stopped
        // reading; unsent frames are abandoned.
        tokio::select! {
            () = pump => {}
            () = cancelled => {}
        }
    });
    (sink, task)
}

async fn control<R, W>(server: Arc<Server>, hello: Frame, reader: R, writer: W, peer: Peer, on_admit: Option<tokio::sync::oneshot::Sender<()>>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let hello: ClientHello = hello.parse()?;
    let (tx, writer_task) = spawn_writer(writer);
    // Builds are compared first: a mismatched build never enrolls.
    let admitted = if hello.build != BUILD {
        Err(RpcError::different_build(BUILD, &hello.build))
    } else if let Peer::Remote { endpoint_id, .. } = &peer {
        let mut auth = server.auth();
        // Checked under the lock: once the upgrade commits, pairing is
        // frozen and the successor's re-read sees every grant.
        if server.upgrading.load(Ordering::SeqCst) {
            Err(RpcError::new("upgrading", "the server is handing over to a new build; retry"))
        } else {
            auth.authorize(endpoint_id, &hello.client_name, hello.invite.as_deref())
        }
    } else {
        Ok(())
    };
    if let Err(error) = admitted {
        tx.send(Frame::json(kind::CONTROL, &ServerMessage::Response {
            id: 0,
            result: None,
            error: Some(error),
        })?);
        drop(tx);
        let _ = writer_task.await;
        return Ok(());
    }
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel();
    let client_id = {
        let mut reg = server.registry.lock().unwrap();
        let id = reg.add_client(hello.client_name.clone(), hello.passive);
        if let Some(entry) = reg.clients.get_mut(&id) {
            entry.close = Some(close_tx);
            entry.origin = peer.conn_id();
        }
        id
    };
    let reply = ServerHello { build: BUILD.to_owned(), client_id, server: server.info_now().await };
    tx.send(Frame::json(kind::CONTROL_HELLO, &reply)?);
    server.touch();
    if let Some(on_admit) = on_admit {
        let _ = on_admit.send(());
    }

    let mut frames = spawn_reader(reader, 64);
    let result = loop {
        tokio::select! {
            frame = frames.recv() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    None => break Ok(()),
                    Some(Err(e)) => break Err(e),
                };
                if frame.kind != kind::CONTROL {
                    continue;
                }
                let request: Request = match frame.parse() {
                    Ok(request) => request,
                    Err(e) => {
                        let id = serde_json::from_slice::<Value>(&frame.payload).ok().and_then(|v| v.get("id")?.as_u64()).unwrap_or(0);
                        respond(&tx, id, Err(RpcError::bad_request(e.to_string())));
                        continue;
                    }
                };
                let id = request.id;
                if let Err(error) = permitted(&peer, &request.method) {
                    respond(&tx, id, Err(error));
                    continue;
                }
                let subscribe = matches!(request.method, Method::EventsSubscribe {});
                if subscribe {
                    server.registry.lock().unwrap().subscribe(client_id, tx.clone());
                }
                let shutdown = matches!(request.method, Method::ServerShutdown {});
                let read_only = matches!(request.method, Method::ServerInfo {} | Method::ApiList {} | Method::EventsSubscribe {} | Method::SessionList {} | Method::SessionView { .. });
                let result = if server.upgrading.load(Ordering::SeqCst) && !read_only {
                    Err(RpcError::new("upgrading", "the server is handing over to a new build; retry"))
                } else {
                    handle(&server, client_id, request.method).await
                };
                respond(&tx, id, result);
                if shutdown {
                    server.shutdown.notify_one();
                }
            }
            _ = &mut close_rx => break Ok(()),
        }
    };
    server.registry.lock().unwrap().remove_client(client_id);
    server.touch();
    drop(tx);
    let _ = writer_task.await;
    result
}

fn respond(tx: &Sink, id: u64, result: Result<Value, RpcError>) {
    let message = match result {
        Ok(value) => ServerMessage::Response { id, result: Some(value), error: None },
        Err(error) => ServerMessage::Response { id, result: None, error: Some(error) },
    };
    if let Ok(frame) = Frame::json(kind::CONTROL, &message) {
        tx.send(frame);
    }
}

fn to_value(value: impl serde::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::internal)
}

/// Owner-only methods: pairing and stopping the server. (Upgrades arrive
/// only over the unix socket.) Everything else is open to paired devices.
fn permitted(peer: &Peer, method: &Method) -> Result<(), RpcError> {
    let name = match method {
        Method::ServerShutdown {} => "server.shutdown",
        Method::PairCreate { .. } => "pair.create",
        Method::PairList {} => "pair.list",
        Method::PairRevoke { .. } => "pair.revoke",
        _ => return Ok(()),
    };
    match peer {
        Peer::Local => Ok(()),
        Peer::Remote { .. } => Err(RpcError::forbidden(name)),
    }
}

async fn handle(server: &Server, client_id: u64, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::ServerInfo {} => to_value(server.info_now().await),
        Method::ServerShutdown {} => Ok(json!({})),
        Method::ApiList {} => Ok(json!(METHODS.iter().map(|(name, description)| json!({"name": name, "description": description})).collect::<Vec<_>>())),
        Method::EventsSubscribe {} => Ok(json!({})),
        Method::SessionList {} => to_value(server.registry.lock().unwrap().summaries()),
        Method::SessionCreate { name, spawn } => to_value(server.registry.lock().unwrap().create_session(name, &spawn)?),
        Method::SessionView { session_id } => to_value(server.registry.lock().unwrap().view(&session_id)?),
        Method::SessionRename { session_id, name } => {
            server.registry.lock().unwrap().rename_session(&session_id, name)?;
            Ok(json!({}))
        }
        Method::SessionClose { session_id } => {
            server.registry.lock().unwrap().close_session(&session_id)?;
            Ok(json!({}))
        }
        Method::WindowCreate { session_id, spawn } => to_value(server.registry.lock().unwrap().create_window(&session_id, &spawn)?),
        Method::WindowSelect { session_id, window_id } => {
            server.registry.lock().unwrap().select_window(&session_id, &window_id)?;
            Ok(json!({}))
        }
        Method::WindowClose { window_id } => {
            server.registry.lock().unwrap().close_window(&window_id)?;
            Ok(json!({}))
        }
        Method::WindowRename { window_id, name } => {
            server.registry.lock().unwrap().rename_window(&window_id, name)?;
            Ok(json!({}))
        }
        Method::WindowMove { window_id, index } => {
            server.registry.lock().unwrap().move_window(&window_id, index)?;
            Ok(json!({}))
        }
        Method::BlockSplit { block_id, axis, spawn } => to_value(server.registry.lock().unwrap().split(&block_id, axis, &spawn)?),
        Method::BlockClose { block_id } => {
            server.registry.lock().unwrap().close_block(&block_id)?;
            Ok(json!({}))
        }
        Method::BlockFocus { block_id } => {
            server.registry.lock().unwrap().focus(&block_id)?;
            Ok(json!({}))
        }
        Method::BlockZoom { block_id, zoomed } => {
            server.registry.lock().unwrap().zoom(&block_id, zoomed)?;
            Ok(json!({}))
        }
        Method::BlockSwap { block_id, other_block_id } => {
            server.registry.lock().unwrap().swap(&block_id, &other_block_id)?;
            Ok(json!({}))
        }
        Method::LayoutRatio { window_id, path, ratio } => {
            server.registry.lock().unwrap().set_ratio(&window_id, &path, ratio)?;
            Ok(json!({}))
        }
        Method::TerminalSend { block_id, text } => {
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::Input(text.into_bytes()));
            Ok(json!({}))
        }
        Method::TerminalCapture { block_id, history } => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::Capture { history, reply });
            to_value(rx.await.map_err(|_| RpcError::not_found(format!("block {block_id}")))?)
        }
        Method::TerminalClear { block_id } => {
            // Home + erase screen + erase scrollback for every client, then
            // ask the program to redraw (shells repaint their prompt on ^L).
            let msg = BlockMsg::Inject { bytes: b"\x1b[H\x1b[2J\x1b[3J".to_vec(), input: b"\x0c".to_vec() };
            server.registry.lock().unwrap().block(&block_id)?.send(msg);
            Ok(json!({}))
        }
        Method::TerminalReset { block_id } => {
            let msg = BlockMsg::Inject { bytes: b"\x1bc".to_vec(), input: b"\x0c".to_vec() };
            server.registry.lock().unwrap().block(&block_id)?.send(msg);
            Ok(json!({}))
        }
        Method::TerminalSize { block_id } => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::SizeInfo { reply });
            to_value(rx.await.map_err(|_| RpcError::not_found(format!("block {block_id}")))?)
        }
        Method::TerminalResize { block_id, size } => {
            if size.columns == 0 || size.rows == 0 {
                return Err(RpcError::bad_request("size must be at least 1x1"));
            }
            let reg = server.registry.lock().unwrap();
            let client_name = reg.client_name(client_id);
            reg.block(&block_id)?.send(BlockMsg::Desired { client_id, client_name, size });
            Ok(json!({}))
        }
        Method::OwnBlock { block_ids, mode } => {
            let reg = server.registry.lock().unwrap();
            for id in &block_ids {
                reg.block(id)?.send(BlockMsg::Own { client_id, mode });
            }
            Ok(json!({}))
        }
        Method::DisownBlock { block_ids } => {
            let reg = server.registry.lock().unwrap();
            for id in &block_ids {
                reg.block(id)?.send(BlockMsg::Disown { client_id });
            }
            Ok(json!({}))
        }
        Method::DataConnect { block_id } => to_value(server.registry.lock().unwrap().issue_ticket(client_id, &block_id)?),
        Method::UiOpenSession { session_id } => {
            let mut reg = server.registry.lock().unwrap();
            let id = reg.view(&session_id)?.session.id;
            reg.emit(silo_proto::control::Event::OpenSession { session_id: id });
            Ok(json!({}))
        }
        Method::PairCreate { ttl_secs } => {
            let ttl = Duration::from_secs(ttl_secs);
            if ttl.is_zero() || ttl > crate::auth::MAX_INVITE_TTL {
                return Err(RpcError::bad_request(format!("ttl_secs must be between 1 and {}", crate::auth::MAX_INVITE_TTL.as_secs())));
            }
            let addr = {
                let net = server.net.lock().await;
                let net = net.as_ref().ok_or_else(|| RpcError::internal("remote access is not running"))?;
                // Wait for the home relay, or for any address without relays.
                if server.relays || net.addr().is_empty() {
                    let _ = tokio::time::timeout(ONLINE_TIMEOUT, net.online()).await;
                }
                net.addr()
            };
            if addr.is_empty() {
                return Err(RpcError::new("offline", "the server has no relay or direct address yet; retry"));
            }
            let secret = server.auth().create_invite(ttl)?;
            let ticket = silo_transport::PairTicket { addr, secret }.encode();
            to_value(PairInvite { ticket, expires_in_secs: ttl_secs })
        }
        Method::PairList {} => to_value(PairedDevices { devices: server.auth().devices() }),
        Method::PairRevoke { id } => {
            server.auth().revoke(&id)?;
            Ok(json!({}))
        }
    }
}

async fn data<R, W>(server: Arc<Server>, hello: Frame, reader: R, writer: W, peer: Peer) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let hello: DataHello = hello.parse()?;
    let (tx, writer_task) = spawn_writer(writer);
    let redeemed = server.registry.lock().unwrap().redeem(&hello.ticket, peer.conn_id());
    let reject = |code: &str, reason: &str| -> std::io::Result<()> {
        tx.send(Frame::json(kind::REJECT, &Reject { code: code.into(), reason: reason.into() })?);
        Ok(())
    };
    if hello.build != BUILD {
        reject(RpcError::DIFFERENT_BUILD, &RpcError::different_build(BUILD, &hello.build).message)?;
    } else if let Some((block_id, client_id)) = redeemed {
        let block = {
            let reg = server.registry.lock().unwrap();
            reg.blocks.get(&block_id).map(|b| (b.tx.clone(), reg.client_name(client_id)))
        };
        if let Some((block, client_name)) = block {
            let sub_id = rand::random::<u64>();
            let cancelled = tx.cancelled();
            let _ = block.send(BlockMsg::Attach(Box::new(Attach { sub_id, client_id, resume: hello.resume, sink: tx.clone() })));
            drop(tx);
            let mut frames = spawn_reader(reader, 256);
            tokio::pin!(cancelled);
            loop {
                // A cut-off subscriber's stream ends now: dropping both
                // halves closes the connection so the client reconnects.
                let frame = tokio::select! {
                    frame = frames.recv() => frame,
                    () = &mut cancelled => break,
                };
                let Some(Ok(frame)) = frame else { break };
                match ClientData::from_frame(frame) {
                    Ok(ClientData::Ack(offset)) => {
                        let _ = block.send(BlockMsg::Ack { sub_id, offset });
                    }
                    Ok(ClientData::Input(bytes)) => {
                        let _ = block.send(BlockMsg::Input(bytes));
                    }
                    Ok(ClientData::Resize(size)) if size.columns > 0 && size.rows > 0 => {
                        let _ = block.send(BlockMsg::Desired { client_id, client_name: client_name.clone(), size });
                    }
                    Ok(ClientData::Config(config)) => {
                        let _ = block.send(BlockMsg::Config { client_id, config });
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = block.send(BlockMsg::Detach(sub_id));
            writer_task.abort();
            return Ok(());
        }
        reject("not_found", "block no longer exists")?;
    } else {
        reject("ticket", "invalid or expired ticket")?;
    }
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}
