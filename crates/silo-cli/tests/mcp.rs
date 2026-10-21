use serde_json::{Value, json};
use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    model::BlockKind,
    paths,
};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SHELL: &str = r#"printf '\033]133;A\007$ '; while IFS= read -r l; do printf '\033]133;C\007'; sh -c "$l"; s=$?; printf '\033]133;D;%d\007\033]133;A\007$ ' "$s"; done"#;

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_round_trip() {
    scenario(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn report_inside_block_wakes_wait() {
    scenario(true).await;
}

async fn scenario(report_status: bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let home = std::path::PathBuf::from(format!("/tmp/silo-mcp-{}-{report_status}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let config = silo_server::Config { home: home.clone(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
        let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
        while !silo_client::server_running(&home).await { tokio::time::sleep(Duration::from_millis(10)).await; }
        let (control, _) = Control::connect(&paths::socket(&home), "mcp-test").await.unwrap();
        let created: Created = control.call(Method::SessionCreate { name: None, spawn: SpawnSpec { command: vec!["/bin/sh".into(), "-c".into(), SHELL.into()], ..Default::default() }, kind: BlockKind::Terminal, url: None }).await.unwrap();
        let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_silo")).arg("mcp").env("SILO_HOME", &home).env_remove("SILO_TOKEN").env_remove("SILO_BLOCK").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true).spawn().unwrap();
        let mut stdin = process.stdin.take().unwrap();
        let mut stdout = BufReader::new(process.stdout.take().unwrap()).lines();
        let requests = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"terminal_run","arguments":{"block_id":created.block_id,"command":"echo mcp-output; exit 4"}}}),
        ];
        for request in requests {
            stdin.write_all(format!("{request}\n").as_bytes()).await.unwrap(); stdin.flush().await.unwrap();
            let response: Value = serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(response["id"], request["id"]);
            assert!(response.get("error").is_none(), "{response}");
            match request["id"].as_u64().unwrap() {
                1 => assert_eq!(response["result"]["protocolVersion"], "2025-06-18"),
                2 => assert!(response["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "terminal_run")),
                _ => { assert_eq!(response["result"]["isError"], false, "{response}"); let result: Value = serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap(); assert_eq!(result["exit"], 4); assert!(result["output"].as_str().unwrap().contains("mcp-output")); }
            }
        }
        drop(stdin);
        assert!(process.wait().await.unwrap().success());
        let report = tokio::process::Command::new(env!("CARGO_BIN_EXE_silo")).args(["report", "done"]).env_remove("SILO_BLOCK").env("SILO_HOME", &home).output().await.unwrap();
        assert!(report.status.success()); assert!(report.stdout.is_empty() && report.stderr.is_empty());
        if report_status {
        let (wait_control, events) = Control::connect(&paths::socket(&home), "report-wait-test").await.unwrap();
        let tools = silo_tools::Tools::new(wait_control, events, silo_tools::Context { agent_block: None, ..Default::default() });
        let wait = tools.call("terminal_wait", json!({"block_id":created.block_id,"timeout_ms":5000}), tokio_util::sync::CancellationToken::new());
        let send = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
            let text = format!("SILO_HOME={} {} report done --app test; sleep 1\r", quote(&home.to_string_lossy()), quote(env!("CARGO_BIN_EXE_silo")));
            control.call_value(Method::TerminalSend { block_id: created.block_id.clone(), text }).await.unwrap();
        };
        let (waited, ()) = tokio::join!(wait, send);
        let waited = waited.unwrap();
        assert!(matches!(&waited.content[0], silo_tools::Part::Json(value) if value["reason"] == "status"), "{waited:?}");
        }
        control.call_value(Method::ServerShutdown { force: true }).await.unwrap(); server.await.unwrap();
        std::fs::remove_dir_all(home).unwrap();
    }).await.unwrap();
}
