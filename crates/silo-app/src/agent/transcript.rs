//! The transcript model (`docs/AGENT_BLOCK.md` §3): mirror items and live
//! tool state turned into display rows. Pure; the view only draws rows.
//!
//! Grouping: within a turn every non-text part (reasoning, tool calls,
//! errors) joins the current group; only assistant text ends a group.

use std::collections::{HashMap, HashSet};

use otto_client::{ToolState, ToolStatus};
use otto_llm::{Item, Part, ResultPart, Role};
use otto_proto::{SessionStatus, TurnStatus};
use serde_json::Value;

use super::AgentStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    Error,
    /// Called but not started (awaiting approval or queued).
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Reasoning,
    Read,
    List,
    Search,
    Shell,
    Edit,
    Fetch,
    Terminal,
    Browser,
    Ask,
    Todos,
    Subagent,
    Error,
    Other,
}

/// Inline detail under a line, shown by its `›`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detail {
    /// Prose (reasoning, error messages).
    Text(String),
    /// Tool output.
    Mono(String),
    /// A unified diff.
    Diff(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub key: String,
    pub kind: Kind,
    pub title: String,
    pub desc: Option<String>,
    pub status: Status,
    pub detail: Option<Detail>,
    /// A silo block this tool call names (`↗`).
    pub reveal: Option<String>,
    /// Streaming reasoning text, while it streams.
    pub stream: Option<String>,
    /// Files an edit touched (for the fold summary).
    pub files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub key: String,
    pub lines: Vec<Line>,
    /// The newest group of a running turn with nothing after it.
    pub live: bool,
    pub summary: String,
    /// Some line failed (the header reads as an error).
    pub failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    User { key: String, text: String },
    Group(Group),
    Text { key: String, text: String, streaming: bool },
}

impl Row {
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Row::User { key, .. } | Row::Text { key, .. } => key,
            Row::Group(g) => &g.key,
        }
    }
}

/// When the client saw a step start and finish, in ms since the pane's
/// epoch. Items carry no timestamps, so durations exist only for steps
/// seen live; a reloaded session has none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: Option<u64>,
    pub end: Option<u64>,
    /// First-seen order (live tool calls not yet in an item sort by it).
    pub seq: u64,
}

#[derive(Debug, Default)]
pub struct Times {
    spans: HashMap<String, Span>,
    seq: u64,
    primed: bool,
}

impl Times {
    /// For a session created here: everything after this is seen live.
    #[must_use]
    pub fn live() -> Self {
        Self { primed: true, ..Self::default() }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<Span> {
        self.spans.get(key).copied()
    }

    fn seq(&self, key: &str) -> u64 {
        self.spans.get(key).map_or(u64::MAX, |s| s.seq)
    }

    /// Duration of `key`: until its end, or `now` while open.
    #[must_use]
    pub fn elapsed(&self, key: &str, now: u64) -> Option<u64> {
        let span = self.spans.get(key)?;
        Some(span.end.unwrap_or(now).saturating_sub(span.start?))
    }

    fn see(&mut self, key: &str, known: bool, done: bool, now: u64) {
        let seq = self.seq;
        let span = self.spans.entry(key.to_owned()).or_insert(Span { start: known.then_some(now), end: None, seq });
        if span.seq == seq {
            self.seq += 1;
        }
        if done && span.end.is_none() && span.start.is_some() {
            span.end = Some(now);
        }
    }

    /// Record what is new or finished since the last observation. The
    /// first observation (a loaded session) records no times.
    pub fn observe(&mut self, items: &[Item], tools: &HashMap<String, ToolState>, running: bool, now: u64) {
        let known = std::mem::replace(&mut self.primed, true);
        let results = results(items);
        let pending = pending_tools(items, tools);
        let last = items.len().checked_sub(1);
        for (ix, item) in items.iter().enumerate() {
            for (pi, part) in item.parts.iter().enumerate() {
                match part {
                    Part::Reasoning { .. } => {
                        let live = running && Some(ix) == last && pi + 1 == item.parts.len() && pending.is_empty();
                        self.see(&format!("{}#{pi}", item.id), known, !live, now);
                    }
                    Part::ToolCall { call_id, .. } => {
                        let done = !running
                            || results.contains_key(call_id.as_str())
                            || tools.get(call_id).is_some_and(|t| matches!(t.status, ToolStatus::Done | ToolStatus::Error));
                        self.see(call_id, known, done, now);
                    }
                    _ => {}
                }
            }
        }
        let mut live: Vec<(&String, &ToolState)> = tools.iter().collect();
        live.sort_by(|a, b| a.0.cmp(b.0));
        for (id, t) in live {
            let done = !running || matches!(t.status, ToolStatus::Done | ToolStatus::Error);
            self.see(id, known, done, now);
        }
    }
}

/// Live tool calls not yet part of an item, in first-seen order.
fn pending_tools<'a>(items: &[Item], tools: &'a HashMap<String, ToolState>) -> Vec<(&'a String, &'a ToolState)> {
    let called: HashSet<&str> = items.iter().flat_map(Item::tool_calls).map(|(id, _, _)| id).collect();
    tools.iter().filter(|(id, _)| !called.contains(id.as_str())).collect()
}

fn results(items: &[Item]) -> HashMap<&str, (String, bool)> {
    let mut map = HashMap::new();
    for item in items {
        for part in &item.parts {
            if let Part::ToolResult { call_id, content, is_error } = part {
                let text = content
                    .iter()
                    .filter_map(|c| match c {
                        ResultPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                map.insert(call_id.as_str(), (text, *is_error));
            }
        }
    }
    map
}

pub struct Input<'a> {
    pub items: &'a [Item],
    pub tools: &'a HashMap<String, ToolState>,
    /// State of the latest turn.
    pub turn: Option<TurnStatus>,
    pub error: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub times: &'a Times,
    pub now: u64,
}

struct Ctx<'a> {
    cwd: Option<&'a str>,
    times: &'a Times,
    now: u64,
    running: bool,
}

#[must_use]
pub fn build(input: &Input<'_>) -> Vec<Row> {
    let running = input.turn == Some(TurnStatus::Running);
    let cx = Ctx { cwd: input.cwd, times: input.times, now: input.now, running };
    let results = results(input.items);
    let mut pending = pending_tools(input.items, input.tools);
    let any_pending = !pending.is_empty();
    pending.retain(|(_, t)| !t.name.is_empty());
    pending.sort_by_key(|(id, _)| input.times.seq(id));
    let last = input.items.len().checked_sub(1);
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    let flush = |rows: &mut Vec<Row>, lines: &mut Vec<Line>, live: bool, stopped: bool| {
        if !lines.is_empty() {
            rows.push(Row::Group(group(std::mem::take(lines), live, stopped, &cx)));
        }
    };
    for (ix, item) in input.items.iter().enumerate() {
        match item.role {
            Role::User => {
                let text = item.text();
                if text.trim().is_empty() {
                    continue;
                }
                flush(&mut rows, &mut lines, false, false);
                rows.push(Row::User { key: item.id.clone(), text: text.trim().to_owned() });
            }
            Role::Assistant => {
                for (pi, part) in item.parts.iter().enumerate() {
                    match part {
                        Part::Text { text, .. } if !text.trim().is_empty() => {
                            flush(&mut rows, &mut lines, false, false);
                            rows.push(Row::Text { key: format!("{}#{pi}", item.id), text: text.clone(), streaming: false });
                        }
                        Part::Reasoning { summary, text, .. } => {
                            let live = running && Some(ix) == last && pi + 1 == item.parts.len() && !any_pending;
                            let body = text.as_deref().filter(|t| !t.trim().is_empty()).or(summary.as_deref()).unwrap_or("").trim();
                            lines.extend(reasoning(&format!("{}#{pi}", item.id), body, live, &cx));
                        }
                        Part::ToolCall { call_id, name, input: args, .. } => {
                            lines.push(tool(call_id, name, args, input.tools.get(call_id), results.get(call_id.as_str()), &cx));
                        }
                        _ => {}
                    }
                }
            }
            Role::System | Role::Tool => {}
        }
    }
    for (id, t) in pending {
        lines.push(tool(id, &t.name, &t.input, Some(t), None, &cx));
    }
    if let Some(error) = input.error.map(str::trim).filter(|e| !e.is_empty()) {
        lines.push(Line {
            title: clip(error, 120),
            status: Status::Error,
            detail: (error.lines().count() > 1 || error.chars().count() > 120).then(|| Detail::Text(error.to_owned())),
            ..Line::new("error", Kind::Error)
        });
    }
    let stopped = matches!(input.turn, Some(TurnStatus::Cancelled | TurnStatus::Interrupted));
    flush(&mut rows, &mut lines, running, stopped);
    if running && let Some(Row::Text { streaming, .. }) = rows.last_mut() {
        *streaming = true;
    }
    rows
}

impl Line {
    fn new(key: &str, kind: Kind) -> Self {
        Self {
            key: key.to_owned(),
            kind,
            title: String::new(),
            desc: None,
            status: Status::Done,
            detail: None,
            reveal: None,
            stream: None,
            files: Vec::new(),
        }
    }
}

fn group(lines: Vec<Line>, live: bool, stopped: bool, cx: &Ctx<'_>) -> Group {
    let key = format!("g:{}", lines[0].key);
    let failed = lines.iter().any(|l| l.status == Status::Error);
    let summary = summary(&lines, duration(&lines, cx), stopped);
    Group { key, lines, live, summary, failed }
}

/// From the first line's start to the last line's end, when every line was seen live.
fn duration(lines: &[Line], cx: &Ctx<'_>) -> Option<u64> {
    let (mut start, mut end) = (u64::MAX, 0);
    for line in lines.iter().filter(|l| l.kind != Kind::Error) {
        let span = cx.times.get(&line.key)?;
        start = start.min(span.start?);
        end = end.max(span.end.unwrap_or(cx.now));
    }
    (start != u64::MAX).then(|| end.saturating_sub(start))
}

/// The folded header: `Worked for 42s · read 3 files · ran 2 commands, 1 failed`.
#[must_use]
pub fn summary(lines: &[Line], duration: Option<u64>, stopped: bool) -> String {
    let count = |k: Kind| lines.iter().filter(|l| l.kind == k).count();
    let failed = |k: Kind| lines.iter().filter(|l| l.kind == k && l.status == Status::Error).count();
    let mut parts = Vec::new();
    let mut add = |n: usize, text: String| {
        if n > 0 {
            parts.push(text);
        }
    };
    let n = count(Kind::Read);
    add(n, format!("read {}", plural(n, "file", "files")));
    let n = count(Kind::List);
    add(n, format!("listed {}", plural(n, "folder", "folders")));
    let n = count(Kind::Search);
    add(n, format!("searched {}", plural(n, "time", "times")));
    let n = count(Kind::Shell);
    let f = failed(Kind::Shell);
    add(n, if f > 0 { format!("ran {}, {f} failed", plural(n, "command", "commands")) } else { format!("ran {}", plural(n, "command", "commands")) });
    let mut files: Vec<&String> = lines.iter().filter(|l| l.kind == Kind::Edit).flat_map(|l| &l.files).collect();
    files.sort();
    files.dedup();
    let n = if files.is_empty() { count(Kind::Edit) } else { files.len() };
    add(n, format!("edited {}", plural(n, "file", "files")));
    let n = count(Kind::Fetch);
    add(n, format!("fetched {}", plural(n, "page", "pages")));
    let n = count(Kind::Terminal);
    add(n, plural(n, "terminal step", "terminal steps"));
    let n = count(Kind::Browser);
    add(n, plural(n, "browser step", "browser steps"));
    let n = count(Kind::Ask);
    add(n, format!("asked {}", plural(n, "question", "questions")));
    let n = count(Kind::Subagent);
    add(n, plural(n, "subagent step", "subagent steps"));
    let n = count(Kind::Todos);
    add(n, "updated todos".to_owned());
    let n = count(Kind::Other);
    add(n, format!("used {}", plural(n, "tool", "tools")));
    let n = lines.iter().filter(|l| l.status == Status::Error && !matches!(l.kind, Kind::Shell | Kind::Error)).count();
    add(n, format!("{n} failed"));
    let n = count(Kind::Error);
    add(n, plural(n, "error", "errors"));
    add(usize::from(stopped), "stopped".to_owned());
    let thought = !lines.is_empty() && lines.iter().all(|l| l.kind == Kind::Reasoning);
    let head = duration.map(|ms| format!("{} for {}", if thought { "Thought" } else { "Worked" }, dur(ms)));
    match head {
        Some(head) => std::iter::once(head).chain(parts).collect::<Vec<_>>().join(" · "),
        None if parts.is_empty() => if thought { "Thought" } else { "Worked" }.to_owned(),
        None => capitalize(&parts.join(" · ")),
    }
}

fn reasoning(key: &str, body: &str, live: bool, cx: &Ctx<'_>) -> Option<Line> {
    let base = Line::new(key, Kind::Reasoning);
    if live {
        return Some(Line { title: "Thinking".into(), status: Status::Running, stream: Some(body.to_owned()), ..base });
    }
    let took = cx.times.elapsed(key, cx.now);
    if body.is_empty() && took.is_none() {
        return None;
    }
    Some(Line {
        title: took.map_or_else(|| "Thought".into(), |ms| format!("Thought for {}", dur(ms))),
        detail: (!body.is_empty()).then(|| Detail::Text(body.to_owned())),
        ..base
    })
}

struct Outcome {
    text: String,
    error: bool,
    meta: Value,
}

fn outcome(live: Option<&ToolState>, result: Option<&(String, bool)>) -> Option<Outcome> {
    if let Some(t) = live
        && let Some(out) = &t.output
    {
        let text = out["content"].as_array().map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"));
        let error = t.status == ToolStatus::Error || out["is_error"].as_bool() == Some(true);
        return Some(Outcome { text: text.unwrap_or_default(), error, meta: out["metadata"].clone() });
    }
    result.map(|(text, error)| Outcome { text: text.clone(), error: *error, meta: Value::Null })
}

#[allow(clippy::too_many_lines)]
fn tool(call_id: &str, name: &str, input: &Value, live: Option<&ToolState>, result: Option<&(String, bool)>, cx: &Ctx<'_>) -> Line {
    let input = if input.is_null() { live.map_or(input, |t| &t.input) } else { input };
    let out = outcome(live, result);
    let mut status = match (&out, live.map(|t| t.status)) {
        (Some(o), _) if o.error => Status::Error,
        (Some(_), _) => Status::Done,
        (None, Some(ToolStatus::Running | ToolStatus::Input)) if cx.running => Status::Running,
        (None, _) if cx.running => Status::Pending,
        (None, _) => Status::Error,
    };
    let elapsed = cx.times.elapsed(call_id, cx.now);
    let text = out.as_ref().map_or("", |o| o.text.as_str());
    let meta = out.as_ref().map_or(&Value::Null, |o| &o.meta);
    let mut line = Line::new(call_id, Kind::Other);
    let mut failed = None;
    let path = |key: &str| str_at(input, key).map(|p| rel(p, cx.cwd));
    let (kind, run, done, subject): (Kind, &str, &str, String) = match name {
        "read" => (Kind::Read, "Reading", "Read", path("path").unwrap_or_default()),
        "ls" | "tree" => (Kind::List, "Listing", "Listed", path("path").filter(|p| !p.is_empty() && p != ".").unwrap_or_else(|| "files".into())),
        "glob" | "search" | "grep" => {
            let glob = name == "glob";
            if status == Status::Done {
                let n = count_lines(text);
                line.desc = Some(if glob { plural(n, "file", "files") } else { plural(n, "result", "results") });
            }
            let pattern = clip(str_at(input, "pattern").unwrap_or(""), 60);
            if glob { (Kind::Search, "Finding", "Found", pattern) } else { (Kind::Search, "Searching for", "Searched for", pattern) }
        }
        "shell" => {
            let cmd = clip(str_at(input, "command").unwrap_or(""), 80);
            failed = Some(title("Failed to run", &cmd));
            line.desc = match status {
                Status::Error => meta["exit_code"].as_i64().map(|c| format!("exit {c}")).or_else(|| elapsed.map(short)),
                Status::Running | Status::Done => elapsed.map(short),
                Status::Pending => None,
            };
            let progress: String = live.map(|t| t.progress.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            if status == Status::Running && !progress.trim().is_empty() {
                line.detail = Some(Detail::Mono(tail(&progress)));
            }
            (Kind::Shell, "Running", "Ran", cmd)
        }
        "terminal_run" => {
            let cmd = clip(str_at(input, "command").unwrap_or(""), 80);
            failed = Some(title("Failed to run", &cmd));
            let json = json_of(text);
            let exit = json["exit"].as_i64();
            if status == Status::Done && exit.is_some_and(|c| c != 0) {
                status = Status::Error;
            }
            let took = json["duration_ms"].as_u64().or(elapsed);
            line.desc = match status {
                Status::Error => exit.map(|c| format!("exit {c}")).or_else(|| took.map(short)),
                Status::Pending => None,
                _ => took.map(short),
            };
            if let Some(output) = json["output"].as_str().filter(|o| !o.trim().is_empty()) {
                line.detail = Some(Detail::Mono(tail(output)));
            }
            (Kind::Shell, "Running", "Ran", cmd)
        }
        "job" => {
            let id = str_at(input, "job_id").unwrap_or("").to_owned();
            match str_at(input, "operation") {
                Some("kill") => (Kind::Shell, "Stopping job", "Stopped job", id),
                Some("output") => (Kind::Shell, "Reading job", "Read job", id),
                _ => (Kind::Shell, "Checking job", "Checked job", id),
            }
        }
        "edit" | "multiedit" | "write" | "apply_patch" => {
            let edit = edit_stats(name, input, meta, cx.cwd);
            line.desc = edit.desc();
            line.detail = edit.diff.map(Detail::Diff);
            let subject = match edit.files.as_slice() {
                [] => String::new(),
                [one] => one.clone(),
                many => format!("{} files", many.len()),
            };
            line.files = edit.files;
            if name == "write" { (Kind::Edit, "Writing", "Wrote", subject) } else { (Kind::Edit, "Editing", "Edited", subject) }
        }
        "web_fetch" => (Kind::Fetch, "Fetching", "Fetched", host(str_at(input, "url").unwrap_or(""))),
        "ask_user" => {
            if status == Status::Done && !text.trim().is_empty() {
                line.desc = Some(clip(text, 40));
            }
            (Kind::Ask, "Asking", "Asked", clip(str_at(input, "question").unwrap_or(""), 80))
        }
        "update_todos" => {
            let todos = parse_todos(input);
            if !todos.is_empty() {
                let (done, total) = progress(&todos);
                line.desc = Some(format!("{done}/{total} done"));
            }
            (Kind::Todos, "Updating todos", "Updated todos", String::new())
        }
        "skill" => (Kind::Other, "Loading skill", "Loaded skill", clip(str_at(input, "name").unwrap_or(""), 60)),
        "subagent" => match str_at(input, "action").unwrap_or("delegate") {
            "delegate" => {
                line.desc = str_at(input, "agent").map(str::to_owned);
                (Kind::Subagent, "Delegating", "Delegated", clip(str_at(input, "task").unwrap_or(""), 80))
            }
            "list" => (Kind::Subagent, "Listing subagents", "Listed subagents", String::new()),
            "message" => (Kind::Subagent, "Messaging subagent", "Messaged subagent", String::new()),
            "stop" => (Kind::Subagent, "Stopping subagent", "Stopped subagent", String::new()),
            _ => (Kind::Subagent, "Checking subagent", "Checked subagent", String::new()),
        },
        other => match silo(other) {
            Some((kind, run, done, field)) => {
                let subject = field.and_then(|f| subject_at(input, f)).unwrap_or_default();
                let subject = if field == Some("url") { host(&subject) } else { clip(&subject, 60) };
                if other == "browser_navigate" && subject.is_empty() {
                    let how = ["back", "forward", "reload"].into_iter().find(|k| input[*k].as_bool() == Some(true));
                    match how {
                        Some("back") => (kind, "Going back", "Went back", subject),
                        Some("forward") => (kind, "Going forward", "Went forward", subject),
                        _ => (kind, "Reloading page", "Reloaded page", subject),
                    }
                } else {
                    (kind, run, done, subject)
                }
            }
            None => (Kind::Other, "Using", "Used", other.to_owned()),
        },
    };
    if name.starts_with("terminal_") || name.starts_with("browser_") || name == "notify" {
        line.reveal = str_at(input, "block_id").map(str::to_owned).or_else(|| json_of(text)["block_id"].as_str().map(str::to_owned));
    }
    line.kind = kind;
    line.status = status;
    line.title = match (status, failed) {
        (Status::Error, Some(failed)) => failed,
        (Status::Running | Status::Pending, _) => title(run, &subject),
        _ => title(done, &subject),
    };
    if status == Status::Error && out.is_none() {
        line.desc = Some("interrupted".into());
    }
    if line.detail.is_none() && !text.trim().is_empty() {
        line.detail = Some(if status == Status::Error { Detail::Text(tail(text)) } else { Detail::Mono(tail(text)) });
    }
    line
}

/// Silo tools: kind, running and done verbs, and the input field naming the subject.
fn silo(name: &str) -> Option<(Kind, &'static str, &'static str, Option<&'static str>)> {
    use Kind::{Browser, Other, Terminal};
    Some(match name {
        "terminal_open" => (Terminal, "Opening terminal", "Opened terminal", Some("command")),
        "terminal_send" => (Terminal, "Typing in terminal", "Typed in terminal", Some("text")),
        "terminal_read" => (Terminal, "Reading terminal", "Read terminal", None),
        "terminal_wait" => (Terminal, "Waiting for terminal", "Waited for terminal", None),
        "terminal_list" => (Terminal, "Listing terminals", "Listed terminals", None),
        "terminal_close" => (Terminal, "Closing terminal", "Closed terminal", None),
        "browser_open" => (Browser, "Opening browser", "Opened browser", Some("url")),
        "browser_navigate" => (Browser, "Navigating to", "Navigated to", Some("url")),
        "browser_snapshot" => (Browser, "Reading page", "Read page", None),
        "browser_find" => (Browser, "Finding", "Found", Some("text")),
        "browser_click" => (Browser, "Clicking", "Clicked", Some("target")),
        "browser_hover" => (Browser, "Hovering", "Hovered", Some("target")),
        "browser_fill" => (Browser, "Filling", "Filled", Some("target")),
        "browser_fill_form" => (Browser, "Filling form", "Filled form", None),
        "browser_type" => (Browser, "Typing", "Typed", Some("text")),
        "browser_press" => (Browser, "Pressing", "Pressed", Some("key")),
        "browser_scroll" => (Browser, "Scrolling page", "Scrolled page", None),
        "browser_wait_for" => (Browser, "Waiting for", "Waited for", Some("text")),
        "browser_dialog" => (Browser, "Answering dialog", "Answered dialog", None),
        "browser_eval" => (Browser, "Running script", "Ran script", None),
        "browser_console" => (Browser, "Reading console", "Read console", None),
        "browser_network" => (Browser, "Reading network log", "Read network log", None),
        "browser_screenshot" => (Browser, "Taking screenshot", "Took screenshot", None),
        "session_list" => (Other, "Listing sessions", "Listed sessions", None),
        "notify" => (Other, "Notifying", "Notified", Some("body")),
        _ => return None,
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct EditStats {
    files: Vec<String>,
    added: usize,
    removed: Option<usize>,
    diff: Option<String>,
}

impl EditStats {
    fn desc(&self) -> Option<String> {
        match self.removed {
            _ if self.added == 0 && self.removed.unwrap_or(0) == 0 => None,
            Some(removed) => Some(format!("+{} \u{2212}{removed}", self.added)),
            None => Some(format!("+{}", self.added)),
        }
    }
}

fn line_count(text: &str) -> usize {
    text.lines().count()
}

fn prefixed(text: &str, mark: char) -> impl Iterator<Item = String> + '_ {
    text.lines().map(move |l| format!("{mark}{l}"))
}

type FileMeta<'a> = (Option<String>, Option<u64>, Option<u64>, Option<&'a str>);

fn file_meta<'a>(m: &'a Value, cwd: Option<&str>) -> FileMeta<'a> {
    (m["path"].as_str().map(|p| rel(p, cwd)), m["added"].as_u64(), m["removed"].as_u64(), m["diff"].as_str())
}

fn edit_stats(name: &str, input: &Value, meta: &Value, cwd: Option<&str>) -> EditStats {
    let one = |m| file_meta(m, cwd);
    if let Some(files) = meta["files"].as_array() {
        let mut stats = EditStats { removed: Some(0), ..EditStats::default() };
        let mut diffs = Vec::new();
        for file in files {
            let (path, added, removed, diff) = one(file);
            stats.files.extend(path);
            stats.added += usize::try_from(added.unwrap_or(0)).unwrap_or(0);
            stats.removed = stats.removed.map(|r| r + usize::try_from(removed.unwrap_or(0)).unwrap_or(0));
            diffs.extend(diff);
        }
        stats.diff = (!diffs.is_empty()).then(|| diffs.join("\n"));
        return stats;
    }
    if let (Some(path), Some(added), Some(removed), diff) = one(meta) {
        return EditStats {
            files: vec![path],
            added: usize::try_from(added).unwrap_or(0),
            removed: Some(usize::try_from(removed).unwrap_or(0)),
            diff: diff.map(str::to_owned),
        };
    }
    let path = str_at(input, "path").map(|p| rel(p, cwd));
    match name {
        "edit" | "multiedit" => {
            let edits: Vec<&Value> =
                if name == "edit" { vec![input] } else { input["edits"].as_array().map(|e| e.iter().collect()).unwrap_or_default() };
            let mut stats = EditStats { files: path.into_iter().collect(), removed: Some(0), ..EditStats::default() };
            let mut diff = Vec::new();
            for edit in edits {
                let (old, new) = (edit["old_string"].as_str().unwrap_or(""), edit["new_string"].as_str().unwrap_or(""));
                stats.added += line_count(new);
                stats.removed = stats.removed.map(|r| r + line_count(old));
                diff.extend(prefixed(old, '-').chain(prefixed(new, '+')));
            }
            stats.diff = (!diff.is_empty()).then(|| diff.join("\n"));
            stats
        }
        "write" => {
            let content = input["content"].as_str().unwrap_or("");
            EditStats {
                files: path.into_iter().collect(),
                added: line_count(content),
                removed: None,
                diff: (!content.is_empty()).then(|| prefixed(content, '+').collect::<Vec<_>>().join("\n")),
            }
        }
        _ => {
            let patch = input.as_str().or_else(|| input["input"].as_str()).or_else(|| input["patch"].as_str()).unwrap_or("");
            patch_stats(patch, cwd)
        }
    }
}

/// Files and line counts of an `apply_patch` envelope.
fn patch_stats(patch: &str, cwd: Option<&str>) -> EditStats {
    let mut stats = EditStats { removed: Some(0), ..EditStats::default() };
    for line in patch.lines() {
        let file = ["*** Update File: ", "*** Add File: ", "*** Delete File: ", "*** Move to: "].iter().find_map(|p| line.strip_prefix(p));
        if let Some(file) = file {
            let file = rel(file.trim(), cwd);
            if !stats.files.contains(&file) {
                stats.files.push(file);
            }
        } else if line.starts_with("***") || line.starts_with("+++") || line.starts_with("---") {
        } else if line.starts_with('+') {
            stats.added += 1;
        } else if line.starts_with('-') {
            stats.removed = stats.removed.map(|r| r + 1);
        }
    }
    stats.diff = (!patch.trim().is_empty()).then(|| patch.trim_end().to_owned());
    stats
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub step: String,
    pub status: TodoStatus,
}

fn parse_todos(input: &Value) -> Vec<Todo> {
    let Some(list) = input["todos"].as_array() else { return Vec::new() };
    list.iter()
        .filter_map(|t| {
            let step = t["step"].as_str()?.trim();
            let status = match t["status"].as_str() {
                Some("in_progress") => TodoStatus::InProgress,
                Some("completed") => TodoStatus::Completed,
                Some("cancelled") => TodoStatus::Cancelled,
                _ => TodoStatus::Pending,
            };
            (!step.is_empty()).then(|| Todo { step: step.to_owned(), status })
        })
        .collect()
}

/// The session's todo list: the latest `update_todos` call's input.
#[must_use]
pub fn todos(items: &[Item], tools: &HashMap<String, ToolState>, times: &Times) -> Vec<Todo> {
    let mut latest = None;
    for item in items {
        for (_, name, input) in item.tool_calls() {
            if name == "update_todos" {
                latest = Some(input);
            }
        }
    }
    let mut pending = pending_tools(items, tools);
    pending.sort_by_key(|(id, _)| times.seq(id));
    for (_, t) in pending {
        if t.name == "update_todos" && !t.input.is_null() {
            latest = Some(&t.input);
        }
    }
    latest.map(parse_todos).unwrap_or_default()
}

/// Completed and total (cancelled ones do not count).
#[must_use]
pub fn progress(todos: &[Todo]) -> (usize, usize) {
    let done = todos.iter().filter(|t| t.status == TodoStatus::Completed).count();
    (done, todos.iter().filter(|t| t.status != TodoStatus::Cancelled).count())
}

/// The item being worked on, else the next one, else the last.
#[must_use]
pub fn current(todos: &[Todo]) -> Option<&Todo> {
    todos
        .iter()
        .find(|t| t.status == TodoStatus::InProgress)
        .or_else(|| todos.iter().find(|t| t.status == TodoStatus::Pending))
        .or_else(|| todos.last())
}

/// The latest turn's state; a loaded session knows only its status.
#[must_use]
pub fn effective_turn(turn: Option<TurnStatus>, session: SessionStatus) -> Option<TurnStatus> {
    turn.or(match session {
        SessionStatus::Busy | SessionStatus::WaitingApproval => Some(TurnStatus::Running),
        _ => None,
    })
}

#[must_use]
pub fn agent_status(turn: Option<TurnStatus>, session: SessionStatus, approvals: bool, questions: bool) -> AgentStatus {
    if questions {
        return AgentStatus::Blocked { question: true };
    }
    if approvals {
        return AgentStatus::Blocked { question: false };
    }
    match effective_turn(turn, session) {
        Some(TurnStatus::Running) => AgentStatus::Working,
        Some(TurnStatus::Error) => AgentStatus::Error,
        Some(_) => AgentStatus::Done,
        None if session == SessionStatus::Error => AgentStatus::Error,
        None => AgentStatus::Idle,
    }
}

// ---- text helpers ----

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v[key].as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// A string field, or a list of strings joined.
fn subject_at(v: &Value, key: &str) -> Option<String> {
    match &v[key] {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Array(a) => Some(a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or(Value::Null)
}

/// The first non-empty line, at most `max` characters.
#[must_use]
pub fn clip(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn title(verb: &str, subject: &str) -> String {
    if subject.is_empty() { verb.to_owned() } else { format!("{verb} {subject}") }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn count_lines(text: &str) -> usize {
    if text.trim_start().starts_with("No ") {
        return 0;
    }
    text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('[')).count()
}

/// The last 40 lines, at most ~6 KB.
fn tail(text: &str) -> String {
    let text = text.trim_end();
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines[lines.len().saturating_sub(40)..].join("\n");
    if out.len() > 6000 {
        let mut cut = out.len() - 6000;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out.drain(..cut);
    }
    out
}

/// `path` relative to `cwd` (or `~`), shortened to its last components.
#[must_use]
pub fn rel(path: &str, cwd: Option<&str>) -> String {
    let path = path.trim();
    let inside = cwd
        .map(|c| c.trim_end_matches('/'))
        .filter(|c| !c.is_empty())
        .and_then(|c| path.strip_prefix(c))
        .and_then(|r| if r.is_empty() { Some(".".to_owned()) } else { r.strip_prefix('/').map(str::to_owned) });
    let home = std::env::var("HOME").ok().filter(|h| h.len() > 1);
    let shown = inside
        .or_else(|| home.and_then(|h| path.strip_prefix(h.as_str()).filter(|r| r.starts_with('/')).map(|r| format!("~{r}"))))
        .unwrap_or_else(|| path.to_owned());
    if shown.chars().count() <= 60 {
        return shown;
    }
    let parts: Vec<&str> = shown.rsplitn(3, '/').collect();
    if parts.len() == 3 { format!("…/{}/{}", parts[1], parts[0]) } else { clip(&shown, 60) }
}

fn host(url: &str) -> String {
    let url = url.trim();
    let bare = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    clip(bare.trim_end_matches('/'), 60)
}

/// `42s`, `1m 5s`, `2h 3m`; at least 1s.
#[must_use]
pub fn dur(ms: u64) -> String {
    let s = (ms + 500) / 1000;
    let s = s.max(1);
    match s {
        0..60 => format!("{s}s"),
        60..3600 if s.is_multiple_of(60) => format!("{}m", s / 60),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// `1.2s` under ten seconds, else [`dur`].
#[must_use]
pub fn short(ms: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    if ms < 10_000 { format!("{:.1}s", ms as f64 / 1000.0) } else { dur(ms) }
}

#[cfg(test)]
mod tests {
    use otto_llm::CallKind;
    use serde_json::json;

    use super::*;

    fn call(id: &str, name: &str, input: Value) -> Part {
        Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None }
    }

    fn result(id: &str, text: &str, is_error: bool) -> Part {
        Part::ToolResult { call_id: id.into(), content: vec![ResultPart::text(text)], is_error }
    }

    fn user(id: &str, text: &str) -> Item {
        Item::user_text(id, text)
    }

    fn assistant(id: &str, parts: Vec<Part>) -> Item {
        Item::new(id, Role::Assistant, parts)
    }

    fn tool_item(id: &str, parts: Vec<Part>) -> Item {
        Item::new(id, Role::Tool, parts)
    }

    fn build_rows(items: &[Item], tools: &HashMap<String, ToolState>, turn: Option<TurnStatus>, times: &Times, now: u64) -> Vec<Row> {
        build(&Input { items, tools, turn, error: None, cwd: Some("/work/app"), times, now })
    }

    fn group_of(row: &Row) -> &Group {
        match row {
            Row::Group(g) => g,
            other => panic!("not a group: {other:?}"),
        }
    }

    fn span(times: &mut Times, key: &str, start: u64, end: Option<u64>) {
        let seq = times.seq;
        times.seq += 1;
        times.spans.insert(key.into(), Span { start: Some(start), end, seq });
    }

    #[test]
    fn non_text_parts_group_until_text() {
        let items = vec![
            user("u1", "why does pnpm install fail?"),
            assistant(
                "a1",
                vec![
                    Part::Reasoning { summary: None, text: Some("look at the lockfile".into()), opaque: None },
                    call("c1", "read", json!({"path": "/work/app/package.json"})),
                    call("c2", "shell", json!({"command": "pnpm install --frozen-lockfile"})),
                ],
            ),
            tool_item("t1", vec![result("c1", "{...}", false), result("c2", "ERR", true)]),
            assistant(
                "a2",
                vec![
                    Part::text("The lockfile is stale."),
                    call("c3", "edit", json!({"path": "/work/app/pnpm-lock.yaml", "old_string": "a\nb", "new_string": "c"})),
                ],
            ),
            tool_item("t2", vec![result("c3", "Edited", false)]),
            assistant("a3", vec![Part::text("Fixed.")]),
        ];
        let times = Times::default();
        let rows = build_rows(&items, &HashMap::new(), Some(TurnStatus::Done), &times, 0);
        let keys: Vec<&str> = rows.iter().map(Row::key).collect();
        assert_eq!(keys, ["u1", "g:a1#0", "a2#0", "g:c3", "a3#0"]);
        let g = group_of(&rows[1]);
        assert!(!g.live);
        assert_eq!(
            g.lines.iter().map(|l| l.title.as_str()).collect::<Vec<_>>(),
            ["Thought", "Read package.json", "Failed to run pnpm install --frozen-lockfile"]
        );
        assert_eq!(g.lines[2].status, Status::Error);
        assert!(g.failed);
        assert_eq!(g.summary, "Read 1 file · ran 1 command, 1 failed");
        let edit = &group_of(&rows[3]).lines[0];
        assert_eq!((edit.title.as_str(), edit.desc.as_deref()), ("Edited pnpm-lock.yaml", Some("+1 \u{2212}2")));
        assert_eq!(edit.detail, Some(Detail::Diff("-a\n-b\n+c".into())));
        assert!(matches!(&rows[4], Row::Text { streaming: false, .. }));
    }

    #[test]
    fn live_group_streams_and_text_streams() {
        let mut tools = HashMap::new();
        tools.insert(
            "c9".to_owned(),
            ToolState { name: "shell".into(), input: json!({"command": "cargo test"}), status: ToolStatus::Running, ..ToolState::default() },
        );
        let mut times = Times::default();
        span(&mut times, "c9", 1_000, None);
        let items = vec![user("u1", "test it"), assistant("a1", vec![])];
        let rows = build_rows(&items, &tools, Some(TurnStatus::Running), &times, 15_000);
        let g = group_of(&rows[1]);
        assert!(g.live);
        assert_eq!((g.lines[0].title.as_str(), g.lines[0].desc.as_deref(), g.lines[0].status), ("Running cargo test", Some("14s"), Status::Running));
        assert_eq!(g.summary, "Worked for 14s · ran 1 command");

        let items = vec![user("u1", "hi"), assistant("a1", vec![Part::text("Hel")])];
        let rows = build_rows(&items, &HashMap::new(), Some(TurnStatus::Running), &Times::default(), 0);
        assert!(matches!(&rows[1], Row::Text { streaming: true, .. }));
    }

    #[test]
    fn reasoning_streams_then_reports_its_duration() {
        let items = vec![user("u", "q"), assistant("a", vec![Part::Reasoning { summary: None, text: Some("hmm".into()), opaque: None }])];
        let rows = build_rows(&items, &HashMap::new(), Some(TurnStatus::Running), &Times::default(), 0);
        let line = &group_of(&rows[1]).lines[0];
        assert_eq!((line.title.as_str(), line.status, line.stream.as_deref()), ("Thinking", Status::Running, Some("hmm")));

        let mut times = Times::default();
        span(&mut times, "a#0", 0, Some(3_200));
        let rows = build_rows(&items, &HashMap::new(), Some(TurnStatus::Done), &times, 9_000);
        let g = group_of(&rows[1]);
        assert_eq!(g.lines[0].title, "Thought for 3s");
        assert_eq!(g.lines[0].detail, Some(Detail::Text("hmm".into())));
        assert_eq!(g.summary, "Thought for 3s");
    }

    #[test]
    fn fold_summary_counts_kinds() {
        let line = |kind, status, files: &[&str]| Line { status, files: files.iter().map(|f| (*f).to_owned()).collect(), ..Line::new("k", kind) };
        let lines = vec![
            line(Kind::Read, Status::Done, &[]),
            line(Kind::Read, Status::Done, &[]),
            line(Kind::Read, Status::Done, &[]),
            line(Kind::Shell, Status::Done, &[]),
            line(Kind::Shell, Status::Error, &[]),
            line(Kind::Edit, Status::Done, &["a.rs"]),
            line(Kind::Edit, Status::Done, &["a.rs"]),
        ];
        assert_eq!(summary(&lines, Some(42_000), false), "Worked for 42s · read 3 files · ran 2 commands, 1 failed · edited 1 file");
        assert_eq!(summary(&lines[..1], None, true), "Read 1 file · stopped");
        let mixed = vec![line(Kind::Browser, Status::Done, &[]), line(Kind::Other, Status::Error, &[]), line(Kind::Error, Status::Error, &[])];
        assert_eq!(summary(&mixed, Some(65_000), false), "Worked for 1m 5s · 1 browser step · used 1 tool · 1 failed · 1 error");
        assert_eq!(summary(&[], None, false), "Worked");
    }

    #[test]
    fn tool_titles_and_descriptions() {
        let cx = Ctx { cwd: Some("/work/app"), times: &Times::default(), now: 0, running: false };
        let ok = |text: &str| (text.to_owned(), false);
        let line = tool("s", "search", &json!({"pattern": "frozen-lockfile"}), None, Some(&ok("a.rs:1: x\nb.rs:2: y\n\n")), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref()), ("Searched for frozen-lockfile", Some("2 results")));
        let line = tool("s", "search", &json!({"pattern": "nope"}), None, Some(&ok("No matches found")), &cx);
        assert_eq!(line.desc.as_deref(), Some("0 results"));
        let line = tool("g", "glob", &json!({"pattern": "**/*.rs"}), None, Some(&ok("a.rs\nb.rs\nc.rs")), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref()), ("Found **/*.rs", Some("3 files")));
        let line = tool("l", "ls", &json!({}), None, Some(&ok("x")), &cx);
        assert_eq!(line.title, "Listed files");
        let line = tool("l", "ls", &json!({"path": "/work/app/src"}), None, Some(&ok("x")), &cx);
        assert_eq!(line.title, "Listed src");
        let line = tool("w", "web_fetch", &json!({"url": "https://docs.rs/gpui/"}), None, Some(&ok("x")), &cx);
        assert_eq!(line.title, "Fetched docs.rs/gpui");
        let line = tool("x", "mystery", &json!({}), None, Some(&ok("x")), &cx);
        assert_eq!((line.title.as_str(), line.kind), ("Used mystery", Kind::Other));
        let line = tool("q", "ask_user", &json!({"question": "Which branch?"}), None, Some(&ok("main")), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref()), ("Asked Which branch?", Some("main")));
        let line = tool("d", "subagent", &json!({"action": "delegate", "task": "Review the diff", "agent": "plan"}), None, None, &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref(), line.status), ("Delegated Review the diff", Some("interrupted"), Status::Error));
    }

    #[test]
    fn shell_lines_report_exit_and_duration() {
        let mut times = Times::default();
        span(&mut times, "c", 0, Some(1_234));
        let cx = Ctx { cwd: None, times: &times, now: 5_000, running: true };
        let live = ToolState {
            name: "shell".into(),
            status: ToolStatus::Error,
            output: Some(json!({"content": [{"type": "text", "text": "boom\n"}], "is_error": true, "metadata": {"exit_code": 1}})),
            ..ToolState::default()
        };
        let line = tool("c", "shell", &json!({"command": "pnpm install"}), Some(&live), None, &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref(), line.status), ("Failed to run pnpm install", Some("exit 1"), Status::Error));
        assert_eq!(line.detail, Some(Detail::Text("boom".into())));
        let line = tool("c", "shell", &json!({"command": "ls"}), None, Some(&("out".into(), false)), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref()), ("Ran ls", Some("1.2s")));
        assert_eq!(line.detail, Some(Detail::Mono("out".into())));
        let line = tool("p", "shell", &json!({"command": "rm -rf build"}), None, None, &cx);
        assert_eq!((line.title.as_str(), line.status), ("Running rm -rf build", Status::Pending));
    }

    #[test]
    fn silo_tools_reveal_blocks() {
        let cx = Ctx { cwd: None, times: &Times::default(), now: 0, running: false };
        let out = (r#"{"block_id":"b42","url":"http://localhost:3000/"}"#.to_owned(), false);
        let line = tool("b", "browser_open", &json!({"url": "http://localhost:3000/"}), None, Some(&out), &cx);
        assert_eq!((line.title.as_str(), line.kind, line.reveal.as_deref()), ("Opened browser localhost:3000", Kind::Browser, Some("b42")));
        let out = (r#"{"exit":2,"duration_ms":800,"output":"nope"}"#.to_owned(), false);
        let line = tool("t", "terminal_run", &json!({"command": "make", "block_id": "b7"}), None, Some(&out), &cx);
        assert_eq!(
            (line.title.as_str(), line.desc.as_deref(), line.status, line.reveal.as_deref()),
            ("Failed to run make", Some("exit 2"), Status::Error, Some("b7"))
        );
        let line = tool("n", "browser_navigate", &json!({"back": true}), None, Some(&("{}".into(), false)), &cx);
        assert_eq!(line.title, "Went back");
    }

    #[test]
    fn edits_count_lines_from_metadata_input_or_patch() {
        let cx = Ctx { cwd: Some("/w"), times: &Times::default(), now: 0, running: false };
        let live = ToolState {
            name: "edit".into(),
            status: ToolStatus::Done,
            output: Some(json!({"content": [], "metadata": {"path": "/w/src/a.rs", "added": 48, "removed": 12, "diff": "@@"}})),
            ..ToolState::default()
        };
        let line = tool("e", "edit", &json!({"path": "/w/src/a.rs"}), Some(&live), None, &cx);
        assert_eq!(
            (line.title.as_str(), line.desc.as_deref(), line.files.clone()),
            ("Edited src/a.rs", Some("+48 \u{2212}12"), vec!["src/a.rs".to_owned()])
        );
        let line = tool("w", "write", &json!({"path": "/w/new.txt", "content": "a\nb\n"}), None, Some(&("Wrote".into(), false)), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref()), ("Wrote new.txt", Some("+2")));
        let patch = "*** Begin Patch\n*** Update File: /w/a.rs\n@@\n-x\n+y\n+z\n*** Add File: b.rs\n+hi\n*** End Patch";
        let line = tool("p", "apply_patch", &json!({"input": patch}), None, Some(&("ok".into(), false)), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref()), ("Edited 2 files", Some("+3 \u{2212}1")));
        let line = tool("p", "apply_patch", &json!(patch), None, Some(&("ok".into(), false)), &cx);
        assert_eq!(line.files, ["a.rs", "b.rs"]);
        let multi = json!({"path": "/w/m.rs", "edits": [{"old_string": "a", "new_string": "b\nc"}, {"old_string": "d", "new_string": ""}]});
        let line = tool("m", "multiedit", &multi, None, Some(&("ok".into(), false)), &cx);
        assert_eq!(line.desc.as_deref(), Some("+2 \u{2212}2"));
    }

    #[test]
    fn live_calls_follow_items_in_first_seen_order() {
        let mut tools = HashMap::new();
        for (id, cmd) in [("z", "second"), ("a", "first")] {
            tools.insert(
                id.to_owned(),
                ToolState { name: "shell".into(), input: json!({"command": cmd}), status: ToolStatus::Running, ..ToolState::default() },
            );
        }
        tools.insert("in".to_owned(), ToolState { status: ToolStatus::Input, ..ToolState::default() });
        let mut times = Times::default();
        span(&mut times, "a", 0, None);
        span(&mut times, "z", 10, None);
        let items = vec![user("u", "go"), assistant("m", vec![Part::text("Starting.")])];
        let rows = build_rows(&items, &tools, Some(TurnStatus::Running), &times, 20);
        assert_eq!(rows.len(), 3, "text, then a new live group");
        let g = group_of(&rows[2]);
        assert!(g.live);
        assert_eq!(g.lines.iter().map(|l| l.title.as_str()).collect::<Vec<_>>(), ["Running first", "Running second"]);
    }

    #[test]
    fn errors_join_the_group_and_stopped_turns_say_so() {
        let items =
            vec![user("u", "go"), assistant("a", vec![call("c", "read", json!({"path": "x"}))]), tool_item("t", vec![result("c", "x", false)])];
        let times = Times::default();
        let rows = build(&Input {
            items: &items,
            tools: &HashMap::new(),
            turn: Some(TurnStatus::Error),
            error: Some("rate limited"),
            cwd: None,
            times: &times,
            now: 0,
        });
        let g = group_of(&rows[1]);
        assert_eq!((g.lines[1].title.as_str(), g.lines[1].kind), ("rate limited", Kind::Error));
        assert_eq!(g.summary, "Read 1 file · 1 error");
        let rows = build_rows(&items, &HashMap::new(), Some(TurnStatus::Cancelled), &times, 0);
        assert_eq!(group_of(&rows[1]).summary, "Read 1 file · stopped");
    }

    #[test]
    fn times_skip_the_loaded_history() {
        let items = vec![user("u", "go"), assistant("a", vec![call("c1", "read", json!({}))]), tool_item("t", vec![result("c1", "x", false)])];
        let mut times = Times::default();
        times.observe(&items, &HashMap::new(), true, 100);
        assert_eq!(times.get("c1").and_then(|s| s.start), None, "already there when loaded");
        let mut items = items;
        items.push(assistant("b", vec![call("c2", "read", json!({}))]));
        times.observe(&items, &HashMap::new(), true, 200);
        assert_eq!(times.get("c2").map(|s| (s.start, s.end)), Some((Some(200), None)));
        items.push(tool_item("t2", vec![result("c2", "x", false)]));
        times.observe(&items, &HashMap::new(), true, 900);
        assert_eq!(times.elapsed("c2", 5_000), Some(700));
        assert_eq!(times.elapsed("c1", 5_000), None);
    }

    #[test]
    fn todos_come_from_the_latest_call() {
        let items = vec![
            assistant("a", vec![call("t1", "update_todos", json!({"todos": [{"step": "old", "status": "pending"}]}))]),
            assistant(
                "b",
                vec![call(
                    "t2",
                    "update_todos",
                    json!({"todos": [
                        {"step": "Read", "status": "completed"},
                        {"step": "Fix", "status": "in_progress"},
                        {"step": "Skip", "status": "cancelled"},
                        {"step": "Test", "status": "pending"},
                    ]}),
                )],
            ),
        ];
        let todos = todos(&items, &HashMap::new(), &Times::default());
        assert_eq!(todos.len(), 4);
        assert_eq!(progress(&todos), (1, 3));
        assert_eq!(current(&todos).map(|t| t.step.as_str()), Some("Fix"));
        let line = tool(
            "t2",
            "update_todos",
            &items[1].parts.iter().find_map(|p| if let Part::ToolCall { input, .. } = p { Some(input.clone()) } else { None }).unwrap(),
            None,
            Some(&("ok".into(), false)),
            &Ctx { cwd: None, times: &Times::default(), now: 0, running: false },
        );
        assert_eq!((line.title.as_str(), line.desc.as_deref()), ("Updated todos", Some("1/3 done")));
    }

    #[test]
    fn status_follows_turn_and_pending_requests() {
        use AgentStatus as A;
        assert_eq!(agent_status(Some(TurnStatus::Running), SessionStatus::Busy, false, false), A::Working);
        assert_eq!(agent_status(Some(TurnStatus::Running), SessionStatus::Busy, true, false), A::Blocked { question: false });
        assert_eq!(agent_status(None, SessionStatus::Idle, false, true), A::Blocked { question: true });
        assert_eq!(agent_status(Some(TurnStatus::Cancelled), SessionStatus::Idle, false, false), A::Done);
        assert_eq!(agent_status(Some(TurnStatus::Error), SessionStatus::Error, false, false), A::Error);
        assert_eq!(agent_status(None, SessionStatus::Busy, false, false), A::Working);
        assert_eq!(agent_status(None, SessionStatus::Idle, false, false), A::Idle);
    }

    #[test]
    fn paths_and_durations_stay_short() {
        assert_eq!(rel("/work/app/src/main.rs", Some("/work/app/")), "src/main.rs");
        assert_eq!(rel("/work/app", Some("/work/app")), ".");
        assert_eq!(rel("/elsewhere/x.rs", Some("/work/app")), "/elsewhere/x.rs");
        let long = format!("/a/{}/deep/file.rs", "x".repeat(80));
        assert_eq!(rel(&long, None), "…/deep/file.rs");
        assert_eq!(dur(400), "1s");
        assert_eq!(dur(42_000), "42s");
        assert_eq!(dur(120_000), "2m");
        assert_eq!(dur(3_700_000), "1h 1m");
        assert_eq!(short(1_234), "1.2s");
        assert_eq!(clip("  first line\nsecond", 5), "firs…");
    }
}
