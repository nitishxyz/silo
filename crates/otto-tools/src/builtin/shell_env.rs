//! The user's environment as their own terminal has it.
//!
//! The daemon is started by an app or a service manager, so its `PATH` is the system's; and `$SHELL -lc` reads the
//! login files only, while most tools add themselves to the interactive one (`~/.zshrc`: bun, nvm, pyenv, cargo).
//! So a command an agent runs would not find what the user's terminal finds. Once, and again after [`FRESH`], the
//! interactive login shell is asked for its environment (`$SHELL -ilc env`); what differs from this process's is
//! given to each command, which then runs through `$SHELL -c` (the login files already had their say, and running
//! them again would put the system's directories back in front).

use std::{
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

/// What a command gets on top of the daemon's environment.
pub type Vars = Arc<Vec<(String, String)>>;

/// A resolved environment serves this long, so a tool installed meanwhile is found without a restart.
const FRESH: Duration = Duration::from_secs(300);
/// A failure is not retried sooner.
const RETRY: Duration = Duration::from_secs(60);
/// A shell whose rc files hang without a terminal is given up on.
const PATIENCE: Duration = Duration::from_secs(10);
/// Set for the resolving shell: an rc file can skip what it only does for a person.
const RESOLVING: &str = "OTTO_RESOLVING_ENV";
/// Never taken from the resolving shell: its own state, not the user's setup.
const VOLATILE: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_", RESOLVING, "TERM", "COLUMNS", "LINES", "TTY"];

static CACHE: tokio::sync::Mutex<Option<(Instant, Option<Vars>)>> = tokio::sync::Mutex::const_new(None);

/// The variables the user's interactive login shell sets beyond this process's; `None` when it can't be asked
/// (no `$SHELL`, one that isn't zsh, bash or fish, a failure or a hang, or `OTTO_SHELL_ENV=0`): commands then run
/// through `$SHELL -lc` as before.
pub async fn user_env() -> Option<Vars> {
    let mut cache = CACHE.lock().await;
    if let Some((at, vars)) = &*cache
        && at.elapsed() < if vars.is_some() { FRESH } else { RETRY }
    {
        return vars.clone();
    }
    let vars = resolve().await.map(Arc::new);
    *cache = Some((Instant::now(), vars.clone()));
    vars
}

async fn resolve() -> Option<Vec<(String, String)>> {
    if std::env::var("OTTO_SHELL_ENV").is_ok_and(|v| v == "0") {
        return None;
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let name = shell.rsplit('/').next().unwrap_or(&shell);
    if !["zsh", "bash", "fish"].contains(&name) {
        return None;
    }
    let mark = format!("__otto_env_{}__", std::process::id());
    let script = format!("printf '%s' {mark}; command env -0; printf '%s' {mark}");
    let mut cmd = tokio::process::Command::new(&shell);
    cmd.args(["-ilc", &script]).env(RESOLVING, "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    detach(&mut cmd);
    let child = cmd.spawn().ok()?;
    let group = child.id();
    let out = tokio::time::timeout(PATIENCE, child.wait_with_output()).await;
    if out.is_err()
        && let Some(group) = group.and_then(|g| i32::try_from(g).ok())
    {
        // What its rc files started goes with it.
        // SAFETY: a signal to the session this function made (its leader's pid is its group's id).
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    let out = out.ok()?.ok()?;
    let found = parse(&out.stdout, &mark)?;
    Some(found.into_iter().filter(|(k, v)| std::env::var(k).ok().as_deref() != Some(v.as_str())).collect())
}

/// Its own session, so no controlling terminal: an interactive shell in a background group of the daemon's
/// terminal (a daemon run in the foreground, a test) would be stopped the moment it reached for it.
fn detach(cmd: &mut tokio::process::Command) {
    // SAFETY: `setsid` is async-signal-safe and touches nothing of the parent's.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

/// The variables between the two `mark`s of the resolving shell's output (`env -0`: NUL-separated `KEY=value`;
/// anything an rc file printed is outside the marks), without the [`VOLATILE`] ones. `None` without both marks
/// or a `PATH`.
fn parse(out: &[u8], mark: &str) -> Option<Vec<(String, String)>> {
    let mark = mark.as_bytes();
    let find = |hay: &[u8]| hay.windows(mark.len()).position(|w| w == mark);
    let start = find(out)? + mark.len();
    let end = start + find(&out[start..])?;
    let vars: Vec<(String, String)> = out[start..end]
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            let (key, value) = entry.split_once('=')?;
            (!key.is_empty() && !VOLATILE.contains(&key)).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect();
    vars.iter().any(|(k, v)| k == "PATH" && !v.is_empty()).then_some(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_read_between_the_marks() {
        let out = b"welcome banner\n__m__PATH=/Users/a/.bun/bin:/usr/bin\0BUN_INSTALL=/Users/a/.bun\0PWD=/tmp\0SHLVL=2\0MULTI=a\nb=c\0\xff\xfe=bad\0=empty\0__m__trailing prompt";
        let vars = parse(out, "__m__").unwrap();
        let want = [("PATH", "/Users/a/.bun/bin:/usr/bin"), ("BUN_INSTALL", "/Users/a/.bun"), ("MULTI", "a\nb=c")];
        assert_eq!(vars, want.map(|(k, v)| (k.to_owned(), v.to_owned())), "the shell's own state and broken entries are left out");
        // One mark only (the shell died or printed nothing), or no PATH: not an environment.
        assert_eq!(parse(b"noise __m__PATH=/usr/bin\0", "__m__"), None);
        assert_eq!(parse(b"__m__HOME=/Users/a\0__m__", "__m__"), None);
        assert_eq!(parse(b"", "__m__"), None);
    }

    #[tokio::test]
    async fn a_shell_s_interactive_setup_reaches_the_environment() {
        // A zsh whose interactive rc file adds to PATH, as installers do.
        let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"].into_iter().find(|p| std::path::Path::new(p).exists()) else { return };
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".zshrc"), "echo hello from rc\nexport PATH=\"$HOME/.tool/bin:$PATH\"\nexport TOOL_HOME=\"$HOME/.tool\"\n")
            .unwrap();
        let mark = "__m__";
        let script = format!("printf '%s' {mark}; command env -0; printf '%s' {mark}");
        let mut cmd = tokio::process::Command::new(zsh);
        detach(&mut cmd);
        let out = cmd
            .args(["-ilc", &script])
            .env("HOME", home.path())
            .env("ZDOTDIR", home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .unwrap();
        let vars = parse(&out.stdout, mark).expect("an environment");
        let get = |k: &str| vars.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert!(get("PATH").unwrap().starts_with(&format!("{}/.tool/bin:", home.path().display())), "{:?}", get("PATH"));
        assert_eq!(get("TOOL_HOME"), Some(format!("{}/.tool", home.path().display()).as_str()));
    }
}
