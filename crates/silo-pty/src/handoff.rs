//! Passing descriptors between processes over unix sockets (`SCM_RIGHTS`).

use std::{
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
};

/// Most descriptors accepted in one message (Linux `SCM_MAX_FD`).
pub const MAX_FDS: usize = 253;

fn cmsg_space(fds: usize) -> usize {
    // SAFETY: CMSG_SPACE is a pure size computation.
    unsafe { libc::CMSG_SPACE((fds * size_of::<RawFd>()) as u32) as usize }
}

/// Send `payload` with `fds` attached. `payload` must be non-empty (a stream
/// socket needs at least one data byte to carry ancillary data) and is sent
/// whole or the call fails.
///
/// # Errors
/// Invalid counts or `sendmsg` failures.
pub fn send_fds(sock: &UnixStream, payload: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    if payload.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "payload must not be empty"));
    }
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many descriptors"));
    }
    // u64 storage keeps the control buffer aligned for cmsghdr.
    let mut control = vec![0u64; cmsg_space(fds.len()).div_ceil(8)];
    let mut iov = libc::iovec { iov_base: payload.as_ptr() as *mut _, iov_len: payload.len() };
    // SAFETY: msghdr is plain data; all pointers stay valid for the call and
    // CMSG_* only touch the control buffer sized by CMSG_SPACE.
    let sent = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = cmsg_space(fds.len()) as _;
            let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN((fds.len() * size_of::<RawFd>()) as u32) as _;
            let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            for (i, fd) in fds.iter().enumerate() {
                data.add(i).write_unaligned(fd.as_raw_fd());
            }
        }
        loop {
            let n = libc::sendmsg(sock.as_raw_fd(), &raw const msg, 0);
            if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break n;
            }
        }
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent as usize != payload.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short sendmsg"));
    }
    Ok(())
}

/// Receive one message of at most `max_payload` bytes and `max_fds`
/// descriptors. Received descriptors are close-on-exec. Truncated payload or
/// control data is an error (any descriptors that did arrive are closed).
///
/// # Errors
/// Truncation, invalid control data, EOF, or `recvmsg` failures.
pub fn recv_fds(sock: &UnixStream, max_fds: usize, max_payload: usize) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    if max_fds > MAX_FDS || max_payload == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid limits"));
    }
    let mut payload = vec![0u8; max_payload];
    let space = cmsg_space(max_fds.max(1));
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec { iov_base: payload.as_mut_ptr().cast(), iov_len: payload.len() };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = 0;
    let mut fds = Vec::new();
    // SAFETY: as in send_fds; descriptors read from SCM_RIGHTS are new and
    // owned by us, so wrapping them in OwnedFd is sound.
    let (n, msg_flags) = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let n = loop {
            let n = libc::recvmsg(sock.as_raw_fd(), &raw mut msg, flags);
            if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break n;
            }
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // On truncation macOS may leave cmsg_len at the untruncated size, so
        // only read descriptors inside the control bytes actually filled.
        let filled = (msg.msg_controllen as usize).min(space);
        let end = control.as_ptr().cast::<u8>().add(filled);
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                let header = libc::CMSG_LEN(0) as usize;
                let claimed = ((*cmsg).cmsg_len as usize).saturating_sub(header) / size_of::<RawFd>();
                let room = (end as usize).saturating_sub(data as usize) / size_of::<RawFd>();
                let count = claimed.min(room);
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
        }
        (n as usize, msg.msg_flags)
    };
    if msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "control data truncated (too many descriptors)"));
    }
    if msg_flags & libc::MSG_TRUNC != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "payload truncated"));
    }
    if fds.len() > max_fds {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too many descriptors"));
    }
    if n == 0 && fds.is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
    }
    for fd in &fds {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    }
    payload.truncate(n);
    Ok((payload, fds))
}

/// Effective uid of the process on the other end of `sock`.
///
/// # Errors
/// Fails if the kernel cannot report peer credentials.
pub fn peer_uid(sock: &UnixStream) -> io::Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: getsockopt writes at most `len` bytes into the zeroed ucred.
        unsafe {
            let mut cred: libc::ucred = std::mem::zeroed();
            let mut len = size_of::<libc::ucred>() as libc::socklen_t;
            let rc = libc::getsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &raw mut len);
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(cred.uid)
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: getpeereid writes two integers.
        if unsafe { libc::getpeereid(sock.as_raw_fd(), &raw mut uid, &raw mut gid) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Fail unless the peer runs as our effective uid.
///
/// # Errors
/// `PermissionDenied` for another uid, or credential lookup failures.
pub fn check_peer_uid(sock: &UnixStream) -> io::Result<()> {
    // SAFETY: geteuid cannot fail.
    let ours = unsafe { libc::geteuid() };
    let theirs = peer_uid(sock)?;
    if theirs == ours { Ok(()) } else { Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("peer uid {theirs} is not {ours}"))) }
}
