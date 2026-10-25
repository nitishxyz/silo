//! Running `git` for changes blocks: in the block's root, with a timeout,
//! capped output and, for reads, `GIT_OPTIONAL_LOCKS=0` so a review never
//! takes the index lock from a running agent.

use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use silo_proto::control::RpcError;

/// How long one git command may take (reads and most writes).
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// `git commit`: its hooks may run formatters and tests.
pub const COMMIT_TIMEOUT: Duration = Duration::from_secs(300);
/// Most bytes kept from a command's stdout unless asked for more.
pub const STDOUT_MAX: usize = 64 * 1024 * 1024;
const STDERR_MAX: usize = 64 * 1024;

/// A command to run.
pub struct Git<'a> {
    root: &'a Path,
    args: Vec<String>,
    read: bool,
    timeout: Duration,
    stdout_max: usize,
    stdin: Option<Vec<u8>>,
}

pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    /// Stdout went over the cap; what came before it is kept.
    pub truncated: bool,
}

impl Output {
    /// What git said, for an error message: stderr, else stdout.
    pub fn said(&self) -> String {
        let text = if self.stderr.trim().is_empty() { String::from_utf8_lossy(&self.stdout).into_owned() } else { self.stderr.clone() };
        let text = text.trim();
        // The end says most (hooks print their failure last).
        let start = text.char_indices().rev().nth(4000).map_or(0, |(i, _)| i);
        text[start..].to_owned()
    }
}

impl<'a> Git<'a> {
    /// A read in `root` (`GIT_OPTIONAL_LOCKS=0`).
    pub fn read<S: AsRef<str>>(root: &'a Path, args: impl IntoIterator<Item = S>) -> Self {
        Self {
            root,
            args: args.into_iter().map(|a| a.as_ref().to_owned()).collect(),
            read: true,
            timeout: TIMEOUT,
            stdout_max: STDOUT_MAX,
            stdin: None,
        }
    }

    /// A command that changes the repo (takes locks as git does).
    pub fn write<S: AsRef<str>>(root: &'a Path, args: impl IntoIterator<Item = S>) -> Self {
        Self { read: false, ..Self::read(root, args) }
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn stdout_max(mut self, max: usize) -> Self {
        self.stdout_max = max;
        self
    }

    #[must_use]
    pub fn stdin(mut self, bytes: Vec<u8>) -> Self {
        self.stdin = Some(bytes);
        self
    }

    /// Run it, whatever its exit code.
    ///
    /// # Errors
    /// `git` cannot start (`git`), or took longer than its timeout
    /// (`timeout`).
    pub fn output(self) -> Result<Output, RpcError> {
        let mut command = Command::new("git");
        command
            .current_dir(self.root)
            .args(["-c", "core.quotepath=false", "-c", "color.ui=false", "-c", "core.pager=cat"])
            .args(&self.args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_EXTERNAL_DIFF")
            .stdin(if self.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.read {
            // Reads are parsed: git's own words, whatever the locale.
            command.env("GIT_OPTIONAL_LOCKS", "0").env("LC_ALL", "C");
        }
        let mut child = command.spawn().map_err(|e| RpcError::new(RpcError::GIT, format!("git could not start: {e}")))?;
        if let (Some(bytes), Some(mut pipe)) = (self.stdin, child.stdin.take()) {
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            });
        }
        let max = self.stdout_max;
        let stdout = child.stdout.take().map(|pipe| std::thread::spawn(move || capped(pipe, max)));
        let stderr = child.stderr.take().map(|pipe| std::thread::spawn(move || capped(pipe, STDERR_MAX)));
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let what = self.args.iter().find(|a| !a.starts_with('-')).cloned().unwrap_or_default();
                    return Err(RpcError::new(RpcError::TIMEOUT, format!("git {what} took longer than {} s", self.timeout.as_secs())));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => return Err(RpcError::internal(format!("git: {e}"))),
            }
        };
        let (stdout, truncated) = stdout.and_then(|t| t.join().ok()).unwrap_or_default();
        let (stderr, _) = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
        Ok(Output { code: status.code(), stdout, stderr: String::from_utf8_lossy(&stderr).into_owned(), truncated })
    }

    /// Run it; exit code 0 (or one of `ok`) is success.
    ///
    /// # Errors
    /// As [`Self::output`]; another exit code is `git` with what it said.
    pub fn run_ok(self, ok: &[i32]) -> Result<Output, RpcError> {
        let out = self.output()?;
        match out.code {
            Some(0) => Ok(out),
            Some(code) if ok.contains(&code) => Ok(out),
            // Stopped reading: the pipe closed under it.
            _ if out.truncated => Ok(out),
            _ => Err(RpcError::new(RpcError::GIT, out.said())),
        }
    }

    /// Run it, expecting exit code 0.
    ///
    /// # Errors
    /// See [`Self::run_ok`].
    pub fn run(self) -> Result<Output, RpcError> {
        self.run_ok(&[])
    }
}

/// Up to `max` bytes of `pipe`, and whether there was more (the pipe is
/// closed then, so the writer stops).
fn capped(mut pipe: impl Read, max: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => return (out, false),
            Ok(n) => {
                if out.len() + n > max {
                    out.extend_from_slice(&buf[..max - out.len()]);
                    return (out, true);
                }
                out.extend_from_slice(&buf[..n]);
            }
        }
    }
}

/// Split NUL-terminated `-z` output into fields.
pub fn fields(bytes: &[u8]) -> Vec<String> {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes.split(|b| *b == 0).map(|f| String::from_utf8_lossy(f).into_owned()).collect()
}
