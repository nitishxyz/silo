//! End-to-end: in-process server, real PTYs, snapshot attach, resume,
//! ownership and exit over the unix socket.

use std::{path::{Path, PathBuf}, time::Duration};

use silo_client::{Applied, Control, DataStream, Events, Mirror};
use silo_proto::{
    control::{Capture, Created, Event, Method, OwnMode, SizeInfo, SpawnSpec},
    data::{ClientData, StartMode},
    model::{Axis, SessionView, TermSize},
    paths,
};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Hard limit per test; aborts the whole process even if the runtime is
/// wedged, so a hang never blocks `cargo test` forever.
const TEST_LIMIT: Duration = Duration::from_secs(90);

async fn guarded(name: &'static str, body: impl Future<Output = ()>) {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(TEST_LIMIT) {
            eprintln!("test {name} exceeded {TEST_LIMIT:?}; aborting");
            std::process::exit(101);
        }
    });
    tokio::time::timeout(TEST_LIMIT, body).await.unwrap_or_else(|_| panic!("test {name} timed out"));
    let _ = done_tx.send(());
}

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start_server(home: &Path) {
    let config = silo_server::Config { home: home.to_owned(), idle_exit: None };
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
    Control::connect(&paths::socket(home), name).await.unwrap()
}

/// Apply frames until `predicate` holds for the mirror.
async fn pump_until(stream: &mut DataStream, mirror: &mut Mirror, mut predicate: impl FnMut(&mut Mirror) -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if predicate(mirror) {
                return;
            }
            let data = stream.rx.recv().await.expect("stream closed");
            mirror.apply(data).unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; screen:\n{}", mirror.screen_text()));
}

async fn attach(control: &Control, block: &str, mirror: &mut Mirror) -> DataStream {
    let stream = DataStream::open(control, block, mirror.resume()).await.unwrap();
    mirror.begin(&stream.welcome).unwrap();
    stream
}

fn shell() -> SpawnSpec {
    SpawnSpec {
        command: vec!["/bin/sh".into()],
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
    let (control, mut events) = connect(&home, "test-a").await;
    control.subscribe().await.unwrap();
    let created: Created = control.call(Method::SessionCreate { name: Some("main".into()), spawn: shell() }).await.unwrap();
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
    let capture: Capture = control.call(Method::TerminalCapture { block_id: block.clone(), history: false }).await.unwrap();
    assert_eq!(capture.text, a.screen_text());

    // Resume after disconnect: no snapshot, missed output replayed.
    drop(stream_b);
    let resume = b.resume().unwrap();
    control.call_value(Method::TerminalSend { block_id: block.clone(), text: "echo while-away\n".into() }).await.unwrap();
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\nwhile-away\n$")).await;
    let mut stream_b = DataStream::open(&control, &block, Some(resume)).await.unwrap();
    assert_eq!(stream_b.welcome.mode, StartMode::Resume);
    b.begin(&stream_b.welcome).unwrap();
    pump_until(&mut stream_b, &mut b, |m| m.screen_text().contains("\nwhile-away\n$")).await;
    assert_eq!(a.screen_text(), b.screen_text());

    // Ownership: owner's size applies; others are remembered.
    let (control2, _events2) = connect(&home, "test-b").await;
    control.call_value(Method::TerminalResize { block_id: block.clone(), size: TermSize::new(100, 30) }).await.unwrap();
    control.call_value(Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Take }).await.unwrap();
    pump_until(&mut stream_a, &mut a, |m| m.size().columns == 100).await;
    control2.call_value(Method::TerminalResize { block_id: block.clone(), size: TermSize::new(60, 20) }).await.unwrap();
    control2.call_value(Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Request }).await.unwrap();
    let info: SizeInfo = control.call(Method::TerminalSize { block_id: block.clone() }).await.unwrap();
    assert_eq!((info.size.columns, info.size.rows), (100, 30));
    assert_eq!(info.owner, Some(control.client_id()));
    assert_eq!(info.desired.len(), 2);
    control2.call_value(Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Take }).await.unwrap();
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
    let split: Created = control.call(Method::BlockSplit { block_id: block.clone(), axis: Axis::Horizontal, spawn: shell() }).await.unwrap();
    let view: SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    assert_eq!(view.session.windows[0].layout.blocks(), [block.clone(), split.block_id.clone()]);
    assert_eq!(view.blocks.len(), 2);
    control.call_value(Method::BlockClose { block_id: split.block_id.clone() }).await.unwrap();

    // Exit: stream reports it and the session disappears.
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
    assert_eq!(code, 7);
    tokio::time::timeout(TIMEOUT, async {
        while let Some((_, event)) = events.recv().await {
            if let Event::SessionClosed { session_id } = event {
                assert_eq!(session_id, created.session_id);
                return;
            }
        }
        panic!("events closed");
    })
    .await
    .unwrap();
    let sessions: Vec<serde_json::Value> = control.call(Method::SessionList {}).await.unwrap();
    assert!(sessions.is_empty());
    control.call_value(Method::ServerShutdown {}).await.unwrap();
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
    let created: Created = control.call(Method::SessionCreate { name: None, spawn }).await.unwrap();
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
    control.call_value(Method::SessionClose { session_id: created.session_id }).await.unwrap();
    control.call_value(Method::ServerShutdown {}).await.unwrap();
}
