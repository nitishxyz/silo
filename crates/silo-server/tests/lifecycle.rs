//! Block lifecycle: exited blocks stay readable; checkpoints restore into
//! process-less blocks that ignore input.

use std::time::Duration;

use silo_proto::{
    control::Capture,
    data::ServerData,
    kind,
    model::{BlockInfo, BlockKind, Lifecycle, TermSize},
};
use silo_pty::SpawnConfig;
use silo_server::block::{self, Attach, BlockHandle, BlockMsg, Checkpoint, Note, Sink};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn info(id: &str) -> BlockInfo {
    BlockInfo {
        id: id.into(),
        session_id: "s-test".into(),
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
    }
}

fn spawn(id: &str, script: &str) -> (BlockHandle, UnboundedReceiver<Note>) {
    let (notes, rx) = unbounded_channel();
    let config = SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), script.into()], cwd: Some(std::env::temp_dir()), env: Vec::new() };
    (block::spawn(info(id), &config, notes).expect("spawn"), rx)
}

async fn capture(block: &BlockHandle, history: bool) -> Capture {
    let (reply, rx) = tokio::sync::oneshot::channel();
    block.send(BlockMsg::Capture { history, reply });
    rx.await.expect("capture")
}

async fn wait_for(block: &BlockHandle, text: &str) {
    for _ in 0..200 {
        if capture(block, false).await.text.contains(text) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never saw {text:?}");
}

async fn wait_exited(notes: &mut UnboundedReceiver<Note>) {
    let wait = async {
        while let Some(note) = notes.recv().await {
            if matches!(note, Note::Exited(_)) {
                return;
            }
        }
        panic!("notes closed");
    };
    tokio::time::timeout(Duration::from_secs(10), wait).await.expect("exit note");
}

/// Attach and collect frames until the stream goes quiet.
async fn attach(block: &BlockHandle) -> Vec<silo_proto::Frame> {
    let (tx, mut rx) = unbounded_channel();
    let sink = Sink::new(tx);
    block.send(BlockMsg::Attach(Box::new(Attach { sub_id: 1, client_id: 7, resume: None, sink, remote: false })));
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        frames.push(frame);
    }
    frames
}

const SCRIPT: &str = "seq 1 60; printf 'final screen'";

#[tokio::test]
async fn exited_block_stays_attachable() {
    let (block, mut notes) = spawn("b-exit", &format!("{SCRIPT}; exit 3"));
    wait_exited(&mut notes).await;
    {
        let info = block.info.lock().unwrap();
        assert_eq!((info.lifecycle, info.exited), (Lifecycle::Exited, Some(3)));
    }
    let screen = capture(&block, false).await;
    assert!(screen.text.contains("final screen"), "{}", screen.text);
    assert!(capture(&block, true).await.text.starts_with("1\n2\n3"));

    let frames = attach(&block).await;
    assert_eq!(frames.first().map(|f| f.kind), Some(kind::WELCOME));
    assert!(frames.iter().any(|f| f.kind == kind::SNAPSHOT_DONE));
    let last = ServerData::from_frame(frames.last().unwrap().clone()).unwrap();
    assert!(matches!(last, ServerData::Exited { code: Some(3) }));

    // Input to an exited block is dropped; the screen does not change.
    block.send(BlockMsg::Input(b"echo nope\n".to_vec()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(capture(&block, false).await.text, screen.text);
    block.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn checkpoint_restores_screen_and_ignores_input() {
    let (block, _notes) = spawn("b-live", &format!("{SCRIPT}; sleep 30"));
    wait_for(&block, "final screen").await;
    let checkpoint = block.checkpoint().await.expect("checkpoint");
    assert_eq!(checkpoint.lifecycle, Lifecycle::Running);
    let screen = capture(&block, false).await;
    let history = capture(&block, true).await;
    block.send(BlockMsg::Shutdown);

    let json = serde_json::to_vec(&checkpoint).unwrap();
    let decoded: Checkpoint = serde_json::from_slice(&json).unwrap();
    assert_eq!(decoded, checkpoint);

    let (notes, _rx) = unbounded_channel();
    let restored = block::spawn_restored(info("b-live"), decoded, notes).expect("restore");
    assert_eq!(restored.info.lock().unwrap().lifecycle, Lifecycle::Restored);
    let restored_screen = capture(&restored, false).await;
    assert_eq!((restored_screen.text, restored_screen.size), (screen.text, screen.size));
    assert_eq!(capture(&restored, true).await.text, history.text);

    let again = restored.checkpoint().await.unwrap();
    assert_ne!(again.epoch, checkpoint.epoch, "restore starts a new epoch");
    assert_eq!((again.lifecycle, again.exit_code), (Lifecycle::Restored, None));

    restored.send(BlockMsg::Input(b"echo nope\n".to_vec()));
    restored.send(BlockMsg::Inject { bytes: b"\r\ninjected".to_vec(), input: b"x".to_vec() });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(capture(&restored, true).await.text, history.text);

    let frames = attach(&restored).await;
    let last = ServerData::from_frame(frames.last().unwrap().clone()).unwrap();
    assert!(matches!(last, ServerData::Exited { code: None }));
    restored.send(BlockMsg::Shutdown);
}
