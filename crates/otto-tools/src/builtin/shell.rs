use super::*;
use crate::{OutputPart, Tool, ToolErrorKind, ToolOutput, output::Capture};
use std::{
    collections::BTreeMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, mpsc},
};
use tokio_util::sync::CancellationToken;
const MAX_JOBS: usize = 64;
const JOB_TTL: Duration = Duration::from_secs(300);
/// What every command gets: no pager waits, and a marker that otto runs it.
const ENV: [(&str, &str); 3] = [("PAGER", "cat"), ("GIT_PAGER", "cat"), ("OTTO_SILO", "1")];

/// Runs commands where the user can watch and take them over: silo's
/// parked command blocks (docs/OTTO.md §6.2).
#[async_trait::async_trait]
pub trait Terminals: Send + Sync {
    /// Start `command` in `cwd` with `env` for the session calling (`cx`);
    /// `None` when no terminal host serves this call (pipes run it).
    async fn start(&self, command: &str, cwd: &Path, env: &[(&str, &str)], cx: &ToolCx) -> Result<Option<Arc<dyn Terminal>>, ToolError>;
}

/// One command running in a terminal host.
#[async_trait::async_trait]
pub trait Terminal: Send + Sync {
    /// Its block.
    fn id(&self) -> &str;
    /// Until it exits or, with `prompt`, waits for input; `screen` gets the
    /// screen as it changes.
    async fn wait(&self, prompt: bool, screen: &mut (dyn for<'s> FnMut(&'s str) + Send)) -> Result<Finished, ToolError>;
    /// End it: its text so far.
    async fn stop(&self) -> Result<String, ToolError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finished {
    /// Exit code (`None`: killed by a signal) and the final screen and
    /// scrollback as plain text.
    Exited { code: Option<i32>, text: String },
    /// Reads a line and has been quiet: the last screen.
    Waiting { screen: String },
    /// Its block was closed before it ended (the user stopped it).
    Closed,
}

async fn expire(registry: std::sync::Weak<Jobs>, id: String, ttl: Duration) {
    tokio::time::sleep(ttl).await;
    if let Some(registry) = registry.upgrade() {
        registry.jobs.lock().await.remove(&id);
    }
}
pub struct Jobs {
    /// Start time, in job ids: ids stay unique across restarts.
    epoch: u64,
    next: AtomicU64,
    jobs: Mutex<BTreeMap<String, Arc<Mutex<JobState>>>>,
}
impl Default for Jobs {
    fn default() -> Self {
        let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() & 0xff_ffff);
        Self { epoch, next: AtomicU64::new(0), jobs: Mutex::default() }
    }
}
impl Jobs {
    /// Stop every running job of `session` (it closed).
    pub async fn stop_session(&self, session: &str) {
        for job in self.jobs.lock().await.values() {
            let job = job.lock().await;
            if job.session.as_deref() == Some(session) {
                job.cancel.cancel();
            }
        }
    }
}
struct JobState {
    cancel: CancellationToken,
    output: ToolOutput,
    result: Option<Result<ToolOutput, ToolError>>,
    /// The session that started it.
    session: Option<String>,
}
/// What shells share: background jobs, and the terminal host commands run
/// in when there is one.
#[derive(Clone, Default)]
pub struct Host {
    pub jobs: Arc<Jobs>,
    pub terminals: Option<Arc<dyn Terminals>>,
}
pub struct Shell {
    spec: ToolSpec,
    host: Host,
}
pub struct Job {
    spec: ToolSpec,
    jobs: Arc<Jobs>,
}
impl Default for Shell {
    fn default() -> Self {
        Self::new(Arc::new(Jobs::default()))
    }
}
impl Shell {
    pub fn new(jobs: Arc<Jobs>) -> Self {
        Self::with(Host { jobs, terminals: None })
    }
    pub fn with(host: Host) -> Self {
        let spec = spec(
            "shell",
            "Run a command through $SHELL -lc (fallback /bin/sh -c) and return its exit code and output. It starts in the working directory (or cwd): use paths relative to it and don't cd into it. With Silo it runs in its own hidden terminal the user can open: a prompt for input (password, y/N) returns waiting_for_input with the screen and a job_id; answer with terminal_send to its block_id or let the user type, and the result arrives when it exits. Without Silo stdin is closed. timeout_ms defaults to 120000; a timeout or cancellation kills it. background:true is for finite work that reports later (tests, a build): it returns a job_id at once and its result arrives as a message when it exits; check or stop it with job. Use terminal_open for servers and watchers, not background or nohup. Long output keeps its head and tail; the full text goes to a file.",
            json!({"command":{"type":"string"},"cwd":{"type":"string","description":"Relative to the working directory; default: the working directory"},"timeout_ms":{"type":"integer","minimum":1},"background":{"type":"boolean"}}),
            &["command"],
            Effects::PROCESS,
        );
        Self { host, spec }
    }
}
impl Job {
    pub fn new(jobs: Arc<Jobs>) -> Self {
        Self {
            jobs,
            spec: spec(
                "job",
                "Inspect or stop a background shell job (its result also arrives as a message when it exits). status reports completion; output returns bounded output plus a full spill path; kill stops it. Completed jobs expire after five minutes and are removed when output is read. At most 64 jobs are retained.",
                json!({"job_id":{"type":"string"},"operation":{"type":"string","enum":["status","output","kill"]}}),
                &["job_id", "operation"],
                Effects::PROCESS,
            ),
        }
    }
}
/// Short read-only commands (`git status`, `ls`, `cat`): pipes run them even
/// with a terminal host, where a block costs more than they take (§6.2).
#[must_use]
pub fn quick(command: &str) -> bool {
    if command.contains(['>', '`', '\n', '\\']) || command.contains("$(") || command.trim().is_empty() {
        return false;
    }
    command.split(['|', ';', '&']).map(str::trim).filter(|s| !s.is_empty()).all(|segment| {
        let mut words = segment.split_whitespace();
        match words.next().unwrap_or_default() {
            "git" => matches!(
                words.next(),
                Some("status" | "diff" | "log" | "show" | "branch" | "rev-parse" | "ls-files" | "grep" | "blame" | "describe" | "shortlog")
            ),
            "find" => !["-exec", "-delete", "-ok", "-fprint"].iter().any(|a| segment.contains(a)),
            "sed" => !segment.contains("-i"),
            "tail" => !segment.contains(" -f") && !segment.contains(" -F"),
            "ls" | "cat" | "head" | "wc" | "pwd" | "echo" | "printf" | "rg" | "grep" | "which" | "file" | "stat" | "date" | "whoami" | "uname"
            | "basename" | "dirname" | "realpath" | "readlink" | "tree" | "du" | "df" | "cut" | "sort" | "uniq" | "tr" | "nl" | "diff" | "cmp"
            | "jq" | "true" | "false" | "test" | "[" => true,
            _ => false,
        }
    })
}
#[async_trait::async_trait]
impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        crate::policy::validate_prepared(&input, &cx)?;
        let command = string(&input, "command")?.to_owned();
        let terminal = match &self.host.terminals {
            Some(terminals) if !quick(&command) => {
                let cwd = crate::policy::resolve_path(&cx.cwd, &cx.project_root, input.get("cwd").and_then(Value::as_str).unwrap_or("."))?.path;
                terminals.start(&command, &cwd, &ENV, &cx).await?
            }
            _ => None,
        };
        // Clients show the block (`block:<id>`) while the call runs.
        if let Some(terminal) = &terminal {
            cx.progress(Some(format!("block:{}", terminal.id())), None);
        }
        if input.get("background").and_then(Value::as_bool).unwrap_or(false) {
            let explicit = input.get("timeout_ms").and_then(Value::as_u64).map(Duration::from_millis);
            let block = terminal.as_ref().map(|t| t.id().to_owned());
            let id = self
                .spawn_job(command, cx.clone(), terminal.clone(), move |cx, state| async move {
                    match terminal {
                        Some(terminal) => watch(terminal.as_ref(), &cx, explicit, true).await,
                        None => run(input, cx, Some(state)).await,
                    }
                })
                .await?;
            let mut metadata = json!({"job_id": id, "background": true});
            let place = block.as_ref().map(|b| format!(" in block {b}")).unwrap_or_default();
            if let Some(block) = block {
                metadata["block_id"] = json!(block);
            }
            return Ok(ToolOutput::text(format!("Started {id}{place}; its result arrives as a message when it exits.")).with_metadata(metadata));
        }
        let Some(terminal) = terminal else { return run(input, cx, None).await };
        let timeout = Duration::from_millis(input.get("timeout_ms").and_then(Value::as_u64).unwrap_or(120_000));
        let mut sent = 0;
        let mut last = String::new();
        let progress = cx.clone();
        let mut screen = move |s: &str| progress_screen(&progress, &mut last, &mut sent, s);
        let finished = tokio::select! {
            () = cx.cancel.cancelled() => return stopped(terminal.as_ref(), &cx, ToolErrorKind::Cancelled).await,
            () = tokio::time::sleep(timeout) => return stopped(terminal.as_ref(), &cx, ToolErrorKind::Timeout).await,
            finished = terminal.wait(true, &mut screen) => finished?,
        };
        match finished {
            Finished::Exited { code, text } => exited(terminal.id(), code, &text, &cx).await,
            Finished::Closed => closed(terminal.id(), &cx).await,
            Finished::Waiting { screen } => {
                // The user (or terminal_send) answers; the result arrives later.
                let block = terminal.id().to_owned();
                let id =
                    self.spawn_job(command, cx.clone(), Some(terminal.clone()), move |cx, _| async move {
                        watch(terminal.as_ref(), &cx, None, false).await
                    })
                    .await?;
                let text = format!(
                    "waiting_for_input: the command waits for input (job {id}). Answer with terminal_send to block {block}, or ask the user to open it and type; its result arrives as a message when it exits.\n{screen}"
                );
                Ok(ToolOutput::text(text).with_metadata(json!({"status":"waiting_for_input","job_id":id,"block_id":block})))
            }
        }
    }
}
impl Shell {
    /// Register a job running `work`, whose result is delivered to the
    /// calling session when it ends.
    async fn spawn_job<F, Fut>(&self, command: String, cx: ToolCx, terminal: Option<Arc<dyn Terminal>>, work: F) -> Result<String, ToolError>
    where
        F: FnOnce(ToolCx, Arc<Mutex<JobState>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        // Unique across restarts too: a client matches a job's report to its call by id.
        let id = format!("job-{:06x}{}", self.host.jobs.epoch, self.host.jobs.next.fetch_add(1, Ordering::Relaxed));
        // A job outlives the turn: only `job kill` or its session closing stop it.
        let cancel = CancellationToken::new();
        let state = Arc::new(Mutex::new(JobState {
            cancel: cancel.clone(),
            output: ToolOutput::default(),
            result: None,
            session: cx.sessions.first().cloned(),
        }));
        {
            let mut jobs = self.host.jobs.jobs.lock().await;
            if jobs.len() >= MAX_JOBS {
                return Err(ToolError::failed("Job limit reached; read completed job output or kill a job"));
            }
            jobs.insert(id.clone(), state.clone());
        }
        let mut cx = cx;
        cx.cancel = cancel;
        cx.progress = None;
        let mailbox = cx.mailbox.clone();
        let registry = Arc::downgrade(&self.host.jobs);
        let (job_id, job) = (id.clone(), state);
        tokio::spawn(async move {
            let result = work(cx, job.clone()).await;
            if let Some(mailbox) = mailbox {
                mailbox.deliver(report(&job_id, &command, terminal.as_deref().map(Terminal::id), &result));
            }
            {
                let mut state = job.lock().await;
                state.output = ToolOutput::default();
                state.result = Some(result);
            }
            drop(job);
            expire(registry, job_id, JOB_TTL).await;
        });
        Ok(id)
    }
}
/// The message a finished job delivers.
fn report(id: &str, command: &str, block: Option<&str>, result: &Result<ToolOutput, ToolError>) -> String {
    let short: String = command.chars().take(120).collect();
    match result {
        Ok(output) => {
            let status = match (&output.metadata["exit_code"], output.metadata["termination"].as_str()) {
                (_, Some(stop)) => format!("was stopped ({stop})"),
                (Value::Number(code), _) => format!("exited with code {code}"),
                _ => "was killed by a signal".into(),
            };
            let block = block.map(|b| format!(" in block {b}")).unwrap_or_default();
            format!("Background shell {id} (`{short}`){block} {status}.\n{}", output.joined_text())
        }
        Err(error) => format!("Background shell {id} (`{short}`) failed: {}", error.message),
    }
}
/// Send screen changes as progress: appended text, else the last line.
fn progress_screen(cx: &ToolCx, last: &mut String, sent: &mut usize, screen: &str) {
    if *sent >= 64 {
        return;
    }
    let delta = match screen.strip_prefix(last.as_str()) {
        Some(added) => added.to_owned(),
        None => format!("\n{}", screen.lines().last().unwrap_or_default()),
    };
    screen.clone_into(last);
    if !delta.trim().is_empty() {
        let mut end = delta.len().min(8192);
        while !delta.is_char_boundary(end) {
            end -= 1;
        }
        cx.progress(None, Some(delta[..end].to_owned()));
        *sent += 1;
    }
}
/// A background or answered command in a terminal: until it exits, is
/// stopped (`cx.cancel`) or times out; a prompt (with `prompt`) is delivered
/// as a notice once, then it waits on.
async fn watch(terminal: &dyn Terminal, cx: &ToolCx, timeout: Option<Duration>, mut prompt: bool) -> Result<ToolOutput, ToolError> {
    let deadline = async {
        match timeout {
            Some(timeout) => tokio::time::sleep(timeout).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    let mut ignore = |_: &str| {};
    loop {
        let finished = tokio::select! {
            () = cx.cancel.cancelled() => return stopped(terminal, cx, ToolErrorKind::Cancelled).await,
            () = &mut deadline => return stopped(terminal, cx, ToolErrorKind::Timeout).await,
            finished = terminal.wait(prompt, &mut ignore) => finished?,
        };
        match finished {
            Finished::Exited { code, text } => return exited(terminal.id(), code, &text, cx).await,
            Finished::Closed => return closed(terminal.id(), cx).await,
            Finished::Waiting { screen } => {
                prompt = false;
                if let Some(mailbox) = &cx.mailbox {
                    mailbox.deliver(format!(
                        "A background command in block {} waits for input; answer with terminal_send or ask the user to open it.\n{screen}",
                        terminal.id()
                    ));
                }
            }
        }
    }
}
/// The result of a command that ran in terminal block `block`: the same
/// shape as through pipes.
async fn exited(block: &str, code: Option<i32>, text: &str, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
    let mut capture = Capture::new(cx)?;
    capture.push(text).await?;
    let mut result = capture.finish(cx).await?;
    result.metadata["block_id"] = json!(block);
    result.metadata["exit_code"] = json!(code);
    result.is_error = code != Some(0);
    if result.is_error {
        result.content.retain(|p| !matches!(p, OutputPart::Text { text } if text.is_empty()));
        result.content.push(OutputPart::Text { text: code.map_or_else(|| "[killed by signal]".into(), |c| format!("[exit code {c}]")) });
    }
    Ok(result)
}
/// Stop a command in a terminal: its text so far, marked like pipes'.
async fn stopped(terminal: &dyn Terminal, cx: &ToolCx, kind: ToolErrorKind) -> Result<ToolOutput, ToolError> {
    let text = terminal.stop().await?;
    Ok(mark(exited(terminal.id(), None, &text, cx).await?, json!(kind)))
}
/// A command whose block the user closed.
async fn closed(block: &str, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
    Ok(mark(exited(block, None, "", cx).await?, json!("closed")))
}
fn mark(mut result: ToolOutput, termination: Value) -> ToolOutput {
    result.metadata["termination"] = termination;
    result.content.pop();
    result.content.push(OutputPart::Text { text: format!("[stopped: {}]", result.metadata["termination"].as_str().unwrap_or("stopped")) });
    result
}
#[async_trait::async_trait]
impl Tool for Job {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, _cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let id = string(&input, "job_id")?;
        let state = self.jobs.jobs.lock().await.get(id).cloned().ok_or_else(|| ToolError::new(ToolErrorKind::NotFound, "Unknown job_id"))?;
        let state = state.lock().await;
        match string(&input,"operation")? {
            "kill" => { state.cancel.cancel(); drop(state); self.jobs.jobs.lock().await.remove(id); Ok(ToolOutput::text("Cancellation requested")) },
            "output" => {
                let completed = state.result.is_some();
                let output = state.result.clone().unwrap_or_else(|| Ok(state.output.clone())); drop(state);
                if completed { self.jobs.jobs.lock().await.remove(id); } output
            },
            "status" => Ok(ToolOutput::text(if state.result.is_some(){"completed"}else{"running"}).with_metadata(json!({"job_id":id,"completed":state.result.is_some(),"result":state.result.as_ref().map(|r|match r{Ok(o)=>json!({"is_error":o.is_error,"metadata":o.metadata}),Err(e)=>json!({"error":e})})}))),
            _ => Err(ToolError::invalid("operation must be status, output, or kill")),
        }
    }
}
struct Group(u32);
impl Group {
    fn kill(&self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}
#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
}
impl Decoder {
    fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(text) => {
                    output.push_str(text);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let end = consumed + error.valid_up_to();
                    output.push_str(std::str::from_utf8(&self.pending[consumed..end]).expect("valid UTF-8 prefix"));
                    consumed = end;
                    if let Some(len) = error.error_len() {
                        output.push('\u{fffd}');
                        consumed += len;
                    } else if eof {
                        output.push('\u{fffd}');
                        consumed = self.pending.len();
                    } else {
                        break;
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        output
    }
}
async fn pump<R: tokio::io::AsyncRead + Unpin>(mut reader: R, tx: mpsc::Sender<String>) {
    let mut buf = [0; 8192];
    let mut decoder = Decoder::default();
    loop {
        let n = reader.read(&mut buf).await.unwrap_or(0);
        let text = decoder.decode(&buf[..n], n == 0);
        if !text.is_empty() && tx.send(text).await.is_err() {
            break;
        }
        if n == 0 {
            break;
        }
    }
}
async fn run(input: Value, cx: ToolCx, job: Option<Arc<Mutex<JobState>>>) -> Result<ToolOutput, ToolError> {
    crate::policy::validate_prepared(&input, &cx)?;
    let mut capture = Capture::new(&cx)?;
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
    let mut cmd = tokio::process::Command::new(shell.as_deref().unwrap_or("/bin/sh"));
    cmd.arg(if shell.is_some() { "-lc" } else { "-c" }).arg(string(&input, "command")?);
    let cwd = crate::policy::resolve_path(&cx.cwd, &cx.project_root, input.get("cwd").and_then(Value::as_str).unwrap_or("."))?.path;
    cmd.current_dir(cwd).envs(ENV).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(io)?;
    let group = Group(child.id().ok_or_else(|| ToolError::failed("Missing process id"))?);
    let (tx, mut rx) = mpsc::channel(8);
    let stdout = child.stdout.take().ok_or_else(|| ToolError::failed("Missing stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::failed("Missing stderr"))?;
    let out = tokio::spawn(pump(stdout, tx.clone()));
    let err = tokio::spawn(pump(stderr, tx));
    // A background job without an explicit timeout runs until it ends or is killed.
    let timeout = input.get("timeout_ms").and_then(Value::as_u64).or(job.is_none().then_some(120_000));
    let timeout = async {
        match timeout {
            Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timeout);
    let mut status = None;
    let mut streams_done = false;
    let mut stopped = None;
    let mut last_progress = Instant::now().checked_sub(Duration::from_secs(1)).expect("clock");
    let mut progress_count = 0;
    while status.is_none() || !streams_done {
        tokio::select! {
            () = cx.cancel.cancelled() => { stopped = Some(ToolErrorKind::Cancelled); break; },
            () = &mut timeout => { stopped = Some(ToolErrorKind::Timeout); break; },
            result = child.wait(), if status.is_none() => { status = Some(result.map_err(io)?); },
            chunk = rx.recv(), if !streams_done => {
                if let Some(chunk) = chunk {
                    capture.push(&chunk).await?;
                    if last_progress.elapsed() >= Duration::from_millis(100) {
                        if progress_count < 64 {
                            let mut end = chunk.len().min(8192); while !chunk.is_char_boundary(end) { end -= 1; }
                            cx.progress(None,Some(chunk[..end].to_owned())); progress_count += 1;
                        }
                        if let Some(job) = &job { job.lock().await.output = capture.snapshot(&cx)?; }
                        last_progress = Instant::now();
                    }
                } else { streams_done = true; }
            }
        }
    }
    if stopped.is_some() {
        group.kill();
        let _ = child.wait().await;
        let drain = async {
            while let Some(chunk) = rx.recv().await {
                capture.push(&chunk).await?;
            }
            Ok::<_, ToolError>(())
        };
        if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), drain).await {
            result?;
        }
    }
    out.abort();
    err.abort();
    let mut result = capture.finish(&cx).await?;
    result.metadata["progress_chunks"] = json!(progress_count);
    let note = if let Some(kind) = stopped {
        result.is_error = true;
        result.metadata["exit_code"] = Value::Null;
        result.metadata["termination"] = json!(kind);
        Some(format!("[stopped: {}]", result.metadata["termination"].as_str().unwrap_or("stopped")))
    } else {
        let status = status.ok_or_else(|| ToolError::failed("Missing exit status"))?;
        result.is_error = !status.success();
        result.metadata["exit_code"] = json!(status.code());
        result.is_error.then(|| status.code().map_or_else(|| "[killed by signal]".into(), |c| format!("[exit code {c}]")))
    };
    if let Some(note) = note {
        result.content.retain(|p| !matches!(p, OutputPart::Text { text } if text.is_empty()));
        result.content.push(OutputPart::Text { text: note });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn job_cap_expiration_and_kill_removal() {
        let jobs = Arc::new(Jobs::default());
        for i in 0..MAX_JOBS {
            jobs.jobs.lock().await.insert(
                format!("retained-{i}"),
                Arc::new(Mutex::new(JobState {
                    cancel: CancellationToken::new(),
                    output: ToolOutput::default(),
                    result: Some(Ok(ToolOutput::text("done"))),
                    session: None,
                })),
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let cx = ToolCx::new("cap", dir.path());
        let shell = Shell::new(jobs.clone());
        assert!(shell.call(json!({"command":"true","background":true}), cx.clone()).await.unwrap_err().message.contains("Job limit"));
        expire(Arc::downgrade(&jobs), "retained-0".into(), Duration::from_millis(1)).await;
        assert_eq!(jobs.jobs.lock().await.len(), MAX_JOBS - 1);
        let job = Job::new(jobs.clone());
        job.call(json!({"job_id":"retained-1","operation":"kill"}), cx).await.unwrap();
        assert_eq!(jobs.jobs.lock().await.len(), MAX_JOBS - 2);
    }
    #[test]
    fn decoder_retains_only_incomplete_suffix() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.decode(&[0xe2], false), "");
        assert_eq!(decoder.pending.len(), 1);
        assert_eq!(decoder.decode(&[0x82, 0xac], false), "€");
        assert!(decoder.pending.is_empty());
        assert_eq!(decoder.decode(&[0xff, b'a', 0xf0], false), "�a");
        assert_eq!(decoder.pending.len(), 1);
        assert_eq!(decoder.decode(&[], true), "�");
        assert!(decoder.pending.is_empty());
    }
    #[test]
    fn quick_read_only_commands_skip_blocks() {
        for command in ["git status", "git diff --stat && git log -3", "ls -la src | head", "rg -n foo crates", "cat a; wc -l b", "find . -name x"] {
            assert!(quick(command), "{command}");
        }
        for command in
            ["cargo test", "git commit -m x", "echo hi > f", "sudo ls", "find . -delete", "sed -i s/a/b/ f", "tail -f log", "ls $(pwd)", "npm i", ""]
        {
            assert!(!quick(command), "{command}");
        }
    }
    #[test]
    fn reports_say_how_a_job_ended() {
        let mut output = ToolOutput::text("ok").with_metadata(json!({"exit_code": 0}));
        assert_eq!(
            report("job-1", "cargo test", Some("b1"), &Ok(output.clone())),
            "Background shell job-1 (`cargo test`) in block b1 exited with code 0.\nok"
        );
        output.metadata["termination"] = json!("cancelled");
        assert!(report("job-1", "x", None, &Ok(output)).contains("was stopped (cancelled)"));
        assert!(report("job-2", "x", None, &Err(ToolError::failed("boom"))).ends_with("failed: boom"));
    }
}
