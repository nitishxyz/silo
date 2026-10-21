//! Block status end to end on real PTYs (Phase 7 P7-3): the OSC 7501
//! support query, status events that never bump the revision, busy
//! detection, handoff and restore, and command rows.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use silo_proto::{
    Frame,
    control::{Capture, CommandOutput, Event, ServerMessage, SpawnSpec},
    data::{CommandAnchor, ServerData},
    kind,
    model::{BlockInfo, BlockKind, BlockStatus, Busy, CommandRecord, Lifecycle, StatusState, TermSize},
};
use silo_pty::SpawnConfig;
use silo_server::{
    block::{self, Attach, BlockHandle, BlockMsg, Note, Sink},
    registry::{New, Registry},
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const T: Duration = Duration::from_secs(15);

fn info(id: &str, columns: u16) -> BlockInfo {
    BlockInfo {
        id: id.into(),
        session_id: "s-test".into(),
        kind: BlockKind::Terminal,
        title: "shell".into(),
        cwd: None,
        exited: None,
        lifecycle: Lifecycle::Running,
        size: TermSize::new(columns, 10),
        owner: None,
        owner_name: None,
        pid: None,
        browser: None,
    }
}

struct Block {
    handle: BlockHandle,
    notes: UnboundedReceiver<Note>,
}

impl Block {
    fn spawn(id: &str, columns: u16, command: &[&str]) -> Self {
        let (notes, rx) = unbounded_channel();
        let config = SpawnConfig { command: command.iter().map(Into::into).collect(), cwd: Some(std::env::temp_dir()), env: Vec::new() };
        Self { handle: block::spawn(info(id, columns), &config, notes).expect("spawn"), notes: rx }
    }

    fn sh(id: &str, script: &str) -> Self {
        Self::spawn(id, 120, &["/bin/sh", "-c", script])
    }

    fn status(&self) -> BlockStatus {
        self.handle.status.lock().unwrap().clone()
    }

    /// Wait for a published status matching `done`.
    async fn until(&mut self, what: &str, done: impl Fn(&BlockStatus) -> bool) -> BlockStatus {
        let deadline = Instant::now() + T;
        loop {
            let status = self.status();
            if done(&status) {
                return status;
            }
            match tokio::time::timeout_at(deadline.into(), self.notes.recv()).await {
                Ok(Some(_)) => {}
                _ => panic!("never saw {what}: {status:?}"),
            }
        }
    }

    async fn capture(&self) -> Capture {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.handle.send(BlockMsg::Capture { history: true, reply });
        rx.await.expect("capture")
    }

    async fn wait_for(&self, text: &str) {
        let deadline = Instant::now() + T;
        while Instant::now() < deadline {
            if self.capture().await.text.contains(text) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never saw {text:?}: {}", self.capture().await.text);
    }

    fn input(&self, text: &str) {
        self.handle.send(BlockMsg::Input(text.as_bytes().to_vec()));
    }

    /// The close check: a fresh probe.
    async fn busy(&self) -> Option<Busy> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.handle.send(BlockMsg::Busy { reply });
        rx.await.expect("busy")
    }

    async fn commands(&self) -> Vec<CommandRecord> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.handle.send(BlockMsg::Commands { limit: None, reply });
        rx.await.expect("commands")
    }

    async fn output(&self, index: Option<u64>) -> CommandOutput {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.handle.send(BlockMsg::CommandOutput { index, reply });
        rx.await.expect("output").expect("a finished command")
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        self.handle.send(BlockMsg::Shutdown);
    }
}

/// A shell with `PS1` = `prompt`: bash through an rcfile laid out like
/// Silo's own hook, so the block is a login-shell block, not a command.
fn bash(id: &str, prompt: &str) -> (Block, PathBuf) {
    let dir = PathBuf::from(format!("/tmp/silo-status-{id}-{}", std::process::id()));
    let rc = dir.join("shell/bash/rc");
    std::fs::create_dir_all(rc.parent().unwrap()).unwrap();
    std::fs::write(&rc, format!("PS1='{prompt}'\nunset PROMPT_COMMAND\n")).unwrap();
    let block = Block::spawn(id, 120, &["bash", "--rcfile", rc.to_str().unwrap()]);
    assert!(!block.handle.command, "a hooked login shell is not a command block");
    (block, dir)
}

fn busy_process(status: &BlockStatus) -> Option<&str> {
    status.busy.as_ref().map(|b| b.process.as_str())
}

#[tokio::test]
async fn the_support_query_is_answered_on_the_block_s_input() {
    const REPLY: &str = "E]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,authE\\";
    let script = format!("stty raw -echo; printf '\\033]7501;?\\033\\\\'; dd bs=1 count={} 2>/dev/null | tr '\\033' E; read l", REPLY.len());
    let block = Block::sh("b-query", &script);
    block.wait_for(REPLY).await;
}

#[tokio::test]
async fn busy_without_marks_follows_the_foreground_process() {
    let (block, dir) = bash("b-fg", "READY$ ");
    block.wait_for("READY$").await;
    assert_eq!(block.busy().await, None, "a shell at its prompt");
    let mut block = block;
    block.input("sleep 30\n");
    let typed = Instant::now();
    let status = block.until("sleep busy", |s| busy_process(s) == Some("sleep")).await;
    assert!(typed.elapsed() < Duration::from_millis(1500), "within one probe: {:?}", typed.elapsed());
    assert_eq!(status.busy.as_ref().and_then(|b| b.command.clone()), None);
    block.input("\x03");
    block.until("idle again", |s| s.busy.is_none()).await;
    let nested = dir.join("nested");
    std::fs::write(&nested, "PS1='NESTED$ '\n").unwrap();
    block.input(&format!("bash --rcfile {}\n", nested.display()));
    block.wait_for("NESTED$").await;
    assert_eq!(block.busy().await, None, "a nested shell at its prompt");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn busy_with_marks_sees_a_builtin_until_the_command_ends() {
    let (mut block, dir) = bash("b-marks", "READY$ ");
    block.wait_for("READY$").await;
    block.input("printf '\\033]133;C;cmdline_url=read%%20x\\007'; read x\n");
    let status = block.until("read busy", |s| busy_process(s) == Some("read")).await;
    assert_eq!(status.busy.unwrap().command.as_deref(), Some("read x"));
    assert!(block.busy().await.is_some(), "the shell itself is in the foreground, yet busy");
    block.input("answer\n");
    block.input("printf '\\033]133;D;0\\007'\n");
    block.until("D clears it", |s| s.busy.is_none()).await;
    assert_eq!(block.busy().await, None);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_foreground_child_printing_a_prompt_mark_stays_busy() {
    // ssh to a host with shell integration forwards the remote prompt's A.
    let (mut block, dir) = bash("b-ssh", "READY$ ");
    block.wait_for("READY$").await;
    block.input("printf '\\033]133;C\\007'; perl -e '$|=1; print \"\\e]133;A\\aRUN\" . \"NING\\n\"; sleep 30'\n");
    block.wait_for("RUNNING").await;
    assert_eq!(block.busy().await.map(|b| b.process), Some("perl".to_owned()));
    block.until("perl busy", |s| busy_process(s) == Some("perl")).await;
    block.input("\x03");
    block.until("idle again", |s| s.busy.is_none()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn command_blocks_are_busy_unless_they_run_a_shell() {
    let mut sleeping = Block::spawn("b-sleep", 80, &["sleep", "30"]);
    assert!(sleeping.handle.command);
    sleeping.until("sleep busy", |s| busy_process(s) == Some("sleep")).await;
    assert_eq!(sleeping.busy().await.map(|b| b.process), Some("sleep".to_owned()));

    let shell = if std::path::Path::new("/bin/zsh").exists() { "/bin/zsh" } else { "bash" };
    let (program, args) = if shell == "/bin/zsh" { (shell, ["-f", "-i"]) } else { (shell, ["--norc", "-i"]) };
    let interactive = Block::spawn("b-zsh", 80, &[program, args[0], args[1]]);
    interactive.input("PS1=ZREADY; echo set\n");
    interactive.wait_for("ZREADY").await;
    assert_eq!(interactive.busy().await, None, "{shell} at its prompt");
}

/// Collect frames from a registry client until `done` holds.
fn events(rx: &mut UnboundedReceiver<Frame>, seen: &mut Vec<Event>) {
    while let Ok(frame) = rx.try_recv() {
        if let Ok(ServerMessage::Event { event, .. }) = serde_json::from_slice::<ServerMessage>(&frame.payload) {
            seen.push(*event);
        }
    }
}

#[tokio::test]
async fn progress_ticks_and_busy_changes_emit_status_without_changing_revision() {
    let (notes, mut rx) = unbounded_channel();
    let mut reg = Registry::new(notes);
    let client = reg.add_client("test".into(), false);
    reg.clients.get_mut(&client).unwrap().features = vec![silo_proto::compat::feature::STATUS.to_owned()];
    let (tx, mut frames) = unbounded_channel();
    reg.subscribe(client, Sink::new(tx));
    let script = "printf '\\033]9;4;1;10\\007'; read a; printf '\\033]9;4;1;60\\007'; read b; exec sleep 30";
    let spawn = SpawnSpec { command: vec!["/bin/sh".into(), "-c".into(), script.into()], ..SpawnSpec::default() };
    let created = reg.create_session(None, &New { spawn: &spawn, kind: BlockKind::Terminal, url: None }).unwrap();
    let revision = reg.view(&created.session_id).unwrap().session.revision;
    let mut seen = Vec::new();
    events(&mut frames, &mut seen);
    seen.clear();
    let deadline = Instant::now() + T;
    let mut step = 0;
    let mut statuses: Vec<BlockStatus> = Vec::new();
    while step < 3 {
        let note = tokio::time::timeout_at(deadline.into(), rx.recv()).await.expect("status in time").expect("notes");
        reg.handle_note(note);
        events(&mut frames, &mut seen);
        statuses.extend(seen.iter().filter_map(|e| if let Event::BlockStatus { status } = e { Some(status.clone()) } else { None }));
        let percent = |p| statuses.iter().any(|s| s.progress.is_some_and(|x| x.percent == Some(p)));
        let block = reg.block(&created.block_id).unwrap();
        match step {
            0 if percent(10) => {
                block.send(BlockMsg::Input(b"\n".to_vec()));
                step = 1;
            }
            1 if percent(60) => {
                block.send(BlockMsg::Input(b"\n".to_vec()));
                step = 2;
            }
            2 if statuses.iter().any(|s| s.busy.as_ref().is_some_and(|b| b.process == "sleep")) => step = 3,
            _ => {}
        }
    }
    assert_eq!(reg.view(&created.session_id).unwrap().session.revision, revision, "status never bumps the revision");
    assert!(!seen.iter().any(|e| matches!(e, Event::SessionChanged { .. })), "no session.changed: {seen:?}");
    assert!(!seen.iter().any(|e| matches!(e, Event::SessionsChanged { .. })), "counts did not change");
    let view = reg.view(&created.session_id).unwrap();
    assert_eq!(view.status.len(), 1, "the view carries the status");
    reg.close_session(&created.session_id).unwrap();
}

const RECORDS: &str = "printf '\\033]7501;state=working:id=w\\033\\\\\\033]7501;state=done:id=d\\033\\\\\
                       \\033]7501;state=error:id=e\\033\\\\\\033]7501;state=blocked:id=b\\033\\\\ready'; read l";

fn states(status: &BlockStatus) -> Vec<(String, StatusState)> {
    status.records.iter().map(|r| (r.id.clone(), r.state)).collect()
}

#[tokio::test]
async fn a_hot_upgrade_keeps_working_records_and_a_crash_restore_only_done_and_error() {
    let mut block = Block::sh("b-handoff", RECORDS);
    let all = block.until("four records", |s| s.records.len() == 4).await;
    let handoff = block.handle.handoff().await.expect("handoff");
    let checkpoint = handoff.checkpoint;

    let (notes, _rx) = unbounded_channel();
    let adopted = block::spawn_adopted(info("b-handoff", 120), checkpoint.clone(), handoff.pty.expect("pty"), notes).expect("adopt");
    assert_eq!(states(&adopted.status.lock().unwrap()), states(&all), "the program lives on: everything carries over");
    adopted.send(BlockMsg::Shutdown);

    let (notes, _rx) = unbounded_channel();
    let restored = block::spawn_restored(info("b-handoff", 120), checkpoint.clone(), notes).expect("restore");
    let kept = states(&restored.status.lock().unwrap());
    assert_eq!(kept, [("d".to_owned(), StatusState::Done), ("e".to_owned(), StatusState::Error)]);
    restored.send(BlockMsg::Shutdown);

    let (notes, _rx) = unbounded_channel();
    let config = SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), "read l".into()], cwd: None, env: Vec::new() };
    let resumed = block::spawn_resumed(info("b-handoff", 120), checkpoint, &config, notes).expect("resume");
    assert_eq!(states(&resumed.status.lock().unwrap()), kept, "a fresh shell after a crash: the exit rule ran");
    resumed.send(BlockMsg::Shutdown);
}

const COMMANDS: &str = "i=0; while [ $i -lt 3 ]; do \
    printf '\\033]133;A\\007$ \\033]133;B\\007cmd%s\\r\\n\\033]133;C\\007out%s\\r\\n\\033]133;D;%s\\007MARK-D%s\\r\\n' $i $i $i $i; \
    j=0; while [ $j -lt 4 ]; do echo filler$i.$j; j=$((j+1)); done; i=$((i+1)); done; printf TAIL; read l";

#[tokio::test]
async fn commands_rows_after_a_snapshot_attach_point_at_the_d_lines() {
    let block = Block::sh("b-rows", COMMANDS);
    // A live stream sees each COMMAND_END right after its OUTPUT frame.
    let (tx, mut live) = unbounded_channel();
    block.handle.send(BlockMsg::Attach(Box::new(Attach { sub_id: 1, client_id: 7, resume: None, sink: Sink::new(tx), remote: false })));
    block.wait_for("TAIL").await;
    let records = block.commands().await;
    assert_eq!(records.iter().map(|r| r.exit).collect::<Vec<_>>(), [Some(0), Some(1), Some(2)]);
    assert!(records.iter().all(|r| r.duration_ms.is_some()));
    assert_eq!(block.output(Some(records[1].index)).await.text, "out1");
    assert_eq!(block.output(None).await.text, "out2", "default: the last finished");

    let mut last_output_end = None;
    let mut ends = Vec::new();
    while let Ok(frame) = live.try_recv() {
        match ServerData::from_frame(frame).unwrap() {
            ServerData::Output { offset, bytes } => last_output_end = Some(offset + bytes.len() as u64),
            ServerData::CommandEnd(end) => {
                assert_eq!(Some(end.offset), last_output_end, "right after the output frame holding D");
                ends.push(end.record);
            }
            _ => {}
        }
    }
    assert_eq!(ends, records.iter().map(|r| r.index).collect::<Vec<_>>());

    let (tx, mut rx) = unbounded_channel();
    block.handle.send(BlockMsg::Attach(Box::new(Attach { sub_id: 2, client_id: 8, resume: None, sink: Sink::new(tx), remote: false })));
    let mut anchors: Option<Vec<CommandAnchor>> = None;
    let mut head = false;
    while anchors.is_none() {
        let frame = tokio::time::timeout(T, rx.recv()).await.expect("frames").expect("stream");
        match frame.kind {
            kind::SNAPSHOT_HEAD => head = true,
            kind::COMMANDS => {
                assert!(head, "sent right after the snapshot head");
                let ServerData::Commands(list) = ServerData::from_frame(frame).unwrap() else { unreachable!() };
                anchors = Some(list);
            }
            _ => {}
        }
    }
    let lines: Vec<String> = block.capture().await.text.lines().map(str::to_owned).collect();
    assert_eq!(lines.last().map(String::as_str), Some("TAIL"), "the screen ends at its last row");
    let anchors = anchors.unwrap();
    assert_eq!(anchors.len(), 3);
    for (i, anchor) in anchors.iter().enumerate() {
        let row = lines.iter().position(|l| l == &format!("MARK-D{i}")).expect("D line in capture");
        assert_eq!(anchor.record, records[i].index);
        assert_eq!(anchor.rows_from_bottom as usize, lines.len() - 1 - row, "record {i}");
    }
}

#[tokio::test]
async fn a_reset_clears_the_program_title() {
    let block = Block::sh("b-ris", "printf '\\033]2;custom\\007set'; read a; printf '\\033cafter'; read b");
    block.wait_for("set").await;
    let title = || block.handle.info.lock().unwrap().title.clone();
    assert_eq!(title(), "custom");
    block.input("\n");
    block.wait_for("after").await;
    let deadline = Instant::now() + T;
    while title() == "custom" {
        assert!(Instant::now() < deadline, "title still custom after RIS");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
