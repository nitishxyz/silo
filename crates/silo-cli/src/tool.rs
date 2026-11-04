//! `silo tool`: list and call every tool agents get through Silo (Silo's
//! own and those clients provide), and provide tools from a command
//! (`docs/AGENTS.md` §10).

use std::{path::Path, process::Stdio};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use silo_client::{Control, Events};
use silo_proto::tools::{ProvidedTool, ToolOutcome, ToolRequest};
use silo_tools::{Part, ToolDescriptor, ToolResult, Tools};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

/// `silo tool list`: one line per tool (`--json`: the descriptors).
///
/// # Errors
/// A control failure.
pub async fn list(tools: &Tools, as_json: bool) -> anyhow::Result<String> {
    let all = tools.all_descriptors().await?;
    if as_json {
        return Ok(serde_json::to_string_pretty(&all)?);
    }
    let provided = tools.provided().await?;
    let width = all.iter().map(|d| d.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for d in &all {
        let entry = provided.iter().find(|e| e.tool.name == d.name);
        let source = entry.map_or_else(|| "silo".to_owned(), |e| e.provider.clone());
        let kind = entry.and_then(|e| e.block_kind).or_else(|| silo_tools::block_kind(&d.name));
        let kind = kind.and_then(|k| serde_json::to_value(k).ok()).and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "-".into());
        out.push_str(&format!("{:width$}  {:8}  {:5}  {:12}  {}\n", d.name, kind, mode(d), clip(&source, 12), first_sentence(&d.description)));
    }
    Ok(out)
}

fn mode(d: &ToolDescriptor) -> &'static str {
    if d.effects.read && !d.effects.write && !d.effects.process { "read" } else { "write" }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.to_owned() } else { text.chars().take(max - 1).chain(['…']).collect() }
}

fn first_sentence(text: &str) -> &str {
    let line = text.lines().next().unwrap_or_default();
    line.find(". ").map_or(line, |i| &line[..=i])
}

/// `silo tool call`: run `name` with `input` (a JSON object): the result's
/// text, JSON and image sizes (`--json`: the whole result, images
/// included), and whether it is an error.
///
/// # Errors
/// Invalid input, an unknown tool, or a failed call.
pub async fn call(control: &Control, tools: &Tools, name: &str, input: Value, as_json: bool) -> anyhow::Result<(String, bool)> {
    if !input.is_object() {
        bail!("input must be a JSON object");
    }
    let result = crate::confirmed(control, || tools.call(name, input.clone(), CancellationToken::new())).await?;
    Ok((render(&result, as_json)?, result.is_error))
}

fn render(result: &ToolResult, as_json: bool) -> anyhow::Result<String> {
    if as_json {
        return Ok(serde_json::to_string_pretty(&ToolOutcome::from(result.clone()))? + "\n");
    }
    let mut out = String::new();
    for part in &result.content {
        match part {
            Part::Text(text) => out.push_str(text),
            Part::Json(value) => out.push_str(&serde_json::to_string_pretty(value)?),
            Part::Image { mime, data_base64 } => out.push_str(&format!("[{mime} image, {} base64 bytes; --json shows it]", data_base64.len())),
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

/// The tools a `silo tool provide --file` names: a JSON array of tools, or
/// an object with `tools`.
///
/// # Errors
/// Unreadable or invalid JSON, or an invalid tool.
pub fn read_tools(path: &Path) -> anyhow::Result<Vec<ProvidedTool>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value = serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
    let list = match value {
        Value::Object(mut map) if map.contains_key("tools") => map.remove("tools").unwrap_or_default(),
        other => other,
    };
    let tools: Vec<ProvidedTool> =
        serde_json::from_value(list).context("expected a list of {name, description, input_schema?, read_only?, approval?}")?;
    for tool in &tools {
        tool.validate().map_err(anyhow::Error::msg)?;
    }
    if tools.is_empty() {
        bail!("no tools in {}", path.display());
    }
    Ok(tools)
}

/// `silo tool provide`: offer `tools` (for `block`, else the server) and
/// run `command` for each call: the input on its stdin, `SILO_TOOL` (the
/// name), `SILO_TOOL_BLOCK` and `SILO_TOOL_CALLER` in its environment.
/// Its stdout is the result (JSON of `{content, is_error}` is taken as
/// is, other JSON as data, anything else as text); a failing exit is an
/// error result with its output. Runs until the connection closes.
///
/// # Errors
/// A refused offer (see [`silo_tools::provider::serve`]).
pub async fn provide(control: Control, events: Events, block: Option<String>, tools: Vec<ProvidedTool>, command: Vec<String>) -> anyhow::Result<()> {
    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    let scope = block.as_deref().map_or_else(|| "the server".to_owned(), |b| format!("block {b}"));
    let handler = silo_tools::provider::handler(move |request: ToolRequest| {
        let command = command.clone();
        async move { run_handler(&command, request).await.unwrap_or_else(|e| ToolOutcome::error(format!("{e:#}"))) }
    });
    silo_tools::provider::serve(control, events, block, tools, handler, || {
        eprintln!("silo: providing {} for {scope}; stop with ctrl-c", names.join(", "));
    })
    .await?;
    eprintln!("silo: stopped providing ({scope} or the server went away)");
    Ok(())
}

async fn run_handler(command: &[String], request: ToolRequest) -> anyhow::Result<ToolOutcome> {
    let (program, args) = command.split_first().context("no handler command")?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .env("SILO_TOOL", &request.name)
        .env("SILO_TOOL_BLOCK", request.block_id.as_deref().unwrap_or_default())
        .env("SILO_TOOL_CALLER", request.caller.from.as_deref().unwrap_or(&request.caller.label))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("run {program}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(request.input.to_string().as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(ToolOutcome::error(format!("{program} failed ({}): {}{}", output.status, stdout.trim(), stderr.trim())));
    }
    Ok(parse_output(&stdout))
}

/// A handler's stdout as a result.
fn parse_output(stdout: &str) -> ToolOutcome {
    match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => ToolOutcome::from_value(value),
        _ => ToolOutcome::text(stdout.trim_end()),
    }
}

/// Input from `silo tool call`'s argument: JSON, `-` (stdin) or nothing.
///
/// # Errors
/// Not JSON.
pub fn parse_input(arg: Option<&str>, block: Option<&str>) -> anyhow::Result<Value> {
    let mut input = match arg {
        None => json!({}),
        Some("-") => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
            serde_json::from_str(&text).context("stdin is not JSON")?
        }
        Some(text) => serde_json::from_str(text).context("input is not JSON (an object such as '{\"key\": 1}')")?,
    };
    if let (Some(block), Some(map)) = (block, input.as_object_mut()) {
        map.insert("block_id".into(), json!(block));
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_output() {
        assert_eq!(parse_output("hello\n"), ToolOutcome::text("hello"));
        assert_eq!(parse_output("{\"a\":1}"), ToolOutcome::json(json!({"a": 1})));
        let shaped = parse_output("{\"content\":[{\"type\":\"text\",\"text\":\"x\"}],\"is_error\":true}");
        assert!(shaped.is_error);
        assert_eq!(parse_output("42"), ToolOutcome::text("42"), "scalars are text");
    }

    #[test]
    fn inputs() {
        assert_eq!(parse_input(None, None).unwrap(), json!({}));
        assert_eq!(parse_input(Some("{\"a\":1}"), Some("b1")).unwrap(), json!({"a": 1, "block_id": "b1"}));
        assert!(parse_input(Some("nope"), None).is_err());
    }

    #[test]
    fn tool_files() {
        let path = std::env::temp_dir().join(format!("silo-tools-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"tools":[{"name":"hello","description":"Say hi","read_only":true}]}"#).unwrap();
        assert_eq!(read_tools(&path).unwrap()[0].name, "hello");
        std::fs::write(&path, r#"[{"name":"Bad","description":"x"}]"#).unwrap();
        assert!(read_tools(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
