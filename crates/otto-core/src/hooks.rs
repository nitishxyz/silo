//! Command-hook subset of https://code.claude.com/docs/en/hooks.
use crate::{Error, Result, session::Context};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[derive(Default)]
pub(crate) struct Outcome {
    pub deny: Option<String>,
    pub input: Option<Value>,
    pub context: Vec<String>,
}
pub(crate) async fn run(cx: &Context, event: &str, tool: &str, input: Value) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    for path in [cx.runtime.0.config.user_config_dir.join("hooks.json"), cx.root.join(".otto/hooks.json")] {
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let config: Value = serde_json::from_slice(&bytes)?;
        let alias = match event {
            "SessionStart" => "session_start",
            "PreToolUse" => "pre_tool",
            "PostToolUse" => "post_tool",
            "PreCompact" => "pre_compact",
            _ => "stop",
        };
        let Some(groups) = config["hooks"].get(event).or_else(|| config["hooks"].get(alias)).and_then(Value::as_array) else { continue };
        for group in groups {
            let matcher = group["matcher"].as_str().unwrap_or("*");
            if !matcher.split('|').any(|m| m == "*" || m.is_empty() || m == tool) {
                continue;
            }
            for hook in group["hooks"].as_array().into_iter().flatten() {
                if hook["type"] != "command" {
                    continue;
                }
                let command = hook["command"].as_str().ok_or_else(|| Error::Invalid("hook command missing".into()))?;
                let payload = json!({"session_id":cx.row.id,"cwd":cx.row.cwd,"project_root":cx.root,"hook_event_name":event,"tool_name":tool,"tool_input":input});
                let result = command_output(command, &cx.row.cwd, payload, hook["timeout"].as_u64().unwrap_or(30).clamp(1, 300)).await?;
                let output = result.get("hookSpecificOutput").unwrap_or(&result);
                if output["permissionDecision"] == "deny" || result["decision"] == "block" || result["continue"] == false {
                    outcome.deny =
                        Some(output["permissionDecisionReason"].as_str().or_else(|| result["reason"].as_str()).unwrap_or("blocked by hook").into());
                }
                if let Some(input) = output.get("updatedInput") {
                    outcome.input = Some(input.clone());
                }
                if let Some(context) = output["additionalContext"].as_str().or_else(|| result["systemMessage"].as_str()) {
                    outcome.context.push(context.into());
                }
            }
        }
    }
    Ok(outcome)
}
async fn command_output(command: &str, cwd: &str, input: Value, timeout: u64) -> Result<Value> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or(Error::Closed)?;
    let stdout = child.stdout.take().ok_or(Error::Closed)?;
    let result = tokio::time::timeout(Duration::from_secs(timeout), async {
        stdin.write_all(serde_json::to_string(&input)?.as_bytes()).await?;
        drop(stdin);
        let mut output = Vec::new();
        stdout.take(65537).read_to_end(&mut output).await?;
        if output.len() > 65536 {
            return Err(Error::Invalid("hook output exceeds 64 KiB".into()));
        }
        let status = child.wait().await?;
        if status.code() == Some(2) {
            return Ok(json!({"decision":"block","reason":"hook exited with status 2"}));
        }
        if !status.success() {
            return Err(Error::Invalid(format!("hook failed: {status}")));
        }
        if output.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&output).unwrap_or_else(|_| json!({"systemMessage":String::from_utf8_lossy(&output)})))
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            Err(Error::Invalid("hook timed out".into()))
        }
    }
}
pub(crate) fn expand(root: &Path, user: &Path, text: &str) -> Result<String> {
    let Some(rest) = text.strip_prefix('/') else { return Ok(text.into()) };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Ok(text.into());
    }
    for path in [root.join(".otto/commands").join(format!("{name}.md")), user.join("commands").join(format!("{name}.md"))] {
        match std::fs::read_to_string(path) {
            Ok(template) => return Ok(crate::agents::frontmatter(&template).1.replace("$ARGUMENTS", args)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(text.into())
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn json_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let output = super::command_output(
            "cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\"}}'",
            cwd,
            serde_json::json!({}),
            2,
        )
        .await
        .unwrap();
        assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(super::command_output("sleep 3", cwd, serde_json::json!({}), 1).await.is_err());
    }
    #[test]
    fn command_template() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".otto/commands")).unwrap();
        std::fs::write(dir.path().join(".otto/commands/review.md"), "Review $ARGUMENTS").unwrap();
        assert_eq!(super::expand(dir.path(), dir.path(), "/review src").unwrap(), "Review src");
    }
}
