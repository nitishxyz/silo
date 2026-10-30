//! Line-delimited JSON-RPC over a child process's stdin and stdout.
use crate::{
    client::{CallError, Client, Hook, Roots, Transport},
    config::ServerConfig,
};
use serde_json::Value;
use std::sync::{Arc, Weak};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

impl Client {
    pub(crate) async fn write_line(&self, message: &Value) -> Result<(), CallError> {
        let Transport::Stdio { stdin, .. } = &self.transport else { return Err(CallError::Closed("not a stdio server".into())) };
        let mut stdin = stdin.lock().await;
        let stdin = stdin.as_mut().ok_or_else(|| CallError::Closed("stdin closed".into()))?;
        stdin.write_all(format!("{message}\n").as_bytes()).await.map_err(|e| CallError::Closed(e.to_string()))?;
        stdin.flush().await.map_err(|e| CallError::Closed(e.to_string()))
    }
}

pub(crate) fn spawn(name: &str, config: &ServerConfig, roots: Roots, hook: Hook) -> Result<Arc<Client>, String> {
    let command = config.command.as_deref().ok_or("no command configured")?;
    let mut process = tokio::process::Command::new(command);
    process
        .args(&config.args)
        .envs(&config.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &config.cwd {
        process.current_dir(cwd);
    }
    let mut child = process.spawn().map_err(|e| format!("cannot start {command}: {e}"))?;
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let transport = Transport::Stdio { stdin: tokio::sync::Mutex::new(stdin), child: tokio::sync::Mutex::new(child) };
    let client = Client::new(transport, name, roots, hook);
    let weak: Weak<Client> = Arc::downgrade(&client);
    if let Some(stdout) = stdout {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(client) = weak.upgrade() else { return };
                match serde_json::from_str::<Value>(&line) {
                    Ok(message) => client.incoming(message).await,
                    Err(_) => tracing::debug!(server=%client.name, line, "non-JSON output from MCP server"),
                }
            }
            if let Some(client) = weak.upgrade() {
                client.close("process exited");
            }
        });
    }
    if let Some(stderr) = stderr {
        let name = name.to_owned();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(server=%name, line, "MCP server stderr");
            }
        });
    }
    Ok(client)
}
