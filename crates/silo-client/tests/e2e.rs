//! End-to-end: in-process server, real PTYs, snapshot attach, resume,
//! ownership and exit over the unix socket.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::de::DeserializeOwned;
use silo_client::{Applied, Control, DataStream, Events, Mirror};
use silo_proto::{
    control::{Capture, Created, Event, Method, OwnMode, SizeInfo, SpawnSpec},
    data::{ClientData, Resume, StartMode},
    model::{Axis, BlockKind, Lifecycle, SessionView, TermSize, TerminalConfig},
    paths,
};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for one connect, call or attach: a hang fails fast,
/// naming the step.
const STEP_LIMIT: Duration = Duration::from_secs(15);
/// Hard limit per test; aborts the whole process even if the runtime is
/// wedged, so a hang never blocks `cargo test` forever.
const TEST_LIMIT: Duration = Duration::from_secs(90);
/// Longest wait for each query of a failure report.
const REPORT_LIMIT: Duration = Duration::from_secs(3);

/// Where a test is, for its failure report.
struct Probe {
    name: &'static str,
    home: Mutex<Option<PathBuf>>,
    block: Mutex<Option<String>>,
    step: Mutex<String>,
}

tokio::task_local! {
    static PROBE: Arc<Probe>;
}

fn probe() -> Option<Arc<Probe>> {
    PROBE.try_with(Arc::clone).ok()
}

async fn guarded(name: &'static str, body: impl Future<Output = ()>) {
    capture_logs();
    let probe = Arc::new(Probe { name, home: Mutex::default(), block: Mutex::default(), step: Mutex::new("start".into()) });
    let watched = probe.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(TEST_LIMIT) {
            // The test's runtime may be wedged: query from a runtime of our own.
            let report = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_or_else(|e| format!("no runtime for a report: {e}"), |rt| rt.block_on(report(&watched)));
            eprintln!("test {name} exceeded {TEST_LIMIT:?}; aborting\n{report}");
            std::process::exit(101);
        }
    });
    PROBE
        .scope(probe.clone(), async {
            if tokio::time::timeout(TEST_LIMIT, body).await.is_err() {
                panic!("test {name} timed out\n{}", report(&probe).await);
            }
        })
        .await;
    let _ = done_tx.send(());
}

/// Run one step of a test, failing after [`STEP_LIMIT`] with a report.
async fn step<T>(what: impl Into<String>, fut: impl Future<Output = T>) -> T {
    let what = what.into();
    if let Some(probe) = probe() {
        what.clone_into(&mut probe.step.lock().unwrap());
    }
    match tokio::time::timeout(STEP_LIMIT, fut).await {
        Ok(value) => value,
        Err(_) => fail(&format!("stuck {STEP_LIMIT:?} at {what}")).await,
    }
}

/// The step a test is at, the server log tail and the attached block's
/// state, queried over a fresh connection.
async fn report(probe: &Probe) -> String {
    use std::fmt::Write;
    let mut out = format!("--- {} report; last step: {}\n", probe.name, probe.step.lock().unwrap());
    let home = probe.home.lock().unwrap().clone();
    let block = probe.block.lock().unwrap().clone();
    if let (Some(home), Some(block)) = (home, block) {
        let _ = writeln!(out, "block {block}:");
        match tokio::time::timeout(REPORT_LIMIT, Control::connect(&paths::socket(&home), "e2e-report")).await {
            Ok(Ok((control, _))) => {
                let queries = [
                    Method::BlockView { block_id: block.clone() },
                    Method::TerminalSize { block_id: block.clone() },
                    Method::TerminalCapture { block_id: block.clone(), history: false },
                ];
                for query in queries {
                    let name = method_name(&query);
                    let result = match tokio::time::timeout(REPORT_LIMIT, control.call_value(query)).await {
                        Ok(Ok(value)) => value.to_string(),
                        Ok(Err(e)) => format!("error: {e:#}"),
                        Err(_) => format!("no answer in {REPORT_LIMIT:?} (block actor stuck?)"),
                    };
                    let _ = writeln!(out, "  {name}: {result}");
                }
            }
            Ok(Err(e)) => _ = writeln!(out, "  connect: {e:#}"),
            Err(_) => _ = writeln!(out, "  connect: no answer in {REPORT_LIMIT:?} (server stuck?)"),
        }
    }
    let _ = writeln!(out, "server log tail (every test in this process):");
    for line in log_tail(80) {
        let _ = writeln!(out, "  {line}");
    }
    out
}

/// Server and client logs of this process, newest last.
static LOGS: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
const LOG_LINES: usize = 2000;

struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut logs = LOGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for line in String::from_utf8_lossy(buf).lines().filter(|l| !l.is_empty()) {
            if logs.len() == LOG_LINES {
                logs.pop_front();
            }
            logs.push_back(line.to_owned());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::new("warn,silo_server=debug,silo_client=debug,silo_pty=debug");
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(|| LogWriter).try_init();
    });
}

fn log_tail(lines: usize) -> Vec<String> {
    let logs = LOGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    logs.iter().skip(logs.len().saturating_sub(lines)).cloned().collect()
}

fn method_name(method: &Method) -> String {
    format!("{method:?}").split([' ', '{', '(']).next().unwrap_or_default().to_owned()
}

async fn call<T: DeserializeOwned>(control: &Control, method: Method) -> T {
    let what = format!("call {}", method_name(&method));
    match step(&what, control.call(method)).await {
        Ok(value) => value,
        Err(e) => fail(&format!("{what}: {e:#}")).await,
    }
}

async fn call_value(control: &Control, method: Method) -> serde_json::Value {
    let what = format!("call {}", method_name(&method));
    match step(&what, control.call_value(method)).await {
        Ok(value) => value,
        Err(e) => fail(&format!("{what}: {e:#}")).await,
    }
}

async fn fail(message: &str) -> ! {
    let report = match probe() {
        Some(probe) => report(&probe).await,
        None => String::new(),
    };
    panic!("{message}\n{report}");
}

/// A call expected to fail.
async fn call_err(control: &Control, method: Method) -> anyhow::Error {
    let what = format!("call {}", method_name(&method));
    step(&what, control.call_value(method)).await.expect_err(&what)
}

/// Open a data stream on `block`.
async fn open(control: &Control, block: &str, resume: Option<Resume>) -> DataStream {
    if let Some(probe) = probe() {
        *probe.block.lock().unwrap() = Some(block.to_owned());
    }
    let what = format!("open data stream on {block}");
    match step(&what, DataStream::open(control, block, resume)).await {
        Ok(stream) => stream,
        Err(e) => fail(&format!("{what}: {e:#}")).await,
    }
}

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start_server(home: &Path) {
    if let Some(probe) = probe() {
        *probe.home.lock().unwrap() = Some(home.to_owned());
    }
    let config =
        silo_server::Config { home: home.to_owned(), idle_exit: None, relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
    tokio::spawn(async move {
        silo_server::run(config).await.unwrap();
    });
    for _ in 0..200 {
        if silo_client::server_running(home).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path, name: &str) -> (Control, Events) {
    step(format!("connect {name}"), Control::connect(&paths::socket(home), name)).await.unwrap()
}

const CHILD_HOME: &str = "SILO_TEST_CHILD_HOME";

/// The server process for `heartbeat_detects_stopped_server`; a no-op
/// unless that test runs this binary with [`CHILD_HOME`] set.
#[tokio::test]
async fn heartbeat_server_child() {
    let Some(home) = std::env::var_os(CHILD_HOME) else {
        return;
    };
    let config =
        silo_server::Config { home: home.into(), idle_exit: None, relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
    silo_server::run(config).await.unwrap();
}

/// A server child process, killed on drop.
struct ServerProcess(std::process::Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        // Resume a stopped server so it can handle SIGTERM and shut down cleanly.
        if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::CONT);
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        let _ = self.0.wait();
    }
}

impl ServerProcess {
    fn signal(&self, signal: &str) {
        let status = std::process::Command::new("kill").arg(signal).arg(self.0.id().to_string()).status().unwrap();
        assert!(status.success());
    }
}

/// Apply frames until `predicate` holds for the mirror.
async fn pump_until(stream: &mut DataStream, mirror: &mut Mirror, mut predicate: impl FnMut(&mut Mirror) -> bool) {
    if let Some(probe) = probe() {
        "pump_until".clone_into(&mut probe.step.lock().unwrap());
    }
    let pumped = tokio::time::timeout(TIMEOUT, async {
        loop {
            if predicate(mirror) {
                return;
            }
            let data = stream.rx.recv().await.expect("stream closed");
            mirror.apply(data).unwrap();
        }
    })
    .await;
    if pumped.is_err() {
        fail(&format!("timed out; screen:\n{}", mirror.screen_text())).await;
    }
}

async fn attach(control: &Control, block: &str, mirror: &mut Mirror) -> DataStream {
    let stream = open(control, block, mirror.resume()).await;
    mirror.begin(&stream.welcome).unwrap();
    stream
}

/// An interactive bash without user rc files. Not `/bin/sh`: dash (Debian)
/// has no line editor, so the tty echoes injected control input (the `^L`
/// sent by clear/reset) into the screen and the shell's next line.
fn shell() -> SpawnSpec {
    SpawnSpec {
        command: vec!["/bin/bash".into(), "--norc".into(), "--noprofile".into()],
        cwd: Some("/tmp".into()),
        size: Some(TermSize::new(80, 24)),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_snapshot_resume_ownership_exit() {
    guarded("attach_snapshot_resume_ownership_exit", attach_snapshot_resume_ownership_exit_body()).await;
}

async fn attach_snapshot_resume_ownership_exit_body() {
    let home = temp_home("e2e");
    start_server(&home).await;
    let (control, events) = connect(&home, "test-a").await;
    step("subscribe", control.subscribe()).await.unwrap();
    let created: Created =
        call(&control, Method::SessionCreate { name: Some("main".into()), spawn: shell(), kind: BlockKind::Terminal, url: None }).await;
    let block = created.block_id.clone();

    // Live attach + input + output.
    let mut a = Mirror::new();
    let mut stream_a = attach(&control, &block, &mut a).await;
    assert_eq!(stream_a.welcome.mode, StartMode::Snapshot);
    stream_a.send(&ClientData::Input(b"PS1='$ '; echo hello-$((1+2))\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("hello-3")).await;

    // History, then a second client attaches from a snapshot.
    stream_a.send(&ClientData::Input(b"i=0; while [ $i -lt 3000 ]; do i=$((i+1)); echo line-$i; done; echo done-loop\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\ndone-loop\n$")).await;
    let mut b = Mirror::new();
    let mut stream_b = attach(&control, &block, &mut b).await;
    assert_eq!(stream_b.welcome.mode, StartMode::Snapshot);
    pump_until(&mut stream_b, &mut b, |m| m.screen_text().contains("\ndone-loop\n$")).await;
    // Drain the snapshot history.
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(data) = stream_b.rx.try_recv() {
        b.apply(data).unwrap();
    }
    assert_eq!(a.screen_text(), b.screen_text());
    let scroll_a = a.terminal().unwrap().scroll_state().unwrap();
    let scroll_b = b.terminal().unwrap().scroll_state().unwrap();
    assert_eq!(scroll_a.total, scroll_b.total, "history rows match");
    assert!(scroll_b.total > 3000);
    let capture: Capture = call(&control, Method::TerminalCapture { block_id: block.clone(), history: false }).await;
    assert_eq!(capture.text, a.screen_text());

    // Resume after disconnect: no snapshot, missed output replayed.
    drop(stream_b);
    let resume = b.resume().unwrap();
    call_value(&control, Method::TerminalSend { block_id: block.clone(), text: "echo while-away\n".into() }).await;
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\nwhile-away\n$")).await;
    let mut stream_b = open(&control, &block, Some(resume)).await;
    assert_eq!(stream_b.welcome.mode, StartMode::Resume);
    b.begin(&stream_b.welcome).unwrap();
    pump_until(&mut stream_b, &mut b, |m| m.screen_text().contains("\nwhile-away\n$")).await;
    assert_eq!(a.screen_text(), b.screen_text());

    // Ownership: owner's size applies; others are remembered.
    let (control2, _events2) = connect(&home, "test-b").await;
    call_value(&control, Method::TerminalResize { block_id: block.clone(), size: TermSize::new(100, 30) }).await;
    call_value(&control, Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Take }).await;
    pump_until(&mut stream_a, &mut a, |m| m.size().columns == 100).await;
    call_value(&control2, Method::TerminalResize { block_id: block.clone(), size: TermSize::new(60, 20) }).await;
    call_value(&control2, Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Request }).await;
    let info: SizeInfo = call(&control, Method::TerminalSize { block_id: block.clone() }).await;
    assert_eq!((info.size.columns, info.size.rows), (100, 30));
    assert_eq!(info.owner, Some(control.client_id()));
    assert_eq!(info.desired.len(), 2);
    call_value(&control2, Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Take }).await;
    pump_until(&mut stream_a, &mut a, |m| m.size().columns == 60).await;
    stream_a.send(&ClientData::Input(b"stty size\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\n20 60\n$")).await;
    drop(control2);
    // Owner left: falls back to the remaining candidate.
    pump_until(&mut stream_a, &mut a, |m| m.size().columns == 100).await;
    let a_offset = a.resume().unwrap().offset;
    pump_until(&mut stream_b, &mut b, |m| m.size().columns == 100 && m.resume().unwrap().offset >= a_offset).await;
    assert_eq!(a.screen_text(), b.screen_text());

    // Splits and windows.
    let split: Created = call(
        &control,
        Method::BlockSplit { block_id: block.clone(), axis: Axis::Horizontal, spawn: shell(), kind: BlockKind::Terminal, url: None, agent: None },
    )
    .await;
    let view: SessionView = call(&control, Method::SessionView { session_id: created.session_id.clone() }).await;
    assert_eq!(view.session.windows[0].layout.blocks(), [block.clone(), split.block_id.clone()]);
    assert_eq!(view.blocks.len(), 2);
    call_value(&control, Method::BlockClose { block_id: split.block_id.clone(), force: false }).await;

    // Exit: stream reports it; the block stays readable in its layout.
    stream_a.send(&ClientData::Input(b"exit 7\n".to_vec()));
    let code = tokio::time::timeout(TIMEOUT, async {
        loop {
            match stream_a.rx.recv().await {
                Some(data) => {
                    if let Applied::Exited(code) = a.apply(data).unwrap() {
                        return code;
                    }
                }
                None => panic!("stream closed before exit"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(code, Some(7));
    let info = tokio::time::timeout(TIMEOUT, async {
        loop {
            let view: SessionView = call(&control, Method::SessionView { session_id: created.session_id.clone() }).await;
            let info = view.blocks.into_iter().find(|b| b.id == block).expect("exited block stays in the session");
            if info.lifecycle == Lifecycle::Exited {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(info.exited, Some(7));
    let view: SessionView = call(&control, Method::SessionView { session_id: created.session_id.clone() }).await;
    assert_eq!(view.session.windows[0].layout.blocks(), std::slice::from_ref(&block));
    // Still attachable: snapshot, then the exit status.
    let mut c = Mirror::new();
    let mut stream_c = attach(&control, &block, &mut c).await;
    assert_eq!(stream_c.welcome.mode, StartMode::Snapshot);
    pump_until(&mut stream_c, &mut c, |m| m.exited().is_some() && m.screen_text().contains("exit 7")).await;
    assert_eq!(c.exited(), Some(Some(7)));
    drop(events);
    call_value(&control, Method::ServerShutdown { force: false }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn block_swap_and_window_move() {
    guarded("block_swap_and_window_move", block_swap_and_window_move_body()).await;
}

/// Next `session.changed` view for `session`.
async fn next_view(events: &mut Events, session: &str) -> SessionView {
    tokio::time::timeout(TIMEOUT, async {
        while let Some((_, event)) = events.recv().await {
            if let Event::SessionChanged { view } = event
                && view.session.id == session
            {
                return view;
            }
        }
        panic!("events closed");
    })
    .await
    .unwrap()
}

async fn block_swap_and_window_move_body() {
    let home = temp_home("swap");
    start_server(&home).await;
    let (control, _) = connect(&home, "test-a").await;
    let (observer, mut events) = connect(&home, "test-b").await;
    step("subscribe", observer.subscribe()).await.unwrap();
    let created: Created = call(&control, Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await;
    let sid = created.session_id.clone();
    let a = created.block_id.clone();
    let b: Created = call(
        &control,
        Method::BlockSplit { block_id: a.clone(), axis: Axis::Horizontal, spawn: shell(), kind: BlockKind::Terminal, url: None, agent: None },
    )
    .await;
    let c: Created = call(
        &control,
        Method::BlockSplit { block_id: b.block_id.clone(), axis: Axis::Vertical, spawn: shell(), kind: BlockKind::Terminal, url: None, agent: None },
    )
    .await;
    let (b, c) = (b.block_id, c.block_id);
    call_value(&control, Method::LayoutRatio { window_id: created.window_id.clone(), path: vec![], ratio: 0.3 }).await;
    call_value(&control, Method::BlockFocus { block_id: a.clone() }).await;
    let before: SessionView = call(&control, Method::SessionView { session_id: sid.clone() }).await;
    while next_view(&mut events, &sid).await.session.revision < before.session.revision {}

    // Swap: leaves exchange, ratios and focus stay, revision bumps, event.
    call_value(&control, Method::BlockSwap { block_id: a.clone(), other_block_id: c.clone() }).await;
    let view = next_view(&mut events, &sid).await;
    assert_eq!(view.session.revision, before.session.revision + 1);
    let layout = &view.session.windows[0].layout;
    assert_eq!(layout.blocks(), [c.clone(), b.clone(), a.clone()]);
    assert!(matches!(layout, silo_proto::model::Layout::Split { ratio, .. } if (*ratio - 0.3).abs() < 1e-6));
    assert_eq!(view.session.windows[0].focused, a);
    // The shells keep running.
    let capture: Capture = call(&control, Method::TerminalCapture { block_id: a.clone(), history: false }).await;
    assert!(capture.size.columns > 0);
    assert_eq!(view.blocks.iter().filter(|b| b.exited.is_none()).count(), 3);

    // Cross-window swaps are rejected and change nothing.
    let other: Created = call(&control, Method::WindowCreate { session_id: sid.clone(), spawn: shell(), kind: BlockKind::Terminal, url: None }).await;
    let after_create = next_view(&mut events, &sid).await;
    let err = call_err(&control, Method::BlockSwap { block_id: a.clone(), other_block_id: other.block_id.clone() }).await;
    assert!(err.to_string().contains("different windows"), "{err}");
    let err = call_err(&control, Method::BlockSwap { block_id: a.clone(), other_block_id: "bmissing".into() }).await;
    assert!(err.to_string().contains("not_found"), "{err}");
    let unchanged: SessionView = call(&control, Method::SessionView { session_id: sid.clone() }).await;
    assert_eq!(unchanged.session.revision, after_create.session.revision);

    // window.move: order changes for every client.
    let third: Created = call(&control, Method::WindowCreate { session_id: sid.clone(), spawn: shell(), kind: BlockKind::Terminal, url: None }).await;
    next_view(&mut events, &sid).await;
    call_value(&control, Method::WindowMove { window_id: third.window_id.clone(), index: 0 }).await;
    let view = next_view(&mut events, &sid).await;
    let order: Vec<_> = view.session.windows.iter().map(|w| w.id.clone()).collect();
    assert_eq!(order, [third.window_id.clone(), created.window_id.clone(), other.window_id.clone()]);
    call_value(&control, Method::WindowMove { window_id: third.window_id.clone(), index: 99 }).await;
    let view = next_view(&mut events, &sid).await;
    let order: Vec<_> = view.session.windows.iter().map(|w| w.id.clone()).collect();
    assert_eq!(order, [created.window_id.clone(), other.window_id.clone(), third.window_id.clone()]);
    call_value(&control, Method::ServerShutdown { force: false }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_mid_escape_and_alt_screen() {
    guarded("snapshot_mid_escape_and_alt_screen", snapshot_mid_escape_and_alt_screen_body()).await;
}

async fn snapshot_mid_escape_and_alt_screen_body() {
    let home = temp_home("alt");
    start_server(&home).await;
    let (control, _events) = connect(&home, "test").await;
    let spawn = SpawnSpec {
        // Enter the alt screen, draw, then leave an SGR unfinished and block.
        command: vec!["/bin/sh".into(), "-c".into(), "printf 'base\\n\\033[?1049h\\033[2;5Halt-screen\\033[3'; sleep 30".into()],
        cwd: None,
        size: Some(TermSize::new(40, 10)),
    };
    let created: Created = call(&control, Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await;
    let mut a = Mirror::new();
    let mut stream = attach(&control, &created.block_id, &mut a).await;
    pump_until(&mut stream, &mut a, |m| m.screen_text().contains("alt-screen")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // A fresh client restores the alt screen and the unfinished sequence.
    let mut b = Mirror::new();
    let mut stream_b = attach(&control, &created.block_id, &mut b).await;
    pump_until(&mut stream_b, &mut b, |m| m.screen_text().contains("alt-screen")).await;
    assert_eq!(a.screen_text(), b.screen_text());
    assert!(!b.screen_text().contains("base"));
    call_value(&control, Method::SessionClose { session_id: created.session_id, force: false }).await;
    call_value(&control, Method::ServerShutdown { force: false }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_and_reset_reach_every_client() {
    guarded("clear_and_reset_reach_every_client", clear_and_reset_body()).await;
}

async fn clear_and_reset_body() {
    let home = temp_home("clear");
    start_server(&home).await;
    let (control, _events) = connect(&home, "test").await;
    let created: Created = call(&control, Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await;
    // Generated default names are two lowercase words.
    let sessions: Vec<silo_proto::model::SessionSummary> = call(&control, Method::SessionList {}).await;
    assert!(sessions[0].name.split('-').count() == 2, "{}", sessions[0].name);
    let block = created.block_id.clone();
    let mut a = Mirror::new();
    let mut stream = attach(&control, &block, &mut a).await;
    stream.send(&ClientData::Input(b"PS1='$ '; i=0; while [ $i -lt 100 ]; do i=$((i+1)); echo old-$i; done; echo end-old\n".to_vec()));
    pump_until(&mut stream, &mut a, |m| m.screen_text().contains("\nend-old\n$")).await;
    assert!(a.terminal().unwrap().scroll_state().unwrap().total > 50);
    call_value(&control, Method::TerminalClear { block_id: block.clone() }).await;
    pump_until(&mut stream, &mut a, |m| !m.screen_text().contains("old-")).await;
    pump_until(&mut stream, &mut a, |m| m.terminal().unwrap().scroll_state().unwrap().total < 30).await;
    let capture: Capture = call(&control, Method::TerminalCapture { block_id: block.clone(), history: true }).await;
    assert!(!capture.text.contains("old-"), "{}", capture.text);
    // Reset: alt screen is left and the screen cleared for everyone. Reset
    // only at the prompt: earlier, the rest of the command's output (echo's
    // newline, the prompt) lands after the reset.
    stream.send(&ClientData::Input(b"printf '\\033[?1049hALT'; echo\n".to_vec()));
    pump_until(&mut stream, &mut a, |m| m.screen_text().contains("ALT\n$")).await;
    call_value(&control, Method::TerminalReset { block_id: block.clone() }).await;
    // The shell redraws its prompt alone on the cleared screen (^L).
    pump_until(&mut stream, &mut a, |m| m.screen_text() == "$").await;
    // A snapshot client shows the same screen at the same offset.
    let mut b = Mirror::new();
    let mut stream_b = attach(&control, &block, &mut b).await;
    let a_offset = a.resume().unwrap().offset;
    pump_until(&mut stream_b, &mut b, |m| m.resume().is_some_and(|r| r.offset >= a_offset)).await;
    let b_offset = b.resume().unwrap().offset;
    pump_until(&mut stream, &mut a, |m| m.resume().is_some_and(|r| r.offset >= b_offset)).await;
    assert_eq!(a.resume(), b.resume());
    assert_eq!(a.screen_text(), b.screen_text());
    call_value(&control, Method::ServerShutdown { force: false }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_terminal_config_reaches_server_and_every_client() {
    guarded("owner_terminal_config", owner_terminal_config_body()).await;
}

fn config(background: u32) -> TerminalConfig {
    TerminalConfig { background, ..TerminalConfig::default() }
}

fn background(m: &mut Mirror) -> u32 {
    m.terminal().unwrap().frame().unwrap().background
}

/// Ask the server terminal for its background (OSC 11) and wait for the
/// reply, echoed by `cat -v`, to show `rgb`. `read` ends at the reply's
/// BEL, however late it comes (a read timeout let a slow reply reach the
/// shell's line editor instead), and not on `^C`, whose input flush
/// (Linux) would drop the next command.
async fn query_background(stream: &mut DataStream, m: &mut Mirror, rgb: &str) {
    stream.send(&ClientData::Input(
        b"clear; stty -echo -icanon min 1 time 0; printf '\\033]11;?\\007'; IFS= read -r -d $'\\a' reply; stty sane; printf '%s\\n' \"$reply\" | cat -v; echo query-done\n"
            .to_vec(),
    ));
    let want = format!("11;rgb:{rgb}");
    pump_until(stream, m, |m| {
        let text = m.screen_text();
        text.contains(&want) && text.contains("\nquery-done\n$")
    })
    .await;
    stream.send(&ClientData::Input(b"clear\n".to_vec()));
}

async fn owner_terminal_config_body() {
    let home = temp_home("config");
    start_server(&home).await;
    let (control, _events) = connect(&home, "test-a").await;
    let created: Created = call(&control, Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await;
    let block = created.block_id.clone();
    let red = config(0x0010_2030);
    let blue = config(0x0000_0040);

    // A owns the block: its config becomes effective on the server terminal
    // (OSC 11 answers with it) and is announced to its mirror.
    let mut a = Mirror::new();
    let mut stream_a = attach(&control, &block, &mut a).await;
    pump_until(&mut stream_a, &mut a, |m| m.config() == Some(TerminalConfig::default())).await;
    stream_a.send(&ClientData::Config(red));
    call_value(&control, Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Take }).await;
    pump_until(&mut stream_a, &mut a, |m| m.config() == Some(red)).await;
    assert_eq!(background(&mut a), red.background);
    stream_a.send(&ClientData::Input(b"PS1='$ '\n".to_vec()));
    query_background(&mut stream_a, &mut a, "1010/2020/3030").await;

    // B attaches with its own config but does not own: the server keeps
    // A's, yet B renders with its own colours once it sets them locally.
    let (control_b, _events_b) = connect(&home, "test-b").await;
    let mut b = Mirror::new();
    let mut stream_b = attach(&control_b, &block, &mut b).await;
    stream_b.send(&ClientData::Config(blue));
    call_value(&control_b, Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Request }).await;
    pump_until(&mut stream_b, &mut b, |m| m.config() == Some(red) && m.terminal().is_some()).await;
    assert_eq!(background(&mut b), red.background);
    b.set_local_config(blue);
    assert_eq!(background(&mut b), blue.background);
    assert_eq!((b.config(), b.render_config()), (Some(red), Some(blue)));

    // B takes over: the server (and A, rendering the server's) switches to
    // B's config.
    call_value(&control_b, Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Take }).await;
    pump_until(&mut stream_a, &mut a, |m| m.config() == Some(blue)).await;
    pump_until(&mut stream_b, &mut b, |m| m.config() == Some(blue)).await;
    assert_eq!(background(&mut a), blue.background);
    query_background(&mut stream_b, &mut b, "0000/0000/4040").await;

    // Typing makes A the driver again while B keeps the size.
    stream_a.send(&ClientData::Input(b"true\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| m.config() == Some(red)).await;
    pump_until(&mut stream_b, &mut b, |m| m.config() == Some(red)).await;
    assert_eq!(background(&mut b), blue.background, "B keeps rendering its own");

    // B leaves: A's config stays effective.
    drop(stream_b);
    drop(control_b);
    pump_until(&mut stream_a, &mut a, |m| m.config() == Some(red)).await;
    query_background(&mut stream_a, &mut a, "1010/2020/3030").await;

    // A config change while the program is mid-escape waits for the ground
    // state, so the mirror switches between the same bytes the server did.
    // The escape stays open until the test answers `read`, sent after the
    // config on the same (ordered) stream: ground follows well within the
    // server's bounded wait, however slow the host is to start processes.
    stream_a.send(&ClientData::Input(b"stty -echo; printf '\\033[3'; read -r _; printf '1mX\\033[0m\\n'; stty echo\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| !m.terminal().unwrap().at_ground().unwrap()).await;
    stream_a.send(&ClientData::Config(blue));
    stream_a.send(&ClientData::Input(b"\n".to_vec()));
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let data = stream_a.rx.recv().await.expect("stream closed");
            if matches!(data, silo_proto::data::ServerData::Configured(_)) {
                assert!(a.terminal().unwrap().at_ground().unwrap(), "config applied mid-escape");
                a.apply(data).unwrap();
                return;
            }
            a.apply(data).unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(a.config(), Some(blue));

    // Program overrides (OSC 11) survive a config change on every side.
    stream_a.send(&ClientData::Input(b"printf '\\033]11;#123456\\007'; echo set-bg\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\nset-bg")).await;
    stream_a.send(&ClientData::Config(red));
    pump_until(&mut stream_a, &mut a, |m| m.config() == Some(red)).await;
    assert_eq!(background(&mut a), 0x0012_3456);
    query_background(&mut stream_a, &mut a, "1212/3434/5656").await;
    call_value(&control, Method::ServerShutdown { force: false }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_across_config_change_keeps_byte_order() {
    guarded("resume_across_config_change", resume_across_config_change_body()).await;
}

async fn resume_across_config_change_body() {
    let home = temp_home("config-resume");
    start_server(&home).await;
    let (control, _events) = connect(&home, "test-a").await;
    let created: Created = call(&control, Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await;
    let block = created.block_id.clone();
    let red = config(0x0010_2030);
    let blue = config(0x0000_0040);

    let mut a = Mirror::new();
    let mut stream_a = attach(&control, &block, &mut a).await;
    stream_a.send(&ClientData::Config(red));
    call_value(&control, Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Take }).await;
    pump_until(&mut stream_a, &mut a, |m| m.config() == Some(red)).await;
    stream_a.send(&ClientData::Input(b"PS1='$ '; echo ready-$((0+1))\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\nready-1\n$")).await;

    let (control_b, _events_b) = connect(&home, "test-b").await;
    let mut b = Mirror::new();
    let mut stream_b = attach(&control_b, &block, &mut b).await;
    let a_offset = a.resume().unwrap().offset;
    pump_until(&mut stream_b, &mut b, |m| m.config() == Some(red) && m.resume().is_some_and(|r| r.offset >= a_offset)).await;

    // B goes away. Meanwhile the program sets an OSC 11 override, prints,
    // and is mid-escape when the owner switches theme; more output follows.
    drop(stream_b);
    let resume = b.resume().unwrap();
    stream_a.send(&ClientData::Input(
        b"printf '\\033]11;#123456\\007'; echo before-$((1+1)); printf '\\033[3'; sleep 0.1; printf '1mX\\033[0m\\n'; sleep 0.2; echo after-$((2+2))\n".to_vec(),
    ));
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\nbefore-2")).await;
    pump_until(&mut stream_a, &mut a, |m| !m.terminal().unwrap().at_ground().unwrap()).await;
    stream_a.send(&ClientData::Config(blue));
    pump_until(&mut stream_a, &mut a, |m| m.config() == Some(blue) && m.screen_text().contains("\nafter-4\n$")).await;

    // Resume replays the old config first, then switches exactly where the
    // live stream did: after the earlier output, outside the escape.
    let mut stream_b = open(&control_b, &block, Some(resume)).await;
    assert_eq!(stream_b.welcome.mode, StartMode::Resume);
    b.begin(&stream_b.welcome).unwrap();
    let mut configs = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        while !b.screen_text().contains("\nafter-4\n$") {
            let data = stream_b.rx.recv().await.expect("stream closed");
            if let silo_proto::data::ServerData::Configured(config) = data {
                if config == blue {
                    let text = b.screen_text();
                    assert!(text.contains("\nbefore-2"), "switched before older output:\n{text}");
                    assert!(!text.contains("after-4"), "switched after newer output:\n{text}");
                    assert!(b.terminal().unwrap().at_ground().unwrap(), "config applied mid-escape");
                }
                configs.push(config);
            }
            b.apply(data).unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; screen:\n{}", b.screen_text()));
    assert_eq!(configs, vec![red, blue]);
    assert_eq!(b.config(), Some(blue));
    assert_eq!(background(&mut b), 0x0012_3456, "program override survives");
    assert_eq!(a.screen_text(), b.screen_text());
    call_value(&control, Method::ServerShutdown { force: false }).await;
}

#[tokio::test]
async fn heartbeat_detects_stopped_server() {
    guarded("heartbeat_detects_stopped_server", heartbeat_body()).await;
}

async fn heartbeat_body() {
    let home = temp_home("heartbeat");
    let server = ServerProcess(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["heartbeat_server_child", "--exact", "--nocapture"])
            .env(CHILD_HOME, &home)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(TIMEOUT, async {
        while !silo_client::server_running(&home).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("server did not start");
    let (control, _events) = connect(&home, "heartbeat").await;
    let heartbeat = tokio::spawn(control.clone().heartbeat());
    server.signal("-STOP");
    let limit = 2 * control.ping_timeout() + silo_client::PING_EVERY + Duration::from_secs(1);
    tokio::time::timeout(limit, heartbeat).await.expect("heartbeat did not fire").unwrap();
    tokio::time::timeout(TIMEOUT, control.closed()).await.expect("not closed");
    assert!(call_err(&control, Method::Ping {}).await.is::<silo_client::Closed>());

    server.signal("-CONT");
    let (again, _events) = connect(&home, "heartbeat").await;
    call_value(&again, Method::Ping {}).await;
    drop(server);
    let _ = std::fs::remove_dir_all(&home);
}
