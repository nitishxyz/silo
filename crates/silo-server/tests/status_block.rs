//! Block status end to end on real PTYs (Phase 7 P7-3): the OSC 7501
//! support query, status events that never bump the revision, busy
//! detection, handoff and restore, and command rows.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use silo_proto::{
    Frame,
    control::{AttentionReason, Capture, CommandOutput, Event, ServerMessage, SpawnSpec},
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
        owner_block: None,
        pid: None,
        browser: None,
        agent: None,
        files: None,
        changes: None,
    }
}

struct Block {
    handle: BlockHandle,
    notes: UnboundedReceiver<Note>,
}

impl Block {
    fn spawn(id: &str, columns: u16, command: &[&str]) -> Self {
        let command: Vec<std::ffi::OsString> = command.iter().map(Into::into).collect();
        Self::spawn_env(id, columns, command, Vec::new())
    }

    fn spawn_env(id: &str, columns: u16, command: Vec<std::ffi::OsString>, env: Vec<(std::ffi::OsString, std::ffi::OsString)>) -> Self {
        let (notes, rx) = unbounded_channel();
        let config = SpawnConfig { command, cwd: Some(std::env::temp_dir()), env };
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

    /// A stream attached before any marks: the program waits on `read go`
    /// and gets its line only after the actor has taken the attach.
    fn attach_then_go(&self) -> UnboundedReceiver<Frame> {
        let (tx, live) = unbounded_channel();
        self.handle.send(BlockMsg::Attach(Box::new(Attach { sub_id: 1, client_id: 7, resume: None, sink: Sink::new(tx), remote: false })));
        self.input("\n");
        live
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

    // A script's shell keeps the foreground and runs `sleep` in its own
    // group (dash does even for `sh -c 'sleep 30'`).
    let mut script = Block::sh("b-script", "sleep 30; true");
    assert!(script.handle.command);
    script.until("script busy", |s| busy_process(s) == Some("sleep")).await;
    assert_eq!(script.busy().await.map(|b| b.process), Some("sleep".to_owned()));

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
    let created = reg.create_session(None, &New { spawn: &spawn, kind: BlockKind::Terminal, url: None, agent: None }).unwrap();
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

#[tokio::test]
async fn a_checkpoint_from_before_status_still_restores() {
    let mut block = Block::sh("b-old", RECORDS);
    block.until("four records", |s| s.records.len() == 4).await;
    let (reply, rx) = tokio::sync::oneshot::channel();
    block.handle.send(BlockMsg::Checkpoint { reply });
    let checkpoint = rx.await.expect("checkpoint").expect("captured");
    let mut json = serde_json::to_value(&checkpoint).unwrap();
    assert!(json.as_object_mut().unwrap().remove("status").is_some());
    let old: block::Checkpoint = serde_json::from_value(json).expect("an old checkpoint parses");
    assert_eq!(old.status, None);
    let (notes, _rx) = unbounded_channel();
    let restored = block::spawn_restored(info("b-old", 120), old, notes).expect("restore");
    assert!(restored.status.lock().unwrap().records.is_empty());
    restored.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn a_final_state_right_after_progress_is_never_coalesced_away() {
    // Progress ticks, then working -> done well inside one 250 ms window.
    let script = "printf '\\033]7501;state=working:id=w\\033\\\\'; printf '\\033]9;4;1;10\\007'; printf '\\033]9;4;1;20\\007'; \
                  printf '\\033]7501;state=working:id=w:progress=50\\033\\\\'; printf '\\033]7501;state=done:id=w\\033\\\\'; read l";
    let mut block = Block::sh("b-final", script);
    let done = block.until("done", |s| states(s) == [("w".to_owned(), StatusState::Done)]).await;
    assert_eq!(done.progress.and_then(|p| p.percent), Some(20), "the last progress goes out with it");
}

const COMMANDS: &str = "read go; i=0; while [ $i -lt 3 ]; do \
    printf '\\033]133;A\\007$ \\033]133;B\\007cmd%s\\r\\n\\033]133;C\\007out%s\\r\\n\\033]133;D;%s\\007MARK-D%s\\r\\n' $i $i $i $i; \
    j=0; while [ $j -lt 4 ]; do echo filler$i.$j; j=$((j+1)); done; i=$((i+1)); done; printf TAIL; read l";

#[tokio::test]
async fn commands_rows_after_a_snapshot_attach_point_at_the_d_lines() {
    let block = Block::sh("b-rows", COMMANDS);
    // A live stream sees each COMMAND_END right after its OUTPUT frame.
    let mut live = block.attach_then_go();
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

/// The marks of a D with no command behind it, as shells send them.
#[tokio::test]
async fn a_d_without_a_c_makes_no_record_and_never_flips_busy() {
    let cases = [
        // bash 3.2 (no PS0): every prompt after the first ends with D, an
        // empty line included.
        ("b-d-bash32", "A\\007$ \\033]133;B\\007\\r\\n\\033]133;D;0\\007\\033]133;A\\007$ \\033]133;B\\007\\r\\n\\033]133;D;0", 0),
        // fish runs fish_postexec for `exit`: a trailing D after the last A.
        ("b-d-fish", "A\\007$ \\033]133;C;cmdline_url=ls\\007out\\r\\n\\033]133;D;0\\007\\033]133;A\\007$ exit\\r\\n\\033]133;D;0", 1),
        // A user integration hooked in after the check: every D twice.
        ("b-d-double", "A\\007$ \\033]133;C;cmdline_url=ls\\007out\\r\\n\\033]133;D;0\\007\\033]133;D;0\\007\\033]133;A\\007$ \\033]133;D;0", 1),
    ];
    for (id, marks, expected) in cases {
        let block = Block::sh(id, &format!("read go; printf '\\033]133;{marks}\\007DONE'; read l"));
        let mut live = block.attach_then_go();
        block.wait_for("DONE").await;
        let records = block.commands().await;
        assert_eq!(records.len(), expected, "{id}: {records:?}");
        assert!(records.iter().all(|r| r.command.as_deref() == Some("ls") && r.started_at.is_some()), "{id}: nothing bogus: {records:?}");
        let mut ends = 0;
        while let Ok(frame) = live.try_recv() {
            ends += usize::from(matches!(ServerData::from_frame(frame), Ok(ServerData::CommandEnd(_))));
        }
        assert_eq!(ends, expected, "{id}: one COMMAND_END per record");
        assert_eq!(block.status().busy, None, "{id}");
        assert_eq!(block.busy().await, None, "{id}");
    }
}

/// Silo's own wrapper around a real shell, in a sandboxed HOME.
fn hooked(id: &str, program: &std::path::Path) -> (Block, PathBuf) {
    let dir = PathBuf::from(format!("/tmp/silo-dmark-{id}-{}", std::process::id()));
    let (home, user) = (dir.join("silo"), dir.join("user"));
    std::fs::create_dir_all(&user).unwrap();
    silo_server::shell::install(&home, &[]).unwrap();
    let (args, mut env) = silo_server::shell::hook(silo_server::shell::Shell::of(program).unwrap(), &home);
    for (key, value) in [
        ("HOME", user.clone()),
        ("PATH", "/usr/bin:/bin".into()),
        ("XDG_CONFIG_HOME", user.join(".config")),
        ("XDG_DATA_HOME", user.join(".local/share")),
        ("XDG_CACHE_HOME", user.join(".cache")),
        ("SILO_ZDOTDIR", PathBuf::new()),
        ("PROMPT_COMMAND", PathBuf::new()),
        ("BASH_SILENCE_DEPRECATION_WARNING", "1".into()),
    ] {
        env.push((key.into(), value.into_os_string()));
    }
    let command = std::iter::once(program.as_os_str().to_owned()).chain(args).collect();
    (Block::spawn_env(id, 120, command, env), dir)
}

/// `program`'s builds: `SILO_TEST_SHELL_DIRS` first, then the usual dirs.
fn shells(program: &str) -> Vec<PathBuf> {
    let extra = std::env::var_os("SILO_TEST_SHELL_DIRS").unwrap_or_default();
    let dirs = std::env::split_paths(&extra).filter(|d| !d.as_os_str().is_empty());
    let (mut seen, mut found) = (Vec::new(), Vec::new());
    for path in dirs.chain(["/bin", "/usr/bin", "/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from)).map(|d| d.join(program)) {
        if let Ok(real) = std::fs::canonicalize(&path)
            && !seen.contains(&real)
        {
            seen.push(real);
            found.push(path);
        }
    }
    found
}

fn version_of(program: &std::path::Path) -> String {
    let out = std::process::Command::new(program).arg("--version").output().map(|o| o.stdout).unwrap_or_default();
    String::from_utf8_lossy(&out).lines().next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn bash_3_2_empty_lines_make_no_records() {
    let old: Vec<PathBuf> = shells("bash").into_iter().filter(|p| version_of(p).contains("version 3.")).collect();
    if old.is_empty() {
        eprintln!("skipped: no bash 3.2");
    }
    for program in old {
        let (block, dir) = hooked("b-bash32", &program);
        block.input("PS1='P$ '\n");
        block.wait_for("P$").await;
        for _ in 0..3 {
            block.input("\n");
        }
        block.input("echo E''ND\n");
        block.wait_for("END").await;
        block.wait_for("END\nP$").await;
        // bash 3.2 sends no C, so even `echo` is a D with nothing behind it.
        assert_eq!(block.commands().await, [], "{}: no records from bare Ds", program.display());
        assert_eq!(block.busy().await, None);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test]
async fn fish_exit_makes_no_bogus_record() {
    let found = shells("fish");
    if found.is_empty() {
        eprintln!("skipped: no fish (set SILO_TEST_SHELL_DIRS)");
    }
    for program in found {
        let (block, dir) = hooked("b-fish", &program);
        block.input("function fish_prompt; printf 'F> '; end\n");
        block.wait_for("F>").await;
        block.input("echo E''ND\n");
        block.wait_for("END").await;
        block.input("exit\n");
        let deadline = Instant::now() + T;
        while !block_exited(&block) {
            assert!(Instant::now() < deadline, "{}: fish never exited", program.display());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let records = block.commands().await;
        assert!(records.iter().all(|r| r.command.is_some() && r.started_at.is_some()), "{}: {records:?}", program.display());
        // fish sends C for `exit` too, so its D ends a real record: once.
        let ran: Vec<&str> = records.iter().filter_map(|r| r.command.as_deref()).collect();
        assert_eq!(ran, ["function fish_prompt; printf 'F> '; end", "echo E''ND", "exit"], "{}", program.display());
        assert_eq!(block.status().busy, None);
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn block_exited(block: &Block) -> bool {
    block.handle.info.lock().unwrap().lifecycle == Lifecycle::Exited
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

/// Alternates blocked and done on one record, each in its own write, then
/// ends on error.
const FLOOD: &str = r#"$|=1; for my $i (1..1500) { print "\e]7501;state=", ($i % 2 ? "blocked" : "done"), ":id=w\e\\"; select(undef, undef, undef, 0.001) } print "\e]7501;state=error:id=w\e\\FLOODED"; sleep 30"#;

#[tokio::test]
async fn a_status_flood_is_paced_and_its_last_state_still_arrives() {
    let started = Instant::now();
    let mut block = Block::spawn("b-flood", 80, &["perl", "-e", FLOOD]);
    let (mut statuses, mut attentions) = (0_u32, Vec::new());
    let deadline = started + T;
    while !(states(&block.status()) == [("w".to_owned(), StatusState::Error)] && attentions.last() == Some(&AttentionReason::Error)) {
        match tokio::time::timeout_at(deadline.into(), block.notes.recv()).await {
            Ok(Some(Note::Status(_))) => statuses += 1,
            Ok(Some(Note::Attention { reason, .. })) => attentions.push(reason),
            Ok(Some(_)) => {}
            _ => panic!("never saw the final state: {:?} after {statuses} statuses, {attentions:?}", block.status()),
        }
    }
    block.wait_for("FLOODED").await;
    let secs = started.elapsed().as_secs_f64();
    assert!(secs > 1.0, "the flood took a while: {secs}");
    // 8 at once then 4/s, busy included; 5 at once then 1/s (plus one each for rounding).
    assert!(f64::from(statuses) <= 9.0 + 4.0 * secs, "{statuses} block.status in {secs:.1} s");
    assert!(attentions.len() as f64 <= 6.0 + secs, "{} block.attention in {secs:.1} s: {attentions:?}", attentions.len());
}

#[tokio::test]
async fn lost_hook_events_start_the_status_over_from_what_is_left() {
    let mut block = Block::sh("b-lost", "printf ready; read l");
    block.wait_for("ready").await;
    block.handle.send(BlockMsg::Output(b"\x1b]7501;state=done:id=old\x1b\\\x1b]133;C;cmdline_url=read\x07".to_vec()));
    block.until("old record, command running", |s| states(s) == [("old".to_owned(), StatusState::Done)] && s.busy.is_some()).await;
    // More events in one chunk than the queue keeps (1024): the oldest go.
    let mut flood = b"\x1b]7501;state=working:id=gone\x1b\\".to_vec();
    for _ in 0..1100 {
        flood.extend_from_slice(b"\x1b]9;4;1;50\x07");
    }
    flood.extend_from_slice(b"\x1b]7501;state=working:id=new\x1b\\");
    block.handle.send(BlockMsg::Output(flood));
    let status = block.until("only what was left", |s| states(s) == [("new".to_owned(), StatusState::Working)]).await;
    assert_eq!(status.progress.and_then(|p| p.percent), Some(50), "the surviving progress");
    assert_eq!(status.busy, None, "the open command went with the lost events");
    assert!(block.commands().await.is_empty(), "no half-seen command was recorded");
}

#[tokio::test]
async fn command_output_is_capped_to_its_end() {
    // 800 lines of 1400 bytes: over 1 MiB, but well inside the scrollback.
    let script = r#"$|=1; print "\e]133;A\a$ \e]133;B\abig\r\n\e]133;C;cmdline_url=big\a"; for my $i (0..799) { printf "%04d%s\r\n", $i, "x" x 1396 } print "\e]133;D;0\aDONE"; sleep 30"#;
    let block = Block::spawn("b-big", 500, &["perl", "-e", script]);
    block.wait_for("DONE").await;
    let output = block.output(None).await;
    assert!(output.truncated, "over the cap: {} bytes, starts {:?}", output.text.len(), &output.text[..output.text.len().min(60)]);
    assert!(output.text.len() <= silo_proto::control::MAX_COMMAND_OUTPUT, "{}", output.text.len());
    assert!(output.text.len() > silo_proto::control::MAX_COMMAND_OUTPUT - 1500, "{}", output.text.len());
    assert!(output.text.ends_with(&format!("0799{}", "x".repeat(1396))), "the end is kept");
    assert!(output.text.starts_with(|c: char| c.is_ascii_digit()), "from a line start");

    // The start scrolled out of the scrollback (10000 lines): the end is left.
    let script = r#"$|=1; print "\e]133;A\a$ \e]133;B\aseq\r\n\e]133;C;cmdline_url=seq\a"; print "line$_\r\n" for 0..14999; print "\e]133;D;0\aDONE"; sleep 30"#;
    let block = Block::spawn("b-pruned", 80, &["perl", "-e", script]);
    block.wait_for("DONE").await;
    let output = block.output(None).await;
    assert!(output.truncated, "its start was pruned: {:?}", (output.text.len(), &output.record));
    assert!(output.text.ends_with("line14998\nline14999"), "{:?}", &output.text[output.text.len().saturating_sub(40)..]);
    assert!(!output.text.contains("line0\n"));

    let small =
        Block::spawn("b-small", 80, &["perl", "-e", r#"$|=1; print "\e]133;A\a$ \e]133;B\aone\r\n\e]133;C\aone\r\n\e]133;D;0\aDONE"; sleep 30"#]);
    small.wait_for("DONE").await;
    let output = small.output(None).await;
    assert_eq!((output.text.as_str(), output.truncated), ("one", false));
}
