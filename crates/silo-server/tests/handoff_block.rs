//! Block handoff: a running program's PTY moves to a new actor without a
//! gap in the output stream; exited blocks hand off a checkpoint only.

use std::time::Duration;

use silo_proto::{
    Frame,
    control::Capture,
    data::{Resume, ServerData, StartMode, Welcome},
    kind,
    model::{BlockInfo, BlockKind, Lifecycle, TermSize},
};
use silo_pty::SpawnConfig;
use silo_server::block::{self, AdoptFailed, Attach, BlockHandle, BlockMsg, Note, Sink};
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
    for _ in 0..400 {
        if capture(block, true).await.text.contains(text) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never saw {text:?}");
}

fn attach(block: &BlockHandle, sub_id: u64, resume: Option<Resume>) -> (Sink, UnboundedReceiver<Frame>) {
    let (tx, rx) = unbounded_channel();
    let sink = Sink::new(tx);
    let attach = Attach { sub_id, client_id: 7, resume, sink: sink.clone(), remote: false };
    block.send(BlockMsg::Attach(Box::new(attach)));
    (sink, rx)
}

/// Frames already queued on a stream.
fn drain(rx: &mut UnboundedReceiver<Frame>) -> Vec<Frame> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// Append `OUTPUT` frames to `out`, checking they are contiguous.
fn collect_output(frames: Vec<Frame>, next: &mut u64, out: &mut Vec<u8>) {
    for frame in frames {
        if let Ok(ServerData::Output { offset, bytes }) = ServerData::from_frame(frame) {
            assert_eq!(offset, *next, "output gap or overlap");
            *next += bytes.len() as u64;
            out.extend(bytes);
        }
    }
}

fn welcome(frame: &Frame) -> Welcome {
    assert_eq!(frame.kind, kind::WELCOME);
    serde_json::from_slice(&frame.payload).expect("welcome")
}

fn alive(pid: i32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|s| s.success())
}

const LOOP: &str = "i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo n$i; sleep 0.005; done; echo LOOP''DONE; \
                    while read l; do echo got:$l; done";

#[tokio::test]
async fn running_block_hands_off_without_gap() {
    let (old, _notes) = spawn("b-run", LOOP);
    let (sink1, mut rx1) = attach(&old, 1, None);
    wait_for(&old, "n20").await;
    let pid = old.info.lock().unwrap().pid.expect("pid");

    let handoff = old.handoff().await.expect("handoff");
    let pty = handoff.pty.expect("running block hands off its pty");
    let checkpoint = handoff.checkpoint;
    assert_eq!(pty.pid, pid);
    assert_eq!(checkpoint.lifecycle, Lifecycle::Running);
    assert!(sink1.is_cancelled(), "streams close on handoff");
    assert!(old.checkpoint().await.is_err(), "old actor ended");

    let frames = drain(&mut rx1);
    let first = welcome(&frames[0]);
    let mut next = first.offset;
    let mut out = Vec::new();
    collect_output(frames, &mut next, &mut out);
    assert_eq!(next, checkpoint.offset, "stream ends at the handoff offset");

    let (notes, _adopted_notes) = unbounded_channel();
    let resume = Resume { epoch: checkpoint.epoch, offset: checkpoint.offset };
    let adopted = block::spawn_adopted(info("b-run"), checkpoint, pty, notes).expect("adopt");
    assert!(alive(pid), "program survives the handoff");
    assert_eq!(adopted.info.lock().unwrap().pid, Some(pid));
    let (_sink2, mut rx2) = attach(&adopted, 2, Some(resume));
    wait_for(&adopted, "LOOPDONE").await;

    let frames = drain(&mut rx2);
    let second = welcome(&frames[0]);
    assert!(matches!(second.mode, StartMode::Resume));
    assert_eq!((second.epoch, second.offset), (resume.epoch, resume.offset));
    collect_output(frames, &mut next, &mut out);

    let text = String::from_utf8_lossy(&out).replace('\r', "");
    let numbers: Vec<u32> = text.lines().filter_map(|l| l.strip_prefix('n')?.parse().ok()).collect();
    assert_eq!(numbers.last(), Some(&300));
    assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "gap or duplicate: {numbers:?}");
    if first.offset == 0 {
        assert_eq!(numbers.first(), Some(&1));
    }

    adopted.send(BlockMsg::Input(b"hello\n".to_vec()));
    wait_for(&adopted, "got:hello").await;
    adopted.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn exited_block_hands_off_checkpoint_only() {
    let (old, mut notes) = spawn("b-exit", "seq 1 30; printf 'final screen'; exit 4");
    let exited = async { while !matches!(notes.recv().await, Some(Note::Exited(_)) | None) {} };
    tokio::time::timeout(Duration::from_secs(10), exited).await.expect("exit");
    let screen = capture(&old, true).await;

    let handoff = old.handoff().await.expect("handoff");
    assert!(handoff.pty.is_none());
    assert_eq!((handoff.checkpoint.lifecycle, handoff.checkpoint.exit_code), (Lifecycle::Exited, Some(4)));

    let (notes, _rx) = unbounded_channel();
    let restored = block::spawn_restored(info("b-exit"), handoff.checkpoint, notes).expect("restore");
    assert_eq!(capture(&restored, true).await.text, screen.text);
    restored.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn adopted_block_reports_unknown_exit() {
    let (old, _notes) = spawn("b-kill", "printf ready; read l; exit 5");
    wait_for(&old, "ready").await;
    let handoff = old.handoff().await.expect("handoff");
    let pty = handoff.pty.expect("pty");

    let (notes, mut rx) = unbounded_channel();
    let adopted = block::spawn_adopted(info("b-kill"), handoff.checkpoint, pty, notes).expect("adopt");
    adopted.send(BlockMsg::Input(b"bye\n".to_vec()));
    let exited = async { while !matches!(rx.recv().await, Some(Note::Exited(_)) | None) {} };
    tokio::time::timeout(Duration::from_secs(10), exited).await.expect("exit note");
    let info = adopted.info.lock().unwrap().clone();
    assert_eq!((info.lifecycle, info.exited), (Lifecycle::Exited, None));
    let checkpoint = adopted.checkpoint().await.expect("checkpoint");
    assert_eq!(checkpoint.exit_code, None);
    adopted.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn title_and_cwd_survive_handoff_and_restore() {
    let script = "printf '\\033]2;my-title\\007\\033]7;file://host/tmp/osc-dir\\007ready'; read l";
    let (old, _notes) = spawn("b-osc", script);
    wait_for(&old, "ready").await;
    let handoff = old.handoff().await.expect("handoff");
    assert_eq!(handoff.checkpoint.osc_title, "my-title");
    assert!(handoff.checkpoint.osc_pwd.as_deref().is_some_and(|p| p.ends_with("/tmp/osc-dir")));

    let (notes, _rx) = unbounded_channel();
    let restored = block::spawn_restored(info("b-osc"), handoff.checkpoint.clone(), notes).expect("restore");
    let restored_info = restored.info.lock().unwrap().clone();
    assert_eq!((restored_info.title.as_str(), restored_info.cwd.as_deref()), ("my-title", Some("/tmp/osc-dir")));
    restored.send(BlockMsg::Shutdown);

    let (notes, _rx) = unbounded_channel();
    let pty = handoff.pty.expect("pty");
    let adopted = block::spawn_adopted(info("b-osc"), handoff.checkpoint, pty, notes).expect("adopt");
    let adopted_info = adopted.info.lock().unwrap().clone();
    assert_eq!((adopted_info.title.as_str(), adopted_info.cwd.as_deref()), ("my-title", Some("/tmp/osc-dir")));
    adopted.send(BlockMsg::Input(b"x\n".to_vec()));
    adopted.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn failed_adopt_returns_open_pty() {
    let (old, _notes) = spawn("b-bad", "printf ready; while read l; do echo got:$l; done");
    wait_for(&old, "ready").await;
    let handoff = old.handoff().await.expect("handoff");
    let pty = handoff.pty.expect("pty");
    let pid = pty.pid;

    let mut corrupt = handoff.checkpoint.clone();
    corrupt.snapshot.truncate(8);
    let (notes, _rx) = unbounded_channel();
    let err = block::spawn_adopted(info("b-bad"), corrupt, pty, notes).err().expect("corrupt snapshot fails");
    let failed = err.downcast::<AdoptFailed>().expect("typed error");
    assert_eq!(failed.detached.pid, pid);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(alive(pid), "no hangup on failed adopt");

    let (notes, _rx) = unbounded_channel();
    let adopted = block::spawn_adopted(info("b-bad"), handoff.checkpoint, failed.detached, notes).expect("retry");
    adopted.send(BlockMsg::Input(b"again\n".to_vec()));
    wait_for(&adopted, "got:again").await;
    adopted.send(BlockMsg::Shutdown);
}
