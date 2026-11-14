//! `silo.exec`: a command run where the VM runs, in its own process
//! group, with no shell.

use std::{collections::HashMap, os::unix::process::ExitStatusExt, process::Stdio, time::Duration};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Output kept per stream; the rest is dropped.
const MAX_OUTPUT: u64 = 16 << 20;

/// `silo.exec`'s options.
#[derive(Clone, Debug, Default)]
pub struct ExecOptions {
    pub cwd: Option<String>,
    /// Added to (`None`: removed from) the VM's environment.
    pub env: HashMap<String, Option<String>>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    pub detach: bool,
}

/// How a command ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecResult {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// A detached command: started, not waited for.
    pub pid: Option<u32>,
}

/// Kills a process group when dropped, unless disarmed: a command's
/// group goes with the action, handler or script that started it.
#[derive(Debug)]
pub struct GroupGuard(pub i32);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.0 > 0 {
            // SAFETY: plain syscall; a group already gone is ESRCH.
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
}

/// Start `argv`. A detached command is reaped in the background and
/// returns at once with its pid; otherwise the group is handed to `keep`
/// (killed when the invocation ends) and the output awaited.
///
/// # Errors
/// An empty `argv` or a command that does not start.
pub async fn run(argv: Vec<String>, options: ExecOptions, keep: impl FnOnce(GroupGuard)) -> Result<ExecResult, String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("silo.exec: argv is empty".into());
    };
    let mut command = tokio::process::Command::new(program);
    command.args(args).process_group(0).stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if options.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    if let Some(cwd) = &options.cwd {
        command.current_dir(expand_home(cwd));
    }
    for (key, value) in &options.env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    if options.detach {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let mut child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
    let pid = child.id();
    if let Some(input) = options.stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        tokio::task::spawn_local(async move {
            let _ = stdin.write_all(&input).await;
        });
    }
    if options.detach {
        tokio::task::spawn_local(async move {
            let _ = child.wait().await;
        });
        return Ok(ExecResult { pid, ..ExecResult::default() });
    }
    #[allow(clippy::cast_possible_wrap)]
    keep(GroupGuard(pid.map_or(0, |p| p as i32)));
    let read = |pipe: Option<Box<dyn tokio::io::AsyncRead + Unpin>>| async move {
        let mut out = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.take(MAX_OUTPUT).read_to_end(&mut out).await;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin>));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin>));
    let mut timed_out = false;
    let wait = async {
        match options.timeout {
            Some(limit) => match tokio::time::timeout(limit, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    timed_out = true;
                    if let Some(pid) = pid {
                        #[allow(clippy::cast_possible_wrap)]
                        // SAFETY: plain syscall on the group we started.
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    child.wait().await
                }
            },
            None => child.wait().await,
        }
    };
    let (status, stdout, stderr) = tokio::join!(wait, stdout, stderr);
    let status = status.map_err(|e| format!("{program}: {e}"))?;
    Ok(ExecResult { code: status.code(), signal: status.signal(), stdout, stderr, timed_out, pid: None })
}

/// `~/x` as `$HOME/x`.
fn expand_home(path: &str) -> std::path::PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::Path::new(&home).join(rest),
        _ if path == "~" => std::env::var_os("HOME").map_or_else(|| path.into(), Into::into),
        _ => path.into(),
    }
}
