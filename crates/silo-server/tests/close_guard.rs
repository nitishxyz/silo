//! Confirm before closing busy blocks (Phase 7 §7.5, P7-9) on real PTYs:
//! the all-or-nothing `busy` refusal of the five closes for clients that
//! advertise `close_guard`, `force`, the fresh probe, `blocks.busy`
//! visibility, and the close check running before approval.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use silo_client::Control;
use silo_proto::{
    Frame,
    access::{Rights, Scope},
    compat::{self, feature},
    control::{
        BlockView, BusyBlock, BusyBlocks, ClientHello, ClientRole, Created, Event, GrantCreated, GrantKind, Method, RpcError, ServerMessage,
        SpawnSpec,
    },
    kind,
    model::{Axis, BlockKind},
    paths, read_frame, write_frame,
};
use silo_transport::RelayConfig;
use tokio::{io::AsyncWriteExt, net::UnixStream, task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(15);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-close-guard-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start_with(home: &Path, require_approval: bool) -> JoinHandle<()> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        relay: RelayConfig::Disabled,
        mdns: false,
        require_approval,
        ..Default::default()
    };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn start(home: &Path) -> JoinHandle<()> {
    start_with(home, false).await
}

/// A control client speaking raw frames, so each test chooses its hello
/// (`silo_client::advertise` is per process).
struct Raw {
    stream: UnixStream,
    next: u64,
    events: Vec<Event>,
}

impl Raw {
    async fn connect(home: &Path, features: &[&str], role: ClientRole, grant: Option<String>) -> Self {
        let mut hello = ClientHello { role, grant, ..ClientHello::ours("close-guard-test", false, None) };
        hello.features.extend(features.iter().map(|&f| f.to_owned()));
        let mut stream = UnixStream::connect(paths::socket(home)).await.unwrap();
        write_frame(&mut stream, &Frame::json(kind::CONTROL_HELLO, &hello).unwrap()).await.unwrap();
        stream.flush().await.unwrap();
        let reply = timeout(T, read_frame(&mut stream)).await.unwrap().unwrap().expect("a hello reply");
        assert_eq!(reply.kind, kind::CONTROL_HELLO, "admitted");
        Self { stream, next: 1, events: Vec::new() }
    }

    /// A CLI that handles `busy` refusals.
    async fn guarded(home: &Path) -> Self {
        Self::connect(home, &[feature::CLOSE_GUARD], ClientRole::Cli, None).await
    }

    async fn call(&mut self, method: Method) -> Result<Value, RpcError> {
        let id = self.next;
        self.next += 1;
        let mut request = serde_json::to_value(&method).unwrap();
        request["id"] = id.into();
        write_frame(&mut self.stream, &Frame::json(kind::CONTROL, &request).unwrap()).await.unwrap();
        self.stream.flush().await.unwrap();
        loop {
            let frame = timeout(Duration::from_secs(30), read_frame(&mut self.stream)).await.expect("a reply in time").unwrap().expect("a frame");
            match frame.parse::<ServerMessage>().unwrap() {
                ServerMessage::Response { id: got, result, error } if got == id => return error.map_or(Ok(result.unwrap_or_default()), Err),
                ServerMessage::Event { event, .. } => self.events.push(*event),
                _ => {}
            }
        }
    }

    async fn next_event(&mut self) -> Event {
        if !self.events.is_empty() {
            return self.events.remove(0);
        }
        loop {
            let frame = timeout(T, read_frame(&mut self.stream)).await.expect("an event in time").unwrap().expect("a frame");
            if let ServerMessage::Event { event, .. } = frame.parse::<ServerMessage>().unwrap() {
                return *event;
            }
        }
    }

    async fn busy(&mut self, session_id: Option<&str>, window_id: Option<&str>) -> Vec<BusyBlock> {
        let method = Method::BlocksBusy { session_id: session_id.map(Into::into), window_id: window_id.map(Into::into), block_ids: None };
        let busy: BusyBlocks = serde_json::from_value(self.call(method).await.expect("blocks.busy")).unwrap();
        busy.busy
    }
}

fn command(argv: &[&str]) -> SpawnSpec {
    SpawnSpec { command: argv.iter().map(|&a| a.to_owned()).collect(), ..SpawnSpec::default() }
}

const SLEEP: &[&str] = &["sleep", "30"];
/// Idle: `sh` waits in its builtin `read`, and a shell is harmless.
const IDLE: &[&str] = &["/bin/sh", "-c", "read x"];

async fn session(control: &Control, argv: &[&str]) -> Created {
    control.call(Method::SessionCreate { name: None, spawn: command(argv), kind: BlockKind::Terminal, url: None }).await.unwrap()
}

/// Wait until `blocks.busy` over `session` names exactly `want`: a new
/// block's program may not have started (or exec'd) yet.
async fn until_busy(raw: &mut Raw, session: &str, want: &[&str]) {
    let deadline = tokio::time::Instant::now() + T;
    loop {
        let busy: Vec<String> = raw.busy(Some(session), None).await.into_iter().map(|b| b.block_id).collect();
        if busy == want {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "busy {busy:?}, want {want:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn refused(result: Result<Value, RpcError>) -> Vec<BusyBlock> {
    let error = result.expect_err("refused as busy");
    assert_eq!(error.code, RpcError::BUSY, "{error:?}");
    let busy: BusyBlocks = serde_json::from_value(error.data.expect("busy data")).unwrap();
    busy.busy
}

async fn exists(control: &Control, block_id: &str) -> bool {
    control.call::<BlockView>(Method::BlockView { block_id: block_id.into() }).await.is_ok()
}

async fn stop(control: &Control, task: JoinHandle<()>, home: PathBuf) {
    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    let _ = timeout(T, task).await;
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test(flavor = "multi_thread")]
async fn block_close_of_a_busy_block_is_refused_until_forced() {
    let home = temp_home("block");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut raw = Raw::guarded(&home).await;
    let created = session(&control, SLEEP).await;
    until_busy(&mut raw, &created.session_id, &[&created.block_id]).await;

    let busy = refused(raw.call(Method::BlockClose { block_id: created.block_id.clone(), force: false }).await);
    assert_eq!(busy.len(), 1);
    assert_eq!(busy[0].busy.process, "sleep");
    assert_eq!((busy[0].block_id.as_str(), busy[0].session_id.as_str()), (created.block_id.as_str(), created.session_id.as_str()));
    assert!(exists(&control, &created.block_id).await, "nothing closed");

    raw.call(Method::BlockClose { block_id: created.block_id.clone(), force: true }).await.unwrap();
    assert!(!exists(&control, &created.block_id).await, "forced closed");
    stop(&control, task, home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn window_close_with_one_busy_pane_closes_neither() {
    let home = temp_home("window");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut raw = Raw::guarded(&home).await;
    let created = session(&control, SLEEP).await;
    let split = Method::BlockSplit {
        block_id: created.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: command(IDLE),
        kind: BlockKind::Terminal,
        url: None,
        agent: None,
    };
    let idle: Created = control.call(split).await.unwrap();
    until_busy(&mut raw, &created.session_id, &[&created.block_id]).await;

    let busy = refused(raw.call(Method::WindowClose { window_id: created.window_id.clone(), force: false }).await);
    assert_eq!(busy.iter().map(|b| b.block_id.as_str()).collect::<Vec<_>>(), [created.block_id.as_str()]);
    assert_eq!(busy[0].window_id, created.window_id);
    assert!(exists(&control, &created.block_id).await && exists(&control, &idle.block_id).await, "neither pane closed");

    raw.call(Method::WindowClose { window_id: created.window_id.clone(), force: true }).await.unwrap();
    assert!(!exists(&control, &created.block_id).await && !exists(&control, &idle.block_id).await);
    stop(&control, task, home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn session_close_is_all_or_nothing_until_forced() {
    let home = temp_home("session");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut raw = Raw::guarded(&home).await;
    let created = session(&control, IDLE).await;
    let window: Created = control
        .call(Method::WindowCreate {
            session_id: created.session_id.clone(),
            spawn: command(SLEEP),
            kind: BlockKind::Terminal,
            url: None,
            background: false,
        })
        .await
        .unwrap();
    until_busy(&mut raw, &created.session_id, &[&window.block_id]).await;

    let busy = refused(raw.call(Method::SessionClose { session_id: created.session_id.clone(), force: false }).await);
    assert_eq!(busy.iter().map(|b| b.busy.process.as_str()).collect::<Vec<_>>(), ["sleep"]);
    assert!(exists(&control, &created.block_id).await && exists(&control, &window.block_id).await, "nothing closed");

    raw.call(Method::SessionClose { session_id: created.session_id.clone(), force: true }).await.unwrap();
    assert!(!exists(&control, &created.block_id).await && !exists(&control, &window.block_id).await);
    stop(&control, task, home).await;
}

async fn stopping_the_server_is_refused_until_forced(name: &str, method: fn(bool) -> Method) {
    let home = temp_home(name);
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut raw = Raw::guarded(&home).await;
    let idle = session(&control, IDLE).await;
    let busy = session(&control, SLEEP).await;
    until_busy(&mut raw, &busy.session_id, &[&busy.block_id]).await;
    until_busy(&mut raw, &idle.session_id, &[]).await;

    let refusal = refused(raw.call(method(false)).await);
    assert_eq!(refusal.iter().map(|b| b.block_id.as_str()).collect::<Vec<_>>(), [busy.block_id.as_str()]);
    raw.call(Method::Ping {}).await.expect("still running");
    assert!(exists(&control, &idle.block_id).await && exists(&control, &busy.block_id).await);

    raw.call(method(true)).await.unwrap();
    timeout(T, task).await.expect("the server stopped").unwrap();
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test(flavor = "multi_thread")]
async fn server_shutdown_is_refused_until_forced() {
    stopping_the_server_is_refused_until_forced("shutdown", |force| Method::ServerShutdown { force }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_restart_is_refused_until_forced() {
    stopping_the_server_is_refused_until_forced("restart", |force| Method::ServerRestart { force }).await;
}

/// `ps`'s name for `pid`.
fn process_name(pid: i32) -> String {
    let out = std::process::Command::new("ps").args(["-o", "comm=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_program_started_just_before_the_close_is_still_caught() {
    let home = temp_home("late");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut raw = Raw::guarded(&home).await;
    let created = session(&control, &["/bin/sh", "-c", "read x; exec sleep 30"]).await;
    until_busy(&mut raw, &created.session_id, &[]).await;
    let view: BlockView = control.call(Method::BlockView { block_id: created.block_id.clone() }).await.unwrap();
    let pid = view.block.pid.expect("a running block's pid");

    control.call_value(Method::TerminalSend { block_id: created.block_id.clone(), text: "go\n".into() }).await.unwrap();
    // The moment `sleep` runs, well within one 400 ms probe interval.
    let deadline = std::time::Instant::now() + T;
    while !process_name(pid).ends_with("sleep") {
        assert!(std::time::Instant::now() < deadline, "sleep never started: {}", process_name(pid));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let busy = refused(raw.call(Method::BlockClose { block_id: created.block_id.clone(), force: false }).await);
    assert_eq!(busy[0].busy.process, "sleep");
    assert!(exists(&control, &created.block_id).await);
    stop(&control, task, home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_without_close_guard_closes_as_before() {
    let home = temp_home("old");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    assert!(control.peer_supports(feature::CLOSE_GUARD), "the server announces it");
    assert!(!compat::features().iter().any(|f| f == feature::CLOSE_GUARD), "clients opt in");
    let mut raw = Raw::guarded(&home).await;
    let created = session(&control, SLEEP).await;
    let other = session(&control, SLEEP).await;
    until_busy(&mut raw, &created.session_id, &[&created.block_id]).await;
    until_busy(&mut raw, &other.session_id, &[&other.block_id]).await;

    control.call_value(Method::BlockClose { block_id: created.block_id.clone(), force: false }).await.unwrap();
    assert!(!exists(&control, &created.block_id).await);
    let mut old = Raw::connect(&home, &[], ClientRole::Cli, None).await;
    old.call(Method::SessionClose { session_id: other.session_id.clone(), force: false }).await.unwrap();
    assert!(!exists(&control, &other.block_id).await);
    stop(&control, task, home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blocks_busy_lists_only_what_the_caller_can_view() {
    let home = temp_home("visible");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut owner = Raw::guarded(&home).await;
    let mine = session(&control, SLEEP).await;
    let theirs = session(&control, SLEEP).await;
    until_busy(&mut owner, &mine.session_id, &[&mine.block_id]).await;
    until_busy(&mut owner, &theirs.session_id, &[&theirs.block_id]).await;
    assert_eq!(owner.busy(None, None).await.len(), 2, "the owner sees the whole server");

    let grant = Method::GrantCreate {
        scope: Scope::Block { block_id: mine.block_id.clone() },
        rights: Rights::View,
        kind: GrantKind::Token,
        ttl_secs: None,
        label: None,
    };
    let created: GrantCreated = control.call(grant).await.unwrap();
    let mut viewer = Raw::connect(&home, &[feature::CLOSE_GUARD], ClientRole::Cli, Some(created.secret)).await;
    let seen: Vec<String> = viewer.busy(None, None).await.into_iter().map(|b| b.block_id).collect();
    assert_eq!(seen, std::slice::from_ref(&mine.block_id));
    let in_session: Vec<String> = viewer.busy(Some(&mine.session_id), None).await.into_iter().map(|b| b.block_id).collect();
    assert_eq!(in_session, std::slice::from_ref(&mine.block_id), "its own block of a session it cannot view");
    let other = viewer.call(Method::BlocksBusy { session_id: Some(theirs.session_id.clone()), window_id: None, block_ids: None }).await;
    assert_eq!(other.unwrap_err().code, "not_found", "a session it sees nothing of");
    let named = viewer.call(Method::BlocksBusy { session_id: None, window_id: None, block_ids: Some(vec![theirs.block_id.clone()]) }).await;
    assert_eq!(named.unwrap_err().code, "not_found");
    stop(&control, task, home).await;
}

/// The close check runs before the approval: nobody is asked to approve a
/// stop that would be refused. Forced, the stop is then approved.
#[tokio::test(flavor = "multi_thread")]
async fn a_busy_refusal_comes_before_any_approval() {
    let home = temp_home("approval");
    let task = start_with(&home, true).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut app = Raw::connect(&home, &[], ClientRole::App, None).await;
    app.call(Method::EventsSubscribe {}).await.unwrap();
    let mut cli = Raw::guarded(&home).await;
    let busy = session(&control, SLEEP).await;
    until_busy(&mut cli, &busy.session_id, &[&busy.block_id]).await;

    refused(cli.call(Method::ServerShutdown { force: false }).await);
    // Every event up to here has arrived once the ping is answered.
    app.call(Method::Ping {}).await.unwrap();
    assert!(!app.events.iter().any(|e| matches!(e, Event::ApprovalRequested { .. })), "no approval asked: {:?}", app.events);

    let forced = tokio::spawn(async move { cli.call(Method::ServerShutdown { force: true }).await });
    let id = loop {
        if let Event::ApprovalRequested { id, action, .. } = app.next_event().await {
            assert_eq!(action, "server.shutdown");
            break id;
        }
    };
    app.call(Method::ApprovalAnswer { id, allow: true }).await.unwrap();
    forced.await.unwrap().expect("approved and forced");
    timeout(T, task).await.expect("the server stopped").unwrap();
    let _ = std::fs::remove_dir_all(home);
}

/// And once more after it: a program started while the approval was
/// pending is refused once it is granted.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_started_during_the_approval_is_refused_after_it() {
    let home = temp_home("approval-after");
    let task = start_with(&home, true).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut app = Raw::connect(&home, &[], ClientRole::App, None).await;
    app.call(Method::EventsSubscribe {}).await.unwrap();
    let mut cli = Raw::guarded(&home).await;
    let created = session(&control, &["/bin/sh", "-c", "read x; exec sleep 30"]).await;
    until_busy(&mut cli, &created.session_id, &[]).await;
    let view: BlockView = control.call(Method::BlockView { block_id: created.block_id.clone() }).await.unwrap();
    let pid = view.block.pid.expect("a running block's pid");

    let stop = tokio::spawn(async move { cli.call(Method::ServerShutdown { force: false }).await });
    let id = loop {
        if let Event::ApprovalRequested { id, .. } = app.next_event().await {
            break id;
        }
    };
    control.call_value(Method::TerminalSend { block_id: created.block_id.clone(), text: "go\n".into() }).await.unwrap();
    let deadline = std::time::Instant::now() + T;
    while !process_name(pid).ends_with("sleep") {
        assert!(std::time::Instant::now() < deadline, "sleep never started: {}", process_name(pid));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    app.call(Method::ApprovalAnswer { id, allow: true }).await.unwrap();
    let busy = refused(stop.await.unwrap());
    assert_eq!(busy.iter().map(|b| b.busy.process.as_str()).collect::<Vec<_>>(), ["sleep"]);
    assert!(exists(&control, &created.block_id).await, "still running");

    app.call(Method::ServerShutdown { force: false }).await.unwrap();
    timeout(T, task).await.expect("the server stopped").unwrap();
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_an_owner_counts_its_running_parked_blocks() {
    let home = temp_home("parked");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut raw = Raw::guarded(&home).await;
    let created = session(&control, IDLE).await;
    let split = Method::BlockSplit {
        block_id: created.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: command(IDLE),
        kind: BlockKind::Agent,
        url: None,
        agent: None,
    };
    let agent: Created = control.call(split).await.unwrap();
    let spawn = Method::BlockSpawn {
        block_id: agent.block_id.clone(),
        command: command(SLEEP).command,
        cwd: None,
        env: Default::default(),
        size: None,
        kind: BlockKind::Terminal,
        url: None,
    };
    let run: Created = control.call(spawn).await.unwrap();
    until_busy(&mut raw, &created.session_id, &[&run.block_id]).await;

    let busy = refused(raw.call(Method::BlockClose { block_id: agent.block_id.clone(), force: false }).await);
    assert_eq!(busy.iter().map(|b| (b.block_id.as_str(), b.busy.process.as_str())).collect::<Vec<_>>(), [(run.block_id.as_str(), "sleep")]);
    assert_eq!(busy[0].window_id, created.window_id, "shown in its owner's window");
    assert!(exists(&control, &run.block_id).await, "nothing closed");
    raw.call(Method::BlockClose { block_id: agent.block_id.clone(), force: true }).await.unwrap();
    assert!(!exists(&control, &run.block_id).await, "closed with its owner");
    stop(&control, task, home).await;
}
