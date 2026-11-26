//! Program resume (OSC 7577) on real PTYs: a program reports with the
//! token its block gave it, the record lands in the checkpoint (the token
//! only on a hot upgrade), forged reports do nothing, and a record goes
//! once its owner has exited.

use std::time::{Duration, Instant};

use silo_proto::model::{BlockInfo, BlockKind, Lifecycle, TermSize};
use silo_pty::SpawnConfig;
use silo_server::block::{self, BlockHandle, BlockMsg, Checkpoint};
use tokio::sync::mpsc::unbounded_channel;

/// `r PID ARGV`: report `ARGV` (base64 elements) for `PID`.
const REPORT: &str = r#"r() { body="pid=$1:time=$(date +%s):argv=$2"; mac=$(printf '%s:%s' "$body" "$OSC7577_TOKEN" | { sha256sum 2>/dev/null || shasum -a 256; } | cut -c1-64); printf '\033]7577;%s:mac=%s\033\\' "$body" "$mac"; }; "#;

fn info(id: &str) -> BlockInfo {
    BlockInfo {
        id: id.into(),
        session_id: "s-test".into(),
        kind: BlockKind::Terminal,
        title: "shell".into(),
        cwd: None,
        exited: None,
        lifecycle: Lifecycle::Running,
        size: TermSize::new(100, 10),
        owner: None,
        owner_name: None,
        sizers: Vec::new(),
        owner_block: None,
        pid: None,
        browser: None,
        agent: None,
        files: None,
        changes: None,
        studio: None,
    }
}

fn spawn(id: &str, script: &str) -> BlockHandle {
    let (notes, _rx) = unbounded_channel();
    let config = SpawnConfig {
        command: vec!["/bin/sh".into(), "-c".into(), format!("{REPORT}{script}").into()],
        cwd: Some(std::env::temp_dir()),
        env: Vec::new(),
    };
    block::spawn(info(id), &config, notes).expect("spawn")
}

async fn checkpoint(handle: &BlockHandle) -> Checkpoint {
    let (reply, rx) = tokio::sync::oneshot::channel();
    handle.send(BlockMsg::Checkpoint { reply });
    rx.await.expect("checkpoint").expect("captured")
}

async fn text(handle: &BlockHandle) -> String {
    let (reply, rx) = tokio::sync::oneshot::channel();
    handle.send(BlockMsg::Capture { history: true, reply });
    rx.await.expect("capture").text
}

async fn wait_for(handle: &BlockHandle, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !text(handle).await.contains(what) {
        assert!(Instant::now() < deadline, "never saw {what:?}: {}", text(handle).await);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_report_with_the_block_s_token_is_kept_and_forged_ones_are_not() {
    // A forged clear (wrong mac) after the real report changes nothing.
    let script = r#"r $$ bnZpbQ==,LVM=,L3RtcC9zLnZpbQ==; printf 'x\033]7577;pid=%s:time=%s:clear=1:mac=%064d\033\\' $$ $(date +%s) 0; echo; echo "READY:$OSC7577_TOKEN"; read l"#;
    let handle = spawn("b-resume", script);
    wait_for(&handle, "READY:").await;
    let shown = text(&handle).await;
    assert!(!shown.contains("7577"), "reports are taken out of the output: {shown}");
    let token = shown.split("READY:").nth(1).unwrap().lines().next().unwrap().trim().to_owned();
    assert!(token.len() >= 22, "each block gets a token: {token:?}");
    let cp = checkpoint(&handle).await;
    assert_eq!(cp.resume.token.as_deref(), Some(token.as_str()));
    let record = cp.resume.pick().expect("a record");
    assert_eq!(record.argv, ["nvim", "-S", "/tmp/s.vim"]);
    assert_eq!(record.depth, 0, "the block's own program");

    // A hot upgrade carries the token and the records over.
    let handoff = handle.handoff().await.expect("handoff");
    let (notes, _rx) = unbounded_channel();
    let adopted = block::spawn_adopted(info("b-resume"), handoff.checkpoint, handoff.pty.expect("pty"), notes).expect("adopt");
    let cp = checkpoint(&adopted).await;
    assert_eq!(cp.resume.token.as_deref(), Some(token.as_str()));
    assert_eq!(cp.resume.records.len(), 1);
    adopted.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn a_record_goes_once_its_owner_has_exited() {
    // The owner is a child that reports and exits a second later; the
    // block's program keeps running.
    let lib = std::env::temp_dir().join(format!("silo-resume-report-{}.sh", std::process::id()));
    std::fs::write(&lib, REPORT).unwrap();
    let script = format!("sh -c '. \"{}\"; r $$ eA; echo REPORTED; sleep 1'; echo DONE; read l", lib.display());
    let handle = spawn("b-gone", &script);
    wait_for(&handle, "REPORTED").await;
    let cp = checkpoint(&handle).await;
    assert_eq!(cp.resume.pick().map(|r| (r.argv.clone(), r.depth)), Some((vec!["x".to_owned()], 1)));
    wait_for(&handle, "DONE").await;
    // Kept for a grace period (it may have ended with the terminal), then gone.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !checkpoint(&handle).await.resume.records.is_empty() {
        assert!(Instant::now() < deadline, "the record outlived its owner");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    handle.send(BlockMsg::Shutdown);
    let _ = std::fs::remove_file(lib);
}
