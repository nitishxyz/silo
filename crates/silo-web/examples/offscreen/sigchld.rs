//! Tokio must keep seeing its children exit with CEF up: Chromium installs SIGCHLD handlers
//! (`Runtime::init` keeps tokio's). Checked at each stage: the handler is still the one tokio
//! installed, and a tokio child is seen exiting at once.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

pub struct Probe {
    rt: tokio::runtime::Runtime,
    handler: libc::sighandler_t,
}

fn handler() -> libc::sighandler_t {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: reads the current action into a zeroed `sigaction`.
    unsafe {
        libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr());
        action.assume_init().sa_sigaction
    }
}

impl Probe {
    /// Before `Runtime::init`, like the app: a tokio child has already run (the updater's
    /// `codesign` at launch), so tokio's handler is in place.
    pub fn new() -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()?;
        let probe = Self { rt, handler: libc::SIG_DFL };
        probe.child()?;
        Ok(Self { handler: handler(), ..probe })
    }

    fn child(&self) -> Result<Duration> {
        let started = Instant::now();
        let status =
            self.rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), tokio::process::Command::new("/usr/bin/true").status()).await });
        let took = started.elapsed();
        match status {
            Ok(Ok(status)) if status.success() && took < Duration::from_secs(1) => Ok(took),
            Ok(Ok(status)) => bail!("/usr/bin/true: {status} after {took:?} (SIGCHLD lost?)"),
            Ok(Err(e)) => bail!("/usr/bin/true: {e}"),
            Err(_) => bail!("/usr/bin/true not seen exiting in 5s"),
        }
    }

    pub fn check(&self, stage: &str) -> Result<()> {
        let now = handler();
        if now != self.handler {
            bail!("{stage}: SIGCHLD handler replaced ({:#x}, tokio's {:#x})", now, self.handler);
        }
        let took = self.child()?;
        println!("offscreen: SIGCHLD {stage}: tokio's handler, a child exits in {took:?}");
        Ok(())
    }
}
