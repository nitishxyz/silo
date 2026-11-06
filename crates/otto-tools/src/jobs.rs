//! Background jobs: shell commands and media generations that outlive their call. A session's jobs are listed
//! (`job.list`), stopped (`job kill`, `job.kill`), and each delivers a report through the session's mailbox when
//! it ends. A foreground call can register as detachable: the user then turns it into a job (`shell.detach`).
use crate::{Progress, ToolCx, ToolError, ToolOutput};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub const MAX_JOBS: usize = 64;
const JOB_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JobKind {
    #[default]
    Shell,
    Media,
}

/// The message a job delivers when it ends, from its id and result.
pub type Report = Box<dyn FnOnce(&str, &Result<ToolOutput, ToolError>) -> String + Send>;

/// The latest progress message of a job.
type Latest = Arc<std::sync::Mutex<Option<String>>>;

pub(crate) async fn expire(registry: std::sync::Weak<Jobs>, id: String, ttl: Duration) {
    tokio::time::sleep(ttl).await;
    if let Some(registry) = registry.upgrade() {
        registry.jobs.lock().await.remove(&id);
    }
}

pub struct Jobs {
    /// Start time, in job ids: ids stay unique across restarts.
    epoch: u64,
    next: AtomicU64,
    pub(crate) jobs: Mutex<BTreeMap<String, Arc<Mutex<JobState>>>>,
    /// Foreground calls the user can detach (`detach`), by session and call id.
    calls: std::sync::Mutex<HashMap<(String, String), CancellationToken>>,
}
impl Default for Jobs {
    fn default() -> Self {
        let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() & 0xff_ffff);
        Self { epoch, next: AtomicU64::new(0), jobs: Mutex::default(), calls: std::sync::Mutex::default() }
    }
}
/// A job as clients list it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobInfo {
    pub job_id: String,
    pub kind: JobKind,
    /// What it runs: a shell command, `generate_video: <prompt>`.
    pub command: String,
    /// The call that started it.
    pub call_id: String,
    /// Its terminal block, when it runs in one.
    pub block_id: Option<String>,
    /// Unix milliseconds.
    pub started_at: i64,
    /// It ended (its report was delivered); kept until it expires or its output is read.
    pub completed: bool,
    /// Its latest progress message (`Generating with xAI… 40 %, 12 s`).
    pub progress: Option<String>,
}
pub struct JobState {
    pub(crate) cancel: CancellationToken,
    pub(crate) output: ToolOutput,
    pub(crate) result: Option<Result<ToolOutput, ToolError>>,
    /// The session that started it.
    session: Option<String>,
    label: String,
    kind: JobKind,
    call_id: String,
    block: Option<String>,
    started_at: i64,
    pub(crate) progress: Latest,
}
impl JobState {
    pub(crate) fn new(cancel: CancellationToken, label: &str, kind: JobKind, cx: &ToolCx, block: Option<String>) -> Self {
        let started_at =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Self {
            cancel,
            output: ToolOutput::default(),
            result: None,
            session: cx.sessions.first().cloned(),
            label: label.to_owned(),
            kind,
            call_id: cx.call_id.clone(),
            block,
            started_at,
            progress: Latest::default(),
        }
    }
    /// Its latest progress message.
    pub(crate) fn progress(&self) -> Option<String> {
        self.progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
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
    /// Detach `session`'s running foreground call `call_id` (a shell command, a generation): the call returns at
    /// once and its work runs on as a background job. `false` when no such call runs (it ended, or can't detach).
    pub fn detach(&self, session: &str, call_id: &str) -> bool {
        let calls = self.calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.get(&(session.to_owned(), call_id.to_owned())).inspect(|t| t.cancel()).is_some()
    }
    /// `session`'s jobs, oldest first.
    pub async fn list(&self, session: &str) -> Vec<JobInfo> {
        let jobs: Vec<(String, Arc<Mutex<JobState>>)> = self.jobs.lock().await.iter().map(|(id, j)| (id.clone(), j.clone())).collect();
        let mut listed = Vec::new();
        for (id, job) in jobs {
            let job = job.lock().await;
            if job.session.as_deref() == Some(session) {
                listed.push(JobInfo {
                    job_id: id,
                    kind: job.kind,
                    command: job.label.clone(),
                    call_id: job.call_id.clone(),
                    block_id: job.block.clone(),
                    started_at: job.started_at,
                    completed: job.result.is_some(),
                    progress: job.progress(),
                });
            }
        }
        listed.sort_by_key(|j| j.started_at);
        listed
    }
    /// Stop `session`'s job `job_id` (its report says it was stopped); `false` when it has no such job.
    pub async fn kill(&self, session: &str, job_id: &str) -> bool {
        let mut jobs = self.jobs.lock().await;
        let Some(job) = jobs.get(job_id).cloned() else { return false };
        let job = job.lock().await;
        if job.session.as_deref() != Some(session) {
            return false;
        }
        job.cancel.cancel();
        jobs.remove(job_id);
        true
    }
    pub(crate) fn next_id(&self) -> String {
        // Unique across restarts too: a client matches a job's report to its call by id.
        format!("job-{:06x}{}", self.epoch, self.next.fetch_add(1, Ordering::Relaxed))
    }
    /// Register `state` as job `id`, refused when the registry is full.
    pub(crate) async fn insert(&self, id: &str, state: Arc<Mutex<JobState>>) -> Result<(), ToolError> {
        let mut jobs = self.jobs.lock().await;
        if jobs.len() >= MAX_JOBS {
            return Err(ToolError::failed("Job limit reached; read completed job output or kill a job"));
        }
        jobs.insert(id.to_owned(), state);
        Ok(())
    }
    /// Run `work` as a job of `kind` labelled `label` for `cx`'s session: its id. It reports through `cx`'s mailbox.
    ///
    /// # Errors
    /// The registry is full.
    pub async fn start<F, Fut>(self: &Arc<Self>, label: String, kind: JobKind, cx: ToolCx, work: F, report: Report) -> Result<String, ToolError>
    where
        F: FnOnce(ToolCx) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let id = self.next_id();
        self.spawn(id.clone(), None, &label, kind, None, cx, |cx, _| work(cx), report).await?;
        Ok(id)
    }
    /// Register job `id` running `work`, whose report is delivered to the calling session when it ends.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn<F, Fut>(
        self: &Arc<Self>,
        id: String,
        started_at: Option<i64>,
        label: &str,
        kind: JobKind,
        block: Option<String>,
        mut cx: ToolCx,
        work: F,
        report: Report,
    ) -> Result<(), ToolError>
    where
        F: FnOnce(ToolCx, Arc<Mutex<JobState>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        // A job outlives the turn: only `job kill` or its session closing stop it.
        let cancel = CancellationToken::new();
        let mut job = JobState::new(cancel.clone(), label, kind, &cx, block);
        job.started_at = started_at.unwrap_or(job.started_at);
        cx.progress = Some(recorder(job.progress.clone(), None));
        let state = Arc::new(Mutex::new(job));
        self.insert(&id, state.clone()).await?;
        cx.cancel = cancel;
        let mailbox = cx.mailbox.clone();
        let registry = Arc::downgrade(self);
        tokio::spawn(async move {
            let result = work(cx, state.clone()).await;
            finish(registry, id, mailbox.as_deref(), state, result, report).await;
        });
        Ok(())
    }
}
/// A sender keeping the latest message in `latest`, and passing everything on to `to` until its flag is set.
fn recorder(latest: Latest, to: Option<(mpsc::UnboundedSender<Progress>, Arc<AtomicBool>)>) -> mpsc::UnboundedSender<Progress> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Progress>();
    tokio::spawn(async move {
        while let Some(progress) = rx.recv().await {
            if let Some(message) = &progress.message {
                *latest.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());
            }
            if let Some((to, quiet)) = &to
                && !quiet.load(Ordering::Relaxed)
            {
                let _ = to.send(progress);
            }
        }
    });
    tx
}
/// A foreground call while it can be detached: registered in `Jobs::calls` until dropped.
pub(crate) struct Detachable {
    pub(crate) jobs: Arc<Jobs>,
    key: (String, String),
    pub(crate) token: CancellationToken,
}
impl Detachable {
    pub(crate) fn new(jobs: &Arc<Jobs>, cx: &ToolCx) -> Self {
        let key = (cx.sessions.first().cloned().unwrap_or_default(), cx.call_id.clone());
        let token = CancellationToken::new();
        jobs.calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key.clone(), token.clone());
        Self { jobs: jobs.clone(), key, token }
    }
    /// It can't be detached any more.
    pub(crate) fn forget(&self) {
        self.jobs.calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&self.key);
    }
}
impl Drop for Detachable {
    fn drop(&mut self) {
        self.forget();
    }
}
/// Where a foreground call's result goes: back to its call, or (detached) to job `job`.
pub(crate) struct Slot {
    pub(crate) job: Option<String>,
    pub(crate) tx: Option<oneshot::Sender<Result<ToolOutput, ToolError>>>,
}
impl Jobs {
    /// Run `work` for the call `cx` in its own task: its result returns to the call, until the user detaches it
    /// ([`Jobs::detach`]); then it runs on as a job of `kind` labelled `label`, the call returns `detached(id)` and
    /// `report` is delivered when it ends. A cancelled turn, or the call being dropped, stops it until then.
    ///
    /// # Errors
    /// What `work` returns, or its task ended without a result.
    pub async fn foreground<F, Fut>(
        self: &Arc<Self>,
        label: &str,
        kind: JobKind,
        cx: &ToolCx,
        work: F,
        report: Report,
        detached: impl FnOnce(&str) -> ToolOutput,
    ) -> Result<ToolOutput, ToolError>
    where
        F: FnOnce(ToolCx) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let detach = Detachable::new(self, cx);
        let stop = CancellationToken::new();
        let quiet = Arc::new(AtomicBool::new(false));
        let job = JobState::new(stop.clone(), label, kind, cx, None);
        let mut run_cx = cx.clone();
        run_cx.cancel = stop.clone();
        run_cx.progress = Some(recorder(job.progress.clone(), cx.progress.clone().map(|tx| (tx, quiet.clone()))));
        let state = Arc::new(Mutex::new(job));
        let (tx, mut rx) = oneshot::channel();
        let slot = Arc::new(std::sync::Mutex::new(Slot { job: None, tx: Some(tx) }));
        let (task_slot, task_state, registry, mailbox) = (slot.clone(), state.clone(), Arc::downgrade(self), cx.mailbox.clone());
        tokio::spawn(async move {
            let result = work(run_cx).await;
            let id = {
                let mut slot = task_slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                match slot.job.clone() {
                    Some(id) => id,
                    None => {
                        if let Some(tx) = slot.tx.take() {
                            let _ = tx.send(result);
                        }
                        return;
                    }
                }
            };
            finish(registry, id, mailbox.as_deref(), task_state, result, report).await;
        });
        // The call dropped (its turn ended) before it returned: the work stops too.
        let guard = stop.clone().drop_guard();
        let gone = |_| ToolError::failed("The call's task ended without a result");
        let mut detachable = true;
        loop {
            tokio::select! {
                result = &mut rx => {
                    guard.disarm();
                    return result.map_err(gone)?;
                }
                () = cx.cancel.cancelled() => {
                    stop.cancel();
                    guard.disarm();
                    return rx.await.map_err(gone)?;
                }
                () = detach.token.cancelled(), if detachable => {
                    let id = self.next_id();
                    if self.insert(&id, state.clone()).await.is_err() {
                        // No room: it stays in the foreground.
                        detach.forget();
                        detachable = false;
                        continue;
                    }
                    let finished = {
                        let mut slot = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        rx.try_recv().map_or_else(|_| { slot.job = Some(id.clone()); None }, Some)
                    };
                    guard.disarm();
                    if let Some(result) = finished {
                        // It ended as it was detached: an ordinary result.
                        self.jobs.lock().await.remove(&id);
                        return result;
                    }
                    quiet.store(true, Ordering::Relaxed);
                    return Ok(detached(&id));
                }
            }
        }
    }
}
/// A job ended: deliver its report, keep its result until it expires.
pub(crate) async fn finish(
    registry: std::sync::Weak<Jobs>,
    id: String,
    mailbox: Option<&dyn crate::Mailbox>,
    job: Arc<Mutex<JobState>>,
    result: Result<ToolOutput, ToolError>,
    report: Report,
) {
    if let Some(mailbox) = mailbox {
        mailbox.deliver(report(&id, &result));
    }
    {
        let mut state = job.lock().await;
        state.output = ToolOutput::default();
        state.result = Some(result);
    }
    drop(job);
    expire(registry, id, JOB_TTL).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolErrorKind;

    #[derive(Default)]
    struct Mail(std::sync::Mutex<Vec<String>>);
    impl crate::Mailbox for Mail {
        fn deliver(&self, text: String) {
            self.0.lock().unwrap().push(text);
        }
    }
    async fn delivered(mail: &Mail) -> String {
        for _ in 0..200 {
            if let Some(text) = mail.0.lock().unwrap().pop() {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("nothing delivered")
    }
    fn cx(mail: &Arc<Mail>) -> ToolCx {
        let mut cx = ToolCx::new("c1", ".");
        cx.sessions = vec!["ses".into()];
        cx.mailbox = Some(mail.clone());
        cx
    }
    fn said(what: &'static str) -> Report {
        Box::new(move |id, r| format!("{id} {what}: {}", r.as_ref().map_or_else(|e| e.message.clone(), ToolOutput::joined_text)))
    }

    #[tokio::test]
    async fn a_labelled_job_keeps_its_progress_reports_and_stops() {
        let (jobs, mail) = (Arc::new(Jobs::default()), Arc::new(Mail::default()));
        let (go, wait) = oneshot::channel::<()>();
        let work = |cx: ToolCx| async move {
            cx.progress(Some("Generating… 40 %".into()), None);
            let _ = wait.await;
            Ok(ToolOutput::text("made it"))
        };
        let id = jobs.start("generate_video: a fox".into(), JobKind::Media, cx(&mail), work, said("media")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let listed = jobs.list("ses").await;
        assert_eq!((listed[0].kind, listed[0].command.as_str(), listed[0].call_id.as_str()), (JobKind::Media, "generate_video: a fox", "c1"));
        assert_eq!((listed[0].progress.as_deref(), listed[0].completed), (Some("Generating… 40 %"), false));
        go.send(()).unwrap();
        assert_eq!(delivered(&mail).await, format!("{id} media: made it"));
        assert!(jobs.list("ses").await[0].completed);
        // `kill` cancels its work.
        let work = |cx: ToolCx| async move {
            cx.cancel.cancelled().await;
            Err(ToolError::new(ToolErrorKind::Cancelled, "Cancelled"))
        };
        let id = jobs.start("x".into(), JobKind::Media, cx(&mail), work, said("media")).await.unwrap();
        assert!(!jobs.kill("other", &id).await);
        assert!(jobs.kill("ses", &id).await);
        assert_eq!(delivered(&mail).await, format!("{id} media: Cancelled"));
    }

    #[tokio::test]
    async fn a_foreground_call_returns_or_is_detached_into_a_job() {
        let (jobs, mail) = (Arc::new(Jobs::default()), Arc::new(Mail::default()));
        let first = cx(&mail);
        let quick = jobs.foreground("x", JobKind::Media, &first, |_| async { Ok(ToolOutput::text("now")) }, said("m"), |_| unreachable!());
        assert_eq!(quick.await.unwrap().joined_text(), "now");
        assert!(!jobs.detach("ses", "c1"), "no longer detachable");
        let (tx, mut progress) = mpsc::unbounded_channel();
        let mut call = cx(&mail);
        call.progress = Some(tx);
        let work = |cx: ToolCx| async move {
            cx.progress(Some("early".into()), None);
            tokio::time::sleep(Duration::from_millis(100)).await;
            cx.progress(Some("late".into()), None);
            Ok(ToolOutput::text("slow"))
        };
        let run = jobs.foreground("generate_image: p", JobKind::Media, &call, work, said("m"), |id| ToolOutput::text(format!("Started {id}")));
        let detach = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert!(jobs.detach("ses", "c1"));
        };
        let (out, ()) = tokio::join!(run, detach);
        let id = out.unwrap().joined_text().trim_start_matches("Started ").to_owned();
        assert_eq!(jobs.list("ses").await[0].kind, JobKind::Media);
        assert_eq!(delivered(&mail).await, format!("{id} m: slow"));
        assert_eq!(progress.try_recv().unwrap().message.as_deref(), Some("early"));
        assert!(progress.try_recv().is_err(), "no progress for a returned call");
        assert_eq!(jobs.list("ses").await[0].progress.as_deref(), Some("late"));
    }
}
