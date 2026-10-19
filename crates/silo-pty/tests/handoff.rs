use std::{
    os::{
        fd::{AsFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::ExitStatusExt},
    },
    sync::mpsc,
    time::{Duration, Instant},
};

use silo_pty::{Pty, PtyEvent, PtySize, SpawnConfig, handoff};

const SIZE: PtySize = PtySize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
const LOOP: &str = "stty -echo; echo ready; while read x; do echo got:$x; done";

fn shell(script: &str, on_exit: impl FnOnce(std::process::ExitStatus) + Send + 'static) -> Pty {
    let config = SpawnConfig {
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        ..Default::default()
    };
    Pty::spawn(&config, SIZE, on_exit).unwrap()
}

fn io(pty: &mut Pty) -> (silo_pty::PtyInput, mpsc::Receiver<PtyEvent>) {
    let (tx, rx) = mpsc::channel();
    let input = pty
        .start_io("test", move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
    (input, rx)
}

fn wait_for(rx: &mpsc::Receiver<PtyEvent>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    while !text.contains(needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(PtyEvent::Output(bytes)) => text.push_str(&String::from_utf8_lossy(&bytes)),
            other => panic!("waiting for {needle:?}: {other:?}, got {text:?}"),
        }
    }
    text
}

#[test]
fn detach_then_adopt_keeps_shell_running() {
    let mut pty = shell(LOOP, |_| panic!("exit must not be reported after detach"));
    let (input, rx) = io(&mut pty);
    wait_for(&rx, "ready");
    assert!(input.send(b"one\n".to_vec()));
    wait_for(&rx, "got:one");

    let detached = pty.detach();
    assert_eq!(detached.size, SIZE);
    // Written while nobody owns the PTY; the output waits in the master.
    rustix::io::write(&detached.master, b"two\n").unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let (exit_tx, exit_rx) = mpsc::channel();
    let mut adopted = Pty::adopt(detached.master, detached.pid, detached.size, move |code| {
        let _ = exit_tx.send(code);
    })
    .unwrap();
    let (input, rx) = io(&mut adopted);
    wait_for(&rx, "got:two");
    assert!(input.send(b"three\n".to_vec()));
    wait_for(&rx, "got:three");

    drop(adopted);
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(detached.pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "shell survived drop of adopted pty");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(exit_rx.try_recv().is_err(), "drop must not report exit");
}

#[test]
fn master_sent_over_socket_still_drives_shell() {
    let mut pty = shell(LOOP, |_| {});
    let (_input, rx) = io(&mut pty);
    wait_for(&rx, "ready");
    let detached = pty.detach();
    let (a, b) = UnixStream::pair().unwrap();
    let receiver = std::thread::spawn(move || {
        handoff::check_peer_uid(&b).unwrap();
        let (payload, mut fds) = handoff::recv_fds(&b, 4, 64).unwrap();
        assert_eq!(payload, b"pty:1");
        assert_eq!(fds.len(), 1);
        let flags = rustix::io::fcntl_getfd(&fds[0]).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        fds.pop().unwrap()
    });
    handoff::send_fds(&a, b"pty:1", &[detached.master.as_fd()]).unwrap();
    drop(detached.master);
    let master = receiver.join().unwrap();

    let mut adopted = Pty::adopt(master, detached.pid, detached.size, |_| {}).unwrap();
    let (input, rx) = io(&mut adopted);
    assert!(input.send(b"moved\n".to_vec()));
    wait_for(&rx, "got:moved");
}

#[test]
fn adopted_pty_reports_unknown_exit() {
    let mut pty = shell("stty -echo; echo ready; read x; exit 7", |_| {});
    let (_input, rx) = io(&mut pty);
    wait_for(&rx, "ready");
    let detached = pty.detach();
    let (tx, exit_rx) = mpsc::channel();
    let mut adopted = Pty::adopt(detached.master, detached.pid, detached.size, move |code| {
        let _ = tx.send(code);
    })
    .unwrap();
    let (input, _rx) = io(&mut adopted);
    assert!(input.send(b"bye\n".to_vec()));
    assert_eq!(exit_rx.recv_timeout(Duration::from_secs(5)).unwrap(), None);
    assert!(adopted.exited());
    assert!(adopted.exit_status().is_none());
}

#[test]
fn failed_adopt_hands_master_back() {
    let mut pty = shell(LOOP, |_| {});
    let (_input, rx) = io(&mut pty);
    wait_for(&rx, "ready");
    let detached = pty.detach();
    let pid = detached.pid;

    let err = Pty::adopt(detached.master, 0, detached.size, |_| {}).err().expect("pid 0 must fail");
    assert_eq!(err.error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(err.detached.pid, 0);

    // A non-terminal descriptor fails too, and the pipe comes back open.
    let (r, w) = rustix::pipe::pipe().unwrap();
    let back = Pty::adopt(r, pid, SIZE, |_| {}).err().expect("pipe is not a pty");
    rustix::io::write(&w, b"x").unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(rustix::io::read(&back.detached.master, &mut byte).unwrap(), 1);

    // The shell survived the failed attempt; the returned master still works.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "shell was hung up");
    let detached = silo_pty::Detached { pid, ..err.detached };
    let (tx, rx) = mpsc::channel();
    let (_adopted, input) = Pty::adopt_with_io(detached, "test", |_| {}, move |event| {
        let _ = tx.send(event);
    })
    .unwrap();
    assert!(input.send(b"alive\n".to_vec()));
    wait_for(&rx, "got:alive");
}

#[test]
fn truncation_is_an_error() {
    let (a, b) = UnixStream::pair().unwrap();
    let (r, w) = rustix::pipe::pipe().unwrap();
    handoff::send_fds(&a, b"x", &[r.as_fd(), w.as_fd(), r.as_fd()]).unwrap();
    let err = handoff::recv_fds(&b, 1, 16).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");

    // Datagram sockets report payload truncation.
    let mut pair = [0; 2];
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, pair.as_mut_ptr()) }, 0);
    let (a, b) = unsafe { (UnixStream::from(OwnedFd::from_raw_fd(pair[0])), UnixStream::from(OwnedFd::from_raw_fd(pair[1]))) };
    handoff::send_fds(&a, b"0123456789", &[r.as_fd()]).unwrap();
    let err = handoff::recv_fds(&b, 4, 4).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");

    assert!(handoff::send_fds(&a, b"", &[]).is_err());
}

#[test]
fn dropping_a_normal_pty_hangs_up() {
    let (tx, rx) = mpsc::channel();
    let mut pty = shell("echo ready; sleep 30", move |status| {
        let _ = tx.send(status);
    });
    let (_input, events) = io(&mut pty);
    wait_for(&events, "ready");
    drop(pty);
    let status = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(status.signal(), Some(libc::SIGHUP), "{status:?}");
}
