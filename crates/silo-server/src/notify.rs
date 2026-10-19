//! Minimal `sd_notify(3)`: one datagram to `$NOTIFY_SOCKET`, if set.
//! A leading `@` names a Linux abstract socket.

use std::{io, path::Path};

/// Send `state` (e.g. `READY=1`) to the service manager. A no-op without
/// `NOTIFY_SOCKET`; failures are logged, never fatal.
pub fn notify(state: &str) {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET").filter(|s| !s.is_empty()) else {
        return;
    };
    if let Err(error) = send(Path::new(&socket), state) {
        tracing::warn!("sd_notify {state:?}: {error}");
    }
}

fn send(socket: &Path, state: &str) -> io::Result<()> {
    let sock = std::os::unix::net::UnixDatagram::unbound()?;
    let bytes = socket.as_os_str().as_encoded_bytes();
    if let Some(name) = bytes.strip_prefix(b"@") {
        return send_abstract(&sock, name, state);
    }
    sock.send_to(state.as_bytes(), socket).map(drop)
}

#[cfg(target_os = "linux")]
fn send_abstract(sock: &std::os::unix::net::UnixDatagram, name: &[u8], state: &str) -> io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
    sock.send_to_addr(state.as_bytes(), &addr).map(drop)
}

#[cfg(not(target_os = "linux"))]
fn send_abstract(_: &std::os::unix::net::UnixDatagram, _: &[u8], _: &str) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "abstract sockets are Linux-only"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixDatagram;

    use super::send;

    fn recv(sock: &UnixDatagram) -> String {
        sock.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut buf = [0; 256];
        let n = sock.recv(&mut buf).unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    #[test]
    fn sends_to_a_path_socket() {
        let path = std::path::PathBuf::from(format!("/tmp/silo-notify-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixDatagram::bind(&path).unwrap();
        send(&path, "MAINPID=42\nREADY=1").unwrap();
        assert_eq!(recv(&listener), "MAINPID=42\nREADY=1");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sends_to_an_abstract_socket() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("silo-notify-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixDatagram::bind_addr(&addr).unwrap();
        send(std::path::Path::new(&format!("@{name}")), "READY=1").unwrap();
        assert_eq!(recv(&listener), "READY=1");
    }
}
