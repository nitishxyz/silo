//! Control and data stream handling, transport-independent.

use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use silo_proto::{
    Frame,
    access::{DEFAULT_LINK_TTL_SECS, Grant, MAX_GRANT_TTL_SECS, Need, Rights, Scope, SecretKind, Who, encode_secret, need, secret_hash},
    api, compat,
    control::{
        ApiMethod, BlockView, BrowserCommand, ClientHello, ClientRole, Empty, Event, GrantCreated, GrantKind, Grants, METHODS, Method, OfferSource,
        PairInvite, PairedDevices, Pasted, Request, RpcError, ServerHello, ServerMessage, ServiceSocket,
    },
    data::{ClientData, DataHello, MAX_CLIP_TEXT, Reject},
    kind,
    model::{BlockKind, short_client_name},
    read_frame, spawn_reader,
    upload::UploadHello,
    write_frame,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

use crate::{
    Server,
    access::{On, Principal, rights_on},
    approval,
    auth::{Admission, Peer},
    block::{Attach, BlockMsg, Sink},
    browser::{Block, Report},
    grants::{new_id, unix_secs},
    handoff,
    registry::New,
};

/// A remote peer must send its first frame within this time.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `pair.create` waits for the endpoint to learn an address.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(4);
/// Largest first frame (a hello) accepted from a remote peer.
pub const REMOTE_HELLO_MAX: usize = 8 * 1024;
/// A join-invite connection must offer its host within this time.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `clipboard.read` waits for an attached client to answer.
pub const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_secs(3);

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
        Peer::Local { .. } => read_frame(&mut reader).await,
        Peer::Remote { .. } => tokio::time::timeout(HELLO_TIMEOUT, read_frame_max(&mut reader, REMOTE_HELLO_MAX)).await.unwrap_or(Ok(None)),
    };
    let first = match first {
        Ok(Some(frame)) => frame,
        _ => return,
    };
    let result = match (first.kind, expect) {
        (kind::CONTROL_HELLO, Expect::Any | Expect::Control) => control(server, first, reader, writer, peer, admitted).await,
        (kind::DATA_HELLO, Expect::Any | Expect::Data) => data(server, first, reader, writer, peer).await,
        (kind::UPLOAD_HELLO, Expect::Any | Expect::Data) => upload(server, first, reader, writer, peer).await,
        (kind::DOWNLOAD_HELLO, Expect::Any | Expect::Data) => download(server, first, reader, writer, peer).await,
        (kind::UPGRADE_HELLO, Expect::Any | Expect::Control) => crate::upgrade::serve(server, first, reader, writer, peer).await,
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
        Ok(false) => {
            let pid = stream.peer_cred().ok().and_then(|c| c.pid());
            return serve(server, stream, Peer::Local { pid }, Expect::Any, None).await;
        }
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
/// [`Sink`] so the unsent-bytes counter stays balanced. `guard` is dropped
/// when the task ends, however it ends.
fn spawn_writer<W>(mut writer: W, guard: impl Send + 'static) -> (Sink, tokio::task::JoinHandle<()>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    let sink = Sink::new(tx);
    let counter = sink.queued.clone();
    let cancelled = sink.cancelled();
    let task = tokio::spawn(async move {
        let _guard = guard;
        let pump = async {
            while let Some(frame) = rx.recv().await {
                let len = frame.payload.len() + 5;
                let ok = write_frame(&mut writer, &frame).await.is_ok();
                counter.fetch_sub(len, Ordering::AcqRel);
                if !ok {
                    return;
                }
                // Coalesce small frames before flushing.
                if rx.is_empty() {
                    if writer.flush().await.is_err() {
                        return;
                    }
                    tracing::trace!(target: "silo::latency", stage = "server_flushed", kind = frame.kind);
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

async fn control<R, W>(
    server: Arc<Server>,
    hello: Frame,
    reader: R,
    writer: W,
    peer: Peer,
    on_admit: Option<tokio::sync::oneshot::Sender<()>>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let hello: ClientHello = hello.parse()?;
    let (tx, writer_task) = spawn_writer(writer, ());
    // Compatibility is checked first: an incompatible peer never enrolls.
    // The server's own connection acting for a `tools.call` caller.
    let stand_in = match (&peer, &hello.grant) {
        (Peer::Local { .. }, Some(secret)) => server.stand_ins.take(secret),
        _ => None,
    };
    let admitted = match (compat::check(hello.protocol, &hello.vt), &stand_in) {
        (Err(error), _) => Err(error),
        (Ok(()), Some(stand_in)) => Ok((stand_in.principal.clone(), None)),
        (Ok(()), None) => admit(&server, &peer, &hello),
    };
    let (principal, device) = match admitted {
        Ok(admitted) => admitted,
        Err(error) => {
            if error.code == "internal" {
                tracing::warn!(client = hello.client_name, "hello refused: {}", error.message);
            }
            tx.send(Frame::json(kind::CONTROL, &ServerMessage::Response { id: 0, result: None, error: Some(error) })?);
            drop(tx);
            let _ = writer_task.await;
            return Ok(());
        }
    };
    // A join-invite connection's invite id; it ends once it has offered.
    let join = match &principal {
        Principal::Offer { invite_id } => Some(invite_id.clone()),
        _ => None,
    };
    let role = stand_in.as_ref().map_or(hello.role, |s| s.role);
    let (close_tx, mut close_rx) = tokio::sync::oneshot::channel();
    let client_id = {
        let mut reg = server.registry.lock().unwrap();
        let (pane, tty) = match (&peer, &stand_in) {
            (_, Some(stand_in)) => (stand_in.pane.clone(), stand_in.tty),
            (Peer::Local { pid }, None) => approval::origin(*pid, &reg),
            (Peer::Remote { .. }, None) => (None, false),
        };
        let id = reg.add_client(hello.client_name.clone(), hello.passive);
        if let Some(entry) = reg.clients.get_mut(&id) {
            entry.close = Some(close_tx);
            entry.origin = peer.conn_id();
            entry.label = stand_in.as_ref().map_or_else(|| label(&hello.client_name, device.as_deref()), |s| s.label.clone());
            entry.features.clone_from(&hello.features);
            entry.principal = principal.clone();
            entry.role = role;
            entry.pane = pane;
            entry.tty = tty;
        }
        id
    };
    let granted = match &principal {
        Principal::Grant(grant) if matches!(grant.who, Who::Secret { .. }) => Some(silo_proto::access::Granted::from(grant.as_ref())),
        _ => None,
    };
    let mut reply = ServerHello { granted, ..ServerHello::ours(client_id, server.info_now().await) };
    if server.services.contains_key(silo_proto::compat::feature::OTTO) {
        reply.features.push(silo_proto::compat::feature::OTTO.to_owned());
    }
    tx.send(Frame::json(kind::CONTROL_HELLO, &reply)?);
    server.touch();
    // Join connections get no data streams.
    if let Some(on_admit) = on_admit.filter(|_| join.is_none()) {
        let _ = on_admit.send(());
    }
    let ctx = Ctx { id: client_id, principal, role };

    let mut frames = spawn_reader(reader, 64);
    let joining = join.is_some();
    let join_deadline = async move {
        if joining {
            tokio::time::sleep(JOIN_TIMEOUT).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(join_deadline);
    let (slow_lane, mut slow_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, bool, Method)>();
    let slow_worker = {
        let (server, ctx, tx) = (server.clone(), ctx.clone(), tx.clone());
        tokio::spawn(async move {
            while let Some((id, approval, method)) = slow_rx.recv().await {
                let started = std::time::Instant::now();
                let name = silo_proto::api::method_name(&method);
                let shutdown = matches!(method, Method::ServerShutdown { .. } | Method::ServerRestart { .. });
                let result = run(&server, &ctx, approval, method).await;
                if started.elapsed() > SLOW_REQUEST {
                    tracing::info!(method = %name, ms = started.elapsed().as_millis() as u64, "slow request");
                }
                let done = result.is_ok();
                respond(&tx, id, result);
                after(&server, shutdown && done);
            }
        })
    };
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
                        let raw = serde_json::from_slice::<Value>(&frame.payload).ok();
                        let id = raw.as_ref().and_then(|v| v.get("id")?.as_u64()).unwrap_or(0);
                        let method = raw.as_ref().and_then(|v| v.get("method")?.as_str().map(str::to_owned));
                        let error = match method {
                            Some(method) if !METHODS.iter().any(|(name, _)| *name == method) => RpcError::method_not_found(&method),
                            _ => RpcError::bad_request(e.to_string()),
                        };
                        respond(&tx, id, Err(error));
                        continue;
                    }
                };
                let id = request.id;
                let gate = {
                    let reg = server.registry.lock().unwrap();
                    if matches!(&request.method, Method::BlockReport(p) if reg.clients.get(&ctx.id).is_some_and(|c| c.pane.as_deref() == Some(&p.block_id))) {
                        Ok(crate::access::Gate { approval: false })
                    } else {
                        crate::access::check(&ctx.principal, ctx.role, &request.method, &reg)
                    }
                };
                let gate = match gate {
                    Ok(gate) => gate,
                    Err(error) => {
                        respond(&tx, id, Err(error));
                        continue;
                    }
                };
                if let (Some(invite_id), Method::HostOffer { ticket, name }) = (&join, &request.method) {
                    let source = OfferSource::Join { invite_id: invite_id.clone() };
                    let Some(offer) = crate::auth::host_offer(ticket, name, source) else {
                        respond(&tx, id, Err(RpcError::bad_request("ticket is not a valid pairing ticket")));
                        continue;
                    };
                    tracing::info!(host = offer.name, "a host offered itself over a join invite");
                    server.registry.lock().unwrap().emit_owner(Event::HostOffered { offer });
                    respond(&tx, id, to_value(Empty {}));
                    break Ok(());
                }
                let subscribe = matches!(request.method, Method::EventsSubscribe {});
                if subscribe {
                    server.registry.lock().unwrap().subscribe(client_id, tx.clone());
                }
                let shutdown = matches!(request.method, Method::ServerShutdown { .. } | Method::ServerRestart { .. });
                // A tool call waits for its provider (and maybe a human):
                // the connection's other calls go on meanwhile.
                if let Method::ToolsCall(_) = &request.method {
                    let (server, principal, tx) = (server.clone(), ctx.principal.clone(), tx.clone());
                    tokio::spawn(async move {
                        let result = crate::tools::handle(&server, client_id, &principal, request.method).await;
                        respond(&tx, id, result);
                    });
                    continue;
                }
                // Disk, git, the app or a human may take seconds: they queue
                // in their own lane (in order among themselves), so creating,
                // closing and laying out blocks never waits behind them.
                // Reads go on at once, each on its own and with a deadline: one
                // that hangs (a sleeping disk, a prompt on the host's screen)
                // holds up nothing else, and its caller hears so.
                if let Some(limit) = (!gate.approval).then(|| parallel(&request.method)).flatten() {
                    let (server, ctx, tx) = (server.clone(), ctx.clone(), tx.clone());
                    tokio::spawn(async move {
                        let name = silo_proto::api::method_name(&request.method);
                        let result = deadline(&name, limit, run(&server, &ctx, false, request.method)).await;
                        respond(&tx, id, result);
                    });
                    continue;
                }
                if gate.approval || slow(&request.method) {
                    if slow_lane.send((id, gate.approval, request.method)).is_err() {
                        respond(&tx, id, Err(RpcError::new("closed", "connection closing")));
                    }
                    continue;
                }
                let result = run(&server, &ctx, gate.approval, request.method).await;
                let done = result.is_ok();
                respond(&tx, id, result);
                after(&server, shutdown && done);
            }
            _ = &mut close_rx => break Ok(()),
            () = &mut join_deadline => break Ok(()),
        }
    };
    slow_worker.abort();
    server.registry.lock().unwrap().remove_client(client_id);
    server.stand_ins.client_gone(client_id).await;
    server.touch();
    drop(tx);
    let _ = writer_task.await;
    result
}

/// A client's short name, with a remote peer's paired device name
/// appended when it adds anything.
fn label(client_name: &str, device: Option<&str>) -> String {
    let short = short_client_name(client_name);
    match device.map(short_client_name).filter(|d| !d.is_empty() && !short.contains(d)) {
        Some(device) => format!("{short} · {device}"),
        None => short.to_owned(),
    }
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

fn browser(server: &Server, block_id: &str, command: BrowserCommand) -> Result<Value, RpcError> {
    server.registry.lock().unwrap().browser_command(block_id, command)?;
    Ok(json!({}))
}

/// What follows any answered request: grants of what it closed settle,
/// and an agreed shutdown starts.
fn after(server: &Server, shutdown: bool) {
    if !server.registry.lock().unwrap().gone.is_empty() {
        server.settle_grants();
    }
    if shutdown {
        server.shutdown.notify_one();
    }
}

/// A slow-lane or parallel request taking longer than this is logged.
const SLOW_REQUEST: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a quick read (a listing, a file, git marks) may take.
const READ_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a search over a folder may take.
const SEARCH_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Requests that only read: run beside everything else, each on its own
/// (their order doesn't matter), with how long they may take.
fn parallel(method: &Method) -> Option<std::time::Duration> {
    use Method as M;
    match method {
        M::FilesList(_)
        | M::FilesRead(_)
        | M::FilesReadImage(_)
        | M::FilesRoot(_)
        | M::FilesGit(_)
        | M::FilesDiff(_)
        | M::ChangesStatus(_)
        | M::ChangesDiff(_)
        | M::ChangesRoot(_)
        | M::BlockCommands { .. }
        | M::TerminalCommandOutput { .. } => Some(READ_LIMIT),
        M::FilesFind(_) | M::FilesSearch(_) | M::SearchFiles(_) | M::SearchGrep(_) | M::SearchOpened(_) => Some(SEARCH_LIMIT),
        _ => None,
    }
}

/// `call`'s answer, or a `timeout` error once it has taken `limit`
/// (the work itself may go on: a blocked read can't be stopped).
async fn deadline(
    method: &str,
    limit: std::time::Duration,
    call: impl std::future::Future<Output = Result<Value, RpcError>>,
) -> Result<Value, RpcError> {
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(limit, call).await.unwrap_or_else(|_| {
        tracing::warn!(method, secs = limit.as_secs(), "request timed out");
        Err(RpcError::new(
            "timeout",
            format!("{method} took longer than {} s on the host; it may be waiting on its disk or a prompt on its screen", limit.as_secs()),
        ))
    });
    if started.elapsed() > SLOW_REQUEST {
        tracing::info!(method, ms = started.elapsed().as_millis() as u64, "slow request");
    }
    result
}

#[cfg(test)]
mod lane_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_read_that_never_ends_answers_timeout() {
        let stuck = deadline("files.list", READ_LIMIT, std::future::pending()).await.unwrap_err();
        assert_eq!(stuck.code, "timeout");
        assert!(stuck.message.contains("files.list took longer than 30 s"), "{}", stuck.message);
        assert_eq!(deadline("files.list", READ_LIMIT, async { Ok(json!([])) }).await.unwrap(), json!([]));
        // Only reads run this way, each with its limit.
        let list = Method::FilesList(silo_proto::control::FilesListParams { block_id: "b".into(), path: String::new() });
        assert_eq!(parallel(&list), Some(READ_LIMIT));
        let write = Method::FilesTrash(silo_proto::control::FilesTrashParams { block_id: "b".into(), paths: vec![] });
        assert_eq!(parallel(&write), None);
    }
}

/// Requests that may take seconds (disk, git, a search, a probe, or
/// waiting on the app): the connection's slow lane runs them, in order,
/// beside everything else. Not `files.set`/`changes.set` (block state,
/// ordered with layout) nor `app.reply` (what forwarded requests wait on).
fn slow(method: &Method) -> bool {
    use Method as M;
    matches!(
        method,
        M::AppAppearance { .. }
            | M::AppConnectionInfo { .. }
            | M::AppCopy { .. }
            | M::AppCopyOutput { .. }
            | M::AppFullScreen { .. }
            | M::AppHide { .. }
            | M::AppHideOthers { .. }
            | M::AppHostsAdd { .. }
            | M::AppHostsList { .. }
            | M::AppHostsRemove { .. }
            | M::AppHostsRename { .. }
            | M::AppJumpPrompt { .. }
            | M::AppOpenShare { .. }
            | M::AppPalette { .. }
            | M::AppPaneHeaders { .. }
            | M::AppPaste { .. }
            | M::AppPermissionsShow { .. }
            | M::AppQuit { .. }
            | M::AppSelectAll { .. }
            | M::AppSelectOutput { .. }
            | M::AppSessionStep { .. }
            | M::AppSettingsGet { .. }
            | M::AppSettingsOpen { .. }
            | M::AppSettingsOpenFile { .. }
            | M::AppSettingsReload { .. }
            | M::AppSettingsSet { .. }
            | M::AppShowAll { .. }
            | M::AppSwitcher { .. }
            | M::AppUpdateCheck { .. }
            | M::AppWindowClose { .. }
            | M::AppWindowCycle { .. }
            | M::AppWindowNew { .. }
            | M::BlockCommands { .. }
            | M::BrowserClick(_)
            | M::BrowserConsole(_)
            | M::BrowserDialog(_)
            | M::BrowserEval(_)
            | M::BrowserFetch(_)
            | M::BrowserFill(_)
            | M::BrowserFillForm(_)
            | M::BrowserFind(_)
            | M::BrowserHover(_)
            | M::BrowserNetwork(_)
            | M::BrowserPress(_)
            | M::BrowserScreenshot(_)
            | M::BrowserScroll(_)
            | M::BrowserSnapshot(_)
            | M::BrowserType(_)
            | M::BrowserWaitFor(_)
            | M::ChangesAbort(_)
            | M::ChangesCommit(_)
            | M::ChangesConflict(_)
            | M::ChangesContinue(_)
            | M::ChangesDiff(_)
            | M::ChangesFetch(_)
            | M::ChangesPr(_)
            | M::ChangesPrCreate(_)
            | M::ChangesPull(_)
            | M::ChangesPush(_)
            | M::ChangesResolve(_)
            | M::ChangesRevert(_)
            | M::ChangesRoot(_)
            | M::ChangesStage(_)
            | M::ChangesStatus(_)
            | M::ChangesUnstage(_)
            | M::FilesCopy(_)
            | M::FilesCreate(_)
            | M::FilesDiff(_)
            | M::FilesFind(_)
            | M::FilesGit(_)
            | M::FilesList(_)
            | M::FilesMkdir(_)
            | M::FilesMove(_)
            | M::FilesRead(_)
            | M::FilesReadImage(_)
            | M::FilesRename(_)
            | M::FilesRoot(_)
            | M::FilesSearch(_)
            | M::FilesTrash(_)
            | M::FilesWrite(_)
            | M::PermissionsRequest { .. }
            | M::SearchFiles(_)
            | M::SearchGrep(_)
            | M::SearchOpened(_)
            | M::TerminalCommandOutput { .. }
    )
}

/// A control connection's caller.
#[derive(Clone)]
pub struct Ctx {
    pub id: u64,
    pub principal: Principal,
    pub role: ClientRole,
}

/// Who a hello is admitted as, and the paired device's (or link's) name for its label.
/// Remote hellos carry exactly one of a paired endpoint (or device proof),
/// an invite or a grant's secret; local ones may carry an agent token.
fn admit(server: &Server, peer: &Peer, hello: &ClientHello) -> Result<(Principal, Option<String>), RpcError> {
    let mut auth = server.auth();
    let (endpoint_id, conn_id) = match peer {
        Peer::Local { .. } => {
            return match &hello.grant {
                None => Ok((Principal::Owner, None)),
                Some(token) => Ok((Principal::Grant(Box::new(auth.redeem_secret(SecretKind::Token, token)?)), None)),
            };
        }
        Peer::Remote { endpoint_id, conn_id } => (endpoint_id, *conn_id),
    };
    // Checked under the lock: once the upgrade commits, pairing is frozen
    // and the successor's re-read sees every grant.
    if server.upgrading.load(Ordering::SeqCst) {
        return Err(RpcError::new("upgrading", "the server is handing over to a new build; retry"));
    }
    if let Some(secret) = &hello.grant {
        if hello.invite.is_some() || hello.device.is_some() {
            return Err(RpcError::unauthorized("a hello carries one of a device, an invite or a grant"));
        }
        let grant = auth.redeem_secret(SecretKind::Link, secret)?;
        auth.bind(conn_id, &grant.id);
        // Watchers show who the link is for ("Silo · Ana").
        let name = Some(grant.label.clone()).filter(|l| !l.is_empty());
        return Ok((Principal::Grant(Box::new(grant)), name));
    }
    let device = crate::auth::device_of(&server.info.endpoint_id, endpoint_id, hello.device.as_ref())?;
    // Only an endpoint itself redeems invites: one speaking for a device
    // never pairs it or joins.
    let invite = hello.invite.as_deref().filter(|_| hello.device.is_none());
    match auth.authorize(&device, &hello.client_name, invite)? {
        Admission::Device(grant) => {
            auth.bind(conn_id, &grant.id);
            let name = grant.label.clone();
            Ok((Principal::Grant(grant), Some(name)))
        }
        Admission::Join { invite_id } => Ok((Principal::Offer { invite_id }, None)),
    }
}

/// Whether `method` only reads (allowed while upgrading).
fn read_only(method: &Method) -> bool {
    matches!(need(method), Need::Open | Need::Rights { rights: Rights::View, .. })
}

/// Run a checked call: refuse mutations while upgrading, check closes,
/// wait for any approval, then handle it.
///
/// A close is checked before its approval, so nobody is asked to approve
/// what would then be refused as busy, and once more after the approval,
/// whose wait may have been long, on a fresh probe.
async fn run(server: &Server, ctx: &Ctx, approval: bool, method: Method) -> Result<Value, RpcError> {
    if server.upgrading.load(Ordering::SeqCst) && !read_only(&method) {
        return Err(RpcError::new("upgrading", "the server is handing over to a new build; retry"));
    }
    let close = crate::close_guard::is_close(&method);
    if close {
        crate::close_guard::check(server, ctx.id, &method).await?;
    }
    if approval {
        approval::gate(server, ctx.id, &method, Rights::Owner).await?;
        if close {
            crate::close_guard::check(server, ctx.id, &method).await?;
        }
    }
    handle(server, ctx, method).await
}

/// The server's address for a share ticket, its home relay once known
/// (the ticket keeps only relay URLs).
async fn share_addr(server: &Server) -> Result<silo_transport::EndpointAddr, RpcError> {
    let net = server.net.lock().await;
    let net = net.as_ref().ok_or_else(|| RpcError::internal("remote access is not running"))?;
    if server.relays {
        let _ = tokio::time::timeout(ONLINE_TIMEOUT, net.online()).await;
    }
    Ok(net.addr())
}

async fn grant_create(
    server: &Server,
    ctx: &Ctx,
    scope: Scope,
    rights: Rights,
    kind: GrantKind,
    ttl: Option<u64>,
    label: Option<String>,
) -> Result<Value, RpcError> {
    let kind = match kind {
        GrantKind::Token => SecretKind::Token,
        GrantKind::Link => SecretKind::Link,
        GrantKind::Account => return Err(RpcError::bad_request("accounts need a login server")),
    };
    if rights == Rights::Owner {
        return Err(RpcError::bad_request("tokens and links cannot hold owner rights"));
    }
    let addr = match kind {
        SecretKind::Link => Some(share_addr(server).await?),
        _ => None,
    };
    let (scope, created_by) = {
        let reg = server.registry.lock().unwrap();
        let scope = match scope {
            Scope::Server => Scope::Server,
            Scope::Session { session_id } => {
                Scope::Session { session_id: reg.session_id(&session_id).ok_or_else(|| RpcError::not_found(format!("session {session_id}")))? }
            }
            Scope::Block { block_id } if reg.blocks.contains_key(&block_id) => Scope::Block { block_id },
            Scope::Block { block_id } => return Err(RpcError::not_found(format!("block {block_id}"))),
            Scope::Unknown => return Err(RpcError::bad_request("unsupported scope")),
        };
        (scope, reg.client_label(ctx.id))
    };
    let ttl = ttl.or((kind == SecretKind::Link).then_some(DEFAULT_LINK_TTL_SECS));
    if ttl.is_some_and(|t| t == 0 || t > MAX_GRANT_TTL_SECS) {
        return Err(RpcError::bad_request(format!("ttl_secs must be between 1 and {MAX_GRANT_TTL_SECS}")));
    }
    let secret = rand::random::<[u8; silo_proto::access::SECRET_LEN]>();
    let now = unix_secs();
    let grant = Grant {
        id: new_id(),
        who: Who::Secret { kind, hash: secret_hash(&secret) },
        scope,
        rights,
        label: label.map(|l| silo_proto::text::untrusted(&l, silo_proto::text::MAX_TITLE)).unwrap_or_default(),
        created_at: now,
        created_by,
        created_by_grant: ctx.principal.grant_id().map(str::to_owned),
        expires_at: ttl.map(|t| now + t),
        last_seen: 0,
    };
    let grants = {
        let mut auth = server.auth();
        auth.add_grant(grant.clone())?;
        auth.grants()
    };
    server.registry.lock().unwrap().emit_owner(Event::GrantsChanged { grants });
    tracing::info!(grant = grant.id, ?kind, "grant created");
    let ticket = addr.map(|addr| silo_transport::ShareTicket::new(&addr, secret, silo_proto::VT).encode());
    to_value(GrantCreated { grant, secret: encode_secret(kind, &secret), ticket })
}

/// `created` with a data ticket for its block when `attach` (a terminal the
/// client laid out itself, feature `instant_blocks`): it attaches without
/// a `data.connect` round trip. A ticket that can't be issued is left out;
/// the client then asks as usual.
fn with_ticket(
    reg: &mut crate::registry::Registry,
    ctx: &Ctx,
    client_id: u64,
    mut created: silo_proto::control::Created,
    attach: bool,
) -> silo_proto::control::Created {
    if attach {
        let session = Some(created.session_id.as_str());
        let rights = rights_on(&ctx.principal, On::Block { block: &created.block_id, session }).unwrap_or(Rights::View);
        created.ticket = reg.issue_ticket(client_id, &created.block_id, rights).ok();
    }
    created
}

async fn handle(server: &Server, ctx: &Ctx, method: Method) -> Result<Value, RpcError> {
    let client_id = ctx.id;
    match method {
        Method::ServerInfo {} => to_value(server.info_now().await),
        Method::ServerShutdown { .. } => Ok(json!({})),
        Method::ApiList {} => to_value(
            METHODS.iter().map(|(name, description)| ApiMethod { name: (*name).into(), description: (*description).into() }).collect::<Vec<_>>(),
        ),
        Method::ApiDescribe { method } => to_value(api::describe(&method).ok_or_else(|| RpcError::not_found(format!("method {method}")))?),
        // Touch shared state, so a wedged registry reads as unresponsive.
        Method::Ping {} => {
            drop(server.registry.lock().unwrap());
            to_value(Empty {})
        }
        Method::EventsSubscribe {} => Ok(json!({})),
        Method::SessionList {} => to_value(server.registry.lock().unwrap().summaries_for(&ctx.principal)),
        Method::SessionCreate { name, spawn, kind, url } => {
            let new = New { spawn: &spawn, kind, url: url.as_deref(), agent: None, ids: None };
            to_value(server.registry.lock().unwrap().create_session(name, &new)?)
        }
        Method::SessionView { session_id } => to_value(server.registry.lock().unwrap().view(&session_id)?),
        Method::SessionRename { session_id, name } => {
            server.registry.lock().unwrap().rename_session(&session_id, name)?;
            Ok(json!({}))
        }
        Method::SessionClose { session_id, force: _ } => {
            server.registry.lock().unwrap().close_session(&session_id)?;
            Ok(json!({}))
        }
        Method::WindowCreate { session_id, spawn, kind, url, background, ids } => {
            let new = New { spawn: &spawn, kind, url: url.as_deref(), agent: None, ids: ids.as_ref() };
            let mut reg = server.registry.lock().unwrap();
            let created = reg.create_window(&session_id, &new, background)?;
            to_value(with_ticket(&mut reg, ctx, client_id, created, ids.is_some() && kind == BlockKind::Terminal))
        }
        Method::WindowSelect { session_id, window_id } => {
            server.registry.lock().unwrap().select_window(&session_id, &window_id)?;
            Ok(json!({}))
        }
        Method::WindowClose { window_id, force: _ } => {
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
        Method::WindowMerge { window_id, source, axis, at } => {
            server.registry.lock().unwrap().merge_window(&window_id, &source, axis, at)?;
            Ok(json!({}))
        }
        Method::BlockToWindow { block_id, window_id, index, axis } => {
            server.registry.lock().unwrap().block_to_window(&block_id, window_id.as_deref(), index, axis)?;
            Ok(json!({}))
        }
        Method::BlockSplit { block_id, axis, spawn, kind, url, agent, ids } => {
            let new = New { spawn: &spawn, kind, url: url.as_deref(), agent: agent.as_ref(), ids: ids.as_ref() };
            let mut reg = server.registry.lock().unwrap();
            let created = reg.split(&block_id, axis, &new)?;
            to_value(with_ticket(&mut reg, ctx, client_id, created, ids.is_some() && kind == BlockKind::Terminal))
        }
        Method::BlockSpawn { block_id, command, cwd, env, size, kind, url } => {
            let mut reg = server.registry.lock().unwrap();
            match kind {
                BlockKind::Terminal => to_value(reg.spawn_parked(&block_id, &command, cwd, env, size)?),
                BlockKind::Browser => to_value(reg.spawn_parked_browser(&block_id, url.as_deref())?),
                _ => Err(RpcError::bad_request("block.spawn makes terminal or browser blocks")),
            }
        }
        Method::BlockPark { block_id } => {
            server.registry.lock().unwrap().park(&block_id)?;
            Ok(json!({}))
        }
        Method::BlockReveal { block_id, reveal } => {
            let mut reg = server.registry.lock().unwrap();
            reg.reveal(&block_id, reveal)?;
            crate::app_bridge::active(&mut reg, client_id);
            Ok(json!({}))
        }
        Method::BlockClose { block_id, force: _ } => {
            server.registry.lock().unwrap().close_block(&block_id)?;
            Ok(json!({}))
        }
        Method::BlockFocus { block_id } => {
            let mut reg = server.registry.lock().unwrap();
            reg.focus(&block_id)?;
            crate::app_bridge::active(&mut reg, client_id);
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
        Method::BlockFloat { block_id, rect } => {
            server.registry.lock().unwrap().float(&block_id, rect)?;
            Ok(json!({}))
        }
        Method::BlockMove { block_id, rect, dock } => {
            server.registry.lock().unwrap().move_block(&block_id, rect, dock)?;
            Ok(json!({}))
        }
        Method::LayoutRatio { window_id, path, ratio, float } => {
            server.registry.lock().unwrap().set_ratio(&window_id, float.as_deref(), &path, ratio)?;
            Ok(json!({}))
        }
        Method::BlockMinimize { block_id, minimized } => {
            server.registry.lock().unwrap().minimize(&block_id, minimized)?;
            Ok(json!({}))
        }
        Method::TerminalSend { block_id, text } => {
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::Input(text.into_bytes()));
            Ok(json!({}))
        }
        Method::TerminalCapture { block_id, history } => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            {
                let mut reg = server.registry.lock().unwrap();
                match reg.block(&block_id) {
                    Ok(block) => block.send(BlockMsg::Capture { history, reply }),
                    // A closed command block answers from its final capture.
                    Err(error) => {
                        let mut capture = reg.ended(&block_id).ok_or(error)?;
                        if !history {
                            let lines: Vec<&str> = capture.text.lines().collect();
                            capture.text = lines[lines.len().saturating_sub(usize::from(capture.size.rows))..].join("\n");
                        }
                        return to_value(capture);
                    }
                }
            }
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
        // The attached apps answer only right after a paste into the block:
        // the user's paste is the consent, so no approval is asked.
        Method::ClipboardRead { block_id, kind } => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::ClipboardRead { kind, reply });
            let content = tokio::time::timeout(CLIPBOARD_READ_TIMEOUT, rx).await.ok().and_then(Result::ok).flatten();
            to_value(Pasted { content })
        }
        Method::ClipboardWrite { block_id, location: _, text } => {
            if text.len() > MAX_CLIP_TEXT {
                let (len, max) = (silo_proto::drops::human_size(text.len() as u64), silo_proto::drops::human_size(MAX_CLIP_TEXT as u64));
                return Err(RpcError::bad_request(format!("Too large to copy ({len}, max {max})")));
            }
            let (reply, rx) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::ClipboardWrite { text, reply });
            match rx.await.map_err(|_| RpcError::not_found(format!("block {block_id}")))? {
                0 => Err(RpcError::new("unavailable", "No Silo client is attached to this block")),
                _ => to_value(Empty {}),
            }
        }
        Method::TerminalResize { block_id, size } => {
            if size.columns == 0 || size.rows == 0 {
                return Err(RpcError::bad_request("size must be at least 1x1"));
            }
            let reg = server.registry.lock().unwrap();
            let client_name = reg.client_name(client_id);
            let label = reg.client_label(client_id);
            reg.block(&block_id)?.send(BlockMsg::Desired { client_id, client_name, label, size });
            Ok(json!({}))
        }
        Method::OwnBlock { block_ids, mode } => {
            let mut reg = server.registry.lock().unwrap();
            reg.own(client_id, &block_ids, Some(mode))?;
            if mode == silo_proto::control::OwnMode::Take {
                crate::app_bridge::active(&mut reg, client_id);
            }
            Ok(json!({}))
        }
        Method::DisownBlock { block_ids } => {
            server.registry.lock().unwrap().own(client_id, &block_ids, None)?;
            Ok(json!({}))
        }
        Method::DataConnect { block_id } => {
            let mut reg = server.registry.lock().unwrap();
            let session = reg.block_session(&block_id);
            let rights = rights_on(&ctx.principal, On::Block { block: &block_id, session: session.as_deref() }).unwrap_or(Rights::View);
            to_value(reg.issue_ticket(client_id, &block_id, rights)?)
        }
        Method::BlockView { block_id } => {
            let reg = server.registry.lock().unwrap();
            let block = reg.block_info(&block_id).ok_or_else(|| RpcError::not_found(format!("block {block_id}")))?;
            let status = reg.block_status(&block_id);
            drop(reg);
            to_value(BlockView { block, status })
        }
        Method::BrowserNavigate { block_id, url } => browser(server, &block_id, BrowserCommand::Navigate { url }),
        Method::BrowserBack { block_id } => browser(server, &block_id, BrowserCommand::Back),
        Method::BrowserForward { block_id } => browser(server, &block_id, BrowserCommand::Forward),
        Method::BrowserReload { block_id, ignore_cache } => browser(server, &block_id, BrowserCommand::Reload { ignore_cache }),
        Method::BrowserZoom { block_id, zoom } => browser(server, &block_id, BrowserCommand::Zoom { zoom }),
        Method::BrowserReport { block_id, url, title, favicon, loading, history, index } => {
            let report = Report { url, title, favicon, loading, history, index };
            server.registry.lock().unwrap().browser_report(client_id, &block_id, report)?;
            Ok(json!({}))
        }
        Method::AgentSet { block_id, session_id, title, cwd } => {
            server.registry.lock().unwrap().agent_set(&block_id, session_id, title, cwd)?;
            Ok(json!({}))
        }
        Method::UiOpenSession { session_id } => {
            let mut reg = server.registry.lock().unwrap();
            let id = reg.view(&session_id)?.session.id;
            reg.emit(silo_proto::control::Event::OpenSession { session_id: id });
            Ok(json!({}))
        }
        Method::PairCreate { ttl_secs, purpose } => {
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
            let (secret, invite_id) = server.auth().create_invite(ttl, purpose, ctx.principal.grant_id())?;
            let ticket = silo_transport::PairTicket { addr, secret }.encode();
            to_value(PairInvite { invite_id, ticket, expires_in_secs: ttl_secs })
        }
        Method::PairList {} => to_value(PairedDevices { devices: server.auth().devices() }),
        Method::PairRevoke { id } => {
            let gone = server.auth().revoke_device(&id)?;
            server.revoked(&gone);
            Ok(json!({}))
        }
        // Only join connections, answered in the control loop.
        Method::HostOffer { .. } => Err(RpcError::forbidden("host.offer")),
        Method::ServiceSocket { name } => to_value(ServiceSocket { path: crate::tunnel::service_socket(server, &name).await? }),
        Method::GrantCreate { scope, rights, kind, ttl_secs, label } => grant_create(server, ctx, scope, rights, kind, ttl_secs, label).await,
        Method::GrantList {} => to_value(Grants { grants: server.auth().grants() }),
        Method::GrantRevoke { id } => {
            let gone = server.auth().remove_where(|g| g.id == id)?;
            if gone.is_empty() {
                return Err(RpcError::not_found(format!("grant {id}")));
            }
            server.revoked(&gone);
            Ok(json!({}))
        }
        Method::ApprovalAnswer { id, allow } => {
            server.approvals.answer(client_id, &id, allow)?;
            to_value(Empty {})
        }
        Method::BlocksBusy { session_id, window_id, block_ids } => {
            to_value(crate::close_guard::busy_blocks(server, &ctx.principal, session_id, window_id, block_ids).await?)
        }
        Method::BlockStatusSeen { .. } | Method::BlockReport(_) | Method::BlockNotify(_) => crate::status::handle(server, client_id, method).await,
        Method::BrowserSnapshot(_)
        | Method::BrowserFind(_)
        | Method::BrowserClick(_)
        | Method::BrowserHover(_)
        | Method::BrowserFill(_)
        | Method::BrowserFillForm(_)
        | Method::BrowserType(_)
        | Method::BrowserPress(_)
        | Method::BrowserScroll(_)
        | Method::BrowserWaitFor(_)
        | Method::BrowserDialog(_)
        | Method::BrowserEval(_)
        | Method::BrowserConsole(_)
        | Method::BrowserNetwork(_)
        | Method::BrowserScreenshot(_)
        | Method::BrowserFetch(_) => crate::app_bridge::handle(server, client_id, method).await,
        Method::ToolsProvide(_) | Method::ToolsList {} | Method::ToolsCall(_) | Method::ToolsReply(_) => {
            crate::tools::handle(server, client_id, &ctx.principal, method).await
        }
        Method::BlockCommands { .. } | Method::TerminalCommandOutput { .. } => crate::prompts::handle(server, client_id, method).await,
        Method::FilesList(_)
        | Method::FilesRead(_)
        | Method::FilesReadImage(_)
        | Method::FilesFind(_)
        | Method::FilesSet(_)
        | Method::FilesRoot(_)
        | Method::FilesWrite(_)
        | Method::FilesCreate(_)
        | Method::FilesMkdir(_)
        | Method::FilesRename(_)
        | Method::FilesCopy(_)
        | Method::FilesMove(_)
        | Method::FilesTrash(_)
        | Method::FilesSearch(_)
        | Method::FilesGit(_)
        | Method::FilesDiff(_) => crate::files::handle(server, method).await,
        Method::ChangesStatus(_)
        | Method::ChangesDiff(_)
        | Method::ChangesSet(_)
        | Method::ChangesStage(_)
        | Method::ChangesUnstage(_)
        | Method::ChangesRevert(_)
        | Method::ChangesCommit(_)
        | Method::ChangesRoot(_)
        | Method::ChangesPush(_)
        | Method::ChangesPull(_)
        | Method::ChangesFetch(_)
        | Method::ChangesPr(_)
        | Method::ChangesPrCreate(_)
        | Method::ChangesConflict(_)
        | Method::ChangesResolve(_)
        | Method::ChangesContinue(_)
        | Method::ChangesAbort(_) => crate::changes::handle(server, method).await,
        Method::SearchFiles(_) | Method::SearchGrep(_) | Method::SearchOpened(_) => crate::search::handle(server, &ctx.principal, method).await,
        Method::ServerRestart { .. }
        | Method::ServerLog { .. }
        | Method::WindowStep { .. }
        | Method::BlockFocusStep { .. }
        | Method::BlockFocusDir { .. }
        | Method::LayoutGrow { .. }
        | Method::LayoutEdge { .. }
        | Method::LayoutEqualize { .. } => crate::layout_cmds::handle(server, client_id, method).await,
        Method::AppReply { .. }
        | Method::AppWindowNew { .. }
        | Method::AppWindowCycle { .. }
        | Method::AppSwitcher { .. }
        | Method::AppSessionStep { .. }
        | Method::AppHostsAdd { .. }
        | Method::AppHostsRename { .. }
        | Method::AppHostsRemove { .. }
        | Method::AppHostsList { .. }
        | Method::AppCopy { .. }
        | Method::AppPaste { .. }
        | Method::AppSelectAll { .. }
        | Method::AppAppearance { .. }
        | Method::AppPaneHeaders { .. }
        | Method::AppFullScreen { .. }
        | Method::AppConnectionInfo { .. }
        | Method::AppSettingsOpen { .. }
        | Method::AppSettingsReload { .. }
        | Method::AppSettingsOpenFile { .. }
        | Method::AppSettingsGet { .. }
        | Method::AppSettingsSet { .. }
        | Method::AppUpdateCheck { .. }
        | Method::AppPalette { .. }
        | Method::AppHide { .. }
        | Method::AppWindowClose { .. }
        | Method::AppHideOthers { .. }
        | Method::AppShowAll { .. }
        | Method::AppQuit { .. }
        | Method::AppJumpPrompt { .. }
        | Method::AppSelectOutput { .. }
        | Method::AppCopyOutput { .. }
        | Method::AppOpenShare { .. }
        | Method::AppPermissionsShow { .. } => crate::app_bridge::handle(server, client_id, method).await,
        Method::PermissionsStatus {} | Method::PermissionsRequest { .. } => crate::permissions::handle(server, client_id, method).await,
    }
}

async fn data<R, W>(server: Arc<Server>, hello: Frame, reader: R, writer: W, peer: Peer) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let hello: DataHello = hello.parse()?;
    let (tx, writer_task) = spawn_writer(writer, crate::DataWriter::new(&server));
    let redeemed = server.registry.lock().unwrap().redeem(&hello.ticket, peer.conn_id());
    let reject = |code: &str, reason: &str| -> std::io::Result<()> {
        tx.send(Frame::json(kind::REJECT, &Reject { code: code.into(), reason: reason.into() })?);
        Ok(())
    };
    if let Err(error) = compat::check(hello.protocol, &hello.vt) {
        reject(&error.code, &error.message)?;
    } else if let Some((block_id, client_id, rights)) = redeemed {
        let block = {
            let reg = server.registry.lock().unwrap();
            reg.blocks.get(&block_id).and_then(Block::terminal).map(|b| (b.tx.clone(), reg.client_name(client_id), reg.client_label(client_id)))
        };
        if let Some((block, client_name, label)) = block {
            let sub_id = rand::random::<u64>();
            let cancelled = tx.cancelled();
            let remote = matches!(peer, Peer::Remote { .. });
            // Below interact a stream only watches: what it sends that
            // would type, size, configure or touch clipboards is dropped.
            let interact = rights >= Rights::Interact;
            let (end, mut ended) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().add_stream(client_id, sub_id, &block_id, rights, end);
            let _ = block.send(BlockMsg::Attach(Box::new(Attach { sub_id, client_id, resume: hello.resume, sink: tx.clone(), remote })));
            // A block handed to a successor drops its sink after queueing
            // everything sent so far: the writer then flushes and ends.
            let actor_sink = tx.tx.downgrade();
            drop(tx);
            let mut frames = spawn_reader(reader, 256);
            tokio::pin!(cancelled);
            if remote {
                server.clipboard.remote_streams.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let mut marked: Option<std::time::Instant> = None;
            loop {
                // A cut-off subscriber's stream ends now: dropping both
                // halves closes the connection so the client reconnects.
                let frame = tokio::select! {
                    frame = frames.recv() => frame,
                    () = &mut cancelled => break,
                    _ = &mut ended => break,
                };
                let Some(Ok(frame)) = frame else { break };
                match ClientData::from_frame(frame) {
                    Ok(ClientData::Ack(offset)) => {
                        let _ = block.send(BlockMsg::Ack { sub_id, offset });
                    }
                    Ok(
                        ClientData::Input(_)
                        | ClientData::Resize(_)
                        | ClientData::Config(_)
                        | ClientData::ClipboardContent { .. }
                        | ClientData::ClipboardPush { .. },
                    ) if !interact => {}
                    Ok(ClientData::Input(bytes)) => {
                        tracing::trace!(target: "silo::latency", stage = "server_input");
                        if marked.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                            marked = Some(std::time::Instant::now());
                            crate::app_bridge::active(&mut server.registry.lock().unwrap(), client_id);
                        }
                        if remote {
                            *server.clipboard.last_input.lock().unwrap() = Some(block_id.clone());
                        }
                        let _ = block.send(BlockMsg::StreamInput { sub_id, bytes });
                    }
                    Ok(ClientData::Resize(size)) if size.columns > 0 && size.rows > 0 => {
                        let _ = block.send(BlockMsg::Desired { client_id, client_name: client_name.clone(), label: label.clone(), size });
                    }
                    Ok(ClientData::Config(config)) => {
                        let _ = block.send(BlockMsg::Config { client_id, config });
                    }
                    Ok(ClientData::ClipboardContent { request, content }) => {
                        let _ = block.send(BlockMsg::ClipboardContent { sub_id, request, content });
                    }
                    // Local clients share the host's clipboard. Awaited so
                    // the paste input that follows sees the new content.
                    Ok(ClientData::ClipboardPush { content }) if remote => server.clipboard.push(content).await,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            if remote {
                server.clipboard.remote_streams.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = block.send(BlockMsg::Detach(sub_id));
            server.registry.lock().unwrap().remove_stream(client_id, sub_id);
            if actor_sink.strong_count() > 0 {
                writer_task.abort();
            }
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

/// An upload stream: files for the host of the ticket's terminal block.
async fn upload<R, W>(server: Arc<Server>, hello: Frame, reader: R, writer: W, peer: Peer) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let hello: UploadHello = hello.parse()?;
    let (tx, writer_task) = spawn_writer(writer, ());
    let redeemed = server.registry.lock().unwrap().redeem(&hello.ticket, peer.conn_id());
    let result = if let Err(error) = compat::check(hello.protocol, &hello.vt) {
        Err((RpcError::INCOMPATIBLE, error.message))
    } else if redeemed.as_ref().is_some_and(|(_, _, rights)| *rights < Rights::Interact) {
        Err((RpcError::FORBIDDEN, "this grant cannot upload files".to_owned()))
    } else if let Some((block_id, _, _)) = redeemed {
        let (terminal, files) = {
            let reg = server.registry.lock().unwrap();
            let block = reg.blocks.get(&block_id);
            (
                block.and_then(Block::terminal).is_some(),
                match block {
                    Some(Block::Files(files)) => Some(files.root()),
                    _ => None,
                },
            )
        };
        match (terminal, files, hello.to_dir) {
            (true, _, None) => {
                let mut frames = spawn_reader(reader, 16);
                crate::drops::receive(&server.home, hello.total, &mut frames).await
            }
            (true, _, Some(_)) => Err(("bad_request", "a terminal block's uploads take no folder".to_owned())),
            (_, Some(root), to_dir) => {
                let mut frames = spawn_reader(reader, 16);
                let to_dir = to_dir.unwrap_or_default();
                let result = crate::files::transfer::receive(&root, &to_dir, hello.replace, server.trash.clone(), hello.total, &mut frames).await;
                if let Ok(uploaded) = &result {
                    let mut paths: Vec<String> = uploaded.paths.clone();
                    paths.push(to_dir);
                    server.registry.lock().unwrap().emit(silo_proto::control::Event::FilesChanged { block_id, paths, truncated: false });
                }
                result
            }
            _ => Err(("not_found", "block no longer exists".to_owned())),
        }
    } else {
        Err(("ticket", "invalid or expired ticket".to_owned()))
    };
    match result {
        Ok(uploaded) => {
            tx.send(Frame::json(kind::UPLOADED, &uploaded)?);
        }
        Err((code, reason)) => {
            tracing::debug!(code, reason, "upload refused");
            tx.send(Frame::json(kind::REJECT, &Reject { code: code.into(), reason })?);
        }
    }
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

/// A download stream: items of the ticket's files block, to the client.
async fn download<R, W>(server: Arc<Server>, hello: Frame, reader: R, mut writer: W, peer: Peer) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let hello: silo_proto::download::DownloadHello = hello.parse()?;
    let redeemed = server.registry.lock().unwrap().redeem(&hello.ticket, peer.conn_id());
    let root = if let Err(error) = compat::check(hello.protocol, &hello.vt) {
        Err((RpcError::INCOMPATIBLE, error.message))
    } else if let Some((block_id, _, _)) = redeemed {
        match server.registry.lock().unwrap().blocks.get(&block_id) {
            Some(Block::Files(files)) => Ok(files.root()),
            Some(_) => Err(("bad_request", "only files blocks download".to_owned())),
            None => Err(("not_found", "block no longer exists".to_owned())),
        }
    } else {
        Err(("ticket", "invalid or expired ticket".to_owned()))
    };
    let result = match root {
        Ok(root) => {
            let mut frames = spawn_reader(reader, 4);
            crate::files::transfer::send(root, &mut frames, &mut writer).await
        }
        Err(refusal) => Err(refusal),
    };
    if let Err((code, reason)) = result {
        tracing::debug!(code, reason, "download refused");
        if code != "closed" {
            write_frame(&mut writer, &Frame::json(kind::REJECT, &Reject { code: code.into(), reason })?).await?;
            writer.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_short_with_a_distinct_device_name() {
        assert_eq!(label("Silo 0.1.0 (macOS, pid 7)", None), "Silo");
        assert_eq!(label("Silo 0.1.0 (macOS, pid 9)", Some("Silo 0.1.0 (macOS, pid 7)")), "Silo");
        assert_eq!(label("silo cli", Some("work laptop")), "silo cli · work laptop");
        assert_eq!(label("silo cli on mini", Some("silo cli on mini")), "silo cli on mini");
    }
}
