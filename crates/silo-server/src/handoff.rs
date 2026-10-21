//! Live upgrade: a successor server takes every block (PTY masters
//! included) from the running server without hanging up any program.
//!
//! Fixed across builds: [`MAGIC`] and the framing below. The payload
//! (`Transfer`, `Session`, `BlockInfo`, encoded `Checkpoint`s) is serde of
//! the old build's types; when the successor cannot read it, the upgrade
//! fails before `READY` and the old server keeps serving. Over the control
//! socket:
//!
//! 1. successor -> old: [`MAGIC`] (`SILO-UPGRADE 1\n`) instead of a hello.
//! 2. old -> successor: frames of `u32 LE length` + bytes:
//!    - one JSON [`Transfer`] (tree, block infos, fd index);
//!    - one [`crate::persist::encode`]d checkpoint per `Transfer::blocks`
//!      entry, in order (browser and agent blocks travel as
//!      `Transfer::browsers` state only).
//! 3. old -> successor: the PTY masters and PTY host status pipes via
//!    `SCM_RIGHTS`, in messages of at most [`FDS_PER_MESSAGE`] descriptors,
//!    each with a 4-byte `u32 LE` payload holding its descriptor count
//!    (`PtyRecord::fd` and `PtyRecord::status` index their concatenation);
//!    then one message (payload `2`) with the server lock
//!    and the listening socket. `flock` follows the open file description,
//!    so the lock never lapses, and connections queue in the shared backlog.
//! 4. successor -> old: [`READY`] once every block runs and its state store
//!    is open. This is the commit point.
//! 5. old -> successor: [`COMMIT`]; the old server closes its copies and
//!    connections and exits without hanging up anything.
//!
//! Exactly one server owns the blocks at any time:
//!
//! - The successor keeps everything once it has tried to write `READY`,
//!   whether or not the write succeeded and whether or not a `COMMIT`
//!   follows. Failing before that, it drops its copies and closes the
//!   socket without writing anything.
//! - The old server, once its last descriptor message is sent, resumes
//!   only on proof that the successor gave up: end of stream (or a reset)
//!   with no byte read. A stream socket delivers every byte written before
//!   its close, so then no `READY` was written. Anything else (`READY`, a
//!   read error, partial or different bytes) means `READY` may have been
//!   written: it exits as on `COMMIT`, never re-adopting.
//! - A failed `READY` write that is not the old server's close could leave
//!   it still reading; the successor then holds the socket open until the
//!   old server ends, so that it never reads an end of stream.
//!
//! The old server waits for `READY` without a timeout (the successor's
//! steps are bounded). Before its last descriptor message any failure
//! aborts: the successor cannot have received everything, the old server
//! re-adopts its blocks and keeps serving. A master it cannot re-adopt is
//! parked open in the registry (the block shows its restored screen) and
//! travels with the next upgrade. The one case without a server: the old
//! server cannot read the outcome and exits, and the successor then fails
//! before `READY`; programs are hung up, never run by two servers.
//!
//! Remote access is not transferred: the old server closes its iroh
//! endpoint (and every remote connection) before `COMMIT`, and the
//! successor binds its own with the same identity from `<home>/state/iroh`.
//! Paired devices persist in `<home>/state`, re-read after the takeover;
//! pairing invites live only in memory and are lost: create new ones.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::Path,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use silo_proto::model::{BlockId, BlockInfo};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    Server,
    block::{self, BlockHandle, Handoff, Note},
    browser::Block,
    persist,
    registry::Session,
};

pub const MAGIC: &[u8] = b"SILO-UPGRADE 1\n";
pub const READY: &[u8] = b"READY\n";
pub const COMMIT: &[u8] = b"COMMIT\n";
pub const FDS_PER_MESSAGE: usize = 200;
/// Largest frame accepted (a manifest or one checkpoint).
const MAX_FRAME: usize = 1 << 30;
/// Longest wait for one step of the other side (restoring many blocks).
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize)]
pub struct Transfer {
    pub server_id: String,
    pub sessions: Vec<Session>,
    pub blocks: Vec<Record>,
    /// Browser and agent blocks: no process, so their info is all there is.
    pub browsers: Vec<BlockInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct Record {
    pub info: BlockInfo,
    /// The running program's PTY; `None` for exited or restored blocks.
    pub pty: Option<PtyRecord>,
}

#[derive(Serialize, Deserialize)]
pub struct PtyRecord {
    /// Index into the transferred descriptors.
    pub fd: usize,
    /// Index of the PTY host's status pipe; `None` for programs started
    /// without one.
    #[serde(default)]
    pub status: Option<usize>,
    /// Raw wait status the old server already read from that pipe.
    #[serde(default)]
    pub exit: Option<i32>,
    pub pid: i32,
    pub columns: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

fn write_frame(sock: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| io::Error::other("frame too large"))?;
    sock.write_all(&len.to_le_bytes())?;
    sock.write_all(bytes)
}

fn read_frame(sock: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    sock.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut bytes = vec![0u8; len];
    sock.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Old side: send the transfer, its checkpoints and descriptors (steps 2-3).
///
/// # Errors
/// Socket failures or a checkpoint count not matching the records.
pub fn send(sock: &mut UnixStream, transfer: &Transfer, checkpoints: &[Vec<u8>], fds: &[OwnedFd], server: ServerFds<'_>) -> io::Result<()> {
    if checkpoints.len() != transfer.blocks.len() {
        return Err(io::Error::other("one checkpoint per block"));
    }
    write_frame(sock, &serde_json::to_vec(transfer)?)?;
    for checkpoint in checkpoints {
        write_frame(sock, checkpoint)?;
    }
    sock.flush()?;
    for chunk in fds.chunks(FDS_PER_MESSAGE) {
        let fds: Vec<_> = chunk.iter().map(AsFd::as_fd).collect();
        silo_pty::handoff::send_fds(sock, &(chunk.len() as u32).to_le_bytes(), &fds)?;
    }
    silo_pty::handoff::send_fds(sock, &2u32.to_le_bytes(), &[server.lock, server.listener])
}

/// The server lock and listening socket, sent last: `flock` follows the
/// open file description and the socket keeps its backlog, so the server
/// passes on without a moment where neither side holds them.
#[derive(Clone, Copy)]
pub struct ServerFds<'a> {
    pub lock: std::os::fd::BorrowedFd<'a>,
    pub listener: std::os::fd::BorrowedFd<'a>,
}

/// The transfer, one encoded checkpoint per block, the PTY masters, and the
/// server lock and listening socket.
pub type Received = (Transfer, Vec<Vec<u8>>, Vec<OwnedFd>, [OwnedFd; 2]);

/// Successor side: receive what [`send`] sent.
///
/// # Errors
/// Socket failures or malformed data.
pub fn recv(sock: &mut UnixStream) -> io::Result<Received> {
    let transfer: Transfer = serde_json::from_slice(&read_frame(sock).map_err(refused)?)?;
    let checkpoints = (0..transfer.blocks.len()).map(|_| read_frame(sock)).collect::<io::Result<Vec<_>>>()?;
    let ptys = || transfer.blocks.iter().filter_map(|b| b.pty.as_ref());
    let want = ptys().map(|p| 1 + usize::from(p.status.is_some())).sum();
    let mut fds = Vec::with_capacity(want);
    while fds.len() < want {
        let (payload, got) = silo_pty::handoff::recv_fds(sock, FDS_PER_MESSAGE, 4)?;
        let count = payload.try_into().map(u32::from_le_bytes).map_err(|_| invalid("short descriptor header"))?;
        if count as usize != got.len() || got.is_empty() {
            return Err(invalid("descriptor count mismatch"));
        }
        fds.extend(got);
    }
    if fds.len() != want || ptys().any(|p| p.fd >= want || p.status.is_some_and(|s| s >= want)) {
        return Err(invalid("descriptor index out of range"));
    }
    let (payload, got) = silo_pty::handoff::recv_fds(sock, 2, 4)?;
    let server: [OwnedFd; 2] = got.try_into().map_err(|_| invalid("expected the server lock and socket"))?;
    if payload != 2u32.to_le_bytes() {
        return Err(invalid("expected the server lock and socket"));
    }
    Ok((transfer, checkpoints, fds, server))
}

/// The running server closed on [`MAGIC`]: a build from before live
/// upgrade. Nothing was transferred; it keeps serving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotUpgradable;

impl std::fmt::Display for NotUpgradable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the running server does not support live upgrade")
    }
}

impl std::error::Error for NotUpgradable {}

/// The old server hung up before answering [`MAGIC`] is [`NotUpgradable`].
fn refused(error: io::Error) -> io::Error {
    match error.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => io::Error::other(NotUpgradable),
        _ => error,
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// A block during an upgrade on the old server: released for transfer, or
/// still running because releasing it failed.
enum Slot {
    Released(Box<BlockInfo>, Box<Handoff>),
    Kept(BlockHandle),
}

/// A fault injected into the old server's read of [`READY`] (a test hook).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyFault {
    /// The read fails, once `READY` arrived, without consuming it.
    Error,
    /// The bytes read differ from `READY`.
    Corrupt,
}

/// How a handoff the successor may have committed to ended on the old side.
pub(crate) enum Given {
    /// `READY` was read.
    Committed(UnixStream),
    /// `READY` may have been written but was not read: the successor may
    /// serve, so this server must exit as if committed.
    Unknown(UnixStream, io::Error),
}

/// What the old server learned waiting for [`READY`].
enum Outcome {
    Ready,
    /// The successor closed without writing a byte: it never wrote READY.
    Gone,
    Unknown(io::Error),
}

fn read_ready(sock: &mut UnixStream, fault: Option<ReadyFault>) -> Outcome {
    if fault == Some(ReadyFault::Error) {
        let _ = wait_readable(sock);
        return Outcome::Unknown(io::Error::other("injected READY read error"));
    }
    let mut got = vec![0u8; READY.len()];
    let mut n = 0;
    while n < got.len() {
        match sock.read(&mut got[n..]) {
            Ok(0) if n == 0 => return Outcome::Gone,
            Ok(0) => return Outcome::Unknown(io::Error::new(io::ErrorKind::UnexpectedEof, "partial READY")),
            Ok(read) => n += read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset && n == 0 => return Outcome::Gone,
            Err(e) => return Outcome::Unknown(e),
        }
    }
    if fault == Some(ReadyFault::Corrupt) {
        got[0] ^= 0xff;
    }
    if got == READY { Outcome::Ready } else { Outcome::Unknown(invalid("unexpected upgrade message")) }
}

/// Block until `sock` has data (or its end) to read, consuming nothing.
#[allow(unsafe_code)]
fn wait_readable(sock: &UnixStream) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        // SAFETY: poll reads and writes one pollfd.
        if unsafe { libc::poll(&raw mut pfd, 1, -1) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Old side, after [`MAGIC`] was read: release every block, transfer them
/// and wait for [`READY`]. Unless it fails, the caller must exit without
/// serving again (release the lock, then write [`COMMIT`] to the returned
/// stream); on failure every block runs here again.
///
/// # Errors
/// Releasing or transferring, or the successor closing before `READY`.
pub(crate) async fn give(
    server: &Server,
    sock: UnixStream,
    server_id: String,
    own: [OwnedFd; 2],
    fault: Option<ReadyFault>,
) -> anyhow::Result<Given> {
    let (handles, browsers, sessions, mut parked) = {
        let mut reg = server.registry.lock().unwrap();
        // Browsers and agents stay registered: mutations are refused while upgrading.
        let terminals = reg.blocks.extract_if(|_, b| matches!(b, Block::Terminal(_)));
        let handles: Vec<_> = terminals.filter_map(|(id, b)| b.into_terminal().map(|h| (id, h))).collect();
        let browsers = reg.blocks.values().filter_map(Block::info).collect::<Vec<_>>();
        (handles, browsers, reg.sessions.clone(), std::mem::take(&mut reg.parked))
    };
    let mut slots = Vec::with_capacity(handles.len());
    let mut failed = None;
    for (id, handle) in handles {
        let info = Box::new(handle.info.lock().unwrap().clone());
        if failed.is_none() {
            match handle.handoff().await {
                Ok(mut released) => {
                    // A program parked here travels with its block.
                    if released.pty.is_none() {
                        released.pty = parked.remove(&id);
                    }
                    slots.push((id, Slot::Released(info, Box::new(released))));
                    continue;
                }
                Err(error) => failed = Some(error.context(format!("release block {id}"))),
            }
        }
        slots.push((id, Slot::Kept(handle)));
    }
    // Kept blocks keep their parked programs.
    server.registry.lock().unwrap().parked.extend(parked);
    let result = match failed {
        Some(error) => Err(error),
        None => transfer(sock, Transfer { server_id, sessions, blocks: Vec::new(), browsers }, &slots, own, fault).await,
    };
    if result.is_err() {
        readopt(&server.registry, slots);
    }
    result
}

/// Send `transfer` with the released `slots` as its blocks.
async fn transfer(
    mut sock: UnixStream,
    mut transfer: Transfer,
    slots: &[(BlockId, Slot)],
    own: [OwnedFd; 2],
    fault: Option<ReadyFault>,
) -> anyhow::Result<Given> {
    let mut checkpoints = Vec::with_capacity(slots.len());
    let mut fds = Vec::new();
    for (_, slot) in slots {
        let Slot::Released(info, released) = slot else { anyhow::bail!("block not released") };
        let pty = match &released.pty {
            Some(pty) => {
                fds.push(pty.master.try_clone()?);
                let fd = fds.len() - 1;
                let status = match &pty.status {
                    Some(status) => {
                        fds.push(status.try_clone()?);
                        Some(fds.len() - 1)
                    }
                    None => None,
                };
                let s = pty.size;
                let (columns, rows, cell_width, cell_height) = (s.columns, s.rows, s.cell_width, s.cell_height);
                Some(PtyRecord { fd, status, exit: pty.exit, pid: pty.pid, columns, rows, cell_width, cell_height })
            }
            None => None,
        };
        transfer.blocks.push(Record { info: (**info).clone(), pty });
        checkpoints.push(persist::encode(released.checkpoint.clone())?);
    }
    tokio::task::spawn_blocking(move || -> anyhow::Result<Given> {
        sock.set_nonblocking(false)?;
        sock.set_write_timeout(Some(STEP_TIMEOUT))?;
        sock.set_read_timeout(Some(STEP_TIMEOUT))?;
        expect(&mut sock, MAGIC)?;
        // READY is the commit point, so wait for it or the successor's end
        // (its own steps are bounded), never time out: a READY racing a
        // timeout would leave both servers running the same programs.
        sock.set_read_timeout(None)?;
        let [lock, listener] = &own;
        send(&mut sock, &transfer, &checkpoints, &fds, ServerFds { lock: lock.as_fd(), listener: listener.as_fd() })?;
        drop((fds, own));
        // From here the successor may hold everything: nothing but its
        // close without a byte gives the blocks back.
        match read_ready(&mut sock, fault) {
            Outcome::Ready => Ok(Given::Committed(sock)),
            Outcome::Gone => anyhow::bail!("the successor closed before READY"),
            Outcome::Unknown(error) => Ok(Given::Unknown(sock, error)),
        }
    })
    .await?
}

/// Run every released block here again (an aborted upgrade).
fn readopt(registry: &std::sync::Mutex<crate::registry::Registry>, slots: Vec<(BlockId, Slot)>) {
    let mut reg = registry.lock().unwrap();
    let notes = reg.notes();
    for (id, slot) in slots {
        let handle = match slot {
            Slot::Kept(handle) => Ok(handle),
            Slot::Released(info, released) => match *released {
                Handoff { checkpoint, pty: Some(pty) } => match block::spawn_adopted((*info).clone(), checkpoint.clone(), pty, notes.clone()) {
                    Ok(handle) => Ok(handle),
                    Err(error) => match error.downcast::<block::AdoptFailed>() {
                        Ok(failed) => {
                            tracing::error!(block = %id, "re-adopt after aborted upgrade: {:#}; program parked", failed.error);
                            reg.parked.insert(id.clone(), failed.detached);
                            restore(*info, checkpoint, &notes)
                        }
                        Err(error) => Err(error),
                    },
                },
                Handoff { checkpoint, pty: None } => restore(*info, checkpoint, &notes),
            },
        };
        match handle {
            Ok(handle) => drop(reg.blocks.insert(id, Block::Terminal(handle))),
            Err(error) => tracing::error!(block = %id, "re-adopt after aborted upgrade: {error:#}"),
        }
    }
}

/// A block without its program, showing `checkpoint`, or an empty screen
/// if that cannot be restored.
fn restore(info: BlockInfo, checkpoint: block::Checkpoint, notes: &UnboundedSender<Note>) -> anyhow::Result<BlockHandle> {
    block::spawn_restored(info.clone(), checkpoint, notes.clone()).or_else(|error| {
        tracing::warn!(block = %info.id, "restoring with an empty screen: {error:#}");
        let empty = persist::empty_checkpoint(info.size)?;
        block::spawn_restored(info, empty, notes.clone())
    })
}

/// What a successor took over: the tree with every block running here,
/// and the old server's lock and listening socket.
pub struct Taken {
    pub server_id: String,
    pub sessions: Vec<Session>,
    pub blocks: HashMap<BlockId, Block>,
    pub lock: std::fs::File,
    pub listener: tokio::net::UnixListener,
    pub persist: persist::Persist,
}

/// Successor side: take every block from the server of `home` and run them
/// here. Every failure happens before [`READY`], so the old server re-adopts
/// its blocks; with `fail_before_ready` it fails right there (a test hook).
///
/// # Errors
/// No server is running, it refuses, or the transfer fails.
pub async fn take(home: &Path, notes: &UnboundedSender<Note>, fail_before_ready: bool) -> anyhow::Result<Taken> {
    let socket = silo_proto::paths::socket(home);
    let (sock, transfer, checkpoints, fds, [lock, listener]) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let mut sock = UnixStream::connect(&socket).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => io::Error::new(e.kind(), "no running server to take over"),
            _ => e,
        })?;
        silo_pty::handoff::check_peer_uid(&sock)?;
        sock.set_read_timeout(Some(STEP_TIMEOUT))?;
        sock.set_write_timeout(Some(STEP_TIMEOUT))?;
        sock.write_all(MAGIC).map_err(refused)?;
        let (transfer, checkpoints, fds, server) = recv(&mut sock)?;
        Ok((sock, transfer, checkpoints, fds, server))
    })
    .await?
    .map_err(|e| if e.get_ref().is_some_and(|e| e.is::<NotUpgradable>()) { anyhow::Error::from(NotUpgradable) } else { e.into() })?;
    let persist = persist::Persist::attach(home, transfer.server_id.clone())?;
    if fail_before_ready {
        anyhow::bail!("upgrade aborted before READY (test hook)");
    }
    let (lock, listener) = (std::fs::File::from(lock), std::os::unix::net::UnixListener::from(listener));
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let browsers = transfer.browsers.into_iter().map(|info| Ok((info.id.clone(), Block::restore(info)?)));
    let browsers = browsers.collect::<anyhow::Result<Vec<_>>>()?;
    let blocks = adopt(transfer.blocks, checkpoints, fds, notes).await?;
    let blocks = commit(sock, blocks).await;
    let blocks = blocks.into_iter().map(|(id, h)| (id, Block::Terminal(h))).chain(browsers).collect();
    Ok(Taken { server_id: transfer.server_id, sessions: transfer.sessions, blocks, lock, listener, persist })
}

/// Write [`READY`] and wait for [`COMMIT`]. From the attempt on, the blocks
/// are ours whatever follows: the old server resumes only after reading an
/// end of stream with no byte, which this never gives it. A missing COMMIT
/// (the old server died committing) is only logged, never a hangup.
///
/// A READY that cannot be written means the old server closed (it exits
/// without serving again) or, in principle, a write error with it still
/// reading: a thread then holds the socket open until the old server ends,
/// so it never mistakes our silence for an abort.
///
/// Under systemd this process becomes the unit's main PID before READY:
/// the old server cannot exit before reading READY, so systemd never sees
/// the unit lose its main process.
async fn commit(mut sock: UnixStream, blocks: HashMap<BlockId, BlockHandle>) -> HashMap<BlockId, BlockHandle> {
    crate::notify::notify(&format!("MAINPID={}", std::process::id()));
    // Six bytes into the old server's empty receive buffer: no wait.
    match sock.write_all(READY) {
        Ok(()) => {
            let answer = tokio::task::spawn_blocking(move || expect(&mut sock, COMMIT)).await;
            if !matches!(answer, Ok(Ok(()))) {
                tracing::warn!("no COMMIT from the old server; serving its blocks anyway");
            }
        }
        Err(error) => {
            tracing::warn!("send READY: {error}; serving the blocks (the old server exits without reading it)");
            std::thread::spawn(move || {
                let _ = sock.set_read_timeout(None);
                let _ = io::copy(&mut sock, &mut io::sink());
            });
        }
    }
    blocks
}

async fn adopt(
    records: Vec<Record>,
    checkpoints: Vec<Vec<u8>>,
    fds: Vec<OwnedFd>,
    notes: &UnboundedSender<Note>,
) -> anyhow::Result<HashMap<BlockId, BlockHandle>> {
    let mut fds: Vec<Option<OwnedFd>> = fds.into_iter().map(Some).collect();
    let mut blocks = HashMap::new();
    for (record, bytes) in records.into_iter().zip(checkpoints) {
        let id = record.info.id.clone();
        let spawned = persist::decode(&bytes).and_then(|checkpoint| match record.pty {
            Some(p) => {
                let master = fds[p.fd].take().ok_or_else(|| anyhow::anyhow!("descriptor {} reused", p.fd))?;
                let status = p.status.map(|i| fds[i].take().ok_or_else(|| anyhow::anyhow!("descriptor {i} reused"))).transpose()?;
                let size = silo_pty::PtySize { columns: p.columns, rows: p.rows, cell_width: p.cell_width, cell_height: p.cell_height };
                block::spawn_adopted(record.info, checkpoint, silo_pty::Detached { master, status, exit: p.exit, pid: p.pid, size }, notes.clone())
            }
            None => block::spawn_restored(record.info, checkpoint, notes.clone()),
        });
        match spawned {
            Ok(handle) => drop(blocks.insert(id, handle)),
            // Before READY the old server holds its own copy of every master
            // (it re-adopts them), so dropping ours, including the one in
            // `AdoptFailed`, hangs nothing up.
            Err(error) => {
                release(blocks.into_values()).await;
                return Err(error.context(format!("adopt block {id}")));
            }
        }
    }
    Ok(blocks)
}

/// Stop blocks adopted here without hanging up their programs, leaving
/// them to the old server.
async fn release(blocks: impl Iterator<Item = BlockHandle>) {
    for handle in blocks {
        if let Err(error) = handle.handoff().await {
            tracing::warn!("release adopted block: {error:#}");
        }
    }
}

/// Read exactly `line` from `sock`.
///
/// # Errors
/// Socket failures, EOF, or different bytes.
pub fn expect(sock: &mut UnixStream, line: &[u8]) -> io::Result<()> {
    let mut got = vec![0u8; line.len()];
    sock.read_exact(&mut got)?;
    if got == line { Ok(()) } else { Err(invalid("unexpected upgrade message")) }
}

/// Whether a freshly accepted stream starts with [`MAGIC`], without
/// consuming anything. A JSON hello differs from it at the first byte.
///
/// # Errors
/// Socket failures.
pub async fn peek_magic(stream: &tokio::net::UnixStream) -> io::Result<bool> {
    // A peer that stalls (or closes) mid-prefix never completes it.
    tokio::time::timeout(PEEK_TIMEOUT, peek_loop(stream)).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "incomplete first message"))?
}

const PEEK_TIMEOUT: Duration = Duration::from_secs(2);

async fn peek_loop(stream: &tokio::net::UnixStream) -> io::Result<bool> {
    let mut buf = [0u8; MAGIC.len()];
    loop {
        stream.readable().await?;
        let n = match peek_once(stream, &mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        };
        if n == 0 || buf[..n] != MAGIC[..n] {
            return Ok(false);
        }
        if n == MAGIC.len() {
            return Ok(true);
        }
        // A partial prefix stays readable; wait for the rest.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[allow(unsafe_code)]
fn peek_once(stream: &tokio::net::UnixStream, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    // SAFETY: recv writes at most `buf.len()` bytes into `buf`.
    let n = unsafe { libc::recv(stream.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_PEEK) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn peek(bytes: &[u8]) -> bool {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let mut a = a.into_std().unwrap();
        a.set_nonblocking(false).unwrap();
        a.write_all(bytes).unwrap();
        let found = peek_magic(&b).await.unwrap();
        let mut b = b.into_std().unwrap();
        b.set_nonblocking(false).unwrap();
        let mut first = vec![0u8; bytes.len()];
        b.read_exact(&mut first).unwrap();
        assert_eq!(first, bytes, "peeking consumes nothing");
        found
    }

    /// Only a close with no byte proves the successor never wrote READY.
    #[test]
    fn ready_outcomes() {
        let outcome = |bytes: &[u8], fault| {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            a.write_all(bytes).unwrap();
            drop(a);
            read_ready(&mut b, fault)
        };
        assert!(matches!(outcome(READY, None), Outcome::Ready));
        assert!(matches!(outcome(b"READY\nmore", None), Outcome::Ready));
        assert!(matches!(outcome(b"", None), Outcome::Gone));
        assert!(matches!(outcome(b"REA", None), Outcome::Unknown(_)), "partial");
        assert!(matches!(outcome(b"RESET\n", None), Outcome::Unknown(_)), "different");
        assert!(matches!(outcome(READY, Some(ReadyFault::Corrupt)), Outcome::Unknown(_)));
        assert!(matches!(outcome(READY, Some(ReadyFault::Error)), Outcome::Unknown(_)), "READY there but not read");
        assert!(matches!(outcome(b"", Some(ReadyFault::Error)), Outcome::Unknown(_)), "a failed read proves nothing");
    }

    #[tokio::test]
    async fn magic_is_peeked_not_consumed() {
        assert!(peek(MAGIC).await);
        assert!(peek(b"SILO-UPGRADE 1\nmore").await);
        assert!(!peek(b"\x10\0\0\0\x01{}").await);
        assert!(!peek(b"SILO-UPGRADE 2\n").await);
    }

    /// A master that cannot be re-adopted after an aborted upgrade stays
    /// open (parked); its program keeps running and the block stays.
    #[tokio::test]
    async fn failed_readopt_parks_and_keeps_program() {
        use silo_proto::model::{BlockKind, Lifecycle, TermSize};
        let (notes, _rx) = tokio::sync::mpsc::unbounded_channel();
        let info = BlockInfo {
            id: "b1".into(),
            session_id: "s1".into(),
            kind: BlockKind::Terminal,
            title: "shell".into(),
            cwd: None,
            exited: None,
            lifecycle: Lifecycle::Running,
            size: TermSize::new(40, 10),
            owner: None,
            owner_name: None,
            pid: None,
            browser: None,
            agent: None,
        };
        let config = silo_pty::SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()], ..Default::default() };
        let handle = block::spawn(info.clone(), &config, notes.clone()).unwrap();
        let mut released = handle.handoff().await.unwrap();
        let pid = released.pty.as_ref().unwrap().pid;
        released.checkpoint.snapshot = b"not a snapshot".to_vec();
        let registry = std::sync::Mutex::new(crate::registry::Registry::new(notes));
        readopt(&registry, vec![("b1".into(), Slot::Released(Box::new(info), Box::new(released)))]);
        tokio::time::sleep(Duration::from_millis(200)).await;
        {
            let reg = registry.lock().unwrap();
            assert!(reg.parked.contains_key("b1"), "master parked");
            assert!(reg.blocks.contains_key("b1"), "block kept");
        }
        assert!(rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_ok(), "program alive");
        registry.lock().unwrap().shutdown();
    }

    #[tokio::test]
    async fn stalled_or_closed_prefix_times_out() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let mut a = a.into_std().unwrap();
        a.set_nonblocking(false).unwrap();
        a.write_all(&MAGIC[..5]).unwrap();
        let error = peek_magic(&b).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(a);
        assert_eq!(peek_magic(&b).await.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
