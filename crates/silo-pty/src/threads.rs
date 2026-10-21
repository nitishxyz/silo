//! Cancellable reader/writer threads around a PTY master.

use std::{
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    sync::{Arc, mpsc},
    thread::JoinHandle,
};

use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
};

/// Output from the PTY reader thread.
#[derive(Debug)]
pub enum PtyEvent {
    Output(Vec<u8>),
    /// The master hit EOF/EIO (every slave descriptor closed). Not sent when
    /// the threads are stopped by `detach`.
    Closed,
}

enum WriteMsg {
    Data(Vec<u8>),
    Stop,
}

/// Queues input for the PTY writer thread. Cheap to clone.
#[derive(Clone)]
pub struct PtyInput(mpsc::Sender<WriteMsg>);

impl PtyInput {
    /// Queue `bytes`; returns false once the writer stopped.
    pub fn send(&self, bytes: Vec<u8>) -> bool {
        self.0.send(WriteMsg::Data(bytes)).is_ok()
    }
}

pub(crate) struct IoThreads {
    master: Arc<OwnedFd>,
    cancel_rx: OwnedFd,
    cancel_tx: OwnedFd,
    input: mpsc::Sender<WriteMsg>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

/// Waits until `fd` has `flags` ready. Returns false when cancelled.
fn wait(fd: BorrowedFd<'_>, flags: PollFlags, cancel: BorrowedFd<'_>) -> io::Result<bool> {
    loop {
        let mut fds = [PollFd::from_borrowed_fd(fd, flags), PollFd::from_borrowed_fd(cancel, PollFlags::IN)];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        if !fds[1].revents().is_empty() {
            return Ok(false);
        }
        if !fds[0].revents().is_empty() {
            return Ok(true);
        }
    }
}

impl IoThreads {
    pub(crate) fn start(master: Arc<OwnedFd>, name: &str, mut on_event: impl FnMut(PtyEvent) + Send + 'static) -> io::Result<(Self, PtyInput)> {
        let (cancel_rx, cancel_tx) = crate::cloexec::pipe()?;
        let flags = fcntl_getfl(&*master)?;
        fcntl_setfl(&*master, flags | OFlags::NONBLOCK)?;
        let (input, input_rx) = mpsc::channel();

        let (fd, cancel) = (master.clone(), cancel_rx.try_clone()?);
        let reader = std::thread::Builder::new().name(format!("{name}-read")).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match wait(fd.as_fd(), PollFlags::IN, cancel.as_fd()) {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(_) => break,
                }
                match rustix::io::read(&*fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => on_event(PtyEvent::Output(buf[..n].to_vec())),
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                    Err(_) => break,
                }
            }
            on_event(PtyEvent::Closed);
        })?;

        let (fd, cancel) = (master.clone(), cancel_rx.try_clone()?);
        let writer = std::thread::Builder::new().name(format!("{name}-write")).spawn(move || {
            while let Ok(WriteMsg::Data(bytes)) = input_rx.recv() {
                let mut rest = &bytes[..];
                while !rest.is_empty() {
                    match rustix::io::write(&*fd, rest) {
                        Ok(n) => rest = &rest[n..],
                        Err(rustix::io::Errno::INTR) => {}
                        Err(rustix::io::Errno::AGAIN) => match wait(fd.as_fd(), PollFlags::OUT, cancel.as_fd()) {
                            Ok(true) => {}
                            _ => return,
                        },
                        Err(_) => return,
                    }
                }
            }
        })?;
        let threads = Self { master, cancel_rx, cancel_tx, input: input.clone(), reader, writer };
        Ok((threads, PtyInput(input)))
    }

    /// Wake and join both threads, then restore blocking mode on the master.
    /// Input still queued is discarded.
    pub(crate) fn stop(self) {
        let _ = self.input.send(WriteMsg::Stop);
        // The pipe is never drained, so it stays readable for both polls.
        let _ = rustix::io::write(&self.cancel_tx, &[1]);
        let _ = self.reader.join();
        let _ = self.writer.join();
        drop(self.cancel_rx);
        if let Ok(flags) = fcntl_getfl(&*self.master) {
            let _ = fcntl_setfl(&*self.master, flags - OFlags::NONBLOCK);
        }
    }
}
