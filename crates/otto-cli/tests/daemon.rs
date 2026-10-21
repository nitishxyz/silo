use otto_client::Connection;
use otto_llm::{CallKind, FinishReason, Part, StreamEvent};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn command(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_otto-silo"));
    c.env("OTTO_SILO_HOME", home)
        .env("OTTO_SILO_CONFIG_DIR", home.join("config"))
        .env("OTTO_SILO_OFFLINE", "1")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OTTO_SILO_MODEL")
        .env_remove("OTTO_SILO_TEST_SCRIPT")
        .stdin(Stdio::null());
    c
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn start(home: &Path, script: Option<&Path>) -> Server {
    let mut c = command(home);
    c.args(["server", "--foreground"]).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(script) = script {
        c.env("OTTO_SILO_TEST_SCRIPT", script);
    }
    let mut server = Server(c.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Connection::connect(home).await.is_ok() {
            return server;
        }
        assert!(server.0.try_wait().unwrap().is_none(), "server exited during startup");
        assert!(Instant::now() < deadline, "server startup timeout");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
fn script(path: &Path, tool: &str, input: serde_json::Value) {
    let steps = vec![
        vec![
            StreamEvent::PartDone {
                part: Part::ToolCall { call_id: "call-1".into(), name: tool.into(), input, kind: CallKind::Function, opaque: None },
            },
            StreamEvent::Finish { reason: FinishReason::ToolUse, response_id: None },
        ],
        vec![
            StreamEvent::TextDelta { text: "The files are listed.".into() },
            StreamEvent::PartDone { part: Part::text("The files are listed.") },
            StreamEvent::Finish { reason: FinishReason::Stop, response_id: None },
        ],
    ];
    std::fs::write(path, serde_json::to_vec(&serde_json::json!({"steps": steps})).unwrap()).unwrap();
}
async fn output(mut c: Command) -> std::process::Output {
    c.stdin(Stdio::null());
    tokio::time::timeout(Duration::from_secs(15), tokio::process::Command::from(c).kill_on_drop(true).output()).await.unwrap().unwrap()
}
fn args(home: &Path, args: &[&str]) -> Command {
    let mut c = command(home);
    c.args(args);
    c
}

#[tokio::test]
async fn lifecycle_and_tool_turn_with_second_client() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script_path = home.join("script.json");
    script(&script_path, "ls", serde_json::json!({"path":"."}));
    let mut server = start(home, Some(&script_path)).await;
    assert!(output(args(home, &["sessions"])).await.status.success());
    assert!(output(args(home, &["server", "status"])).await.status.success());
    let connection = Connection::connect(home).await.unwrap();
    let session = connection
        .session_create(otto_client::SessionCreateOptions {
            project_root: Some(std::env::current_dir().unwrap().display().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    let live_path = home.join("attached.txt");
    let mut live =
        Server(args(home, &["attach", &session.id]).stdout(std::fs::File::create(&live_path).unwrap()).stderr(Stdio::null()).spawn().unwrap());
    let turn = output(args(home, &["run", "--session", &session.id, "--approve", "auto", "list files"])).await;
    assert!(turn.status.success(), "{}", String::from_utf8_lossy(&turn.stderr));
    assert!(String::from_utf8_lossy(&turn.stdout).contains("The files are listed."));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&live_path).unwrap().contains("The files are listed.") {
            break;
        }
        assert!(Instant::now() < deadline, "second live client missed transcript");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    live.0.kill().unwrap();
    live.0.wait().unwrap();
    let sessions = connection.session_list(None, false).await.unwrap();
    let id = &sessions[0].id;
    let view = connection.session_view(id).await.unwrap();
    assert!(view.items.iter().any(|item| item.parts.iter().any(|part| matches!(part, Part::ToolResult { is_error: false, .. }))));
    let attached = output(args(home, &["attach", id, "--once"])).await;
    assert!(attached.status.success());
    assert!(String::from_utf8_lossy(&attached.stdout).contains("The files are listed."));
    assert!(output(args(home, &["session", "rename", id, "renamed"])).await.status.success());
    assert!(output(args(home, &["session", "fork", id])).await.status.success());
    assert!(output(args(home, &["cancel", id])).await.status.success());
    let json = output(args(home, &["run", "--json", "--approve", "auto", "list files"])).await;
    assert!(json.status.success(), "{}", String::from_utf8_lossy(&json.stderr));
    assert!(!json.stdout.is_empty());
    for line in String::from_utf8(json.stdout).unwrap().lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    assert!(output(args(home, &["server", "stop"])).await.status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!otto_proto::paths::socket(home).exists());
}

#[tokio::test]
async fn crash_mid_tool_recovers_unknown_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script_path = home.join("script.json");
    script(&script_path, "shell", serde_json::json!({"command":"sleep 3", "timeout_ms":5000}));
    let mut server = start(home, Some(&script_path)).await;
    let mut runner = Server(args(home, &["run", "--approve", "auto", "slow task"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let connection = Connection::connect(home).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let id = loop {
        let sessions = connection.session_list(None, false).await.unwrap();
        if let Some(session) = sessions.first() {
            let view = connection.session_view(&session.id).await.unwrap();
            if !view.running_tool_calls.is_empty() && view.pending_approvals.is_empty() {
                break session.id.clone();
            }
        }
        assert!(Instant::now() < deadline, "tool never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    runner.0.kill().unwrap();
    runner.0.wait().unwrap();
    let _restarted = start(home, Some(&script_path)).await;
    assert!(output(args(home, &["sessions", "--all"])).await.status.success());
    let attached = output(args(home, &["attach", &id, "--once"])).await;
    assert!(attached.status.success());
    let text = format!("{}{}", String::from_utf8_lossy(&attached.stdout), String::from_utf8_lossy(&attached.stderr)).to_lowercase();
    assert!(text.contains("outcome unknown"), "{text}");
    assert!(output(args(home, &["server", "stop"])).await.status.success());
}

#[tokio::test]
async fn autostart_missing_auth_and_sigterm_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    assert!(output(args(home, &["sessions"])).await.status.success());
    let connection = Connection::connect(home).await.unwrap();
    let info: otto_proto::ServerInfo = connection.call_typed(otto_proto::Method::ServerInfo {}).await.unwrap();
    let result = output(args(home, &["run", "hello"])).await;
    let stopped = output(args(home, &["server", "stop"])).await;
    assert_eq!(result.status.code(), Some(2), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(String::from_utf8_lossy(&result.stderr).contains("otto-silo auth login"));
    assert!(stopped.status.success());
    assert!(info.pid > 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while otto_proto::paths::socket(home).exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut server = start(home, None).await;
    assert!(Command::new("kill").args(["-TERM", &server.0.id().to_string()]).status().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "SIGTERM cleanup timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!otto_proto::paths::socket(home).exists());
}

#[tokio::test]
async fn noninteractive_approval_denial_and_ctrl_c_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script_path = home.join("script.json");
    script(&script_path, "shell", serde_json::json!({"command":"sleep 3", "timeout_ms":5000}));
    assert!(!home.join("config.json").exists());
    let _server = start(home, Some(&script_path)).await;
    let denied = output(args(home, &["run", "--approve", "ask", "slow task"])).await;
    assert!(denied.status.success(), "{}", String::from_utf8_lossy(&denied.stderr));
    assert!(String::from_utf8_lossy(&denied.stderr).contains("approval requires a TTY"));
    let connection = Connection::connect(home).await.unwrap();
    let session = connection
        .session_create(otto_client::SessionCreateOptions {
            project_root: Some(std::env::current_dir().unwrap().display().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    let changed = output(args(home, &["session", "mode", &session.id, "ask-all"])).await;
    assert!(changed.status.success(), "{}", String::from_utf8_lossy(&changed.stderr));
    assert_eq!(connection.session_view(&session.id).await.unwrap().info.approval_mode, otto_proto::ApprovalMode::AskAll);
    let mut runner = Server(
        args(home, &["run", "--session", &session.id, "--approve", "auto", "slow task"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let view = connection.session_view(&session.id).await.unwrap();
        if !view.running_tool_calls.is_empty() && view.pending_approvals.is_empty() {
            assert_eq!(view.info.approval_mode, otto_proto::ApprovalMode::Auto);
            break;
        }
        assert!(Instant::now() < deadline, "tool never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(Command::new("kill").args(["-INT", &runner.0.id().to_string()]).status().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = runner.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(130));
            break;
        }
        assert!(Instant::now() < deadline, "run did not exit on Ctrl-C");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(output(args(home, &["server", "stop"])).await.status.success());
}
