//! Block status from the server terminal's hooks (Phase 7 §5): program
//! status records (OSC 7501), progress (OSC 9;4), notification pacing and
//! busy detection. Everything here is pure; the block actor feeds it.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use silo_proto::{
    api::method_name,
    control::{AttentionReason, Empty, Method, RpcError},
    model::{AttentionCounts, Busy, ProgramStatus, Progress, ProgressState, StatusKind, StatusState},
    text::{MAX_APP, MAX_MESSAGE, MAX_TITLE, untrusted},
};

use crate::{Server, block::BlockMsg};

/// Records kept per block (the protocol asks for 64..=256).
pub const MAX_RECORDS: usize = 256;
/// Progress without an update for this long is dropped (as Ghostty does).
pub const PROGRESS_TTL: Duration = Duration::from_secs(15);
/// Progress-only changes go out at most this often per block.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    state: StatusState,
    #[serde(default)]
    kind: Option<StatusKind>,
    #[serde(default)]
    progress: Option<u8>,
    /// As reported; the effective app is inherited when published.
    #[serde(default)]
    app: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    message: String,
    updated_at: u64,
    /// Update order, for least-recently-updated eviction.
    #[serde(default)]
    tick: u64,
}

/// A block's OSC 7501 records, following `terminal.h` exactly: one record
/// per id, a report replaces its record, CLEAR removes a subtree, apps are
/// inherited when published, at most [`MAX_RECORDS`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Records {
    records: BTreeMap<String, Record>,
    #[serde(default)]
    tick: u64,
}

/// One OSC 7501 report (not a CLEAR); text is raw program text.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub id: String,
    pub state: Option<StatusState>,
    pub kind: Option<StatusKind>,
    /// -1 (or anything outside 0..=100): none.
    pub progress: i8,
    pub app: String,
    pub title: String,
    pub message: String,
}

/// `id` is `ancestor` or lies beneath it (`""` is everyone's ancestor).
fn within(id: &str, ancestor: &str) -> bool {
    ancestor.is_empty() || id == ancestor || id.strip_prefix(ancestor).is_some_and(|rest| rest.starts_with('/'))
}

impl Records {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The state of `id`'s record, if it has one.
    #[must_use]
    pub fn state(&self, id: &str) -> Option<StatusState> {
        self.records.get(id).map(|r| r.state)
    }

    /// Replace `report.id`'s record. Returns the attention reason when the
    /// record enters blocked, done or error.
    pub fn report(&mut self, report: Report, now_ms: u64) -> Option<AttentionReason> {
        let state = report.state?;
        self.tick += 1;
        let previous = self.records.get(&report.id).map(|r| r.state);
        let record = Record {
            state,
            kind: report.kind.filter(|_| state == StatusState::Blocked),
            progress: u8::try_from(report.progress).ok().filter(|p| *p <= 100 && matches!(state, StatusState::Working | StatusState::Blocked)),
            app: untrusted(&report.app, MAX_APP),
            title: untrusted(&report.title, MAX_TITLE),
            message: untrusted(&report.message, MAX_MESSAGE),
            updated_at: now_ms,
            tick: self.tick,
        };
        self.records.insert(report.id, record);
        while self.records.len() > MAX_RECORDS {
            let oldest = self.records.iter().min_by_key(|(_, r)| r.tick).map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                self.records.remove(&id);
            }
        }
        if previous == Some(state) {
            return None;
        }
        match state {
            StatusState::Blocked => Some(AttentionReason::Blocked),
            StatusState::Done => Some(AttentionReason::Done),
            StatusState::Error => Some(AttentionReason::Error),
            _ => None,
        }
    }

    /// CLEAR: remove `id` and every record beneath it; `""` removes all.
    /// Returns whether anything went.
    pub fn clear(&mut self, id: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|key, _| !within(key, id));
        self.records.len() != before
    }

    /// A prompt started or the program exited: `working`, `blocked` and
    /// `idle` go; `done` and `error` stay until seen.
    pub fn settle(&mut self) -> bool {
        let before = self.records.len();
        self.records.retain(|_, r| matches!(r.state, StatusState::Done | StatusState::Error));
        self.records.len() != before
    }

    /// The user saw `ids` (all when `None`): their `done`/`error` records go.
    pub fn seen(&mut self, ids: Option<&[String]>) -> bool {
        let before = self.records.len();
        self.records
            .retain(|id, r| !matches!(r.state, StatusState::Done | StatusState::Error) || ids.is_some_and(|ids| !ids.iter().any(|x| x == id)));
        self.records.len() != before
    }

    /// The effective app of `id`: its own, else its nearest ancestor's.
    fn app(&self, id: &str) -> String {
        let mut at = id;
        loop {
            if let Some(record) = self.records.get(at).filter(|r| !r.app.is_empty()) {
                return record.app.clone();
            }
            if at.is_empty() {
                return String::new();
            }
            at = at.rfind('/').map_or("", |i| &at[..i]);
        }
    }

    /// The records as published, by id, with inherited apps.
    #[must_use]
    pub fn published(&self) -> Vec<ProgramStatus> {
        self.records
            .iter()
            .map(|(id, r)| ProgramStatus {
                id: id.clone(),
                state: r.state,
                kind: r.kind,
                progress: r.progress,
                app: self.app(id),
                title: r.title.clone(),
                message: r.message.clone(),
                updated_at: r.updated_at,
            })
            .collect()
    }
}

/// Records per state.
#[must_use]
pub fn counts<'a>(records: impl IntoIterator<Item = &'a ProgramStatus>) -> AttentionCounts {
    let mut counts = AttentionCounts::default();
    for record in records {
        match record.state {
            StatusState::Working => counts.working += 1,
            StatusState::Blocked => counts.blocked += 1,
            StatusState::Done => counts.done += 1,
            StatusState::Error => counts.error += 1,
            StatusState::Idle | StatusState::Unknown => {}
        }
    }
    counts
}

/// An OSC 9;4 report as block progress; `None` removes it.
#[must_use]
pub fn progress(state: silo_vt::ProgressReportState, percent: i8) -> Option<Progress> {
    use silo_vt::ProgressReportState as S;
    let state = match state {
        S::Remove => return None,
        S::Set => ProgressState::Set,
        S::Error => ProgressState::Error,
        S::Indeterminate => ProgressState::Indeterminate,
        S::Pause => ProgressState::Pause,
        S::Unknown => ProgressState::Unknown,
    };
    Some(Progress { state, percent: u8::try_from(percent).ok().filter(|p| *p <= 100) })
}

/// A token bucket: `rate` per second, bursts of `burst`.
#[derive(Debug)]
pub struct Pacer {
    tokens: f64,
    at: Option<Instant>,
    burst: f64,
    rate: f64,
}

/// Attention a block may raise (OSC 7501 transitions and OSC 9/777).
impl Default for Pacer {
    fn default() -> Self {
        Self::new(5, 1)
    }
}

impl Pacer {
    #[must_use]
    pub fn new(burst: u32, rate: u32) -> Self {
        Self { tokens: f64::from(burst), at: None, burst: f64::from(burst), rate: f64::from(rate) }
    }

    /// State changes a block publishes at once: 4/s, bursts of 8. Later
    /// ones wait (coalesced); the last state always goes out.
    #[must_use]
    pub fn changes() -> Self {
        Self::new(8, 4)
    }

    fn tokens(&self, now: Instant) -> f64 {
        let refill = self.at.map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f64() * self.rate);
        (self.tokens + refill).min(self.burst)
    }

    /// Whether one more may go out at `now` (and count it).
    pub fn allow(&mut self, now: Instant) -> bool {
        self.tokens = self.tokens(now);
        self.at = Some(now);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }

    /// How long from `now` until [`Self::allow`] passes (rounded up).
    #[must_use]
    pub fn wait(&self, now: Instant) -> Duration {
        let missing = 1.0 - self.tokens(now);
        if missing <= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(missing / self.rate) + Duration::from_millis(1) }
    }
}

/// Programs that are only shells: a shell at its prompt has nothing in
/// flight, so it never makes a block busy.
const HARMLESS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "nu", "xonsh", "elvish", "login"];

#[must_use]
pub fn harmless(name: &str) -> bool {
    HARMLESS.contains(&name.trim_start_matches('-'))
}

/// What the actor knows about a running block (§5.7).
#[derive(Clone, Debug, Default)]
pub struct BusyInputs {
    /// Started with an explicit command: `pid` is the program, no shell.
    pub command_block: bool,
    /// The PTY's child (the shell, or the command).
    pub pid: i32,
    pub pid_name: Option<String>,
    /// The PTY's foreground process group and its leader's name.
    pub foreground: Option<i32>,
    pub foreground_name: Option<String>,
    /// Names of the other processes in the program's own group, read only
    /// for a command block whose program is a shell holding the
    /// foreground: a non-interactive shell (`sh -c 'a; b'`, dash even for
    /// one command) runs its commands there, not in a group of their own.
    pub group: Vec<String>,
    /// OSC 133: the last step was C (no D, A or RIS since), with its
    /// command line (empty when the shell sent none).
    pub marks: Option<String>,
}

/// Whether a running block is busy, and with what: `(process, command)`.
#[must_use]
pub fn busy(inputs: &BusyInputs) -> Option<(String, Option<String>)> {
    let command = inputs.marks.clone().filter(|c| !c.is_empty());
    // A process whose name can't be read (setuid, like `top`) goes by the
    // command line's first word.
    let first = inputs.marks.as_deref().and_then(|m| m.split_whitespace().next()).map(str::to_owned);
    let named = |name: &Option<String>| name.clone().filter(|n| !n.is_empty()).or_else(|| first.clone()).unwrap_or_else(|| "a command".to_owned());
    // 3. A command block runs while its program (or what it runs in the
    //    foreground) isn't a shell, or while its shell runs something in
    //    its own group.
    if inputs.command_block {
        let (name, own) = match inputs.foreground {
            Some(fg) if fg != inputs.pid => (&inputs.foreground_name, false),
            _ => (&inputs.pid_name, true),
        };
        if !name.as_deref().is_some_and(harmless) {
            return Some((named(name), command));
        }
        if own && let Some(child) = inputs.group.iter().find(|n| !harmless(n)).or(inputs.group.first()) {
            return Some((named(&Some(child.clone())), command));
        }
    }
    // 2. Something other than the shell holds the foreground.
    if let Some(fg) = inputs.foreground
        && fg != inputs.pid
        && !inputs.foreground_name.as_deref().is_some_and(harmless)
    {
        return Some((named(&inputs.foreground_name), command));
    }
    // 1. The shell said a command runs (a builtin keeps the foreground).
    inputs.marks.as_ref()?;
    Some((named(&None), command))
}

/// [`busy`] as published: `since` carries on while a busy period lasts.
#[must_use]
pub fn busy_status(found: Option<(String, Option<String>)>, previous: Option<&Busy>, now_ms: u64) -> Option<Busy> {
    let (process, command) = found?;
    let process = untrusted(&process, MAX_APP);
    let command = command.map(|c| untrusted(&c, silo_proto::text::MAX_COMMAND));
    Some(Busy { process, command, since: previous.map_or(now_ms, |b| b.since) })
}

pub async fn handle(server: &Server, _client_id: u64, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::BlockReport(params) => {
            if !params.id.is_empty()
                && (params.id.len() > 128
                    || params.id.split('/').count() > 8
                    || params
                        .id
                        .split('/')
                        .any(|part| part.is_empty() || part.len() > 32 || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))))
            {
                return Err(RpcError::bad_request("invalid status id"));
            }
            if params.progress.is_some_and(|p| p > 100) || params.state == StatusState::Unknown || params.kind == Some(StatusKind::Unknown) {
                return Err(RpcError::bad_request("invalid status, kind or progress"));
            }
            let report = Report {
                id: params.id,
                state: Some(params.state),
                kind: params.kind,
                progress: params.progress.map_or(-1, |p| p as i8),
                app: params.app.unwrap_or_default(),
                title: params.title.unwrap_or_default(),
                message: params.message.unwrap_or_default(),
            };
            let (reply, rx) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().block(&params.block_id)?.send(BlockMsg::Report { report, reply });
            rx.await.map_err(|_| RpcError::not_found(format!("block {}", params.block_id)))?;
            serde_json::to_value(Empty {}).map_err(RpcError::internal)
        }
        Method::BlockNotify(params) => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            {
                let mut reg = server.registry.lock().unwrap();
                if let Some(crate::browser::Block::Browser(browser)) = reg.blocks.get_mut(&params.block_id) {
                    if browser.notifications.allow(Instant::now()) {
                        reg.handle_note(crate::block::Note::Attention {
                            block_id: params.block_id,
                            reason: AttentionReason::Notification,
                            title: untrusted(&params.title.unwrap_or_default(), MAX_TITLE),
                            body: untrusted(&params.body, silo_proto::text::MAX_BODY),
                            at: crate::block::now_ms(),
                        });
                    }
                    return serde_json::to_value(Empty {}).map_err(RpcError::internal);
                }
                reg.block(&params.block_id)?.send(BlockMsg::Notify { title: params.title.unwrap_or_default(), body: params.body, reply });
            }
            rx.await.map_err(|_| RpcError::not_found(format!("block {}", params.block_id)))?;
            serde_json::to_value(Empty {}).map_err(RpcError::internal)
        }
        Method::BlockStatusSeen { block_id, ids } => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::StatusSeen { ids, reply });
            rx.await.map_err(|_| RpcError::not_found(format!("block {block_id}")))?;
            serde_json::to_value(Empty {}).map_err(RpcError::internal)
        }
        method => Err(RpcError::method_not_found(&method_name(&method))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: &str, state: StatusState) -> Report {
        Report { id: id.into(), state: Some(state), progress: -1, ..Report::default() }
    }

    fn ids(records: &Records) -> Vec<String> {
        records.published().into_iter().map(|r| r.id).collect()
    }

    #[test]
    fn a_report_replaces_its_record_completely() {
        let mut records = Records::default();
        let first =
            Report { app: "cargo".into(), title: "build".into(), message: "compiling".into(), progress: 40, ..report("", StatusState::Working) };
        records.report(first, 1);
        records.report(report("", StatusState::Idle), 2);
        let published = records.published();
        assert_eq!(published.len(), 1);
        let r = &published[0];
        assert_eq!(
            (r.state, r.app.as_str(), r.title.as_str(), r.message.as_str(), r.progress, r.updated_at),
            (StatusState::Idle, "", "", "", None, 2)
        );
    }

    #[test]
    fn clear_removes_the_subtree_and_an_empty_id_everything() {
        let mut records = Records::default();
        for id in ["build", "build/test", "build/test/unit", "buildx", "deploy"] {
            records.report(report(id, StatusState::Working), 1);
        }
        assert!(records.clear("build"));
        assert_eq!(ids(&records), ["buildx", "deploy"], "siblings with a shared prefix stay");
        assert!(!records.clear("nothing"));
        assert!(records.clear(""));
        assert!(records.is_empty());
    }

    #[test]
    fn apps_are_inherited_from_the_nearest_ancestor_when_published() {
        let mut records = Records::default();
        records.report(Report { app: "deploy".into(), ..report("", StatusState::Working) }, 1);
        records.report(Report { app: "aws".into(), ..report("us", StatusState::Working) }, 1);
        records.report(report("us/east/1", StatusState::Working), 1);
        records.report(report("eu", StatusState::Working), 1);
        let apps: Vec<(String, String)> = records.published().into_iter().map(|r| (r.id, r.app)).collect();
        assert_eq!(apps, [("", "deploy"), ("eu", "deploy"), ("us", "aws"), ("us/east/1", "aws")].map(|(a, b)| (a.to_owned(), b.to_owned())));
        records.clear("us");
        records.report(report("us/east/1", StatusState::Working), 1);
        assert_eq!(records.published().last().unwrap().app, "deploy", "computed when published, never stored");
    }

    fn mixed() -> Records {
        let mut records = Records::default();
        for (id, state) in
            [("a", StatusState::Working), ("b", StatusState::Blocked), ("c", StatusState::Idle), ("d", StatusState::Done), ("e", StatusState::Error)]
        {
            records.report(report(id, state), 1);
        }
        records
    }

    #[test]
    fn a_prompt_start_clears_working_blocked_and_idle() {
        let mut records = mixed();
        assert!(records.settle());
        assert_eq!(ids(&records), ["d", "e"]);
        assert!(!records.settle());
    }

    #[test]
    fn a_process_exit_clears_working_blocked_and_idle() {
        // The actor runs the same rule from `finish` and at restore.
        let mut records = mixed();
        records.settle();
        let states: Vec<StatusState> = records.published().into_iter().map(|r| r.state).collect();
        assert_eq!(states, [StatusState::Done, StatusState::Error]);
    }

    #[test]
    fn done_and_error_stay_until_seen() {
        let mut records = mixed();
        records.settle();
        records.report(report("f", StatusState::Done), 2);
        assert!(records.seen(Some(&["d".to_owned(), "a".to_owned()])));
        assert_eq!(ids(&records), ["e", "f"]);
        records.report(report("w", StatusState::Working), 3);
        assert!(records.seen(None));
        assert_eq!(ids(&records), ["w"], "seen never removes running work");
    }

    #[test]
    fn at_most_256_records_evicting_the_least_recently_updated() {
        let mut records = Records::default();
        for i in 0..MAX_RECORDS {
            records.report(report(&format!("r{i}"), StatusState::Working), 1);
        }
        records.report(report("r0", StatusState::Working), 2);
        records.report(report("new", StatusState::Working), 3);
        let ids = ids(&records);
        assert_eq!(ids.len(), MAX_RECORDS);
        assert!(ids.contains(&"r0".to_owned()), "r0 was updated recently");
        assert!(!ids.contains(&"r1".to_owned()), "r1 was updated longest ago");
        assert!(ids.contains(&"new".to_owned()));
    }

    #[test]
    fn ris_clears_records_and_progress() {
        // RIS arrives as CLEAR "" then progress REMOVE (both from upstream).
        let mut records = mixed();
        assert!(records.clear(""));
        assert!(records.is_empty());
        assert_eq!(progress(silo_vt::ProgressReportState::Remove, -1), None);
        assert_eq!(progress(silo_vt::ProgressReportState::Set, 40), Some(Progress { state: ProgressState::Set, percent: Some(40) }));
    }

    #[test]
    fn attention_on_entering_blocked_done_or_error_only() {
        let mut records = Records::default();
        assert_eq!(records.report(report("", StatusState::Working), 1), None);
        assert_eq!(records.report(report("", StatusState::Blocked), 1), Some(AttentionReason::Blocked));
        assert_eq!(records.report(report("", StatusState::Blocked), 1), None, "still blocked");
        assert_eq!(records.report(report("", StatusState::Done), 1), Some(AttentionReason::Done));
        assert_eq!(records.report(report("x", StatusState::Error), 1), Some(AttentionReason::Error));
        assert_eq!(counts(&records.published()), AttentionCounts { working: 0, blocked: 0, done: 1, error: 1 });
    }

    #[test]
    fn program_text_is_stripped_of_bidi_and_zero_width_chars() {
        let mut records = Records::default();
        let sneaky = Report {
            app: "c\u{200b}argo".into(),
            title: "safe\u{202e}txt.exe".into(),
            message: "a\u{2066}b\u{feff}c\u{1b}[31m\u{ad}d".into(),
            ..report("", StatusState::Working)
        };
        records.report(sneaky, 1);
        let r = &records.published()[0];
        assert_eq!((r.app.as_str(), r.title.as_str(), r.message.as_str()), ("cargo", "safetxt.exe", "abc[31md"));
    }

    #[test]
    fn notifications_are_paced_one_a_second_with_bursts_of_five() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        assert_eq!((0..7).filter(|_| pacer.allow(start)).count(), 5);
        assert!(!pacer.allow(start + Duration::from_millis(500)));
        assert!(pacer.allow(start + Duration::from_millis(1100)));
    }

    #[test]
    fn state_changes_are_paced_four_a_second_with_bursts_of_eight() {
        let mut pacer = Pacer::changes();
        let start = Instant::now();
        assert_eq!((0..20).filter(|_| pacer.allow(start)).count(), 8);
        let wait = pacer.wait(start);
        assert!(wait > Duration::from_millis(240) && wait <= Duration::from_millis(252), "{wait:?}");
        assert!(!pacer.allow(start + wait - Duration::from_millis(5)));
        assert_eq!(pacer.wait(start + wait), Duration::ZERO);
        assert!(pacer.allow(start + wait));
    }

    fn inputs() -> BusyInputs {
        BusyInputs { pid: 10, pid_name: Some("bash".into()), foreground: Some(10), foreground_name: Some("bash".into()), ..BusyInputs::default() }
    }

    #[test]
    fn busy_rules() {
        assert_eq!(busy(&inputs()), None, "a shell at its prompt");
        let sleeping = BusyInputs { foreground: Some(11), foreground_name: Some("sleep".into()), ..inputs() };
        assert_eq!(busy(&sleeping), Some(("sleep".into(), None)));
        let nested = BusyInputs { foreground: Some(11), foreground_name: Some("-zsh".into()), ..inputs() };
        assert_eq!(busy(&nested), None, "a nested shell at its prompt");
        let builtin = BusyInputs { marks: Some("read x".into()), ..inputs() };
        assert_eq!(busy(&builtin), Some(("read".into(), Some("read x".into()))));
        let unnamed = BusyInputs { marks: Some(String::new()), ..inputs() };
        assert_eq!(busy(&unnamed), Some(("a command".into(), None)));
        let ssh = BusyInputs { foreground: Some(12), foreground_name: Some("ssh".into()), marks: None, ..inputs() };
        assert_eq!(busy(&ssh), Some(("ssh".into(), None)), "a remote prompt's marks don't hide a running ssh");
        let setuid = BusyInputs { foreground: Some(13), foreground_name: None, marks: Some("top -o cpu".into()), ..inputs() };
        assert_eq!(busy(&setuid), Some(("top".into(), Some("top -o cpu".into()))), "an unreadable name goes by the command line");
        let command = BusyInputs { command_block: true, pid_name: Some("sleep".into()), foreground_name: Some("sleep".into()), ..inputs() };
        assert_eq!(busy(&command), Some(("sleep".into(), None)));
        let shell_command = BusyInputs { command_block: true, pid_name: Some("zsh".into()), foreground_name: Some("zsh".into()), ..inputs() };
        assert_eq!(busy(&shell_command), None);
        let script = BusyInputs { group: vec!["sh".into(), "sleep".into()], ..shell_command.clone() };
        assert_eq!(busy(&script), Some(("sleep".into(), None)), "sh -c runs its commands in its own group");
        let subshell = BusyInputs { group: vec!["sh".into()], ..shell_command.clone() };
        assert_eq!(busy(&subshell), Some(("sh".into(), None)));
        let elsewhere = BusyInputs { foreground: Some(11), foreground_name: Some("bash".into()), ..script };
        assert_eq!(busy(&elsewhere), None, "the group counts only while the program holds the foreground");
        let previous = busy_status(Some(("vim".into(), None)), None, 5).unwrap();
        assert_eq!(busy_status(Some(("git".into(), None)), Some(&previous), 9).unwrap().since, 5, "one busy period");
    }
}
