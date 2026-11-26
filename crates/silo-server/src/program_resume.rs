//! Program resume (OSC 7577, docs/RESUME.md): a program tells its terminal
//! how to bring it back, and after the server restarts (a crash, a reboot,
//! a stop) the block's fresh shell runs that command at its first prompt.
//!
//! Each launched block gets its own random token in `OSC7577_TOKEN`; a
//! report proves it knows it with `mac = sha256(body ":" token)`. Reports
//! are taken out of the output stream (`silo_proto::offer`), checked here
//! and kept as [`Record`]s in the block's checkpoint. The token itself is
//! never written to disk: it only travels to a successor in a hot upgrade.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The variable that carries a block's token, and announces the feature.
pub const TOKEN_ENV: &str = "OSC7577_TOKEN";
/// Silo's shell integration runs the file it names at the first prompt.
pub const RESUME_ENV: &str = "SILO_RESUME";
/// A report's `time` may be this far from our clock (seconds).
const TIME_WINDOW: u64 = 60;
/// Most `argv` elements accepted (the protocol asks for at least 64).
const MAX_ARGV: usize = 256;
/// Records kept per block; the oldest report goes first.
const MAX_RECORDS: usize = 8;
/// An owner that exited this recently still counts as running: at logout
/// or a shutdown a program may end just before the terminal does.
pub const GONE_GRACE_MS: u64 = 2_000;
/// How far up from the owner the block's own program is looked for.
const MAX_DEPTH: u32 = 64;
/// Keys this implementation knows (for `need`).
const KNOWN: &[&str] = &["pid", "time", "mac", "argv", "cwd", "clear", "need"];

/// A fresh token: 32 characters of `[A-Za-z0-9_-]` (192 bits).
#[must_use]
pub fn new_token() -> String {
    const SET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| SET[usize::from(b & 63)] as char).collect()
}

/// The OSC 7577 sequence reporting `argv` for `pid` (with `cwd`), or a
/// clear when `argv` is `None`, signed with `token`.
#[must_use]
pub fn encode(token: &str, pid: i32, time: u64, argv: Option<&[String]>, cwd: Option<&str>) -> String {
    let b64 = |s: &str| data_encoding::BASE64.encode(s.as_bytes());
    let mut body = format!("pid={pid}:time={time}");
    match argv {
        Some(argv) => {
            body.push_str(":argv=");
            body.push_str(&argv.iter().map(|a| b64(a)).collect::<Vec<_>>().join(","));
            if let Some(cwd) = cwd {
                body.push_str(":cwd=");
                body.push_str(&b64(cwd));
            }
        }
        None => body.push_str(":clear=1"),
    }
    let mut hash = Sha256::new();
    hash.update(body.as_bytes());
    hash.update(b":");
    hash.update(token.as_bytes());
    format!("\x1b]7577;{body}:mac={}\x1b\\", data_encoding::HEXLOWER.encode(&hash.finalize()))
}

/// A report that passed its `mac` and `time` checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub pid: i32,
    pub time: u64,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Set { argv: Vec<String>, cwd: Option<String> },
    Clear,
}

/// What a report amounts to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No effect at all: a wrong or missing `mac`, a stale `time`.
    Ignored,
    /// Authentic but not acceptable: the owner's records go (none without
    /// a usable `pid`).
    Rejected(Option<i32>),
    Report(Report),
}

/// Check and parse a report body (what follows `OSC 7577 ;`).
#[must_use]
pub fn parse(body: &[u8], token: &str, now: u64) -> Verdict {
    let Ok(body) = std::str::from_utf8(body) else { return Verdict::Ignored };
    let Some((signed, mac)) = body.rsplit_once(":mac=") else { return Verdict::Ignored };
    if mac.len() != 64 || !mac.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Verdict::Ignored;
    }
    let mut hash = Sha256::new();
    hash.update(signed.as_bytes());
    hash.update(b":");
    hash.update(token.as_bytes());
    let expected = data_encoding::HEXLOWER.encode(&hash.finalize());
    // Compared without an early exit.
    if expected.bytes().zip(mac.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return Verdict::Ignored;
    }
    let pairs: Vec<Option<(&str, &str)>> = signed.split(':').map(|p| p.split_once('=')).collect();
    let time = pairs.iter().flatten().find(|(k, _)| *k == "time").and_then(|(_, v)| number(v));
    if time.is_none_or(|t| t.abs_diff(now) > TIME_WINDOW) {
        return Verdict::Ignored;
    }
    let pid = pairs.iter().flatten().find(|(k, _)| *k == "pid").and_then(|(_, v)| number(v)).and_then(|p| i32::try_from(p).ok());
    match strict(&pairs) {
        Some(report) => Verdict::Report(report),
        None => Verdict::Rejected(pid),
    }
}

/// `[1-9][0-9]*`.
fn number(value: &str) -> Option<u64> {
    (!value.starts_with('0') && !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())).then(|| value.parse().ok()).flatten()
}

fn strict(pairs: &[Option<(&str, &str)>]) -> Option<Report> {
    let mut seen: Vec<&str> = Vec::new();
    let (mut pid, mut time, mut argv, mut cwd, mut clear, mut need) = (None, None, None, None, false, Vec::new());
    for pair in pairs {
        let (key, value) = (*pair)?;
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase()) || seen.contains(&key) {
            return None;
        }
        if !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b)) {
            return None;
        }
        seen.push(key);
        match key {
            "pid" => pid = Some(i32::try_from(number(value)?).ok()?),
            "time" => time = Some(number(value)?),
            "argv" => argv = Some(value.split(',').map(text).collect::<Option<Vec<String>>>()?),
            "cwd" => cwd = Some(text(value)?),
            "clear" if value == "1" => clear = true,
            "clear" => return None,
            "need" => need = value.split(',').collect(),
            _ => {}
        }
    }
    if need.iter().any(|k| !KNOWN.contains(k)) {
        return None;
    }
    let (pid, time) = (pid?, time?);
    let action = match (argv, clear) {
        (Some(argv), false) => {
            let first = argv.first()?;
            let program_ok = if first.contains('/') { first.starts_with('/') } else { !first.is_empty() && !first.starts_with('-') };
            if !program_ok || argv.len() > MAX_ARGV {
                return None;
            }
            if cwd.as_deref().is_some_and(|c| !c.starts_with('/')) {
                return None;
            }
            Action::Set { argv, cwd }
        }
        (None, true) => Action::Clear,
        _ => return None,
    };
    Some(Report { pid, time, action })
}

/// One base64 element: UTF-8 without control characters. Padding is
/// optional, but checked when present.
fn text(value: &str) -> Option<String> {
    let bytes = if value.contains('=') {
        data_encoding::BASE64.decode(value.as_bytes()).ok()?
    } else {
        data_encoding::BASE64_NOPAD.decode(value.as_bytes()).ok()?
    };
    let text = String::from_utf8(bytes).ok()?;
    (!text.chars().any(char::is_control)).then_some(text)
}

/// What a block remembers: a resume command per owning process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub pid: i32,
    /// The owner's start time ([`silo_pty::ProcessInfo::start`]).
    pub start: u64,
    /// The report's `time`.
    pub time: u64,
    pub argv: Vec<String>,
    /// The report's own `cwd`: resumed only while it exists.
    #[serde(default)]
    pub cwd: Option<String>,
    /// The owner's directory when the report came.
    #[serde(default)]
    pub owner_cwd: Option<String>,
    /// Steps from the block's program down to the owner (0: the program).
    pub depth: u32,
    pub uid: u32,
    /// When the owner was first seen gone (ms since the epoch).
    #[serde(default)]
    pub gone_ms: Option<u64>,
}

/// A block's token and records (in its checkpoint).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeState {
    /// `None` for a block without a program, or one from before this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default)]
    pub records: Vec<Record>,
    /// The last accepted `time` per pid (reports never go backwards).
    #[serde(default)]
    pub last: Vec<(i32, u64)>,
    /// Bumped on every change to `records`.
    #[serde(default)]
    pub rev: u64,
}

/// The processes a report is checked against (the system, or a test's).
pub trait Processes {
    fn info(&self, pid: i32) -> Option<silo_pty::ProcessInfo>;
    fn cwd(&self, pid: i32) -> Option<PathBuf>;
}

/// The running system.
pub struct System;

impl Processes for System {
    fn info(&self, pid: i32) -> Option<silo_pty::ProcessInfo> {
        silo_pty::process_info(pid)
    }

    fn cwd(&self, pid: i32) -> Option<PathBuf> {
        silo_pty::process_cwd(pid)
    }
}

/// The user the server runs as.
#[must_use]
#[allow(unsafe_code)]
pub fn our_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

impl ResumeState {
    /// A launched block's state, with a fresh token.
    #[must_use]
    pub fn fresh() -> Self {
        Self { token: Some(new_token()), ..Self::default() }
    }

    /// Take one report out of `program`'s output (the block's first
    /// process). Returns whether the records changed.
    pub fn report(&mut self, body: &[u8], program: i32, now_ms: u64, procs: &impl Processes) -> bool {
        let Some(token) = self.token.as_deref() else { return false };
        let report = match parse(body, token, now_ms / 1000) {
            Verdict::Ignored => return false,
            Verdict::Rejected(pid) => return pid.is_some_and(|pid| self.remove(pid)),
            Verdict::Report(report) => report,
        };
        if self.last.iter().any(|&(pid, time)| pid == report.pid && time > report.time) {
            return false;
        }
        let Some((info, depth)) = owner(report.pid, program, procs) else {
            return self.remove(report.pid);
        };
        self.last.retain(|&(pid, _)| pid != report.pid);
        self.last.push((report.pid, report.time));
        if self.last.len() > 4 * MAX_RECORDS {
            self.last.remove(0);
        }
        match report.action {
            Action::Clear => self.remove(report.pid),
            Action::Set { argv, cwd } => {
                self.records.retain(|r| r.pid != report.pid);
                let owner_cwd = procs.cwd(report.pid).and_then(|p| p.to_str().map(str::to_owned));
                self.records.push(Record {
                    pid: report.pid,
                    start: info.start,
                    time: report.time,
                    argv,
                    cwd,
                    owner_cwd,
                    depth,
                    uid: info.uid,
                    gone_ms: None,
                });
                if self.records.len() > MAX_RECORDS {
                    let oldest = self.records.iter().enumerate().min_by_key(|(_, r)| r.time).map(|(i, _)| i).unwrap_or(0);
                    self.records.remove(oldest);
                }
                self.rev += 1;
                true
            }
        }
    }

    fn remove(&mut self, pid: i32) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.pid != pid);
        let changed = self.records.len() != before;
        if changed {
            self.rev += 1;
        }
        changed
    }

    /// Notice owners that exited; drop those gone longer than
    /// [`GONE_GRACE_MS`]. Returns whether the records changed.
    pub fn sweep(&mut self, now_ms: u64, procs: &impl Processes) -> bool {
        for record in &mut self.records {
            let alive = procs.info(record.pid).is_some_and(|i| i.start == record.start);
            if alive {
                record.gone_ms = None;
            } else {
                record.gone_ms.get_or_insert(now_ms);
            }
        }
        let before = self.records.len();
        self.records.retain(|r| r.gone_ms.is_none_or(|gone| now_ms.saturating_sub(gone) <= GONE_GRACE_MS));
        let changed = self.records.len() != before;
        if changed {
            self.rev += 1;
        }
        changed
    }

    /// The record to resume: the outermost owner, then the latest report.
    #[must_use]
    pub fn pick(&self) -> Option<&Record> {
        self.records.iter().min_by(|a, b| a.depth.cmp(&b.depth).then(b.time.cmp(&a.time)))
    }
}

/// The owner's process and its distance below `program`, when it runs as
/// us (never as root) inside the block.
fn owner(pid: i32, program: i32, procs: &impl Processes) -> Option<(silo_pty::ProcessInfo, u32)> {
    let info = procs.info(pid)?;
    let uid = our_uid();
    if uid == 0 || info.uid != uid {
        return None;
    }
    let (mut at, mut depth) = (pid, 0);
    while at != program {
        if depth >= MAX_DEPTH || at <= 1 {
            return None;
        }
        at = procs.info(at)?.ppid;
        depth += 1;
    }
    Some((info, depth))
}

/// Where a restored block's program resumes, and the file its shell reads.
pub struct Resumed {
    pub cwd: Option<String>,
    pub file: PathBuf,
    /// The program's name, for the restored banner.
    pub name: String,
}

/// The directory of pending resume files.
#[must_use]
pub fn dir(home: &Path) -> PathBuf {
    home.join("resume")
}

/// Remove files a shell never read (a run before this one).
pub fn clean(home: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir(home)) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Prepare `record` to run in `block`'s next shell: one argument per line,
/// each after a `=` (so empty ones survive), in a file only we can read.
/// `None` when it should not resume: another user's, or its own `cwd` is
/// gone.
#[must_use]
pub fn prepare(home: &Path, block: &str, record: &Record) -> Option<Resumed> {
    use std::{
        io::Write,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    };
    let uid = our_uid();
    if uid == 0 || record.uid != uid || record.argv.is_empty() {
        return None;
    }
    let cwd = match &record.cwd {
        Some(cwd) if !Path::new(cwd).is_dir() => return None,
        Some(cwd) => Some(cwd.clone()),
        None => record.owner_cwd.clone().filter(|c| Path::new(c).is_dir()),
    };
    let dir = dir(home);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).ok()?;
    let file = dir.join(format!("{block}-{:016x}", rand::random::<u64>()));
    let text: String = record.argv.iter().map(|a| format!("={a}\n")).collect();
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&file).ok()?;
    out.write_all(text.as_bytes()).ok()?;
    let name = Path::new(&record.argv[0]).file_name().and_then(|n| n.to_str()).unwrap_or(&record.argv[0]);
    Some(Resumed { cwd, file, name: shown(name) })
}

/// `text` safe to print: no controls, no bidirectional overrides.
fn shown(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const TOKEN: &str = "dummy-token-replace-me";
    const T: u64 = 1_795_543_200;

    fn mac(body: &str, token: &str) -> String {
        let mut hash = Sha256::new();
        hash.update(format!("{body}:{token}").as_bytes());
        data_encoding::HEXLOWER.encode(&hash.finalize())
    }

    fn signed(body: &str) -> String {
        format!("{body}:mac={}", mac(body, TOKEN))
    }

    #[test]
    fn the_specification_s_examples_verify() {
        let nvim = "pid=74877:time=1795543200:argv=bnZpbQ==,LVM=,L2hvbWUvdXNlci8ubG9jYWwvc3RhdGUvbnZpbS9zZXNzaW9ucy84ZjJjMWQudmlt:mac=9dbb99bfb856749a87563839d7ce9062818d78cf25909db93f309e0cd3b1909b";
        let argv = vec!["nvim".to_owned(), "-S".to_owned(), "/home/user/.local/state/nvim/sessions/8f2c1d.vim".to_owned()];
        assert_eq!(parse(nvim.as_bytes(), TOKEN, T), Verdict::Report(Report { pid: 74877, time: T, action: Action::Set { argv, cwd: None } }));
        let clear = "pid=74877:time=1795543200:clear=1:mac=c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93";
        assert_eq!(parse(clear.as_bytes(), TOKEN, T + 59), Verdict::Report(Report { pid: 74877, time: T, action: Action::Clear }));
        let lazygit = "pid=74877:time=1795543200:argv=bGF6eWdpdA==:cwd=L2hvbWUvdXNlci9zcmMvcHJvamVjdA==:mac=35cb909805c380336efd3689a125acc774a9dec8422a1d21492ebec06ca758e1";
        let Verdict::Report(Report { action: Action::Set { argv, cwd }, .. }) = parse(lazygit.as_bytes(), TOKEN, T) else { panic!() };
        assert_eq!((argv, cwd.as_deref()), (vec!["lazygit".to_owned()], Some("/home/user/src/project")));
        let wrap =
            "pid=90412:time=1795543200:argv=cmVzdW1lLXdyYXA=,c3No,ZGV2Ym94:mac=e76149d0ba54a83570ffab0583867dacdfa23928ba9d18604f0badea88b62b65";
        assert!(matches!(parse(wrap.as_bytes(), TOKEN, T), Verdict::Report(_)));
    }

    #[test]
    fn encoded_reports_parse_back() {
        let argv = vec!["claude".to_owned(), "--resume".to_owned(), String::new()];
        let seq = encode(TOKEN, 42, T, Some(&argv), Some("/work dir"));
        let body = seq.strip_prefix("\x1b]7577;").and_then(|s| s.strip_suffix("\x1b\\")).unwrap();
        let action = Action::Set { argv, cwd: Some("/work dir".into()) };
        assert_eq!(parse(body.as_bytes(), TOKEN, T), Verdict::Report(Report { pid: 42, time: T, action }));
        let clear = encode(TOKEN, 42, T, None, None);
        assert!(clear.contains(":clear=1:mac="));
        // The specification's own clear example.
        assert_eq!(
            encode(TOKEN, 74877, T, None, None),
            "\x1b]7577;pid=74877:time=1795543200:clear=1:mac=c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93\x1b\\"
        );
    }

    #[test]
    fn the_specification_s_invalid_reports() {
        let ignored = |body: &str| assert_eq!(parse(body.as_bytes(), TOKEN, T), Verdict::Ignored, "{body}");
        let rejected = |body: &str| assert_eq!(parse(signed(body).as_bytes(), TOKEN, T), Verdict::Rejected(Some(74877)), "{body}");
        ignored(&format!("pid=74877:time={T}:argv=bnZpbQ==:mac={}", "0".repeat(64)));
        ignored(&format!("pid=74877:time={T}:argv=bnZpbQ=="));
        ignored(&format!("mac={}:pid=74877:time={T}:argv=bnZpbQ==", mac(&format!("pid=74877:time={T}:argv=bnZpbQ=="), TOKEN)));
        ignored(&signed("pid=74877:time=1000000000:argv=bnZpbQ=="));
        ignored(&signed("pid=74877:argv=bnZpbQ=="));
        // Another terminal's token.
        ignored(&format!("pid=74877:time={T}:clear=1:mac={}", mac(&format!("pid=74877:time={T}:clear=1"), "another-token-of-22-chars")));
        rejected(&format!("pid=74877:time={T}:argv=Li9ydW4uc2g="));
        rejected(&format!("pid=74877:time={T}:argv=bnZpbQ==:clear=1"));
        rejected(&format!("pid=74877:time={T}:argv=bnZpbQ==:cwd=cHJvamVjdA=="));
        rejected(&format!("pid=74877:time={T}:argv=bnZpbQ==:need=later"));
        // More of the strict rules.
        rejected(&format!("pid=74877:time={T}:argv=")); // no program
        rejected(&format!("pid=74877:time={T}:argv=LWk=")); // `-i`
        rejected(&format!("pid=74877:time={T}:argv=bnZpbQ:argv=bnZpbQ")); // repeated key
        rejected(&format!("pid=74877:time={T}:argv=bnZpbQ=")); // bad padding
        rejected(&format!("pid=74877:time={T}:argv=bnZpbQ,Cg")); // "\n"
        rejected(&format!("pid=74877:time={T}:clear=2"));
        rejected(&format!("pid=74877:time={T}:junk"));
        assert_eq!(parse(signed(&format!("pid=0:time={T}:clear=1")).as_bytes(), TOKEN, T), Verdict::Rejected(None));
        // Unknown keys are ignored; a known one in `need` is fine; no padding.
        let ok = signed(&format!("pid=74877:time={T}:argv=bnZpbQ:later=x:need=cwd,argv"));
        assert!(matches!(parse(ok.as_bytes(), TOKEN, T), Verdict::Report(_)));
    }

    /// `pid -> (ppid, uid, start)`.
    struct Fake(HashMap<i32, (i32, u32, u64)>);

    impl Processes for Fake {
        fn info(&self, pid: i32) -> Option<silo_pty::ProcessInfo> {
            self.0.get(&pid).map(|&(ppid, uid, start)| silo_pty::ProcessInfo { ppid, uid, start })
        }

        fn cwd(&self, _: i32) -> Option<PathBuf> {
            Some("/tmp".into())
        }
    }

    fn report(pid: i32, time: u64, argv: &str) -> Vec<u8> {
        signed(&format!("pid={pid}:time={time}:argv={argv}")).into_bytes()
    }

    #[test]
    fn records_follow_their_owners() {
        let me = our_uid();
        if me == 0 {
            return;
        }
        // 10 is the block's shell, 11 its child, 12 a grandchild, 20 elsewhere.
        let mut procs = Fake(HashMap::from([(10, (1, me, 5)), (11, (10, me, 6)), (12, (11, me, 7)), (20, (1, me, 8))]));
        let mut state = ResumeState { token: Some(TOKEN.into()), ..ResumeState::default() };
        let now = T * 1000;
        assert!(state.report(&report(12, T, "aW5uZXI"), 10, now, &procs));
        assert!(state.report(&report(11, T, "b3V0ZXI"), 10, now, &procs));
        assert_eq!(state.pick().map(|r| (r.argv[0].as_str(), r.depth)), Some(("outer", 1)));
        // Not inside the block, or with a stale time: nothing changes.
        assert!(!state.report(&report(20, T, "eA"), 10, now, &procs));
        assert!(!state.report(&report(11, T - 1, "b2xk"), 10, now, &procs));
        assert_eq!(state.records.len(), 2);
        // Without the token: no effect.
        let mut other = ResumeState::default();
        assert!(!other.report(&report(11, T, "eA"), 10, now, &procs));
        // An owner that exits is kept for the grace period, then dropped.
        procs.0.remove(&11);
        assert!(!state.sweep(now, &procs));
        assert_eq!(state.records.len(), 2);
        assert!(state.sweep(now + GONE_GRACE_MS + 1, &procs));
        assert_eq!(state.pick().map(|r| r.argv[0].as_str()), Some("inner"));
        // A pid reused by a later process is not the owner.
        procs.0.insert(12, (11, me, 99));
        state.sweep(now + 10_000, &procs);
        state.sweep(now + 20_000, &procs);
        assert!(state.records.is_empty());
        // Clear removes; a rejected report removes too.
        procs.0.insert(11, (10, me, 6));
        assert!(state.report(&report(11, T, "eA"), 10, now, &procs));
        assert!(state.report(&signed(&format!("pid=11:time={T}:clear=1")).into_bytes(), 10, now, &procs));
        assert!(state.records.is_empty());
        assert!(state.report(&report(11, T + 1, "eA"), 10, now, &procs));
        assert!(state.report(&report(11, T + 1, "Li9ydW4"), 10, now, &procs));
        assert!(state.records.is_empty());
    }

    #[test]
    fn a_prepared_file_holds_one_argument_a_line() {
        let home = std::env::temp_dir().join(format!("silo-resume-{:x}", rand::random::<u64>()));
        let record = Record {
            pid: 1,
            start: 0,
            time: T,
            argv: vec!["nvim".into(), String::new(), "a b".into()],
            cwd: None,
            owner_cwd: Some("/".into()),
            depth: 1,
            uid: our_uid(),
            gone_ms: None,
        };
        if our_uid() != 0 {
            let resumed = prepare(&home, "b1", &record).expect("prepared");
            assert_eq!(std::fs::read_to_string(&resumed.file).unwrap(), "=nvim\n=\n=a b\n");
            assert_eq!((resumed.cwd.as_deref(), resumed.name.as_str()), (Some("/"), "nvim"));
            // A `cwd` the report named that is gone: not resumed.
            assert!(prepare(&home, "b1", &Record { cwd: Some("/no/such/dir".into()), ..record.clone() }).is_none());
            clean(&home);
            assert_eq!(std::fs::read_dir(dir(&home)).unwrap().count(), 0);
        }
        assert!(prepare(&home, "b1", &Record { uid: our_uid() + 1, ..record }).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }
}
