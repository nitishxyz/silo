//! Live upgrade: a successor takes every block from the running server in
//! one process; programs keep their PIDs and streams continue gaplessly.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, DataStream};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    data::{ClientData, Resume, ServerData, StartMode},
    model::{Axis, BlockInfo, BlockKind, Lifecycle, SessionView, TermSize},
    paths,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-upgrade-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn config(home: &Path) -> silo_server::Config {
    silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        ..Default::default()
    }
}

async fn start(home: &Path) -> JoinHandle<anyhow::Result<()>> {
    start_with(config(home)).await
}

async fn start_with(config: silo_server::Config) -> JoinHandle<anyhow::Result<()>> {
    let home = config.home.clone();
    let home = home.as_path();
    let task = tokio::spawn(silo_server::run(config));
    for _ in 0..300 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path) -> Control {
    Control::connect(&paths::socket(home), "upgrade-test").await.unwrap().0
}

fn counter() -> SpawnSpec {
    let script = "i=0; while :; do i=$((i+1)); echo n$i; sleep 0.01; done";
    SpawnSpec { command: vec!["/bin/sh".into(), "-c".into(), script.into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) }
}

async fn view(control: &Control, session: &str) -> SessionView {
    control.call(Method::SessionView { session_id: session.into() }).await.unwrap()
}

/// Streamed output of one block across reconnects.
struct Tail {
    epoch: u64,
    next: u64,
    text: String,
}

impl Tail {
    fn numbers(&self) -> Vec<u64> {
        self.text.split_whitespace().filter_map(|w| w.strip_prefix('n')?.parse().ok()).collect()
    }
}

/// Apply output, acking it, until `done` holds (true) or the stream ends
/// (false). Every output frame must continue exactly at `tail.next`.
async fn pump(stream: &mut DataStream, tail: &mut Tail, done: impl Fn(&Tail) -> bool) -> bool {
    loop {
        if done(tail) {
            return true;
        }
        let Some(data) = tokio::time::timeout(TIMEOUT, stream.rx.recv()).await.expect("output") else {
            return false;
        };
        if let ServerData::Output { offset, bytes } = data {
            assert_eq!(offset, tail.next, "output continues without gap or overlap");
            tail.next += bytes.len() as u64;
            tail.text.push_str(&String::from_utf8_lossy(&bytes));
            stream.send(&ClientData::Ack(tail.next));
        }
    }
}

fn assert_consecutive(numbers: &[u64]) {
    // The last line may still be partial.
    let numbers = &numbers[..numbers.len().saturating_sub(1)];
    assert!(numbers.len() > 10, "too little output: {numbers:?}");
    for pair in numbers.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "numbers skip or repeat: {numbers:?}");
    }
}

/// Connect once a server answers again.
async fn reconnect(home: &Path) -> Control {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok((control, _)) = Control::connect(&paths::socket(home), "upgrade-test").await {
            return control;
        }
        assert!(tokio::time::Instant::now() < deadline, "no server after upgrade");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A session with a split of two counters, streaming the first block.
async fn populate(control: &Control) -> (Created, SessionView, DataStream, Tail) {
    let created: Created =
        control.call(Method::SessionCreate { name: Some("main".into()), spawn: counter(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: created.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: counter(),
        kind: BlockKind::Terminal,
        url: None,
        agent: None,
    };
    let _: Created = control.call(split).await.unwrap();
    let before = view(control, &created.session_id).await;
    let mut stream = DataStream::open(control, &created.block_id, None).await.unwrap();
    let mut tail = Tail { epoch: stream.welcome.epoch, next: stream.welcome.offset, text: String::new() };
    assert!(pump(&mut stream, &mut tail, |t| t.numbers().len() > 20).await);
    (created, before, stream, tail)
}

/// Same tree, same programs.
fn assert_same(before: &SessionView, after: &SessionView) {
    assert_eq!(serde_json::to_value(&after.session).unwrap(), serde_json::to_value(&before.session).unwrap(), "same tree");
    assert_eq!(before.blocks.len(), after.blocks.len());
    for (a, b) in before.blocks.iter().zip(&after.blocks) {
        assert_eq!((&a.id, a.pid, a.lifecycle), (&b.id, b.pid, b.lifecycle), "same programs");
        assert!(a.pid.is_some());
    }
}

/// Resume `tail` on `control`'s server and read 20 more numbers.
async fn resume(control: &Control, block: &str, tail: &mut Tail) {
    let resume = Resume { epoch: tail.epoch, offset: tail.next };
    let mut stream = DataStream::open(control, block, Some(resume)).await.unwrap();
    let welcome = &stream.welcome;
    assert_eq!((welcome.mode, welcome.epoch, welcome.offset), (StartMode::Resume, tail.epoch, tail.next));
    let seen = tail.numbers().len();
    assert!(pump(&mut stream, tail, |t| t.numbers().len() > seen + 20).await);
    assert_consecutive(&tail.numbers());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_keeps_programs_streams_and_layout() {
    let home = temp_home("live");
    let old = start(&home).await;
    let control = connect(&home).await;
    let (created, before, mut stream, mut tail) = populate(&control).await;

    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    tokio::time::timeout(TIMEOUT, old).await.unwrap().unwrap().expect("old server commits and exits");
    assert!(!pump(&mut stream, &mut tail, |_| false).await, "old stream ends");
    drop((stream, control));

    let control = reconnect(&home).await;
    assert_same(&before, &view(&control, &created.session_id).await);
    resume(&control, &created.block_id, &mut tail).await;

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, new).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// An old server speaking the handoff by hand: hands over one running
/// counter block in session `s1`, reads READY, then dies without COMMIT.
/// Returns the program's pid and the thread.
async fn fake_old_without_commit(home: &Path) -> (i32, std::thread::JoinHandle<()>) {
    use silo_server::{
        block, handoff,
        registry::{Session, Window},
    };
    use std::os::fd::AsFd;
    std::fs::create_dir_all(home).unwrap();
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(paths::server_lock(home)).unwrap();
    rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(paths::socket(home)).unwrap();
    let info = BlockInfo {
        id: "b1".into(),
        session_id: "s1".into(),
        kind: BlockKind::Terminal,
        title: "shell".into(),
        cwd: None,
        exited: None,
        lifecycle: Lifecycle::Running,
        size: TermSize::new(80, 24),
        owner: None,
        owner_name: None,
        owner_block: None,
        pid: None,
        browser: None,
        agent: None,
        files: None,
        changes: None,
    };
    let spec = counter();
    let config = silo_pty::SpawnConfig { command: spec.command.iter().map(Into::into).collect(), ..Default::default() };
    let (notes, _) = tokio::sync::mpsc::unbounded_channel();
    let released = block::spawn(info.clone(), &config, notes).unwrap().handoff().await.unwrap();
    let pty = released.pty.unwrap();
    let (pid, s) = (pty.pid, pty.size);
    let record = handoff::PtyRecord {
        fd: 0,
        status: None,
        exit: None,
        pid,
        columns: s.columns,
        rows: s.rows,
        cell_width: s.cell_width,
        cell_height: s.cell_height,
    };
    let window = Window::new("w1".into(), "b1");
    let session = Session {
        id: "s1".into(),
        name: "main".into(),
        windows: vec![window],
        active_window: "w1".into(),
        revision: 1,
        created_at: 0,
        parked: Vec::new(),
    };
    let blocks = vec![handoff::Record { info, pty: Some(record) }];
    let transfer = handoff::Transfer { server_id: "fake".into(), sessions: vec![session], blocks, browsers: Vec::new() };
    let checkpoint = silo_server::persist::encode(released.checkpoint).unwrap();
    let thread = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        handoff::expect(&mut sock, handoff::MAGIC).unwrap();
        let fds = handoff::ServerFds { lock: lock.as_fd(), listener: listener.as_fd() };
        handoff::send(&mut sock, &transfer, &[checkpoint], &[pty.master], fds).unwrap();
        handoff::expect(&mut sock, handoff::READY).unwrap();
        // Dies here: every descriptor of the old server closes, no COMMIT.
    });
    (pid, thread)
}

/// READY is the successor's commit point: an old server dying before its
/// COMMIT leaves the successor serving the same running program.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_commit_after_ready_keeps_programs() {
    let home = temp_home("nocommit");
    let (pid, old) = fake_old_without_commit(&home).await;
    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    tokio::task::spawn_blocking(move || old.join().unwrap()).await.unwrap();

    let control = reconnect(&home).await;
    let after = view(&control, "s1").await;
    assert_eq!((after.blocks[0].id.as_str(), after.blocks[0].pid), ("b1", Some(pid)));
    let mut stream = DataStream::open(&control, "b1", None).await.unwrap();
    let mut tail = Tail { epoch: stream.welcome.epoch, next: stream.welcome.offset, text: String::new() };
    assert!(pump(&mut stream, &mut tail, |t| t.numbers().len() > 20).await, "the program keeps running");
    assert_consecutive(&tail.numbers());

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop((stream, control));
    tokio::time::timeout(TIMEOUT, new).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// The old server cannot read the successor's READY (`fault`; READY did
/// arrive): as READY may have been written, the old server exits instead
/// of re-adopting, and the successor alone serves the same programs and
/// sessions.
async fn lost_ready_leaves_one_server(name: &str, fault: silo_server::handoff::ReadyFault) {
    let home = temp_home(name);
    let old = start_with(silo_server::Config { ready_fault: Some(fault), ..config(&home) }).await;
    let control = connect(&home).await;
    let (created, before, mut stream, mut tail) = populate(&control).await;

    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    tokio::time::timeout(TIMEOUT, old).await.unwrap().unwrap().expect("old server exits");
    assert!(!pump(&mut stream, &mut tail, |_| false).await, "old stream ends");
    drop((stream, control));
    assert!(!new.is_finished(), "the successor serves");
    assert!(silo_client::mismatch::server_lock_held(&home), "the lock stays held");

    // The listening socket is the successor's alone now: any connection is
    // to it, queued until it accepts.
    let control = connect(&home).await;
    assert_same(&before, &view(&control, &created.session_id).await);
    // Output the old server had not sent yet may be gone from the stream,
    // so attach afresh rather than resume: the same program keeps counting.
    let mut stream = DataStream::open(&control, &created.block_id, None).await.unwrap();
    let mut tail = Tail { epoch: stream.welcome.epoch, next: stream.welcome.offset, text: String::new() };
    assert!(pump(&mut stream, &mut tail, |t| t.numbers().len() > 20).await, "the program keeps running");
    assert_consecutive(&tail.numbers());
    drop(stream);
    control.call_value(Method::SessionRename { session_id: created.session_id.clone(), name: "renamed".into() }).await.unwrap();
    assert_eq!(view(&control, &created.session_id).await.session.name, "renamed");

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, new).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_ready_read_leaves_only_the_successor() {
    lost_ready_leaves_one_server("ready-error", silo_server::handoff::ReadyFault::Error).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_ready_leaves_only_the_successor() {
    lost_ready_leaves_one_server("ready-corrupt", silo_server::handoff::ReadyFault::Corrupt).await;
}

/// Clients starting up during the upgrade never find the home without a
/// server (no spawn attempt; any would fail here) and end on the successor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_connect_or_spawn_never_spawns() {
    let home = temp_home("race");
    let old = start(&home).await;
    let control = connect(&home).await;
    let (created, before, stream, _) = populate(&control).await;
    drop((stream, control));

    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    let mut clients = Vec::new();
    for _ in 0..20 {
        let home = home.clone();
        clients.push(tokio::spawn(async move { silo_client::connect_or_spawn(&home, Path::new("/nonexistent/silo"), "race").await.map(|(c, _)| c) }));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::timeout(TIMEOUT, old).await.unwrap().unwrap().unwrap();
    for client in clients {
        let result = tokio::time::timeout(TIMEOUT, client).await.unwrap().unwrap();
        result.expect("connects to a server, never spawns");
    }
    let control = reconnect(&home).await;
    assert_same(&before, &view(&control, &created.session_id).await);
    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, new).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborted_takeover_keeps_old_server_serving() {
    let home = temp_home("abort");
    let old = start(&home).await;
    let control = connect(&home).await;
    let (created, before, mut stream, mut tail) = populate(&control).await;

    let error = silo_server::takeover(config(&home), true).await.expect_err("hook aborts");
    assert!(format!("{error:#}").contains("test hook"), "{error:#}");
    assert!(!pump(&mut stream, &mut tail, |_| false).await, "released stream ends");
    drop(stream);
    assert!(silo_client::mismatch::server_lock_held(&home), "the successor's copy of the lock went without unlocking it");

    // Same server, same programs, still accepting mutations.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while view(&control, &created.session_id).await.blocks.len() < before.blocks.len() {
        assert!(tokio::time::Instant::now() < deadline, "blocks not re-adopted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_same(&before, &view(&control, &created.session_id).await);
    resume(&control, &created.block_id, &mut tail).await;
    control.call_value(Method::SessionRename { session_id: created.session_id.clone(), name: "renamed".into() }).await.unwrap();

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, old).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// After a takeover the successor still learns each program's exit status
/// from its PTY host: `exit 0` closes the block, `exit 3` shows Exited(3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_keeps_exit_status() {
    let home = temp_home("status");
    let old = start(&home).await;
    let control = connect(&home).await;
    let login = SpawnSpec { command: Vec::new(), cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    let exiting = SpawnSpec { command: vec!["/bin/sh".into(), "-c".into(), "read code; exit $code".into()], ..login.clone() };
    let created: Created =
        control.call(Method::SessionCreate { name: Some("main".into()), spawn: login, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: created.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: exiting,
        kind: BlockKind::Terminal,
        url: None,
        agent: None,
    };
    let failing: Created = control.call(split).await.unwrap();

    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    tokio::time::timeout(TIMEOUT, old).await.unwrap().unwrap().expect("old server commits and exits");
    drop(control);
    let control = reconnect(&home).await;

    let failed = DataStream::open(&control, &failing.block_id, None).await.unwrap();
    failed.send(&ClientData::Input(b"3\n".to_vec()));
    let clean = DataStream::open(&control, &created.block_id, None).await.unwrap();
    clean.send(&ClientData::Input(b"exit 0\n".to_vec()));
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let blocks = view(&control, &created.session_id).await.blocks;
        let ids: Vec<_> = blocks.iter().map(|b| (b.id.as_str(), b.lifecycle, b.exited)).collect();
        if ids == [(failing.block_id.as_str(), Lifecycle::Exited, Some(3))] {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "exit statuses not seen: {ids:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop((failed, clean, control));
    tokio::time::timeout(TIMEOUT, new).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
