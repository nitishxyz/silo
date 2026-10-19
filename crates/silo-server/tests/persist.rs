//! Durable state: the session tree and block checkpoints survive clean
//! shutdowns and crashes and come back as restored blocks.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::Control;
use silo_proto::{
    control::{Capture, Created, Method, SpawnSpec},
    model::{Axis, BlockInfo, BlockKind, Float, FloatRect, Lifecycle, SessionView, TermSize},
    paths,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);
const INTERVAL: Duration = Duration::from_millis(200);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: INTERVAL,
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        ..Default::default()
    };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..300 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path) -> Control {
    Control::connect(&paths::socket(home), "persist-test").await.unwrap().0
}

/// Clean shutdown: request it and wait for `run` to return.
async fn shutdown(control: Control, task: JoinHandle<()>) {
    control.call_value(Method::ServerShutdown {}).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap();
}

/// Crash: drop the server future without its shutdown path.
async fn crash(control: Control, task: JoinHandle<()>) {
    drop(control);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

fn shell() -> SpawnSpec {
    SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) }
}

async fn send(control: &Control, block: &str, text: &str) {
    control.call_value(Method::TerminalSend { block_id: block.into(), text: text.into() }).await.unwrap();
}

async fn capture(control: &Control, block: &str) -> String {
    let capture: Capture = control.call(Method::TerminalCapture { block_id: block.into(), history: true }).await.unwrap();
    capture.text
}

async fn wait_for(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_output(control: &Control, block: &str, needle: &str) {
    wait_for(needle, async || capture(control, block).await.contains(needle)).await;
}

async fn view(control: &Control, session: &str) -> SessionView {
    control.call(Method::SessionView { session_id: session.into() }).await.unwrap()
}

fn block<'a>(view: &'a SessionView, id: &str) -> &'a BlockInfo {
    view.blocks.iter().find(|b| b.id == id).unwrap_or_else(|| panic!("block {id} missing"))
}

fn snapshot_dir(home: &Path, block: &str) -> PathBuf {
    home.join("state").join("snapshots").join(block)
}

/// One session with a split and a second window, each block with output.
async fn populate(control: &Control) -> (Created, Created, Created) {
    let main: Created =
        control.call(Method::SessionCreate { name: Some("main".into()), spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split: Created = control
        .call(Method::BlockSplit { block_id: main.block_id.clone(), axis: Axis::Horizontal, spawn: shell(), kind: BlockKind::Terminal, url: None })
        .await
        .unwrap();
    let window: Created = control
        .call(Method::WindowCreate { session_id: main.session_id.clone(), spawn: shell(), kind: BlockKind::Terminal, url: None })
        .await
        .unwrap();
    control.call_value(Method::WindowRename { window_id: window.window_id.clone(), name: Some("logs".into()) }).await.unwrap();
    // Explicit OSC titles: the process-derived default ("sh"/"bash") is
    // polled and may change after the tree is captured.
    let title = |name: &str| format!("printf '\\033]0;title-{name}\\007'; ");
    let main_cmd = "i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo hist-$i; done; echo mark-main\n";
    send(control, &main.block_id, &format!("{}{main_cmd}", title("main"))).await;
    send(control, &split.block_id, &format!("{}echo mark-split\n", title("split"))).await;
    send(control, &window.block_id, &format!("{}echo mark-window\n", title("window"))).await;
    wait_output(control, &main.block_id, "\nmark-main").await;
    wait_output(control, &split.block_id, "\nmark-split").await;
    wait_output(control, &window.block_id, "\nmark-window").await;
    for (block_id, name) in [(&main.block_id, "main"), (&split.block_id, "split"), (&window.block_id, "window")] {
        let want = format!("title-{name}");
        wait_for(&want, async || block(&view(control, &main.session_id).await, block_id).title == want).await;
    }
    (main, split, window)
}

/// The restored tree equals `before` apart from block lifecycles, and every
/// block kept its screen and history.
async fn assert_restored(control: &Control, before: &SessionView, marks: &[(&str, &str)]) {
    let after = view(control, &before.session.id).await;
    let sessions: Vec<serde_json::Value> = control.call(Method::SessionList {}).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(after.session.id, before.session.id);
    assert_eq!(after.session.name, before.session.name);
    assert_eq!(after.session.windows, before.session.windows);
    assert_eq!(after.session.active_window, before.session.active_window);
    assert_eq!(after.blocks.len(), before.blocks.len());
    for old in &before.blocks {
        let new = block(&after, &old.id);
        assert_eq!(new.lifecycle, Lifecycle::Restored);
        assert_eq!((new.title.as_str(), new.session_id.as_str(), new.size), (old.title.as_str(), old.session_id.as_str(), old.size));
        assert_eq!((new.pid, new.owner), (None, None));
    }
    for (block, mark) in marks {
        let text = capture(control, block).await;
        assert!(text.contains(&format!("\n{mark}")), "{block} lost {mark}:\n{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_shutdown_restores_tree_and_screens() {
    let home = temp_home("clean");
    let task = start(&home).await;
    let control = connect(&home).await;
    let (main, split, window) = populate(&control).await;
    let before = view(&control, &main.session_id).await;
    shutdown(control, task).await;

    let task = start(&home).await;
    let control = connect(&home).await;
    let marks = [(main.block_id.as_str(), "mark-main"), (split.block_id.as_str(), "mark-split"), (window.block_id.as_str(), "mark-window")];
    assert_restored(&control, &before, &marks).await;
    let history = capture(&control, &main.block_id).await;
    assert!(history.contains("\nhist-1\n") && history.contains("\nhist-300\n"), "history lost:\n{history}");

    // Restored blocks ignore input; new blocks work normally.
    let fresh: Created = control
        .call(Method::BlockSplit { block_id: split.block_id.clone(), axis: Axis::Vertical, spawn: shell(), kind: BlockKind::Terminal, url: None })
        .await
        .unwrap();
    assert!(before.blocks.iter().all(|b| b.id != fresh.block_id));
    send(&control, &fresh.block_id, "echo fresh-$((2+3))\n").await;
    wait_output(&control, &fresh.block_id, "\nfresh-5").await;
    let after = view(&control, &main.session_id).await;
    assert_eq!(block(&after, &fresh.block_id).lifecycle, Lifecycle::Running);

    // And that block survives the next restart too.
    shutdown(control, task).await;
    let task = start(&home).await;
    let control = connect(&home).await;
    wait_output(&control, &fresh.block_id, "\nfresh-5").await;
    shutdown(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_restores_last_checkpoint() {
    let home = temp_home("crash");
    let task = start(&home).await;
    let control = connect(&home).await;
    let (main, split, window) = populate(&control).await;
    let before = view(&control, &main.session_id).await;
    tokio::time::sleep(INTERVAL * 4).await;
    crash(control, task).await;

    let task = start(&home).await;
    let control = connect(&home).await;
    let marks = [(main.block_id.as_str(), "mark-main"), (split.block_id.as_str(), "mark-split"), (window.block_id.as_str(), "mark-window")];
    assert_restored(&control, &before, &marks).await;
    shutdown(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn exited_block_stays_and_survives_restart() {
    let home = temp_home("exited");
    let task = start(&home).await;
    let control = connect(&home).await;
    let main: Created = control.call(Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    send(&control, &main.block_id, "echo bye-$((6*7)); exit 3\n").await;
    wait_for("exit", async || block(&view(&control, &main.session_id).await, &main.block_id).lifecycle == Lifecycle::Exited).await;
    let before = view(&control, &main.session_id).await;
    assert_eq!(before.session.windows[0].layout.blocks(), [main.block_id.as_str()]);
    assert_eq!(block(&before, &main.block_id).exited, Some(3));
    wait_output(&control, &main.block_id, "\nbye-42").await;
    shutdown(control, task).await;

    let task = start(&home).await;
    let control = connect(&home).await;
    assert_restored(&control, &before, &[(main.block_id.as_str(), "bye-42")]).await;
    shutdown(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn closed_block_snapshots_are_collected() {
    let home = temp_home("close");
    let task = start(&home).await;
    let control = connect(&home).await;
    let main: Created = control.call(Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split: Created = control
        .call(Method::BlockSplit { block_id: main.block_id.clone(), axis: Axis::Vertical, spawn: shell(), kind: BlockKind::Terminal, url: None })
        .await
        .unwrap();
    send(&control, &split.block_id, "echo doomed\n").await;
    wait_output(&control, &split.block_id, "\ndoomed").await;
    let dir = snapshot_dir(&home, &split.block_id);
    wait_for("split checkpoint", async || dir.is_dir()).await;

    control.call_value(Method::BlockClose { block_id: split.block_id.clone() }).await.unwrap();
    // gc keeps the previous commit's generations: commit again via output.
    for n in 0..20 {
        if !dir.exists() {
            break;
        }
        send(&control, &main.block_id, &format!("echo tick-{n}\n")).await;
        tokio::time::sleep(INTERVAL * 2).await;
    }
    assert!(!dir.exists(), "closed block's snapshots remain");
    assert!(snapshot_dir(&home, &main.block_id).is_dir());
    shutdown(control, task).await;

    let task = start(&home).await;
    let control = connect(&home).await;
    let after = view(&control, &main.session_id).await;
    assert_eq!(after.session.windows[0].layout.blocks(), [main.block_id.as_str()]);
    shutdown(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_snapshot_restores_that_block_empty() {
    let home = temp_home("corrupt");
    let task = start(&home).await;
    let control = connect(&home).await;
    let (main, split, window) = populate(&control).await;
    let before = view(&control, &main.session_id).await;
    shutdown(control, task).await;

    let dir = snapshot_dir(&home, &split.block_id);
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert!(!files.is_empty());
    for file in files {
        let mut bytes = std::fs::read(&file).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&file, bytes).unwrap();
    }

    let task = start(&home).await;
    let control = connect(&home).await;
    let marks = [(main.block_id.as_str(), "mark-main"), (window.block_id.as_str(), "mark-window")];
    assert_restored(&control, &before, &marks).await;
    let text = capture(&control, &split.block_id).await;
    assert!(!text.contains("mark-split") && text.trim().is_empty(), "expected empty screen:\n{text}");
    shutdown(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn floats_survive_restart() {
    let home = temp_home("floats");
    let task = start(&home).await;
    let control = connect(&home).await;
    let main: Created = control.call(Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split: Created = control
        .call(Method::BlockSplit { block_id: main.block_id.clone(), axis: Axis::Vertical, spawn: shell(), kind: BlockKind::Terminal, url: None })
        .await
        .unwrap();
    send(&control, &split.block_id, "echo float-$((4*5))\n").await;
    wait_output(&control, &split.block_id, "\nfloat-20").await;
    let rect = FloatRect { x: 0.1, y: 0.2, w: 0.5, h: 0.4 };
    control.call_value(Method::BlockFloat { block_id: split.block_id.clone(), rect: Some(rect) }).await.unwrap();
    let before = view(&control, &main.session_id).await;
    assert_eq!(before.session.windows[0].floats, [Float { id: split.block_id.clone(), rect }]);
    assert_eq!(before.session.windows[0].layout.blocks(), [main.block_id.as_str()]);
    shutdown(control, task).await;

    let task = start(&home).await;
    let control = connect(&home).await;
    let after = view(&control, &main.session_id).await;
    assert_eq!(after.session.windows, before.session.windows);
    assert_eq!(block(&after, &split.block_id).lifecycle, Lifecycle::Restored);
    wait_output(&control, &split.block_id, "\nfloat-20").await;
    shutdown(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}
