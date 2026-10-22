//! Tools over an ordinary Silo control connection.

mod descriptors;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silo_client::{Control, Events};
use silo_proto::{
    control::{BlockView, Capture, CommandOutput, CommandRecords, Created, Event, Method, SpawnSpec},
    model::{Axis, BlockKind, BlockStatus, Lifecycle, SessionSummary, SessionView},
};
use tokio::sync::{Mutex, OnceCell, broadcast};
use tokio_util::sync::CancellationToken;

pub type ToolError = anyhow::Error;

/// Describe every tool without a connection or async runtime.
#[must_use]
pub fn descriptors() -> Vec<ToolDescriptor> {
    descriptors::descriptors()
}

#[derive(Clone, Debug)]
pub struct Context {
    pub agent_block: Option<String>,
    pub default_session: String,
    pub cwd: Option<PathBuf>,
}

impl Default for Context {
    fn default() -> Self {
        Self { agent_block: std::env::var("SILO_BLOCK").ok().filter(|s| !s.is_empty()), default_session: "agents".into(), cwd: None }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Effects {
    pub read: bool,
    pub write: bool,
    pub process: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub effects: Effects,
    pub deferred: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Part {
    Text(String),
    Image { mime: String, data_base64: String },
    Json(Value),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<Part>,
    pub is_error: bool,
    pub metadata: Value,
}

impl ToolResult {
    fn json(value: Value) -> Self {
        let text = value.to_string();
        let (text, truncated) = cap(&text);
        Self {
            content: vec![if truncated { Part::Text(text) } else { Part::Json(value) }],
            is_error: false,
            metadata: json!({"truncated": truncated}),
        }
    }
}

pub struct Tools {
    control: Control,
    cx: Context,
    events: broadcast::Sender<Event>,
    fan: tokio::task::JoinHandle<()>,
    subscribed: OnceCell<()>,
    shell: Mutex<Option<String>>,
    run_lock: Mutex<()>,
}

impl Drop for Tools {
    fn drop(&mut self) {
        self.fan.abort();
    }
}

impl Tools {
    #[must_use]
    pub fn new(control: Control, mut events: Events, cx: Context) -> Self {
        let (tx, _) = broadcast::channel(1024);
        let out = tx.clone();
        let fan = tokio::spawn(async move {
            while let Some((_, event)) = events.recv().await {
                let _ = out.send(event);
            }
        });
        Self { control, cx, events: tx, fan, subscribed: OnceCell::new(), shell: Mutex::new(None), run_lock: Mutex::new(()) }
    }

    #[must_use]
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        descriptors()
    }

    /// Execute a tool; cancellation stops waiting, not the program in its block.
    ///
    /// # Errors
    /// Invalid input, cancellation, or a control failure.
    pub async fn call(&self, name: &str, input: Value, cancel: CancellationToken) -> Result<ToolResult, ToolError> {
        self.call_in(name, input, cancel, None).await
    }

    /// [`Self::call`] for the agent whose block is `anchor` (default: the
    /// context's `agent_block`): new blocks open next to it.
    ///
    /// # Errors
    /// Invalid input, cancellation, or a control failure.
    pub async fn call_in(&self, name: &str, input: Value, cancel: CancellationToken, anchor: Option<String>) -> Result<ToolResult, ToolError> {
        let anchor = anchor.or_else(|| self.cx.agent_block.clone());
        tokio::select! {
            biased;
            () = cancel.cancelled() => bail!("tool cancelled; the block's program was not stopped"),
            result = self.execute(name, input, anchor.as_deref()) => result,
        }
    }

    async fn execute(&self, name: &str, mut input: Value, anchor: Option<&str>) -> Result<ToolResult, ToolError> {
        if !input.is_object() {
            bail!("tool input must be an object");
        }
        self.subscribed.get_or_try_init(|| self.control.subscribe()).await?;
        let value = match name {
            "terminal_open" | "browser_open" => serde_json::to_value(self.open(&input, name == "browser_open", anchor).await?)?,
            "terminal_run" => self.run(&input, anchor).await?,
            "terminal_send" => {
                let block = self.block(&input, anchor)?;
                let mut text = input["text"].as_str().unwrap_or_default().to_owned();
                if let Some(keys) = input.get("keys") {
                    for key in keys.as_array().context("keys must be an array")? {
                        text.push_str(&key_text(key.as_str().context("key must be text")?)?);
                    }
                }
                if !text.is_empty() {
                    self.send(&block, text).await?;
                }
                if input["submit"].as_bool().unwrap_or(false) {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    self.send(&block, "\r".into()).await?;
                }
                json!({})
            }
            "terminal_read" => {
                let block = self.block(&input, anchor)?;
                let history = input["history"].as_bool().unwrap_or(false) || input["lines"].is_number();
                let capture = self.capture(&block, history).await?;
                let text = input["lines"].as_u64().map_or(capture.text.clone(), |n| tail(&capture.text, usize::try_from(n).unwrap_or(usize::MAX)));
                let (text, truncated) = cap(&text);
                json!({"text": text, "truncated": truncated})
            }
            "terminal_wait" => self.wait(&input, anchor).await?,
            "terminal_list" => self.list().await?,
            "terminal_close" => {
                self.control
                    .call_value(Method::BlockClose { block_id: self.block(&input, anchor)?, force: input["force"].as_bool().unwrap_or(false) })
                    .await?
            }
            "session_list" => self.control.call_value(Method::SessionList {}).await?,
            "notify" => {
                input["block_id"] =
                    json!(anchor.or_else(|| input["block_id"].as_str()).map(str::to_owned).context("notify requires block_id outside a block")?);
                self.control.call_raw("block.notify", input).await?
            }
            "browser_navigate" => {
                let block_id = self.block(&input, anchor)?;
                let choices = [
                    input["url"].is_string(),
                    input["back"].as_bool() == Some(true),
                    input["forward"].as_bool() == Some(true),
                    input["reload"].as_bool() == Some(true),
                ];
                if choices.iter().filter(|v| **v).count() != 1 {
                    bail!("provide exactly one of url, back, forward, reload");
                }
                let method = if let Some(url) = input["url"].as_str() {
                    Method::BrowserNavigate { block_id, url: url.into() }
                } else if choices[1] {
                    Method::BrowserBack { block_id }
                } else if choices[2] {
                    Method::BrowserForward { block_id }
                } else {
                    Method::BrowserReload { block_id, ignore_cache: false }
                };
                match self.control.call_value(method.clone()).await {
                    // The engine reports a followed link up to 1 s later
                    // (debounced): history may not have it yet.
                    Err(error) if choices[1] || choices[2] => {
                        if !error.downcast_ref::<silo_proto::control::RpcError>().is_some_and(|e| e.code == "bad_request") {
                            return Err(error);
                        }
                        tokio::time::sleep(Duration::from_millis(1100)).await;
                        self.control.call_value(method).await?
                    }
                    result => result?,
                }
            }
            name if descriptors::BROWSER.contains(&name) => {
                input["block_id"] = json!(self.block(&input, anchor)?);
                let value = self.control.call_raw(&name.replacen('_', ".", 1), input).await?;
                if name == "browser_screenshot" {
                    return Ok(ToolResult {
                        content: vec![Part::Image { mime: required(&value, "mime")?.into(), data_base64: required(&value, "data_base64")?.into() }],
                        is_error: false,
                        metadata: json!({"width": value["width"], "height": value["height"]}),
                    });
                }
                value
            }
            _ => bail!("unknown tool: {name}"),
        };
        Ok(ToolResult::json(value))
    }

    fn block(&self, input: &Value, anchor: Option<&str>) -> anyhow::Result<String> {
        input["block_id"].as_str().or(anchor).map(str::to_owned).context("block_id is required outside a block")
    }

    async fn open(&self, input: &Value, browser: bool, anchor: Option<&str>) -> anyhow::Result<Created> {
        let kind = if browser { BlockKind::Browser } else { BlockKind::Terminal };
        let url = input["url"].as_str().map(str::to_owned);
        let command = input["command"].as_str();
        let exec = input["exec"].as_bool().unwrap_or(false);
        let spawn = SpawnSpec {
            cwd: input["cwd"].as_str().map(str::to_owned).or_else(|| self.cx.cwd.as_ref().map(|p| p.to_string_lossy().into_owned())),
            command: if exec { command.map(|c| vec!["/bin/sh".into(), "-lc".into(), c.into()]).unwrap_or_default() } else { vec![] },
            ..Default::default()
        };
        let anchor = input["block_id"].as_str().or(anchor).map(str::to_owned);
        // Parked: in the anchor's session, in no layout, closed with it.
        if !browser && input["visible"].as_bool() == Some(false) {
            let owner = anchor.context("visible:false requires block_id or SILO_BLOCK")?;
            let created: Created = self
                .control
                .call(Method::BlockSpawn { block_id: owner, command: spawn.command, cwd: spawn.cwd, env: BTreeMap::new(), size: None })
                .await?;
            self.type_command(&created.block_id, command.filter(|_| !exec)).await?;
            return Ok(created);
        }
        let place = input["place"].as_str().unwrap_or(if anchor.is_some() { "split" } else { "tab" });
        let method = match place {
            "split" => Method::BlockSplit {
                block_id: anchor.context("split requires block_id or SILO_BLOCK")?,
                axis: Axis::Vertical,
                spawn,
                kind,
                url,
                agent: None,
            },
            "tab" => {
                let sessions: Vec<SessionSummary> = self.control.call(Method::SessionList {}).await?;
                let session = input["session"].as_str().unwrap_or(&self.cx.default_session);
                if let Some(s) = sessions.iter().find(|s| s.name == session || s.id == session) {
                    Method::WindowCreate { session_id: s.id.clone(), spawn, kind, url }
                } else {
                    Method::SessionCreate { name: Some(session.into()), spawn, kind, url }
                }
            }
            "session" => Method::SessionCreate { name: input["session"].as_str().map(str::to_owned), spawn, kind, url },
            _ => bail!("place must be split, tab or session"),
        };
        let created: Created = self.control.call(method).await?;
        if !browser {
            self.type_command(&created.block_id, command.filter(|_| !exec)).await?;
        }
        Ok(created)
    }

    /// Type `command` and Enter into a fresh shell block.
    async fn type_command(&self, block: &str, command: Option<&str>) -> anyhow::Result<()> {
        if let Some(command) = command {
            tokio::time::sleep(Duration::from_millis(300)).await;
            self.send(block, command.into()).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
            self.send(block, "\r".into()).await?;
        }
        Ok(())
    }

    async fn send(&self, block: &str, text: String) -> anyhow::Result<()> {
        self.control.call_value(Method::TerminalSend { block_id: block.into(), text }).await?;
        Ok(())
    }

    async fn capture(&self, block: &str, history: bool) -> anyhow::Result<Capture> {
        self.control.call(Method::TerminalCapture { block_id: block.into(), history }).await
    }

    async fn run(&self, input: &Value, anchor: Option<&str>) -> anyhow::Result<Value> {
        let command = required(input, "command")?;
        let _lock = self.run_lock.lock().await;
        let block = if let Some(block) = input["block_id"].as_str() {
            block.to_owned()
        } else {
            let mut shell = self.shell.lock().await;
            if shell.is_none() {
                let created = self.open(&json!({"cwd": input["cwd"]}), false, anchor).await?;
                *shell = Some(created.block_id);
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            shell.clone().unwrap()
        };
        let mut events = self.events.subscribe();
        let before: CommandRecords = self.control.call(Method::BlockCommands { block_id: block.clone(), limit: Some(1) }).await?;
        let after = before.records.iter().map(|r| r.index).max();
        self.send(&block, format!("{command}\r")).await?;
        let wait = async {
            loop {
                tokio::select! {
                    () = self.control.closed() => bail!("server connection closed"),
                    event = events.recv() => match event {
                        Ok(Event::CommandFinished { block_id, record, .. }) if block_id == block && after.is_none_or(|a| record.index > a) => return Ok(record),
                        Err(broadcast::error::RecvError::Closed) => bail!("event stream closed"),
                        Err(broadcast::error::RecvError::Lagged(_)) => bail!("command events lost; inspect the block before retrying"),
                        _ => {},
                    }
                }
            }
        };
        let record = tokio::time::timeout(Duration::from_millis(input["timeout_ms"].as_u64().unwrap_or(120_000)), wait)
            .await
            .context("command timed out; it may still be running")??;
        let output: CommandOutput = self.control.call(Method::TerminalCommandOutput { block_id: block.clone(), index: Some(record.index) }).await?;
        let (text, truncated) = cap(&output.text);
        Ok(
            json!({"block_id": block, "exit": record.exit, "duration_ms": record.duration_ms, "output": text, "truncated": truncated || output.truncated}),
        )
    }

    async fn wait(&self, input: &Value, anchor: Option<&str>) -> anyhow::Result<Value> {
        let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let mut events = self.events.subscribe();
        let block = self.block(input, anchor)?;
        let regex = input["text"].as_str().map(regex::Regex::new).transpose()?;
        let states: Vec<String> = input
            .get("status")
            .map(|s| serde_json::from_value(s.clone()))
            .transpose()?
            .unwrap_or_else(|| vec!["done".into(), "blocked".into(), "error".into()]);
        let view: BlockView = self.control.call(Method::BlockView { block_id: block.clone() }).await?;
        let mut status = view.status;
        let mut screen = String::new();
        let mut changed = tokio::time::Instant::now();
        let deadline = tokio::time::sleep(Duration::from_millis(input["timeout_ms"].as_u64().unwrap_or(120_000)));
        tokio::pin!(deadline);
        let mut poll = tokio::time::interval(Duration::from_millis(300));
        let mut exited = view.block.lifecycle != Lifecycle::Running;
        let reason = loop {
            if exited {
                break "exit";
            }
            if matches_status(status.as_ref(), started, &states) {
                break "status";
            }
            tokio::select! {
                () = self.control.closed() => bail!("server connection closed"),
                () = &mut deadline => break "timeout",
                _ = poll.tick() => {
                    let capture = match self.capture(&block, false).await {
                        Ok(capture) => capture,
                        Err(error) if not_found(&error) => break "exit",
                        Err(error) => return Err(error),
                    };
                    if capture.text != screen { changed = tokio::time::Instant::now(); screen = capture.text; }
                    if regex.as_ref().is_some_and(|r| r.is_match(&screen)) { break "text"; }
                    if input["quiet_ms"].as_u64().is_some_and(|ms| changed.elapsed() >= Duration::from_millis(ms)) { break "quiet"; }
                }
                event = events.recv() => match event {
                    Ok(Event::BlockStatus { status: next }) if next.block_id == block => status = Some(next),
                    Ok(Event::CommandFinished { block_id, .. }) if block_id == block => break "command_finished",
                    Ok(event) => {
                        let value = serde_json::to_value(event)?;
                        if matches!(value["event"].as_str(), Some("block.changed" | "session.changed" | "session.closed")) {
                            match self.control.call::<BlockView>(Method::BlockView { block_id: block.clone() }).await {
                                Ok(view) => { exited = view.block.lifecycle != Lifecycle::Running; status = view.status; }
                                Err(error) if not_found(&error) => exited = true,
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let view: BlockView = self.control.call(Method::BlockView { block_id: block.clone() }).await?;
                        status = view.status;
                        exited = view.block.lifecycle != Lifecycle::Running;
                    }
                    Err(broadcast::error::RecvError::Closed) => bail!("event stream closed"),
                }
            }
        };
        if let Ok(capture) = self.capture(&block, false).await {
            screen = capture.text;
        }
        Ok(json!({"reason": reason, "status": status, "screen": cap(&tail(&screen, 40)).0}))
    }

    async fn list(&self) -> anyhow::Result<Value> {
        let sessions: Vec<SessionSummary> = self.control.call(Method::SessionList {}).await?;
        let busy = self.control.call_value(Method::BlocksBusy { session_id: None, window_id: None, block_ids: None }).await?;
        let mut blocks = Vec::new();
        for session in sessions {
            let view: SessionView = self.control.call(Method::SessionView { session_id: session.id }).await?;
            for block in view.blocks {
                let commands = if block.kind == BlockKind::Terminal {
                    self.control.call_value(Method::BlockCommands { block_id: block.id.clone(), limit: Some(1) }).await?
                } else {
                    Value::Null
                };
                let status = view.status.iter().find(|s| s.block_id == block.id);
                blocks.push(json!({"block": block, "status": status, "commands": commands}));
            }
        }
        Ok(json!({"blocks": blocks, "busy": busy}))
    }
}

/// How long a command reading a line must be quiet before it counts as
/// waiting for input.
pub const PROMPT_QUIET: Duration = Duration::from_secs(2);
/// Columns of a command block: wide, so few lines wrap.
pub const COMMAND_COLUMNS: u16 = 200;

/// Where a watched command block got to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Watch {
    /// The command exited (`code` is `None` when a signal killed it); its
    /// final screen and scrollback as plain text, wrapped lines joined.
    Exited { code: Option<i32>, text: String },
    /// It reads a line (a password, `y/N`) and has been quiet for
    /// [`PROMPT_QUIET`]; the last screen.
    Waiting { screen: String },
    /// Its block was closed before the command ended.
    Closed,
}

impl Tools {
    /// Run `command` through `shell -lc` in a parked command block owned by
    /// `owner` (`block.spawn`); returns the block.
    ///
    /// # Errors
    /// A control failure, or `owner` is gone.
    pub async fn spawn_command(
        &self,
        owner: &str,
        shell: &str,
        command: &str,
        cwd: Option<String>,
        env: BTreeMap<String, String>,
    ) -> anyhow::Result<String> {
        self.subscribed.get_or_try_init(|| self.control.subscribe()).await?;
        let command = vec![shell.to_owned(), "-lc".to_owned(), command.to_owned()];
        let size = Some(silo_proto::model::TermSize::new(COMMAND_COLUMNS, 50));
        let created: Created = self.control.call(Method::BlockSpawn { block_id: owner.into(), command, cwd, env, size }).await?;
        Ok(created.block_id)
    }

    /// Wait until command block `block` exits or, with `prompt`, waits for
    /// input. `screen` gets the screen each time it changes.
    ///
    /// # Errors
    /// A control failure.
    pub async fn watch(&self, block: &str, prompt: bool, mut screen: impl FnMut(&str) + Send) -> anyhow::Result<Watch> {
        self.subscribed.get_or_try_init(|| self.control.subscribe()).await?;
        let mut events = self.events.subscribe();
        let mut poll = tokio::time::interval(Duration::from_millis(250));
        let (mut last, mut changed) = (String::new(), tokio::time::Instant::now());
        loop {
            tokio::select! {
                () = self.control.closed() => bail!("server connection closed"),
                _ = poll.tick() => {
                    let capture = match self.capture(block, false).await {
                        Ok(capture) => capture,
                        Err(error) if not_found(&error) => return Ok(Watch::Closed),
                        Err(error) => return Err(error),
                    };
                    if capture.lifecycle != Lifecycle::Running {
                        return self.finished(block).await;
                    }
                    if capture.text != last {
                        (last, changed) = (capture.text, tokio::time::Instant::now());
                        screen(&last);
                    }
                    if prompt && capture.line_prompt && changed.elapsed() >= PROMPT_QUIET {
                        return Ok(Watch::Waiting { screen: tail(&last, 40) });
                    }
                }
                event = events.recv() => match event {
                    Ok(Event::BlockChanged { block: info }) if info.id == block && info.lifecycle != Lifecycle::Running => return self.finished(block).await,
                    Err(broadcast::error::RecvError::Closed) => bail!("event stream closed"),
                    _ => {}
                }
            }
        }
    }

    async fn finished(&self, block: &str) -> anyhow::Result<Watch> {
        let capture = self.capture(block, true).await?;
        Ok(Watch::Exited { code: capture.exited, text: unwrap(&capture.text, capture.size.columns) })
    }

    /// Stop command block `block`: its text so far, then close it, hanging
    /// up its programs.
    ///
    /// # Errors
    /// A control failure other than the block being gone.
    pub async fn stop(&self, block: &str) -> anyhow::Result<String> {
        let text = match self.capture(block, true).await {
            Ok(capture) => unwrap(&capture.text, capture.size.columns),
            Err(error) if not_found(&error) => String::new(),
            Err(error) => return Err(error),
        };
        match self.control.call_value(Method::BlockClose { block_id: block.into(), force: true }).await {
            Err(error) if !not_found(&error) => Err(error),
            _ => Ok(text),
        }
    }

    /// The agent block showing one of the otto `sessions`, the first found
    /// in their order.
    ///
    /// # Errors
    /// A control failure.
    pub async fn agent_block(&self, sessions: &[String]) -> anyhow::Result<Option<String>> {
        let summaries: Vec<SessionSummary> = self.control.call(Method::SessionList {}).await?;
        let mut found: Vec<(usize, String)> = Vec::new();
        for summary in summaries {
            let view: SessionView = self.control.call(Method::SessionView { session_id: summary.id }).await?;
            for block in view.blocks {
                let shown = block.agent.and_then(|a| a.session_id).and_then(|s| sessions.iter().position(|x| *x == s));
                found.extend(shown.map(|rank| (rank, block.id)));
            }
        }
        Ok(found.into_iter().min_by_key(|(rank, _)| *rank).map(|(_, id)| id))
    }
}

/// Join lines a terminal `columns` wide soft-wrapped: a line filling the
/// width continues on the next.
fn unwrap(text: &str, columns: u16) -> String {
    let mut out = String::with_capacity(text.len());
    let mut full = false;
    for line in text.lines() {
        if !out.is_empty() && !full {
            out.push('\n');
        }
        out.push_str(line);
        full = columns > 0 && line.chars().count() >= usize::from(columns);
    }
    out
}

fn not_found(error: &anyhow::Error) -> bool {
    error.downcast_ref::<silo_proto::control::RpcError>().is_some_and(|e| e.code == "not_found")
}

fn matches_status(status: Option<&BlockStatus>, started: u128, states: &[String]) -> bool {
    status.is_some_and(|s| {
        s.records.iter().any(|r| {
            u128::from(r.updated_at) >= started
                && serde_json::to_value(r.state).ok().and_then(|s| s.as_str().map(str::to_owned)).is_some_and(|s| states.contains(&s))
        })
    })
}

fn required<'a>(input: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    input[key].as_str().with_context(|| format!("{key} must be a string"))
}
fn tail(text: &str, lines: usize) -> String {
    let all: Vec<_> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn cap(text: &str) -> (String, bool) {
    const LIMIT: usize = 30_000;
    if text.len() <= LIMIT {
        return (text.into(), false);
    }
    let mut head = LIMIT / 2;
    let mut end = text.len() - LIMIT / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    while !text.is_char_boundary(end) {
        end += 1;
    }
    (format!("{}\n[output truncated: {} bytes omitted]\n{}", &text[..head], end - head, &text[end..]), true)
}

fn key_text(key: &str) -> anyhow::Result<String> {
    Ok(match key.to_ascii_lowercase().as_str() {
        "enter" => "\r".into(),
        "tab" => "\t".into(),
        "esc" | "escape" => "\x1b".into(),
        "backspace" => "\x7f".into(),
        "up" => "\x1b[A".into(),
        "down" => "\x1b[B".into(),
        "right" => "\x1b[C".into(),
        "left" => "\x1b[D".into(),
        k if k.starts_with("ctrl-") && k.len() == 6 && k.as_bytes()[5].is_ascii_lowercase() => char::from(k.as_bytes()[5] - b'a' + 1).to_string(),
        _ => bail!("unsupported key: {key}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys() {
        assert_eq!(key_text("ctrl-c").unwrap(), "\x03");
        assert_eq!(key_text("up").unwrap(), "\x1b[A");
        assert_eq!(key_text("Enter").unwrap(), "\r");
        assert!(key_text("ctrl-!").is_err());
    }
    #[test]
    fn capping() {
        let text = "界".repeat(20_000);
        let (out, cut) = cap(&text);
        assert!(cut);
        assert!(out.len() < 30_100);
        assert!(out.starts_with('界') && out.ends_with('界'));
        assert_eq!(cap("short"), ("short".into(), false));
    }
}
