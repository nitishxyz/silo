//! Closing busy blocks from the CLI (Phase 7 §7.5, P7-9): `silo kill`,
//! `silo kill-server` and `silo do block.close` refuse without a
//! terminal, close with `--force`, and ask y/N (default no) on one.

use std::{
    io::{Read, Write},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
    time::Duration,
};

use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{BlockView, BusyBlocks, Created, Method, SpawnSpec},
    model::BlockKind,
    paths,
};
use tokio::task::JoinHandle;

const T: Duration = Duration::from_secs(15);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-cli-kill-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config { home: home.to_owned(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    tokio::time::timeout(T, async {
        while !silo_client::server_running(home).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server did not start");
    task
}

/// `silo args…` with stdin closed (not a terminal).
async fn silo(home: &Path, args: &[&str]) -> Output {
    let (home, args) = (home.to_owned(), args.iter().map(|&a| a.to_owned()).collect::<Vec<_>>());
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_silo")).args(&args).env("SILO_HOME", &home).stdin(Stdio::null()).output().unwrap()
    })
    .await
    .unwrap()
}

/// A `silo server` process: `kill-server` waits for the server's exit.
/// SIGTERMed on drop, should a test fail first.
struct ServerProcess(Child);

impl ServerProcess {
    async fn start(home: &Path) -> Self {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_silo"))
            .args(["server", "--idle-exit", "0"])
            .env("SILO_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let server = Self(child);
        tokio::time::timeout(T, async {
            while !silo_client::server_running(home).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server did not start");
        server
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // SAFETY: signalling our own child.
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let _ = self.0.wait();
        }
    }
}

/// `silo args…` on a pseudo-terminal, its controlling terminal (stderr
/// too, unless `quiet`: then to /dev/null): once `prompt` is shown, type
/// `answer`. Returns its exit code and everything shown.
async fn silo_on_a_pty(home: &Path, args: &[&str], quiet: bool, prompt: &'static str, answer: &'static str) -> (i32, String) {
    let (home, args) = (home.to_owned(), args.iter().map(|&a| a.to_owned()).collect::<Vec<_>>());
    tokio::task::spawn_blocking(move || {
        let (mut master, slave) = openpty();
        let stderr = if quiet { Stdio::null() } else { slave.try_clone().unwrap().into() };
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_silo"));
        command.args(&args).env("SILO_HOME", &home).stdin(slave.try_clone().unwrap()).stdout(slave).stderr(stderr);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(|| {
                // A session of its own, with the pty (stdin) as its
                // controlling terminal: what `/dev/tty` opens.
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        // Close our copies of the slave, so the master reads EOF once the
        // child is gone.
        drop(command);
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        while !seen.contains(prompt) {
            match master.read(&mut buf) {
                Ok(0) | Err(_) => panic!("exited before asking: {seen:?}"),
                Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
        master.write_all(answer.as_bytes()).unwrap();
        // Read until the child closes the terminal (EOF, or EIO on
        // macOS) before reaping it: a session leader's exit waits for its
        // terminal's output to drain.
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        (child.wait().unwrap().code().unwrap_or(-1), seen)
    })
    .await
    .unwrap()
}

fn openpty() -> (std::fs::File, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty writes two fresh descriptors we then own.
    let rc = unsafe { libc::openpty(&raw mut master, &raw mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both were just returned by openpty and nothing else owns them.
    unsafe { (std::fs::File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// A session running `sleep 30`, once its block reads as busy and is
/// titled after it (a probe sets the two one after the other), with the
/// block's title.
async fn busy_session(control: &Control) -> (Created, String) {
    let spawn = SpawnSpec { command: vec!["sleep".into(), "30".into()], ..SpawnSpec::default() };
    let created: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let title = tokio::time::timeout(T, async {
        loop {
            let method = Method::BlocksBusy { session_id: Some(created.session_id.clone()), window_id: None, block_ids: None };
            let busy: BusyBlocks = control.call(method).await.unwrap();
            let view: BlockView = control.call(Method::BlockView { block_id: created.block_id.clone() }).await.unwrap();
            if busy.busy.iter().any(|b| b.busy.process == "sleep") && view.block.title == "sleep" {
                return view.block.title;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("sleep reads as busy, and titles its block");
    (created, title)
}

async fn exists(control: &Control, block_id: &str) -> bool {
    control.call::<BlockView>(Method::BlockView { block_id: block_id.into() }).await.is_ok()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_refuses_a_busy_session_without_a_terminal_and_force_closes_it() {
    let home = temp_home("kill");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "test").await.unwrap();
    let (created, title) = busy_session(&control).await;

    let refused = silo(&home, &["kill", &created.session_id]).await;
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(stderr(&refused), format!("silo: sleep is running in '{title}'; pass --force to close it anyway\n"));
    assert!(exists(&control, &created.block_id).await, "kept");

    let forced = silo(&home, &["kill", "--force", &created.session_id]).await;
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(!exists(&control, &created.block_id).await, "closed");

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    let _ = tokio::time::timeout(T, task).await;
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_on_a_terminal_asks_and_keeps_the_session_by_default() {
    let home = temp_home("ask");
    let task = start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "test").await.unwrap();
    let (created, title) = busy_session(&control).await;
    const PROMPT: &str = "Close anyway? [y/N] ";

    let (code, shown) = silo_on_a_pty(&home, &["kill", &created.session_id], false, PROMPT, "\n").await;
    assert_eq!(code, 1, "{shown}");
    assert!(shown.contains(&format!("silo: sleep is running in '{title}'")), "{shown}");
    assert!(exists(&control, &created.block_id).await, "Enter keeps it");

    // The question goes to the terminal even with stderr redirected.
    let (code, shown) = silo_on_a_pty(&home, &["kill", &created.session_id], true, PROMPT, "y\n").await;
    assert_eq!(code, 0, "{shown}");
    assert!(!exists(&control, &created.block_id).await, "y closes it");

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    let _ = tokio::time::timeout(T, task).await;
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_closes_and_kill_server_take_force() {
    let home = temp_home("server");
    let server = ServerProcess::start(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), "test").await.unwrap();
    let (first, title) = busy_session(&control).await;
    let (second, _) = busy_session(&control).await;

    let refused = silo(&home, &["do", "block.close", &first.block_id]).await;
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(stderr(&refused), format!("silo: sleep is running in '{title}'; pass --force to close it anyway\n"));
    let forced = silo(&home, &["do", "block.close", &first.block_id, "--force"]).await;
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(!exists(&control, &first.block_id).await);

    let refused = silo(&home, &["kill-server"]).await;
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).ends_with("; pass --force to close it anyway\n"), "{}", stderr(&refused));
    assert!(exists(&control, &second.block_id).await, "still running");
    // A `silo server` process asks for an approval: on a terminal of
    // its own, the CLI confirms it there.
    let (code, shown) = silo_on_a_pty(&home, &["kill-server", "--force"], false, "Allow? [y/N] ", "y\n").await;
    assert_eq!(code, 0, "{shown}");
    assert!(!shown.contains("Close anyway?"), "forced: {shown}");
    let status = tokio::task::spawn_blocking(move || {
        let mut server = server;
        server.0.wait().unwrap()
    })
    .await
    .unwrap();
    assert!(status.success(), "the server exited: {status}");
    let _ = std::fs::remove_dir_all(home);
}
