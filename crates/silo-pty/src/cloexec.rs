//! Close-on-exec descriptors without a window for a fork to inherit them.
//!
//! Linux creates them atomically (`pipe2`, `O_CLOEXEC`,
//! `MSG_CMSG_CLOEXEC`). macOS has no `pipe2` or `MSG_CMSG_CLOEXEC`, so
//! there a descriptor is created and marked close-on-exec under the shared
//! side of [`FORK`], and [`crate::Pty::spawn`] forks its PTY host under the
//! exclusive side: a host never inherits another PTY's pipe or a descriptor
//! received for a handoff.

use std::{io, os::fd::OwnedFd, sync::RwLock};

static FORK: RwLock<()> = RwLock::new(());

/// Held while creating descriptors that are not close-on-exec at once.
#[cfg(not(target_os = "linux"))]
pub(crate) fn creating() -> std::sync::RwLockReadGuard<'static, ()> {
    FORK.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Held while forking a process that inherits descriptors.
pub(crate) fn forking() -> std::sync::RwLockWriteGuard<'static, ()> {
    FORK.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A close-on-exec pipe: `(read, write)`.
pub(crate) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(target_os = "linux")]
    {
        Ok(rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _creating = creating();
        let (read, write) = rustix::pipe::pipe()?;
        for fd in [&read, &write] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
        }
        Ok((read, write))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_is_close_on_exec() {
        let (read, write) = pipe().unwrap();
        for fd in [&read, &write] {
            assert!(rustix::io::fcntl_getfd(fd).unwrap().contains(rustix::io::FdFlags::CLOEXEC));
        }
    }
}
