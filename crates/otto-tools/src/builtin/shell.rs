use super::*;
use crate::{Tool, ToolErrorKind, ToolOutput, output::Capture};
use std::{
    collections::BTreeMap,
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
async fn expire(registry: std::sync::Weak<Jobs>, id: String, ttl: Duration) {
    tokio::time::sleep(ttl).await;
    if let Some(registry) = registry.upgrade() {
        registry.jobs.lock().await.remove(&id);
    }
}
#[derive(Default)]
pub struct Jobs {
    next: AtomicU64,
    jobs: Mutex<BTreeMap<String, Arc<Mutex<JobState>>>>,
}
struct JobState {
    cancel: CancellationToken,
    output: ToolOutput,
    result: Option<Result<ToolOutput, ToolError>>,
}
pub struct Shell {
    spec: ToolSpec,
    jobs: Arc<Jobs>,
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
        Self {
            jobs,
            spec: spec(
                "shell",
                "Run a non-interactive command through $SHELL -lc (fallback /bin/sh -c), with stdin closed. Supports cwd, timeout_ms (default 120000), and background jobs. Cancellation kills the process group. Full output streams to a spill file; only bounded head/tail text is retained. Progress is sampled and capped.",
                json!({"command":{"type":"string"},"cwd":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1},"background":{"type":"boolean"}}),
                &["command"],
                Effects::PROCESS,
            ),
        }
    }
}
impl Job {
    pub fn new(jobs: Arc<Jobs>) -> Self {
        Self {
            jobs,
            spec: spec(
                "job",
                "Inspect or stop a background shell job. status reports completion; output returns bounded stdout/stderr plus a full spill path; kill cancels the process group. Completed jobs expire after five minutes and are removed when output is read. At most 64 jobs are retained.",
                json!({"job_id":{"type":"string"},"operation":{"type":"string","enum":["status","output","kill"]}}),
                &["job_id", "operation"],
                Effects::PROCESS,
            ),
        }
    }
}
#[async_trait::async_trait]
impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        crate::policy::validate_prepared(&input, &cx)?;
        string(&input, "command")?;
        if input.get("background").and_then(Value::as_bool).unwrap_or(false) {
            let id = format!("job-{}", self.jobs.next.fetch_add(1, Ordering::Relaxed));
            let state = Arc::new(Mutex::new(JobState { cancel: cx.cancel.child_token(), output: ToolOutput::default(), result: None }));
            let mut jobs = self.jobs.jobs.lock().await;
            if jobs.len() >= MAX_JOBS {
                return Err(ToolError::failed("Job limit reached; read completed job output or kill a job"));
            }
            jobs.insert(id.clone(), state.clone());
            drop(jobs);
            let mut cx = cx;
            cx.cancel = state.lock().await.cancel.clone();
            let registry = Arc::downgrade(&self.jobs);
            let job_id = id.clone();
            tokio::spawn(async move {
                let result = run(input, cx, Some(state.clone())).await;
                {
                    let mut state = state.lock().await;
                    state.output = ToolOutput::default();
                    state.result = Some(result);
                }
                drop(state);
                expire(registry, job_id, JOB_TTL).await;
            });
            Ok(ToolOutput::text(format!("Started {id}")).with_metadata(json!({"job_id":id})))
        } else {
            run(input, cx, None).await
        }
    }
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
    cmd.current_dir(cwd)
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("OTTO_SILO", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(io)?;
    let group = Group(child.id().ok_or_else(|| ToolError::failed("Missing process id"))?);
    let (tx, mut rx) = mpsc::channel(8);
    let stdout = child.stdout.take().ok_or_else(|| ToolError::failed("Missing stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::failed("Missing stderr"))?;
    let out = tokio::spawn(pump(stdout, tx.clone()));
    let err = tokio::spawn(pump(stderr, tx));
    let timeout = tokio::time::sleep(Duration::from_millis(input.get("timeout_ms").and_then(Value::as_u64).unwrap_or(120_000)));
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
    if let Some(kind) = stopped {
        result.is_error = true;
        result.metadata["exit_code"] = Value::Null;
        result.metadata["termination"] = json!(kind);
    } else {
        let status = status.ok_or_else(|| ToolError::failed("Missing exit status"))?;
        result.is_error = !status.success();
        result.metadata["exit_code"] = json!(status.code());
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
}
