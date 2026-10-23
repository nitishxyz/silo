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
    /// Waiting for the user: an approval (`false`) or an answer (`true`).
    Waiting(bool),
    /// A command waits for input in its terminal block ("Needs you").
    Input,
    /// Stopped by the user (■, `job kill`, a stopped turn): neutral, not a failure.
    Stopped,
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

impl Kind {
    /// The line's icon (`assets/icons/<name>.svg`).
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Kind::Reasoning => "brain",
            Kind::Read => "file-text",
            Kind::List => "folder",
            Kind::Search => "search",
            Kind::Shell | Kind::Terminal => "terminal",
            Kind::Edit => "pencil",
            Kind::Fetch | Kind::Browser => "globe",
            Kind::Ask => "question",
            Kind::Todos => "list-todo",
            Kind::Subagent => "bot",
            Kind::Error => "x-circle",
            Kind::Other => "circle",
        }
    }
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
    /// A running command's block: closing it stops the command.
    pub stop: Option<String>,
    /// The child session a subagent call names (`↗` opens it).
    pub session: Option<String>,
    /// Streaming reasoning text, while it streams.
    pub stream: Option<String>,
    /// Files an edit touched (for the fold summary).
    pub files: Vec<String>,
    /// `desc` is the call's running time: kept current while it shows.
    pub clock: bool,
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
    User {
        key: String,
        text: String,
        files: Vec<FileChip>,
        mentions: Vec<super::mention::Chip>,
    },
    Group(Group),
    Text {
        key: String,
        text: String,
        streaming: bool,
    },
    /// A quiet line between turns: `Context compacted` where otto compacted the history.
    Marker {
        key: String,
        text: String,
    },
}

/// A file the user attached, shown as a chip on their message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChip {
    pub name: String,
    pub mime: String,
    /// Images: `item#part` of the image sent with it (the thumbnail).
    pub image: Option<String>,
}

/// An attachment note otto adds to a prompt (`[attachment att_…: name, mime, size — hint]`): its name and mime.
#[must_use]
pub fn attachment_note(text: &str) -> Option<(&str, &str)> {
    let body = text.trim().strip_prefix("[attachment att_")?.strip_suffix(']')?;
    let body = body.rsplit_once(" — ").map_or(body, |(note, _)| note);
    let (_, rest) = body.split_once(": ")?;
    let mut parts = rest.rsplitn(3, ", ");
    let (_size, mime, name) = (parts.next()?, parts.next()?, parts.next()?);
    Some((name, mime))
}

/// A user item's text without attachment notes, its files (attached ones by
/// their notes, images sent inline as `Image`) and its mentions in that text.
#[must_use]
pub fn user_message(item: &Item) -> (String, Vec<FileChip>, Vec<super::mention::Chip>) {
    let mut text = String::new();
    let mut files: Vec<FileChip> = Vec::new();
    // Where each text part starts in `text` (`None`: a note, not shown).
    let mut shown = Vec::new();
    for (pi, part) in item.parts.iter().enumerate() {
        match part {
            Part::Text { text: t, .. } => match attachment_note(t) {
                Some((name, mime)) => {
                    shown.push(None);
                    files.push(FileChip { name: name.to_owned(), mime: mime.to_owned(), image: None });
                }
                None => {
                    shown.push(Some(text.len()));
                    text.push_str(t);
                }
            },
            Part::Image(media) => {
                let key = Some(format!("{}#{pi}", item.id));
                match files.iter_mut().rev().find(|f| f.image.is_none() && media.name.as_deref() == Some(f.name.as_str())) {
                    Some(file) => file.image = key,
                    None => files.push(FileChip { name: media.name.clone().unwrap_or_else(|| "Image".into()), mime: media.mime.clone(), image: key }),
                }
            }
            _ => {}
        }
    }
    let lead = text.len() - text.trim_start().len();
    let text = text.trim().to_owned();
    let mentions = if item.mentions.is_empty() { Vec::new() } else { super::mention::item_chips(item, &shown, lead, text.len()) };
    (text, files, mentions)
}

/// Attachment names by id, from the notes in the user's messages.
fn attachment_names(items: &[Item]) -> HashMap<String, String> {
    let named = |text: &str| {
        let id = text.trim().strip_prefix("[attachment ")?.split_once(':')?.0;
        attachment_note(text).map(|(name, _)| (id.to_owned(), name.to_owned()))
    };
    items
        .iter()
        .filter(|i| i.role == Role::User)
        .flat_map(|i| &i.parts)
        .filter_map(|p| if let Part::Text { text, .. } = p { named(text) } else { None })
        .collect()
}

/// Why a copy failed, in a few words and without paths.
fn copy_failure(text: &str) -> &'static str {
    let t = text.to_lowercase();
    if t.contains(" exists") {
        "already exists"
    } else if t.contains("is a folder") || t.contains("is a directory") {
        "a folder is in the way"
    } else if t.contains("unknown attachment") || t.contains("no attachments") {
        "no such attachment"
    } else if t.contains("permission denied") || t.contains("not permitted") || t.contains("read-only") {
        "permission denied"
    } else if t.contains("no space") {
        "disk full"
    } else if t.contains("denied") {
        "denied"
    } else {
        "failed"
    }
}

impl Row {
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Row::User { key, .. } | Row::Text { key, .. } | Row::Marker { key, .. } => key,
            Row::Group(g) => &g.key,
        }
    }
}

/// The running turn's indicator is the live group's header (it ends the transcript), not the activity row.
/// There is only ever one of them.
#[must_use]
pub fn indicator_is_group(rows: &[Row]) -> bool {
    matches!(rows.last(), Some(Row::Group(g)) if g.live)
}

/// A group of thinking only (its header says all its line does).
#[must_use]
pub fn thought(g: &Group) -> bool {
    g.lines.iter().all(|l| l.kind == Kind::Reasoning)
}

/// The one text row that may still reveal (sweep in, its height gliding): the last row, while it's text. A row
/// after it settles it at once; the newer row has the motion.
#[must_use]
pub fn revealing(rows: &[Row]) -> Option<&str> {
    match rows.last() {
        Some(Row::Text { key, .. }) => Some(key),
        _ => None,
    }
}

/// How the transcript list changes from `old` to `new`: rows that kept their
/// key and place are remeasured (the scroll stays anchored in them), only
/// the rest is spliced.
#[derive(Debug, PartialEq, Eq)]
pub struct ListChange {
    pub remeasure: std::ops::Range<usize>,
    /// The old range replaced, and how many rows replace it.
    pub splice: Option<(std::ops::Range<usize>, usize)>,
}

#[must_use]
pub fn list_change(old: &[Row], new: &[Row]) -> ListChange {
    let same = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let at = same + old[same..].iter().zip(&new[same..]).take_while(|(a, b)| a.key() == b.key()).count();
    let splice = (at < old.len() || at < new.len()).then(|| (at..old.len(), new.len() - at));
    ListChange { remeasure: same..at, splice }
}

/// Keys showing for the first time: new rows and new lines of live groups.
/// Whatever was `seen` keeps its entry time, so nothing enters twice.
#[must_use]
pub fn arrivals(old: &[Row], new: &[Row], seen: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    for row in new {
        if !seen(row.key()) && !old.iter().any(|o| o.key() == row.key()) {
            out.push(row.key().to_owned());
        }
        if let Row::Group(g) = row
            && g.live
        {
            out.extend(g.lines.iter().filter(|l| !seen(&l.key)).map(|l| l.key.clone()));
        }
    }
    out
}

/// When a step started and finished, in wall ms (the clock otto records
/// in). Steps seen live are timed as the client saw them; steps that ran
/// before the session loaded take otto's recorded times
/// (`SessionView.part_times`, the same rules from its journal), so a reloaded
/// session reads as it did live. Wall time: a call waiting for an approval or
/// an answer is timed from when it was called, and a group's `Worked for`
/// spans its steps (its live header shows the turn's clock).
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
    /// Calls waiting for an approval (`false`) or an answer (`true`).
    waiting: HashMap<String, bool>,
    /// Starts otto reported for calls already running when the session loaded.
    starts: HashMap<String, u64>,
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

    /// Whether call `key` waits for an approval (`Some(false)`) or an answer (`Some(true)`).
    #[must_use]
    pub fn waiting(&self, key: &str) -> Option<bool> {
        self.waiting.get(key).copied()
    }

    /// Calls that were running before this pane saw them, and when they started.
    pub fn seed(&mut self, starts: impl IntoIterator<Item = (String, u64)>) {
        self.starts.extend(starts);
    }

    /// otto's recorded times of `items`' parts; what this pane timed itself stays.
    pub fn record(&mut self, items: &[Item], times: &[otto_proto::PartTime]) {
        if times.is_empty() {
            return;
        }
        let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.id.as_str(), i)).collect();
        for t in times {
            let Some(part) = by_id.get(t.item_id.as_str()).and_then(|i| i.parts.get(t.part as usize)) else { continue };
            let key = match part {
                Part::ToolCall { call_id, .. } => call_id.clone(),
                _ => format!("{}#{}", t.item_id, t.part),
            };
            if self.spans.contains_key(&key) {
                continue;
            }
            let at = |ms: i64| u64::try_from(ms).unwrap_or(0);
            self.spans.insert(key, Span { start: Some(at(t.started_at)), end: t.finished_at.map(at), seq: self.seq });
            self.seq += 1;
        }
    }

    fn see(&mut self, key: &str, known: bool, done: bool, now: u64) {
        let seq = self.seq;
        let start = known.then_some(now).or_else(|| self.starts.get(key).copied());
        let span = self.spans.entry(key.to_owned()).or_insert(Span { start, end: None, seq });
        if span.seq == seq {
            self.seq += 1;
        }
        if done && span.end.is_none() && span.start.is_some() {
            span.end = Some(now);
        }
    }

    /// Record what is new or finished since the last observation. The
    /// first observation (a loaded session) records no times. `waiting`:
    /// calls with a pending approval (`false`) or question (`true`).
    pub fn observe(&mut self, items: &[Item], tools: &HashMap<String, ToolState>, running: bool, waiting: HashMap<String, bool>, now: u64) {
        let known = std::mem::replace(&mut self.primed, true);
        self.waiting = waiting;
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
    /// The daemon user's home (`hello.home`): paths under it read `~/…`.
    pub home: Option<&'a str>,
    pub times: &'a Times,
    pub now: u64,
}

struct Ctx<'a> {
    cwd: Option<&'a str>,
    home: Option<&'a str>,
    times: &'a Times,
    now: u64,
    running: bool,
    /// The latest turn was stopped by the user: calls left without a result were stopped.
    stopped: bool,
    /// Background shell jobs that reported, by job id.
    jobs: &'a HashMap<String, (JobEnd, String)>,
    /// Attachment names by id (`attachment_names`).
    files: &'a HashMap<String, String>,
}

/// How a shell job (background, or answered after waiting for input) ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum JobEnd {
    Exit(i64),
    Stopped,
    Failed,
}

/// Job reports delivered as system items (`Background shell <id> (…) exited
/// with code N.` then its output): how each ended and the output.
fn job_reports(items: &[Item]) -> HashMap<String, (JobEnd, String)> {
    let mut jobs = HashMap::new();
    for part in items.iter().filter(|i| i.role == Role::System).flat_map(|i| &i.parts) {
        let Part::System { text } = part else { continue };
        let (first, body) = text.split_once('\n').unwrap_or((text, ""));
        let Some(id) = first.strip_prefix("Background shell ").and_then(|r| r.split_whitespace().next()) else { continue };
        let end = match first.rfind(" exited with code ") {
            Some(at) => first[at + 18..].trim_end_matches('.').parse().map_or(JobEnd::Failed, JobEnd::Exit),
            None if first.contains(" was stopped") || first.contains(" was killed") => JobEnd::Stopped,
            None => JobEnd::Failed,
        };
        jobs.insert(id.to_owned(), (end, body.to_owned()));
    }
    jobs
}

/// A shell call that became a job: its id, block and whether it waited for
/// input, from the result's metadata or (loaded from history) its text.
struct ShellJob {
    id: String,
    block: Option<String>,
    waiting: bool,
}

fn shell_job(text: &str, meta: &Value) -> Option<ShellJob> {
    if let Some(id) = meta["job_id"].as_str() {
        let block = meta["block_id"].as_str().map(str::to_owned);
        return Some(ShellJob { id: id.to_owned(), block, waiting: meta["status"] == "waiting_for_input" });
    }
    let word_after = |key: &str| text.split_once(key).and_then(|(_, r)| r.split([' ', ')', ',', ';']).next()).map(str::to_owned);
    if text.starts_with("waiting_for_input:") {
        return Some(ShellJob { id: word_after("(job ")?, block: word_after("to block "), waiting: true });
    }
    text.strip_prefix("Started ").map(|_| ShellJob {
        id: word_after("Started ").unwrap_or_default(),
        block: word_after(" in block "),
        waiting: false,
    })
}

#[must_use]
pub fn build(input: &Input<'_>) -> Vec<Row> {
    let running = input.turn == Some(TurnStatus::Running);
    let jobs = job_reports(input.items);
    let files = attachment_names(input.items);
    let stopped = input.turn == Some(TurnStatus::Cancelled);
    let cx = Ctx { cwd: input.cwd, home: input.home, times: input.times, now: input.now, running, stopped, jobs: &jobs, files: &files };
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
                let (text, files, mentions) = user_message(item);
                if text.is_empty() && files.is_empty() {
                    continue;
                }
                flush(&mut rows, &mut lines, false, false);
                rows.push(Row::User { key: item.id.clone(), text, files, mentions });
            }
            Role::Assistant => {
                // Text rows are keyed by their place among the item's texts, not by part index: the finished item
                // may hold parts its stream never showed (a thinking block without text, a signature), and a
                // key that shifts at `item.done` would make the reply a new row (gone, then in again).
                let (mut texts, mut joined) = (0, None::<usize>);
                for (pi, part) in item.parts.iter().enumerate() {
                    let shown = lines.len();
                    match part {
                        Part::Text { text, .. } if !text.trim().is_empty() => {
                            // Text blocks with nothing shown between them read as one (the stream joined them).
                            if let Some(Row::Text { text: prev, .. }) = joined.and_then(|r| rows.get_mut(r)) {
                                prev.push_str(text);
                                continue;
                            }
                            flush(&mut rows, &mut lines, false, false);
                            rows.push(Row::Text { key: format!("{}#t{texts}", item.id), text: text.clone(), streaming: false });
                            joined = Some(rows.len() - 1);
                            texts += 1;
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
                    if lines.len() > shown {
                        joined = None;
                    }
                }
            }
            Role::System if item.parts.iter().any(|p| matches!(p, Part::Compaction { .. })) => {
                flush(&mut rows, &mut lines, false, false);
                rows.push(Row::Marker { key: item.id.clone(), text: "Context compacted".into() });
            }
            Role::System | Role::Tool => {}
        }
    }
    for (id, t) in pending {
        lines.push(tool(id, &t.name, &t.input, Some(t), None, &cx));
    }
    if let Some(error) = input.error.map(str::trim).filter(|e| !e.is_empty()) {
        lines.push(Line {
            title: clip(&turn_error(error), 120),
            status: Status::Error,
            detail: Some(Detail::Text(error.to_owned())),
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
            stop: None,
            session: None,
            stream: None,
            files: Vec::new(),
            clock: false,
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

/// The folded header: `Worked for 42s · read 3 files · ran 2 commands, 1 failed, 1 stopped`.
#[must_use]
pub fn summary(lines: &[Line], duration: Option<u64>, stopped: bool) -> String {
    let count = |k: Kind| lines.iter().filter(|l| l.kind == k).count();
    // What a verb claims (read, edited, fetched…) counts only what didn't fail: a failure is counted as one.
    let done = |k: Kind| lines.iter().filter(|l| l.kind == k && l.status != Status::Error).count();
    let failed = |k: Kind| lines.iter().filter(|l| l.kind == k && l.status == Status::Error).count();
    let halted = |k: Kind| lines.iter().filter(|l| l.kind == k && l.status == Status::Stopped).count();
    let mut parts = Vec::new();
    let mut add = |n: usize, text: String| {
        if n > 0 {
            parts.push(text);
        }
    };
    let n = done(Kind::Read);
    add(n, format!("read {}", plural(n, "file", "files")));
    let n = done(Kind::List);
    add(n, format!("listed {}", plural(n, "folder", "folders")));
    let n = done(Kind::Search);
    add(n, format!("searched {}", plural(n, "time", "times")));
    let n = count(Kind::Shell);
    let ran = std::iter::once(format!("ran {}", plural(n, "command", "commands")))
        .chain(
            [(failed(Kind::Shell), "failed"), (halted(Kind::Shell), "stopped")].into_iter().filter(|(k, _)| *k > 0).map(|(k, w)| format!("{k} {w}")),
        )
        .collect::<Vec<_>>()
        .join(", ");
    add(n, ran);
    let mut files: Vec<&String> = lines.iter().filter(|l| l.kind == Kind::Edit && l.status != Status::Error).flat_map(|l| &l.files).collect();
    files.sort();
    files.dedup();
    let n = if files.is_empty() { done(Kind::Edit) } else { files.len() };
    add(n, format!("edited {}", plural(n, "file", "files")));
    let n = done(Kind::Fetch);
    add(n, format!("fetched {}", plural(n, "page", "pages")));
    let n = count(Kind::Terminal);
    add(n, plural(n, "terminal step", "terminal steps"));
    let n = count(Kind::Browser);
    add(n, plural(n, "browser step", "browser steps"));
    let n = done(Kind::Ask);
    add(n, format!("asked {}", plural(n, "question", "questions")));
    let n = count(Kind::Subagent);
    add(n, plural(n, "subagent step", "subagent steps"));
    let n = done(Kind::Todos);
    add(n, "updated todos".to_owned());
    let n = count(Kind::Other);
    add(n, format!("used {}", plural(n, "tool", "tools")));
    let n = lines.iter().filter(|l| l.status == Status::Error && !matches!(l.kind, Kind::Shell | Kind::Error)).count();
    add(n, format!("{n} failed"));
    let n = lines.iter().filter(|l| l.status == Status::Stopped && l.kind != Kind::Shell).count();
    add(n, format!("{n} stopped"));
    let n = count(Kind::Error);
    add(n, plural(n, "error", "errors"));
    // The turn's stop, unless a line already says it.
    add(usize::from(stopped && !lines.iter().any(|l| l.status == Status::Stopped)), "stopped".to_owned());
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
        (None, _) if cx.running && cx.times.waiting(call_id).is_some() => Status::Waiting(cx.times.waiting(call_id) == Some(true)),
        (None, Some(ToolStatus::Running | ToolStatus::Input)) if cx.running => Status::Running,
        (None, _) if cx.running => Status::Pending,
        (None, _) => Status::Error,
    };
    let elapsed = cx.times.elapsed(call_id, cx.now);
    let text = out.as_ref().map_or("", |o| o.text.as_str());
    let meta = out.as_ref().map_or(&Value::Null, |o| &o.meta);
    let mut line = Line::new(call_id, Kind::Other);
    let mut failed = None;
    // No detail from the result text (it only says where the result goes).
    let mut quiet = false;
    let path = |key: &str| str_at(input, key).map(|p| rel(p, cx.cwd, cx.home));
    // An attachment by its name (the result's, else its note's), never its id or where it's stored.
    let attached =
        || meta["name"].as_str().or_else(|| str_at(input, "attachment_id").and_then(|id| cx.files.get(id)).map(String::as_str)).map(str::to_owned);
    let (kind, run, done, subject): (Kind, &str, &str, String) = match name {
        "read" => {
            let subject = match path("path") {
                Some(p) => p,
                None if str_at(input, "attachment_id").is_some() => attached().unwrap_or_else(|| "attachment".into()),
                None => String::new(),
            };
            (Kind::Read, "Reading", "Read", subject)
        }
        "copy_attachment" => {
            let dest = path("path").unwrap_or_default();
            let name = attached()
                .or_else(|| text.strip_prefix("Copied ").and_then(|t| t.rsplit_once(" to ")).map(|(name, _)| name.to_owned()))
                .unwrap_or_else(|| "attachment".into());
            if status == Status::Done {
                line.files = vec![dest.clone()];
            }
            line.desc = text
                .trim_end()
                .strip_suffix(')')
                .and_then(|t| t.rsplit_once(" ("))
                .map(|(_, size)| size.to_owned())
                .filter(|_| status == Status::Done);
            failed = out.is_some().then(|| format!("Couldn't copy {name}: {}", copy_failure(text)));
            quiet = true;
            (Kind::Edit, "Copying", "Copied", format!("{name} → {dest}"))
        }
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
            let cmd = clip(&display(str_at(input, "command").unwrap_or(""), cx.cwd, cx.home), 80);
            failed = Some(title("Failed to run", &cmd));
            let job = if status == Status::Done { shell_job(text, meta) } else { None };
            let end = job.as_ref().and_then(|j| cx.jobs.get(&j.id));
            match (job, end) {
                // It ran on and reported (a background command, or one that waited for input).
                (Some(_), Some((end, body))) => {
                    status = match end {
                        JobEnd::Exit(0) => Status::Done,
                        JobEnd::Stopped => Status::Stopped,
                        _ => Status::Error,
                    };
                    line.desc = match end {
                        JobEnd::Exit(code) => Some(format!("exit {code}")),
                        JobEnd::Stopped => None,
                        JobEnd::Failed => Some("failed".into()),
                    };
                    quiet = true;
                    line.detail = (!body.trim().is_empty()).then(|| Detail::Mono(tail(body)));
                }
                (Some(job), None) => {
                    status = if job.waiting { Status::Input } else { Status::Running };
                    line.desc = Some(if job.waiting { cmd.clone() } else { "background".into() });
                    // The screen it waits on, after the note for the model.
                    let screen = text.split_once('\n').map_or("", |(_, s)| s);
                    quiet = true;
                    line.detail = (job.waiting && !screen.trim().is_empty()).then(|| Detail::Mono(tail(screen)));
                    line.reveal.clone_from(&job.block);
                    line.stop = job.block;
                }
                (None, _) => {
                    // Stopped by the user (its block closed, or the turn stopped) or by its timeout.
                    let marked = text.trim_end().rsplit('\n').next().and_then(|l| l.strip_prefix("[stopped: ")).map(|l| l.trim_end_matches(']'));
                    let stop = match meta["termination"].as_str().or(marked) {
                        Some("closed" | "cancelled") => Some("stopped"),
                        Some(_) => Some("timed out"),
                        None => None,
                    };
                    if stop == Some("stopped") && status == Status::Error {
                        status = Status::Stopped;
                    }
                    line.desc = match status {
                        Status::Error => {
                            stop.map(str::to_owned).or_else(|| meta["exit_code"].as_i64().map(|c| format!("exit {c}"))).or_else(|| elapsed.map(short))
                        }
                        Status::Running | Status::Done | Status::Stopped => elapsed.map(short),
                        Status::Pending | Status::Waiting(_) | Status::Input => None,
                    };
                    let progress: String = live.map(|t| t.progress.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    if status == Status::Running && !progress.trim().is_empty() {
                        line.detail = Some(Detail::Mono(tail(&progress)));
                    }
                    // A command block while it runs (it closes when the command ends).
                    if status == Status::Running {
                        line.reveal = live.and_then(|t| t.messages.iter().rev().find_map(|m| m.strip_prefix("block:"))).map(str::to_owned);
                        line.stop.clone_from(&line.reveal);
                    }
                }
            }
            (Kind::Shell, "Running", "Ran", cmd)
        }
        "terminal_run" => {
            let cmd = clip(&display(str_at(input, "command").unwrap_or(""), cx.cwd, cx.home), 80);
            failed = Some(title("Failed to run", &cmd));
            let json = json_of(text);
            let exit = json["exit"].as_i64();
            if status == Status::Done && exit.is_some_and(|c| c != 0) {
                status = Status::Error;
            }
            let took = json["duration_ms"].as_u64().or(elapsed);
            line.desc = match status {
                Status::Error => exit.map(|c| format!("exit {c}")).or_else(|| took.map(short)),
                Status::Pending | Status::Waiting(_) => None,
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
            let edit = edit_stats(name, input, meta, cx.cwd, cx.home);
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
        // Web tools (otto-tools `web_fetch`/`web_search`): matched on their robust fields (metadata `rendered`,
        // `results`; the `rendered in browser` note; numbered hits) so small changes to their text still read.
        "web_fetch" => {
            let url = str_at(input, "url").unwrap_or("");
            let page = host(url.split(['?', '#']).next().unwrap_or(url));
            failed = Some(title("Couldn't fetch", &page));
            if status == Status::Done && (meta["rendered"].as_bool() == Some(true) || text.lines().any(|l| l.trim() == "Note: rendered in browser")) {
                line.desc = Some("rendered".into());
            }
            // The page's markdown, without the header for the model (title, URL, notes).
            if let Some((head, body)) = text.split_once("\n\n")
                && head.lines().all(|l| ["Title:", "URL:", "Status:", "Note:"].iter().any(|p| l.starts_with(p)))
                && !body.trim().is_empty()
            {
                line.detail = Some(if status == Status::Error { Detail::Text(tail(body)) } else { Detail::Mono(tail(body)) });
            }
            (Kind::Fetch, "Fetching", "Fetched", page)
        }
        "web_search" => {
            let query = format!("\"{}\"", clip(str_at(input, "query").unwrap_or(""), 60));
            failed = Some(format!("Couldn't search the web for {query}"));
            if status == Status::Done {
                let n = meta["results"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or_else(|| text.lines().filter(|l| l.split_once(". ").is_some_and(|(n, _)| n.parse::<usize>().is_ok())).count());
                line.desc = Some(plural(n, "result", "results"));
            }
            (Kind::Search, "Searching the web for", "Searched the web for", query)
        }
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
    if name == "subagent" {
        line.session = str_at(input, "session_id").map(str::to_owned).or_else(|| json_of(text)["session_id"].as_str().map(str::to_owned));
    }
    line.kind = kind;
    // Ended by the user: a call the stopped turn cut off, or one that says it was cancelled.
    if status == Status::Error && ((out.is_none() && cx.stopped) || text.trim_start().starts_with("cancelled")) {
        status = Status::Stopped;
    }
    line.status = status;
    line.title = match (status, failed) {
        (Status::Error, Some(failed)) => failed,
        (Status::Stopped, _) if kind == Kind::Shell && run == "Running" => title("Stopped", &subject),
        (Status::Stopped, _) => {
            line.desc = Some("stopped".into());
            title(done, &subject)
        }
        (Status::Input, _) => "Needs you".to_owned(),
        // The card above the composer asks; the line says what waits.
        (Status::Waiting(question), _) => {
            line.desc = (!subject.is_empty()).then(|| clip(&subject, 60));
            (if question { "Needs your answer" } else { "Needs approval" }).to_owned()
        }
        (Status::Running | Status::Pending, _) => title(run, &subject),
        _ => title(done, &subject),
    };
    if status == Status::Error && out.is_none() {
        line.desc = Some("interrupted".into());
    }
    line.clock = status == Status::Running && line.desc.is_some() && line.desc == elapsed.map(short);
    if !quiet && line.detail.is_none() && !text.trim().is_empty() {
        line.detail = Some(if status == Status::Error { Detail::Text(tail(text)) } else { Detail::Mono(tail(text)) });
    }
    // No absolute paths on screen: the project's read relative, the rest of home as `~`.
    let show = |s: &str| display(s, cx.cwd, cx.home);
    line.title = show(&line.title);
    line.desc = line.desc.as_deref().map(show);
    line.detail = line.detail.map(|d| match d {
        Detail::Text(t) => Detail::Text(show(&t)),
        Detail::Mono(t) => Detail::Mono(show(&t)),
        Detail::Diff(t) => Detail::Diff(show(&t)),
    });
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
            Some(removed) if removed > 0 => Some(format!("+{} \u{2212}{removed}", self.added)),
            _ => Some(format!("+{}", self.added)),
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

fn file_meta<'a>(m: &'a Value, cwd: Option<&str>, home: Option<&str>) -> FileMeta<'a> {
    (m["path"].as_str().map(|p| rel(p, cwd, home)), m["added"].as_u64(), m["removed"].as_u64(), m["diff"].as_str())
}

fn edit_stats(name: &str, input: &Value, meta: &Value, cwd: Option<&str>, home: Option<&str>) -> EditStats {
    let one = |m| file_meta(m, cwd, home);
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
    let path = str_at(input, "path").map(|p| rel(p, cwd, home));
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
            patch_stats(patch, cwd, home)
        }
    }
}

/// Files and line counts of an `apply_patch` envelope.
fn patch_stats(patch: &str, cwd: Option<&str>, home: Option<&str>) -> EditStats {
    let mut stats = EditStats { removed: Some(0), ..EditStats::default() };
    for line in patch.lines() {
        let file = ["*** Update File: ", "*** Add File: ", "*** Delete File: ", "*** Move to: "].iter().find_map(|p| line.strip_prefix(p));
        if let Some(file) = file {
            let file = rel(file.trim(), cwd, home);
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

/// A turn's error in words: without its class prefix (`Auth: …`), and a
/// rejected sign-in says so (the full text stays in the line's details).
fn turn_error(error: &str) -> String {
    let line = error.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(error);
    match line.split_once(": ") {
        Some(("Auth", _)) => "The provider didn't accept the sign-in · /providers".to_owned(),
        Some((class, rest)) if !class.contains(' ') && class.chars().next().is_some_and(char::is_uppercase) && !rest.is_empty() => rest.to_owned(),
        _ => line.to_owned(),
    }
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

/// `path` relative to `cwd` (or `~` for `home`, the daemon user's), shortened to its last components.
#[must_use]
pub fn rel(path: &str, cwd: Option<&str>, home: Option<&str>) -> String {
    let path = path.trim();
    let inside = cwd
        .map(|c| c.trim_end_matches('/'))
        .filter(|c| !c.is_empty())
        .and_then(|c| path.strip_prefix(c))
        .and_then(|r| if r.is_empty() { Some(".".to_owned()) } else { r.strip_prefix('/').map(str::to_owned) });
    let home = home.map(|h| h.trim_end_matches('/')).filter(|h| h.len() > 1);
    let shown = inside
        .or_else(|| home.and_then(|h| path.strip_prefix(h).filter(|r| r.starts_with('/')).map(|r| format!("~{r}"))))
        .unwrap_or_else(|| path.to_owned());
    if shown.chars().count() <= 60 {
        return shown;
    }
    let parts: Vec<&str> = shown.rsplitn(3, '/').collect();
    if parts.len() == 3 { format!("…/{}/{}", parts[1], parts[0]) } else { clip(&shown, 60) }
}

/// Text as shown (commands, titles, details): a leading `cd <project> &&`
/// goes, `<project>/x` reads `x` and other paths under `home` (the daemon
/// user's) `~/…`. Display only; what otto runs is unchanged.
#[must_use]
pub fn display(text: &str, cwd: Option<&str>, home: Option<&str>) -> String {
    let cwd = cwd.map(|c| c.trim_end_matches('/')).filter(|c| c.len() > 1);
    let home = home.map(|h| h.trim_end_matches('/')).filter(|h| h.len() > 1);
    // The project as `~/…` too, as a command may name it.
    let tilde = cwd.zip(home).and_then(|(c, h)| c.strip_prefix(h).filter(|r| r.starts_with('/')).map(|r| format!("~{r}")));
    let bases: Vec<&str> = cwd.into_iter().chain(tilde.as_deref()).collect();
    let mut text = text;
    for base in &bases {
        text = drop_cd(text, base);
    }
    let mut out = text.to_owned();
    for base in &bases {
        out = swap(&out, base, "", ".");
    }
    match home {
        Some(h) => swap(&out, h, "~/", "~"),
        None => out,
    }
}

/// `cmd` without a leading `cd <base> &&` / `cd <base>;` (quoted or not).
fn drop_cd<'a>(cmd: &'a str, base: &str) -> &'a str {
    let Some(rest) = cmd.trim_start().strip_prefix("cd ") else { return cmd };
    let rest = rest.trim_start();
    for quote in ["\"", "'", ""] {
        let Some(after) = rest.strip_prefix(quote).and_then(|r| r.strip_prefix(base)) else { continue };
        let after = after.strip_prefix('/').unwrap_or(after);
        let Some(after) = after.strip_prefix(quote) else { continue };
        let after = after.trim_start();
        if let Some(after) = after.strip_prefix("&&").or_else(|| after.strip_prefix(';')) {
            return after.trim_start();
        }
    }
    cmd
}

/// `base/x` → `under` + `x`, `base` on its own → `alone`; only where `base` is a whole path.
fn swap(text: &str, base: &str, under: &str, alone: &str) -> String {
    let pathy = |c: char| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(base) {
        let (before, after) = (&rest[..at], &rest[at + base.len()..]);
        out.push_str(before);
        let edge = before.chars().next_back().is_none_or(|c| !pathy(c));
        let tail = after.strip_prefix('/');
        match tail {
            Some(t) if edge && t.chars().next().is_some_and(pathy) => {
                out.push_str(under);
                rest = t;
            }
            _ if edge && after.chars().next().is_none_or(|c| c == '/' || !pathy(c)) => {
                out.push_str(alone);
                rest = tail.unwrap_or(after);
            }
            _ => {
                out.push_str(base);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
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

    #[test]
    fn turn_errors_read_as_words() {
        assert_eq!(turn_error("Auth: static credential rejected"), "The provider didn't accept the sign-in · /providers");
        assert_eq!(turn_error("Timeout: no response in 60s"), "no response in 60s");
        assert_eq!(turn_error("\nsomething broke: badly"), "something broke: badly");
    }

    fn call(id: &str, name: &str, input: Value) -> Part {
        Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None }
    }

    #[test]
    fn kind_icons_exist() {
        use Kind::*;
        for kind in [Reasoning, Read, List, Search, Shell, Edit, Fetch, Terminal, Browser, Ask, Todos, Subagent, Error, Other] {
            assert!(crate::assets::has_icon(kind.icon()), "{kind:?}: {}", kind.icon());
        }
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
        build(&Input { items, tools, turn, error: None, cwd: Some("/work/app"), home: None, times, now })
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
    fn a_compaction_is_a_marker_between_turns() {
        let compaction = Item::new("c", Role::System, vec![Part::Compaction { summary: Some("so far".into()), opaque: None }]);
        let other = Item::new("s", Role::System, vec![Part::System { text: "note".into() }]);
        let items = vec![user("u", "hi"), assistant("a", vec![Part::text("Hello.")]), compaction, other, user("u2", "more")];
        let rows = build_rows(&items, &HashMap::new(), None, &Times::default(), 0);
        assert_eq!(rows.iter().map(Row::key).collect::<Vec<_>>(), ["u", "a#t0", "c", "u2"]);
        assert!(matches!(&rows[2], Row::Marker { text, .. } if text == "Context compacted"));
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
        assert_eq!(keys, ["u1", "g:a1#0", "a2#t0", "g:c3", "a3#t0"]);
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
    fn a_new_line_keeps_the_group_and_its_lines() {
        let reason = Part::Reasoning { summary: None, text: Some("look".into()), opaque: None };
        let mut items = vec![
            user("u", "go"),
            assistant("a", vec![reason, call("c1", "read", json!({"path": "/work/app/a.rs"}))]),
            tool_item("t", vec![result("c1", "x", false)]),
        ];
        let mut times = Times::default();
        span(&mut times, "a#0", 0, Some(900));
        span(&mut times, "c1", 900, Some(1_000));
        let before = build_rows(&items, &HashMap::new(), Some(TurnStatus::Running), &times, 1_000);
        let mut entered: HashMap<String, u64> = HashMap::new();
        for key in arrivals(&[], &before, |k| entered.contains_key(k)) {
            entered.insert(key, 1);
        }
        assert_eq!(entered.len(), 4, "u, the group and its two lines");

        items.push(assistant("b", vec![call("c2", "shell", json!({"command": "ls"}))]));
        let mut tools = HashMap::new();
        tools.insert(
            "c2".to_owned(),
            ToolState { name: "shell".into(), input: json!({"command": "ls"}), status: ToolStatus::Running, ..ToolState::default() },
        );
        span(&mut times, "c2", 1_100, None);
        let after = build_rows(&items, &tools, Some(TurnStatus::Running), &times, 1_200);
        let (g0, g1) = (group_of(&before[1]), group_of(&after[1]));
        assert_eq!(g0.key, g1.key, "the group stays one element");
        assert_eq!(g1.lines[..2], g0.lines[..], "earlier lines are unchanged");
        assert_eq!(arrivals(&before, &after, |k| entered.contains_key(k)), ["c2"], "only the new line enters");
        // The group is remeasured in place, never spliced (which drops the scroll anchor).
        assert_eq!(list_change(&before, &after), ListChange { remeasure: 1..2, splice: None });
        assert_eq!(list_change(&after, &after), ListChange { remeasure: 2..2, splice: None });

        // Text after the group: the group folds in place, the text is new.
        items.push(assistant("c", vec![Part::text("Done.")]));
        let done = build_rows(&items, &tools, Some(TurnStatus::Running), &times, 1_300);
        assert_eq!(list_change(&after, &done), ListChange { remeasure: 1..2, splice: Some((2..2, 1)) });
        assert_eq!(arrivals(&after, &done, |k| entered.contains_key(k)), ["c#t0"]);
    }

    #[test]
    fn thinking_then_text_folds_above_the_text_with_one_indicator() {
        use otto_client::SessionMirror;
        use otto_proto::Event;
        let view: otto_proto::SessionView = serde_json::from_value(json!({"info":{"id":"s","project_id":"p","agent":"a","provider":"p","model":"m","title":"t","cwd":"/","status":"busy","created_at":0,"updated_at":0,"last_seq":1,"usage":{"usage":{"input":0,"output":0},"cost_usd":0.0}},"items":[serde_json::to_value(user("u", "go")).unwrap()],"pending_approvals":[],"pending_questions":[],"queue":[],"running_tool_calls":[],"seq":1})).unwrap();
        let mut m = SessionMirror::new(view);
        m.apply(2, &Event::TurnStarted { turn: 1 });
        m.apply(3, &Event::ItemStarted { item_id: "a".into(), role: Role::Assistant });
        let mut times = Times::live();
        let think = |t: &str| Event::ReasoningDelta { item_id: "a".into(), text: t.into(), summary: false };
        let text = |t: &str| Event::TextDelta { item_id: "a".into(), text: t.into() };
        // Thinking streams (an empty delta first, as Claude sends without `display`): its live group is the indicator.
        m.apply_live(3, &think(""));
        m.apply_live(3, &think("Plan the"));
        times.observe(&m.items, &m.tools, true, HashMap::new(), 0);
        let rows = build_rows(&m.items, &m.tools, m.turn.map(|t| t.1), &times, 0);
        assert!(indicator_is_group(&rows), "the live group's header is the one indicator");
        // The reply starts; the thinking's records land while it streams.
        m.apply_live(3, &text("Build the runtime"));
        m.apply(4, &think(""));
        m.apply(5, &think("Plan the"));
        m.apply_live(5, &text(" first, then the"));
        m.apply(6, &text("Build the runtime"));
        times.observe(&m.items, &m.tools, true, HashMap::new(), 1_000);
        let rows = build_rows(&m.items, &m.tools, m.turn.map(|t| t.1), &times, 1_000);
        let keys: Vec<&str> = rows.iter().map(Row::key).collect();
        assert_eq!(keys, ["u", "g:a#0", "a#t0"], "the folded group above the text, nothing after it");
        let g = group_of(&rows[1]);
        assert!(!g.live && g.lines.iter().all(|l| l.status == Status::Done), "{g:?}");
        assert_eq!(g.lines[0].title, "Thought for 1s");
        assert!(matches!(&rows[2], Row::Text { text, streaming: true, .. } if text == "Build the runtime first, then the"));
        assert!(!indicator_is_group(&rows), "the activity row is the indicator now, after the text");
    }

    #[test]
    fn web_tools_read_as_searches_and_fetches() {
        let cx = Ctx {
            cwd: Some("/w"),
            home: None,
            times: &Times::default(),
            now: 0,
            running: false,
            stopped: false,
            jobs: &HashMap::new(),
            files: &HashMap::new(),
        };
        let live = |name: &str, error: bool, text: &str, meta: Value| ToolState {
            name: name.into(),
            status: if error { ToolStatus::Error } else { ToolStatus::Done },
            output: Some(json!({"content": [{"type": "text", "text": text}], "metadata": meta, "is_error": error})),
            ..ToolState::default()
        };
        let input = json!({"query": "gpui list remeasure", "limit": 5});
        let hits = "1. List in gpui\n   https://zed.dev/a\n   About lists\n2. Remeasure\n   https://zed.dev/b";
        let out = live("web_search", false, hits, json!({"engine": "duckduckgo", "rendered": true, "results": [{}, {}]}));
        let line = tool("s", "web_search", &input, Some(&out), None, &cx);
        assert_eq!(
            (line.title.as_str(), line.desc.as_deref(), line.kind),
            ("Searched the web for \"gpui list remeasure\"", Some("2 results"), Kind::Search)
        );
        assert_eq!(line.detail, Some(Detail::Mono(hits.into())), "the results list");
        // Counted from the text when the metadata has no list.
        let line = tool("s", "web_search", &input, None, Some(&(hits.into(), false)), &cx);
        assert_eq!(line.desc.as_deref(), Some("2 results"));
        let line = tool("s", "web_search", &input, None, Some(&("Search failed: timed out".into(), true)), &cx);
        assert_eq!((line.title.as_str(), line.status), ("Couldn't search the web for \"gpui list remeasure\"", Status::Error));
        assert_eq!(tool("s", "web_search", &input, None, None, &Ctx { running: true, ..cx }).title, "Searching the web for \"gpui list remeasure\"");

        let input = json!({"url": "https://example.com/docs/intro?ref=x#top"});
        let page = "Title: Intro\nURL: https://example.com/docs/intro\nNote: rendered in browser\n\n# Intro\n\nHello.";
        let out = live("web_fetch", false, page, json!({"url": "https://example.com/docs/intro", "rendered": true}));
        let line = tool("f", "web_fetch", &input, Some(&out), None, &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref()), ("Fetched example.com/docs/intro", Some("rendered")));
        assert_eq!(line.detail, Some(Detail::Mono("# Intro\n\nHello.".into())), "the page's markdown");
        let plain = "URL: https://example.com/docs/intro\n\nHello.";
        let line = tool("f", "web_fetch", &input, None, Some(&(plain.into(), false)), &cx);
        assert_eq!((line.title.as_str(), line.desc), ("Fetched example.com/docs/intro", None));
        let line = tool("f", "web_fetch", &input, None, Some(&("Failed: HTTP 404 for https://example.com/docs/intro".into(), true)), &cx);
        assert_eq!((line.title.as_str(), line.status, line.desc), ("Couldn't fetch example.com/docs/intro", Status::Error, None));
    }

    #[test]
    fn a_streamed_reply_keeps_its_rows_when_it_completes() {
        use otto_client::SessionMirror;
        use otto_proto::Event;
        let view: otto_proto::SessionView = serde_json::from_value(json!({"info":{"id":"s","project_id":"p","agent":"a","provider":"p","model":"m","title":"t","cwd":"/","status":"busy","created_at":0,"updated_at":0,"last_seq":1,"usage":{"usage":{"input":0,"output":0},"cost_usd":0.0}},"items":[serde_json::to_value(user("u", "go")).unwrap()],"pending_approvals":[],"pending_questions":[],"queue":[],"running_tool_calls":[],"seq":1})).unwrap();
        let mut m = SessionMirror::new(view);
        m.apply(2, &Event::TurnStarted { turn: 1 });
        m.apply(3, &Event::ItemStarted { item_id: "a".into(), role: Role::Assistant });
        let times = Times::default();
        let rows = |m: &SessionMirror| build_rows(&m.items, &m.tools, m.turn.map(|t| t.1), &times, 0);
        let keys = |rows: &[Row]| rows.iter().map(|r| r.key().to_owned()).collect::<Vec<_>>();
        // Text, thinking, then text again, in tiny live deltas.
        let text = |t: &str| Event::TextDelta { item_id: "a".into(), text: t.into() };
        let think = |t: &str| Event::ReasoningDelta { item_id: "a".into(), text: t.into(), summary: false };
        let mut seen: Vec<Vec<String>> = Vec::new();
        for event in [text("I'll look "), text("first.\n\n"), think("hm"), think("m"), text("1. **Blocks"), text(" are typed.** Each")] {
            m.apply_live(3, &event);
            seen.push(keys(&rows(&m)));
        }
        let streaming = rows(&m);
        assert_eq!(keys(&streaming), ["u", "a#t0", "g:a#1", "a#t1"]);
        assert!(matches!(streaming.last(), Some(Row::Text { streaming: true, .. })));
        assert_eq!(revealing(&streaming), Some("a#t1"), "only the last row reveals");
        // Once a row exists for a key it stays: nothing is re-keyed (re-entered, re-revealed) mid-stream.
        for (i, k) in keys(&streaming).iter().enumerate() {
            let first = seen.iter().position(|s| s.contains(k)).unwrap();
            assert!(seen[first..].iter().all(|s| s.get(i) == Some(k)), "{k} moved: {seen:?}");
        }
        // The finished item holds a thinking block the stream never showed, and splits the last text in two.
        let reason = |t: Option<&str>| Part::Reasoning { summary: None, text: t.map(str::to_owned), opaque: None };
        let done = assistant(
            "a",
            vec![Part::text("I'll look first.\n\n"), reason(Some("hmm")), reason(None), Part::text("1. **Blocks"), Part::text(" are typed.** Each")],
        );
        m.apply(4, &Event::ItemDone { item: done });
        let finished = rows(&m);
        assert_eq!(keys(&finished), keys(&streaming), "same rows, same keys");
        assert_eq!(finished[3], streaming[3], "the same text, still streaming until the turn ends");
        assert_eq!(list_change(&streaming, &finished).splice, None, "nothing spliced in or out");
        // A group arriving after the text settles it: only the newest row reveals.
        m.apply(5, &Event::ToolCall { call_id: "c".into(), name: "read".into(), input: json!({"path": "a.rs"}) });
        assert_eq!(revealing(&rows(&m)), None);
        m.apply(6, &Event::TurnFinished { turn: 1, status: TurnStatus::Done, usage: otto_llm::Usage::default() });
        let ended = rows(&m);
        assert!(matches!(&ended[3], Row::Text { key, streaming: false, .. } if key == "a#t1"));
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
        let halted = vec![line(Kind::Shell, Status::Stopped, &[]), line(Kind::Shell, Status::Error, &[]), line(Kind::Read, Status::Stopped, &[])];
        assert_eq!(summary(&halted, Some(3_000), true), "Worked for 3s · read 1 file · ran 2 commands, 1 failed, 1 stopped · 1 stopped");
        assert_eq!(summary(&halted[..1], None, false), "Ran 1 command, 1 stopped");
        assert_eq!(summary(&[], None, false), "Worked");
        // A failed copy, write or edit didn't edit anything (nor a failed read read, or fetch fetch): counted as failed.
        let copy = vec![line(Kind::Edit, Status::Error, &["assets/shot.png"]), line(Kind::Read, Status::Done, &[])];
        assert_eq!(summary(&copy, Some(2_000), false), "Worked for 2s · read 1 file · 1 failed");
        let some = vec![line(Kind::Edit, Status::Done, &["a.rs"]), line(Kind::Edit, Status::Error, &["b.rs"]), line(Kind::Edit, Status::Error, &[])];
        assert_eq!(summary(&some, None, false), "Edited 1 file · 2 failed");
        let others = vec![line(Kind::Read, Status::Error, &[]), line(Kind::Fetch, Status::Error, &[]), line(Kind::Search, Status::Error, &[])];
        assert_eq!(summary(&others, None, false), "3 failed");
    }

    #[test]
    fn tool_titles_and_descriptions() {
        let cx = Ctx {
            cwd: Some("/work/app"),
            home: None,
            times: &Times::default(),
            now: 0,
            running: false,
            stopped: false,
            jobs: &HashMap::new(),
            files: &HashMap::new(),
        };
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
        // A subagent line names its child: from the delegate result, or the input of other actions.
        let line =
            tool("d", "subagent", &json!({"action": "delegate", "task": "t"}), None, Some(&ok(r#"{"session_id":"s9","status":"started"}"#)), &cx);
        assert_eq!(line.session.as_deref(), Some("s9"));
        let line = tool("m", "subagent", &json!({"action": "message", "session_id": "s7", "message": "faster"}), None, Some(&ok("sent")), &cx);
        assert_eq!(line.session.as_deref(), Some("s7"));
        assert_eq!(tool("l", "subagent", &json!({"action": "list"}), None, Some(&ok("[]")), &cx).session, None);
    }

    #[test]
    fn shell_lines_report_exit_and_duration() {
        let mut times = Times::default();
        span(&mut times, "c", 0, Some(1_234));
        let cx =
            Ctx { cwd: None, home: None, times: &times, now: 5_000, running: true, stopped: false, jobs: &HashMap::new(), files: &HashMap::new() };
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
        let cx = Ctx {
            cwd: None,
            home: None,
            times: &Times::default(),
            now: 0,
            running: false,
            stopped: false,
            jobs: &HashMap::new(),
            files: &HashMap::new(),
        };
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
    fn command_block_shells_open_wait_and_report() {
        let reports = |text: &str| job_reports(&[Item::new("s", Role::System, vec![Part::System { text: text.into() }])]);
        let jobs = reports("Background shell job-a1 (`cargo test`) in block b9 exited with code 101.\nFAILED");
        let cx = Ctx { cwd: None, home: None, times: &Times::default(), now: 0, running: false, stopped: false, jobs: &jobs, files: &HashMap::new() };
        // Running in its block: open and stop it.
        let live = ToolState { name: "shell".into(), status: ToolStatus::Running, messages: vec!["block:b3".into()], ..ToolState::default() };
        let none = HashMap::new();
        let cx_live = Ctx { running: true, stopped: false, jobs: &none, ..cx };
        let line = tool("r", "shell", &json!({"command": "cargo build"}), Some(&live), None, &cx_live);
        assert_eq!((line.status, line.reveal.as_deref(), line.stop.as_deref()), (Status::Running, Some("b3"), Some("b3")));
        // Waiting for input (from history text): needs the user.
        let waiting =
            "waiting_for_input: the command waits for input (job job-w2). Answer with terminal_send to block b5, or ask the user.\nPassword:";
        let line = tool("w", "shell", &json!({"command": "sudo true"}), None, Some(&(waiting.into(), false)), &cx);
        assert_eq!(
            (line.title.as_str(), line.desc.as_deref(), line.status, line.reveal.as_deref(), line.detail),
            ("Needs you", Some("sudo true"), Status::Input, Some("b5"), Some(Detail::Mono("Password:".into())))
        );
        // Background: running until its report, then done with the exit code.
        let started = ("Started job-b4 in block b8; its result arrives as a message when it exits.".to_owned(), false);
        let line = tool("b", "shell", &json!({"command": "cargo test", "background": true}), None, Some(&started), &cx);
        assert_eq!(
            (line.title.as_str(), line.desc.as_deref(), line.status, line.stop.as_deref()),
            ("Running cargo test", Some("background"), Status::Running, Some("b8"))
        );
        assert!(line.detail.is_none());
        let started = ("Started job-a1 in block b9; its result arrives as a message when it exits.".to_owned(), false);
        let line = tool("d", "shell", &json!({"command": "cargo test", "background": true}), None, Some(&started), &cx);
        assert_eq!(
            (line.title.as_str(), line.desc.as_deref(), line.status, line.stop, line.detail),
            ("Failed to run cargo test", Some("exit 101"), Status::Error, None, Some(Detail::Mono("FAILED".into())))
        );
        assert_eq!(reports("Background shell job-1 (`x`) was stopped (closed).\n")["job-1"].0, JobEnd::Stopped);
        assert_eq!(reports("Background shell job-2 (`x`) in block b exited with code 0.\nok")["job-2"], (JobEnd::Exit(0), "ok".into()));
        // Stopped by the user: from the metadata, or (loaded) the result's last line.
        let stopped = ("partial\n[stopped: closed]".to_owned(), true);
        let line = tool("s", "shell", &json!({"command": "sleep 60"}), None, Some(&stopped), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref(), line.status), ("Stopped sleep 60", None, Status::Stopped));
        let cancelled = ToolState {
            name: "shell".into(),
            status: ToolStatus::Error,
            output: Some(
                json!({"content": [{"type": "text", "text": "x\n[stopped: cancelled]"}], "is_error": true, "metadata": {"termination": "cancelled"}}),
            ),
            ..ToolState::default()
        };
        let line = tool("s", "shell", &json!({"command": "cargo test"}), Some(&cancelled), None, &cx);
        assert_eq!((line.title.as_str(), line.status), ("Stopped cargo test", Status::Stopped));
        // A background job stopped with `job kill` (or its block closed).
        let jobs = reports("Background shell job-k (`cargo run`) was stopped (closed).\n");
        let started = ("Started job-k in block b2; its result arrives as a message when it exits.".to_owned(), false);
        let line = tool("k", "shell", &json!({"command": "cargo run"}), None, Some(&started), &Ctx { jobs: &jobs, ..cx });
        assert_eq!((line.title.as_str(), line.desc.as_deref(), line.status), ("Stopped cargo run", None, Status::Stopped));
        let timed = ("[stopped: timeout]".to_owned(), true);
        let line = tool("t", "shell", &json!({"command": "sleep 60"}), None, Some(&timed), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref(), line.status), ("Failed to run sleep 60", Some("timed out"), Status::Error));
        // Other tools: cancelled by a stop, or cut off by the stopped turn.
        let line = tool("r", "read", &json!({"path": "a.rs"}), None, Some(&("cancelled; outcome may be unknown".into(), true)), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref(), line.status), ("Read a.rs", Some("stopped"), Status::Stopped));
        let line = tool("r", "read", &json!({"path": "a.rs"}), None, None, &Ctx { stopped: true, ..cx });
        assert_eq!((line.desc.as_deref(), line.status), (Some("stopped"), Status::Stopped));
        assert_eq!(tool("r", "read", &json!({"path": "a.rs"}), None, None, &cx).desc.as_deref(), Some("interrupted"));
    }

    #[test]
    fn edits_count_lines_from_metadata_input_or_patch() {
        let cx = Ctx {
            cwd: Some("/w"),
            home: None,
            times: &Times::default(),
            now: 0,
            running: false,
            stopped: false,
            jobs: &HashMap::new(),
            files: &HashMap::new(),
        };
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
            home: None,
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
        times.observe(&items, &HashMap::new(), true, HashMap::new(), 100);
        assert_eq!(times.get("c1").and_then(|s| s.start), None, "already there when loaded");
        let mut items = items;
        items.push(assistant("b", vec![call("c2", "read", json!({}))]));
        times.observe(&items, &HashMap::new(), true, HashMap::new(), 200);
        assert_eq!(times.get("c2").map(|s| (s.start, s.end)), Some((Some(200), None)));
        items.push(tool_item("t2", vec![result("c2", "x", false)]));
        times.observe(&items, &HashMap::new(), true, HashMap::new(), 900);
        assert_eq!(times.elapsed("c2", 5_000), Some(700));
        assert_eq!(times.elapsed("c1", 5_000), None);
    }

    #[test]
    fn calls_running_when_loaded_keep_otto_s_start() {
        let items = vec![user("u", "go"), assistant("a", vec![call("c", "shell", json!({"command": "cargo test"})), call("d", "read", json!({}))])];
        let tools = HashMap::from([(
            "c".to_owned(),
            ToolState { name: "shell".into(), status: ToolStatus::Running, started_at: Some(1), ..ToolState::default() },
        )]);
        let mut times = Times::default();
        times.seed([("c".to_owned(), 2_000)]);
        times.observe(&items, &tools, true, HashMap::new(), 9_000);
        assert_eq!(times.elapsed("c", 14_000), Some(12_000), "timed from otto's start, not the load");
        assert_eq!(times.elapsed("d", 14_000), None, "no start reported: no time");
        let rows = build_rows(&items, &tools, Some(TurnStatus::Running), &times, 14_000);
        let line = &group_of(&rows[1]).lines[0];
        assert_eq!((line.desc.as_deref(), line.clock), (Some("12s"), true));
    }

    #[test]
    fn calls_waiting_for_the_user_say_so_and_count_the_wait() {
        let mut items = vec![user("u", "go")];
        let mut times = Times::default();
        times.observe(&items, &HashMap::new(), true, HashMap::new(), 0);
        items
            .push(assistant("a", vec![call("c", "shell", json!({"command": "rm -rf build"})), call("q", "ask_user", json!({"question": "Which?"}))]));
        let waiting = HashMap::from([("c".to_owned(), false), ("q".to_owned(), true)]);
        times.observe(&items, &HashMap::new(), true, waiting.clone(), 100);
        times.observe(&items, &HashMap::new(), true, waiting, 5_000);
        let rows = build_rows(&items, &HashMap::new(), Some(TurnStatus::Running), &times, 5_000);
        let lines = &group_of(&rows[1]).lines;
        assert_eq!(
            (lines[0].title.as_str(), lines[0].desc.as_deref(), lines[0].status),
            ("Needs approval", Some("rm -rf build"), Status::Waiting(false))
        );
        assert_eq!((lines[1].title.as_str(), lines[1].status), ("Needs your answer", Status::Waiting(true)));
        // Allowed: running, timed from the call (the wait counts).
        let mut tools = HashMap::new();
        tools.insert("c".to_owned(), ToolState { name: "shell".into(), status: ToolStatus::Running, ..ToolState::default() });
        times.observe(&items, &tools, true, HashMap::from([("q".to_owned(), true)]), 14_000);
        let rows = build_rows(&items, &tools, Some(TurnStatus::Running), &times, 14_800);
        let line = &group_of(&rows[1]).lines[0];
        assert_eq!((line.title.as_str(), line.status, line.desc.as_deref()), ("Running rm -rf build", Status::Running, Some("15s")));
        // Done after a 14 s wait and a short run: the fold reads the full wall time.
        items.push(tool_item("t", vec![result("c", "ok", false), result("q", "main", false)]));
        items.push(assistant("b", vec![Part::text("Done.")]));
        times.observe(&items, &HashMap::new(), false, HashMap::new(), 15_100);
        let rows = build_rows(&items, &HashMap::new(), Some(TurnStatus::Done), &times, 20_000);
        let g = group_of(&rows[1]);
        assert!(g.summary.starts_with("Worked for 15s"), "{}", g.summary);
        assert_eq!(g.lines[0].desc.as_deref(), Some("15s"));
    }

    /// What a session shows once it is over: each group's summary and every line's title and time.
    type Folded = Vec<(String, Vec<(String, Option<String>)>)>;
    type Frame = (u64, Vec<Item>, HashMap<String, ToolState>, HashMap<String, bool>);

    fn folded(rows: &[Row]) -> Folded {
        rows.iter()
            .filter_map(|r| match r {
                Row::Group(g) => Some((g.summary.clone(), g.lines.iter().map(|l| (l.title.clone(), l.desc.clone())).collect())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_reloaded_session_reads_the_times_it_showed_live() {
        use otto_proto::timing::{Mark, part_times};
        let shell = || call("c1", "shell", json!({"command": "cargo test"}));
        let reasoning = |text: &str| Part::Reasoning { summary: None, text: Some(text.into()), opaque: None };
        let (u, done) = (user("u", "go"), assistant("a", vec![reasoning("hmm"), Part::text("Let me look."), shell()]));
        let finished = vec![
            u.clone(),
            done.clone(),
            tool_item("r", vec![result("c1", "ok", false)]),
            assistant("b", vec![reasoning("so"), Part::text("Done.")]),
        ];
        // Live, frame by frame (wall ms), as otto journals it.
        let mut live = Times::live();
        let tool = |status| HashMap::from([("c1".to_owned(), ToolState { name: "shell".into(), status, ..ToolState::default() })]);
        let frames: Vec<Frame> = vec![
            (1_000, vec![u.clone(), assistant("a", vec![])], HashMap::new(), HashMap::new()),
            (1_100, vec![u.clone(), assistant("a", vec![reasoning("h")])], HashMap::new(), HashMap::new()),
            (4_100, vec![u.clone(), assistant("a", vec![reasoning("hmm"), Part::text("Let")])], HashMap::new(), HashMap::new()),
            (4_600, vec![u.clone(), assistant("a", vec![reasoning("hmm"), Part::text("Let me look.")])], tool(ToolStatus::Input), HashMap::new()),
            // Called, then waiting for an approval: the wait counts.
            (5_000, vec![u.clone(), done.clone()], tool(ToolStatus::Running), HashMap::from([("c1".to_owned(), false)])),
            (6_500, vec![u.clone(), done.clone()], tool(ToolStatus::Running), HashMap::new()),
            (9_000, vec![u.clone(), done.clone()], tool(ToolStatus::Done), HashMap::new()),
            (9_010, finished[..3].to_vec(), tool(ToolStatus::Done), HashMap::new()),
            (9_100, [&finished[..3], &[assistant("b", vec![reasoning("s")])][..]].concat(), tool(ToolStatus::Done), HashMap::new()),
            (
                9_900,
                [&finished[..3], &[assistant("b", vec![reasoning("so"), Part::text("Do")])][..]].concat(),
                tool(ToolStatus::Done),
                HashMap::new(),
            ),
            (10_000, finished.clone(), tool(ToolStatus::Done), HashMap::new()),
        ];
        for (at, items, tools, waiting) in &frames {
            live.observe(items, tools, true, waiting.clone(), *at);
        }
        live.observe(&finished, &tool(ToolStatus::Done), false, HashMap::new(), 10_050);
        // The same run as otto's journal records it.
        let mark = |kind: &str, at: i64, item: Option<&str>, call: Option<&str>| Mark {
            kind: kind.into(),
            at,
            item_id: item.map(Into::into),
            call_id: call.map(Into::into),
        };
        let marks = [
            mark("item.started", 1_000, Some("a"), None),
            mark("reasoning.delta", 1_100, Some("a"), None),
            mark("reasoning.delta", 3_000, Some("a"), None),
            mark("text.delta", 4_100, Some("a"), None),
            mark("tool.input_delta", 4_600, None, Some("c1")),
            mark("item.done", 5_000, Some("a"), None),
            mark("tool.call", 5_000, None, Some("c1")),
            mark("tool.result", 9_000, None, Some("c1")),
            mark("item.done", 9_010, Some("r"), None),
            mark("item.started", 9_050, Some("b"), None),
            mark("reasoning.delta", 9_100, Some("b"), None),
            mark("text.delta", 9_900, Some("b"), None),
            mark("item.done", 10_000, Some("b"), None),
            mark("turn.finished", 10_050, None, None),
        ];
        let mut history = Times::default();
        history.record(&finished, &part_times(&finished, &marks));
        history.observe(&finished, &HashMap::new(), false, HashMap::new(), 500_000);
        let at = |times: &Times, tools: &HashMap<String, ToolState>| folded(&build_rows(&finished, tools, Some(TurnStatus::Done), times, 500_000));
        let seen = at(&live, &tool(ToolStatus::Done));
        assert_eq!(seen, at(&history, &HashMap::new()), "live and reloaded agree");
        assert_eq!(seen[0].0, "Thought for 3s");
        // From the call's first input to the last reasoning's end, the approval wait included.
        assert_eq!(
            seen[1],
            ("Worked for 5s · ran 1 command".into(), vec![("Ran cargo test".into(), Some("4.4s".into())), ("Thought for 1s".into(), None)])
        );
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
            &Ctx {
                cwd: None,
                home: None,
                times: &Times::default(),
                now: 0,
                running: false,
                stopped: false,
                jobs: &HashMap::new(),
                files: &HashMap::new(),
            },
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
        assert_eq!(rel("/work/app/src/main.rs", Some("/work/app/"), None), "src/main.rs");
        assert_eq!(rel("/work/app", Some("/work/app"), None), ".");
        assert_eq!(rel("/elsewhere/x.rs", Some("/work/app"), None), "/elsewhere/x.rs");
        // Commands and details: the project's `cd` goes, project paths read relative, the rest of home as `~`.
        let (cwd, home) = (Some("/Users/me/dev/app"), Some("/Users/me"));
        let show = |t: &str| display(t, cwd, home);
        assert_eq!(show("cd /Users/me/dev/app && cargo test"), "cargo test");
        assert_eq!(show("cd \"/Users/me/dev/app/\"; ls src"), "ls src");
        assert_eq!(show("cd ~/dev/app && git status"), "git status");
        assert_eq!(show("cd /Users/me/dev/app/crates && ls"), "cd crates && ls");
        assert_eq!(show("rg foo /Users/me/dev/app/src /Users/me/dev/app"), "rg foo src .");
        assert_eq!(show("cat /Users/me/dev/app2/x /Users/me/.zshrc"), "cat ~/dev/app2/x ~/.zshrc");
        assert_eq!(show("ls /tmp/Users/me/x"), "ls /tmp/Users/me/x");
        assert_eq!(show("echo hi"), "echo hi");
        assert_eq!(display("cd /w && make", None, None), "cd /w && make");
        let long = format!("/a/{}/deep/file.rs", "x".repeat(80));
        assert_eq!(rel(&long, None, None), "…/deep/file.rs");
        // A remote daemon's home, not this Mac's.
        assert_eq!(rel("/home/bat/notes.md", Some("/srv/app"), Some("/home/bat/")), "~/notes.md");
        assert_eq!(rel("/home/bat/notes.md", None, None), "/home/bat/notes.md", "no home: as is");
        assert_eq!(display("cat /home/bat/.bashrc /home/batman/x", None, Some("/home/bat")), "cat ~/.bashrc /home/batman/x");
        assert_eq!(display("cd ~/app && make", Some("/home/bat/app"), Some("/home/bat")), "make");
        assert_eq!(display("cd ~/app && make", Some("/home/bat/app"), Some("/Users/me")), "cd ~/app && make");
        assert_eq!(dur(400), "1s");
        assert_eq!(dur(42_000), "42s");
        assert_eq!(dur(120_000), "2m");
        assert_eq!(dur(3_700_000), "1h 1m");
        assert_eq!(short(1_234), "1.2s");
        assert_eq!(clip("  first line\nsecond", 5), "firs…");
    }

    #[test]
    fn user_messages_show_attachments_as_chips() {
        use otto_llm::{Media, MediaSource};
        let image = |name: Option<&str>| {
            Part::Image(Media { mime: "image/png".into(), source: MediaSource::Base64 { data: "AA".into() }, name: name.map(str::to_owned) })
        };
        let item = Item::new(
            "u1",
            Role::User,
            vec![
                Part::text("[attachment att_3f2a9c1b: shot, final.png, image/png, 812 KB — copy_attachment can save it into the project]"),
                image(Some("shot, final.png")),
                Part::text("[attachment att_00000001: spec.pdf, application/pdf, 1.2 MB]"),
                Part::text("add these"),
                image(None),
            ],
        );
        let (text, files, mentions) = user_message(&item);
        assert_eq!(text, "add these");
        assert!(mentions.is_empty());
        assert_eq!(
            files,
            [
                FileChip { name: "shot, final.png".into(), mime: "image/png".into(), image: Some("u1#1".into()) },
                FileChip { name: "spec.pdf".into(), mime: "application/pdf".into(), image: None },
                FileChip { name: "Image".into(), mime: "image/png".into(), image: Some("u1#4".into()) },
            ]
        );
        assert_eq!(attachment_note("[attachment att_1: a.txt, text/plain, 3 B]"), Some(("a.txt", "text/plain")));
        assert_eq!(attachment_note("[attachment] hi"), None);
        assert_eq!(attachment_note("see [attachment att_1: a, b, c]"), None);
    }

    #[test]
    fn user_messages_show_mentions_inline() {
        use otto_llm::{Mention, MentionKind};
        let note = "[attachment att_00000001: a.txt, text/plain, 1 B]";
        let mut item = Item::new("u2", Role::User, vec![Part::text(note), Part::text("  ask @build about @gone.rs ")]);
        let at = |s: usize, e: usize| [note.len() + s, note.len() + e];
        item.mentions = vec![
            Mention { kind: MentionKind::Agent, target: "build".into(), range: at(6, 12), missing: false },
            Mention { kind: MentionKind::File, target: "gone.rs".into(), range: at(19, 27), missing: true },
        ];
        let (text, files, mentions) = user_message(&item);
        assert_eq!(text, "ask @build about @gone.rs");
        assert_eq!(files.len(), 1);
        let shown: Vec<(&str, bool)> = mentions.iter().map(|m| (&text[m.range.clone()], m.missing)).collect();
        assert_eq!(shown, [("@build", false), ("@gone.rs", true)]);
    }

    #[test]
    fn copy_attachment_reads_as_copied_name_to_path() {
        let cx = Ctx {
            cwd: Some("/w"),
            home: None,
            times: &Times::default(),
            now: 0,
            running: false,
            stopped: false,
            jobs: &HashMap::new(),
            files: &HashMap::new(),
        };
        let input = json!({"attachment_id": "att_3f2a9c1b", "path": "/w/assets/shot.png"});
        let line = tool("c", "copy_attachment", &input, None, Some(&("Copied shot.png to /w/assets/shot.png (812 KB)".into(), false)), &cx);
        assert_eq!((line.title.as_str(), line.desc.as_deref(), line.detail.is_none()), ("Copied shot.png → assets/shot.png", Some("812 KB"), true));
        assert_eq!(line.files, ["assets/shot.png"]);
        let live = ToolState {
            name: "copy_attachment".into(),
            status: ToolStatus::Done,
            output: Some(json!({"content": [], "metadata": {"path": "/w/assets/shot.png", "bytes": 3, "name": "shot.png"}})),
            ..ToolState::default()
        };
        assert_eq!(tool("c", "copy_attachment", &input, Some(&live), None, &cx).title, "Copied shot.png → assets/shot.png");
        // A read names the file: from the result's metadata, else the user's note; never the id or a path.
        let files = HashMap::from([("att_3f2a9c1b".to_owned(), "screenshot.png".to_owned())]);
        let named = Ctx { files: &files, ..cx };
        let read = tool("r", "read", &json!({"attachment_id": "att_3f2a9c1b"}), None, Some(&("1: hello".into(), false)), &named);
        assert_eq!(read.title, "Read screenshot.png");
        let live = ToolState {
            name: "read".into(),
            status: ToolStatus::Done,
            output: Some(json!({"content": [{"type": "text", "text": "logo.png 40×30"}], "metadata": {"name": "logo.png"}})),
            ..ToolState::default()
        };
        assert_eq!(tool("r", "read", &json!({"attachment_id": "att_0000ffff"}), Some(&live), None, &cx).title, "Read logo.png");
        assert_eq!(
            tool("r", "read", &json!({"attachment_id": "att_0000ffff"}), None, None, &Ctx { running: true, ..cx }).title,
            "Reading attachment"
        );
        // A failed copy says why, without the path.
        let input = json!({"attachment_id": "att_3f2a9c1b", "path": "/w/assets/logo.png"});
        let exists = "InvalidInput: /w/assets/logo.png exists; pass overwrite: true to replace it";
        let line = tool("c", "copy_attachment", &input, None, Some(&(exists.into(), true)), &named);
        assert_eq!((line.title.as_str(), line.status, line.desc.as_deref()), ("Couldn't copy screenshot.png: already exists", Status::Error, None));
        assert!(line.files.is_empty() && line.detail.is_none());
        let line = tool("c", "copy_attachment", &input, None, Some(&("NotFound: unknown attachment att_3f2a9c1b".into(), true)), &cx);
        assert_eq!(line.title, "Couldn't copy attachment: no such attachment");
        let line = tool("c", "copy_attachment", &input, None, Some(&("Failed: Permission denied (os error 13)".into(), true)), &named);
        assert_eq!(line.title, "Couldn't copy screenshot.png: permission denied");
        let line = tool("c", "copy_attachment", &input, None, Some(&("Failed: Operation not permitted (os error 1)".into(), true)), &named);
        assert_eq!(line.title, "Couldn't copy screenshot.png: permission denied", "EPERM (a system folder)");
        // The user's notes name attachments by id.
        let items = [Item::new(
            "u",
            Role::User,
            vec![Part::text("[attachment att_3f2a9c1b: logo.png, image/png, 180 KB — copy_attachment can save it into the project]")],
        )];
        assert_eq!(attachment_names(&items), HashMap::from([("att_3f2a9c1b".to_owned(), "logo.png".to_owned())]));
    }
}
