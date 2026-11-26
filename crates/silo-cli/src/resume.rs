//! `silo resume`: report a program's resume command to its terminal
//! (OSC 7577, docs/RESUME.md) on `/dev/tty`. Any terminal that sets
//! `OSC7577_TOKEN` takes it, Silo among them.

use std::io::Write;

use anyhow::{Context, bail};
use silo_server::program_resume::{TOKEN_ENV, encode};

/// Shells and wrappers between a hook and the program that ran it.
const BETWEEN: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "env", "silo", "timeout", "nice"];

/// Report (or clear) and return; quietly nothing without a token.
///
/// # Errors
/// Bad input, or the terminal cannot be written.
pub fn run(pid: Option<i32>, cwd: Option<String>, clear: bool, claude: bool, argv: Vec<String>) -> anyhow::Result<()> {
    let Some(token) = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    let (argv, cwd, pid) = if claude {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        let hook: serde_json::Value = serde_json::from_str(&input).context("a Claude Code hook's JSON on stdin")?;
        let session = hook["session_id"].as_str().filter(|s| !s.is_empty()).context("no session_id")?;
        let cwd = cwd.or_else(|| hook["cwd"].as_str().map(str::to_owned));
        let pid = pid.or_else(owner).context("Claude Code's process not found")?;
        (Some(vec!["claude".to_owned(), "--resume".to_owned(), session.to_owned()]), cwd, pid)
    } else {
        let pid = pid.or_else(owner).context("no program to report for")?;
        if clear {
            (None, None, pid)
        } else {
            if argv.is_empty() {
                bail!("give the command after `--`, or --clear");
            }
            (Some(argv), cwd, pid)
        }
    };
    if argv.iter().flatten().chain(&cwd).any(|a| a.chars().any(char::is_control)) {
        bail!("a command with control characters (a newline) cannot be resumed");
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let sequence = encode(&token, pid, now, argv.as_deref(), cwd.as_deref());
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty").context("no terminal")?;
    tty.write_all(sequence.as_bytes())?;
    Ok(())
}

/// The program this was run for: the first ancestor that is not a shell
/// or wrapper (a hook runs through a shell; Claude Code's own process may
/// be named after its version or `node`).
fn owner() -> Option<i32> {
    // SAFETY: getppid has no preconditions.
    #[allow(unsafe_code)]
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..16 {
        let name = silo_pty::process_name(pid)?;
        let name = name.rsplit('/').next().unwrap_or(&name).trim_start_matches('-');
        if !BETWEEN.contains(&name) {
            return Some(pid);
        }
        pid = silo_pty::process_info(pid)?.ppid;
        if pid <= 1 {
            return None;
        }
    }
    None
}
