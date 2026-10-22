//! Command hooks in Claude Code's format (https://code.claude.com/docs/en/hooks).
//!
//! Sources, all merged: user `hooks.json`, project `.claude/settings.json`,
//! `.claude/settings.local.json` and `.otto/hooks.json` (their `hooks` key).
use crate::{Error, Result, session::Context};
use regex::Regex;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// Blocked (`decision: "block"`, `permissionDecision: "deny"`, exit 2), with the reason.
    pub deny: Option<String>,
    /// `permissionDecision: "ask"`: confirm with the user even when policy allows.
    pub ask: bool,
    /// `updatedInput` (PreToolUse).
    pub input: Option<Value>,
    /// Text for the model: `additionalContext`, or plain stdout of
    /// SessionStart / UserPromptSubmit hooks.
    pub context: Vec<String>,
    /// `continue: false`: stop the turn, with `stopReason`.
    pub stop: Option<String>,
    /// `systemMessage`: shown to the user, not the model.
    pub notices: Vec<String>,
}

fn alias(event: &str) -> &str {
    match event {
        "SessionStart" => "session_start",
        "SessionEnd" => "session_end",
        "UserPromptSubmit" => "user_prompt_submit",
        "PreToolUse" => "pre_tool",
        "PostToolUse" => "post_tool",
        "Notification" => "notification",
        "Stop" => "stop",
        "SubagentStop" => "subagent_stop",
        "PreCompact" => "pre_compact",
        other => other,
    }
}

/// Claude Code's name for a built-in otto tool, so ported matchers apply.
fn claude_name(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "shell" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" | "apply_patch" => "Edit",
        "multiedit" => "MultiEdit",
        "glob" => "Glob",
        "search" => "Grep",
        "ls" => "LS",
        "web_fetch" => "WebFetch",
        "update_todos" => "TodoWrite",
        "subagent" => "Task",
        _ => return None,
    })
}

/// Empty and `*` match everything; anything else is an anchored regex.
pub(crate) fn matches(matcher: &str, value: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let names = [Some(value), claude_name(value)];
    match Regex::new(&format!("^(?:{matcher})$")) {
        Ok(re) => names.into_iter().flatten().any(|n| re.is_match(n)),
        Err(_) => matcher.split('|').any(|m| names.iter().flatten().any(|n| *n == m)),
    }
}

async fn configs(root: &Path, user: &Path) -> Result<Vec<Value>> {
    let mut configs = Vec::new();
    for path in [user.join("hooks.json"), root.join(".claude/settings.json"), root.join(".claude/settings.local.json"), root.join(".otto/hooks.json")]
    {
        match tokio::fs::read(&path).await {
            Ok(bytes) => configs.push(serde_json::from_slice::<Value>(&bytes).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(configs)
}

/// Run every hook for `event` whose matcher accepts `target` (the tool name,
/// SessionStart source, or PreCompact trigger). `fields` join the common stdin.
pub(crate) async fn run(cx: &Context, event: &str, target: &str, fields: Value) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    let mode = cx.state(|s| s.policy.mode).await.ok();
    for config in configs(&cx.root, &cx.runtime.0.config.user_config_dir).await? {
        let Some(groups) = config["hooks"].get(event).or_else(|| config["hooks"].get(alias(event))).and_then(Value::as_array) else { continue };
        for group in groups {
            if !matches(group["matcher"].as_str().unwrap_or(""), target) {
                continue;
            }
            for hook in group["hooks"].as_array().into_iter().flatten() {
                if hook["type"] != "command" {
                    continue;
                }
                let command = hook["command"].as_str().ok_or_else(|| Error::Invalid("hook command missing".into()))?;
                let mut payload = json!({
                    "session_id":cx.row.id,"cwd":cx.row.cwd,"project_root":cx.root,"hook_event_name":event,
                    "transcript_path":Value::Null,"permission_mode":mode.map(|m|serde_json::to_value(m).unwrap_or_default()),
                });
                if let (Some(payload), Some(fields)) = (payload.as_object_mut(), fields.as_object()) {
                    payload.extend(fields.clone());
                }
                let timeout = hook["timeout"].as_u64().unwrap_or(60).clamp(1, 600);
                let env = [
                    ("CLAUDE_PROJECT_DIR", cx.root.to_string_lossy().into_owned()),
                    ("OTTO_PROJECT_DIR", cx.root.to_string_lossy().into_owned()),
                    ("OTTO_SESSION_ID", cx.row.id.clone()),
                ];
                let (code, stdout, stderr) = match command_output(command, &cx.row.cwd, &env, payload, timeout).await {
                    Ok(output) => output,
                    Err(error) => {
                        tracing::warn!(session=%cx.row.id, event, %error, "hook failed");
                        outcome.notices.push(format!("{event} hook failed: {error}"));
                        continue;
                    }
                };
                apply(&mut outcome, event, code, &stdout, &stderr);
            }
        }
    }
    Ok(outcome)
}

/// Exit 0: JSON (or plain context for SessionStart/UserPromptSubmit). Exit 2:
/// block with stderr. Other exits are non-blocking errors.
fn apply(outcome: &mut Outcome, event: &str, code: Option<i32>, stdout: &str, stderr: &str) {
    match code {
        Some(0) => {}
        Some(2) => {
            outcome.deny = Some(if stderr.trim().is_empty() { "blocked by hook".into() } else { stderr.trim().into() });
            return;
        }
        _ => {
            outcome.notices.push(format!("{event} hook exited with {}: {}", code.map_or("a signal".into(), |c| c.to_string()), stderr.trim()));
            return;
        }
    }
    let Ok(result) = serde_json::from_str::<Value>(stdout.trim()).map_err(drop).and_then(|v| if v.is_object() { Ok(v) } else { Err(()) }) else {
        if matches!(event, "SessionStart" | "UserPromptSubmit") && !stdout.trim().is_empty() {
            outcome.context.push(stdout.trim().into());
        }
        return;
    };
    let specific = result.get("hookSpecificOutput").cloned().unwrap_or(Value::Null);
    if result["continue"] == false {
        outcome.stop = Some(result["stopReason"].as_str().unwrap_or("stopped by hook").into());
    }
    let reason = specific["permissionDecisionReason"].as_str().or_else(|| result["reason"].as_str()).unwrap_or("blocked by hook").to_owned();
    match specific["permissionDecision"].as_str().or_else(|| result["decision"].as_str()) {
        Some("deny" | "block") => outcome.deny = Some(reason),
        Some("ask") => outcome.ask = true,
        _ => {}
    }
    if let Some(input) = specific.get("updatedInput").filter(|v| v.is_object()) {
        outcome.input = Some(input.clone());
    }
    if let Some(context) = specific["additionalContext"].as_str() {
        outcome.context.push(context.into());
    }
    if let Some(message) = result["systemMessage"].as_str() {
        outcome.notices.push(message.into());
    }
}

async fn command_output(command: &str, cwd: &str, env: &[(&str, String)], input: Value, timeout: u64) -> Result<(Option<i32>, String, String)> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or(Error::Closed)?;
    let stdout = child.stdout.take().ok_or(Error::Closed)?;
    let stderr = child.stderr.take().ok_or(Error::Closed)?;
    let result = tokio::time::timeout(Duration::from_secs(timeout), async {
        // A hook may exit without reading stdin; that is not an error.
        let _ = stdin.write_all(serde_json::to_string(&input)?.as_bytes()).await;
        drop(stdin);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (mut stdout, mut stderr) = (stdout.take(OUTPUT_LIMIT as u64 + 1), stderr.take(OUTPUT_LIMIT as u64));
        let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        a?;
        b?;
        if out.len() > OUTPUT_LIMIT {
            return Err(Error::Invalid("hook output exceeds 64 KiB".into()));
        }
        let status = child.wait().await?;
        Ok((status.code(), String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned()))
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            Err(Error::Invalid(format!("hook timed out after {timeout}s")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn json_exit_codes_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let (code, out, _) =
            command_output("cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\"}}'", cwd, &[], json!({}), 2)
                .await
                .unwrap();
        let mut outcome = Outcome::default();
        apply(&mut outcome, "PreToolUse", code, &out, "");
        assert_eq!(outcome.deny.as_deref(), Some("blocked by hook"));
        let (code, out, err) = command_output("echo no >&2; exit 2", cwd, &[], json!({}), 2).await.unwrap();
        let mut outcome = Outcome::default();
        apply(&mut outcome, "PreToolUse", code, &out, &err);
        assert_eq!(outcome.deny.as_deref(), Some("no"));
        let (code, out, err) = command_output("exit 1", cwd, &[], json!({}), 2).await.unwrap();
        let mut outcome = Outcome::default();
        apply(&mut outcome, "PreToolUse", code, &out, &err);
        assert!(outcome.deny.is_none() && outcome.notices.len() == 1);
        assert!(command_output("sleep 3", cwd, &[], json!({}), 1).await.is_err());
    }

    #[test]
    fn matchers_are_anchored_regexes_with_claude_names() {
        assert!(matches("", "write") && matches("*", "write"));
        assert!(matches("Write|Edit", "write"));
        assert!(matches("Bash", "shell"));
        assert!(matches("mcp__github__.*", "mcp__github__create_issue"));
        assert!(!matches("read", "read_more"));
        assert!(matches("startup", "startup"));
    }

    #[test]
    fn plain_stdout_is_context_only_for_prompt_and_start() {
        let mut outcome = Outcome::default();
        apply(&mut outcome, "UserPromptSubmit", Some(0), "remember the style guide", "");
        apply(&mut outcome, "PostToolUse", Some(0), "ignored", "");
        assert_eq!(outcome.context, ["remember the style guide"]);
        let mut outcome = Outcome::default();
        apply(
            &mut outcome,
            "Stop",
            Some(0),
            r#"{"decision":"block","reason":"run the tests","continue":false,"stopReason":"halt","systemMessage":"hi"}"#,
            "",
        );
        assert_eq!(outcome.deny.as_deref(), Some("run the tests"));
        assert_eq!(outcome.stop.as_deref(), Some("halt"));
        assert_eq!(outcome.notices, ["hi"]);
    }
}
