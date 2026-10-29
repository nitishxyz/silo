//! Tiny HTTP server standing in for a tunnel's local proxy and for a plain origin. It answers every
//! request (absolute-form proxy requests included) with `html`, a 1x1 PNG for `*favicon*`, a
//! second of silent WAV for `*.wav`, an attachment `report.txt` for `/file*`, or a
//! websocket that sends one `hello` frame. `CONNECT` is accepted and the tunnelled request is
//! served on the same stream. Request lines are recorded.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct TestServer {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<String>>>,
}

const ICON: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// A full-viewport page with background `rgb`. On `localhost` it opens a websocket to its own
/// origin and turns green on the first message.
pub fn page(rgb: &str) -> String {
    format!(
        "<html><body style='margin:0;height:100vh;background:rgb({rgb})'><script>\
         if (location.hostname == 'localhost') new WebSocket('ws://' + location.host + '/ws')\
         .onmessage = () => document.body.style.background = 'rgb(0,255,0)';</script></body></html>"
    )
}

/// Listens on `127.0.0.1:<random>`. With `proxy_auth = Some("user:pass")` every proxy request
/// (`CONNECT` or absolute-form) without that Basic `Proxy-Authorization` gets a 407.
pub fn spawn(html: String, proxy_auth: Option<&str>) -> std::io::Result<TestServer> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    let expected = proxy_auth.map(|auth| format!("Basic {}", data_encoding::BASE64.encode(auth.as_bytes())));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (html, log, expected) = (html.clone(), Arc::clone(&log), expected.clone());
            std::thread::spawn(move || serve(stream, &html, &log, expected.as_deref()));
        }
    });
    Ok(TestServer { port, requests })
}

/// Logs each request line, prefixed `[proxy-auth]`/`[bad-proxy-auth]`/`[auth]` when it carried
/// the matching credential header. `/private` answers a (non-proxy) 401 Basic challenge.
fn serve(stream: TcpStream, html: &str, log: &Mutex<Vec<String>>, expected: Option<&str>) -> std::io::Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut tunnelled = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let (mut ws_key, mut proxy_auth, mut auth) = (None, None, false);
        let mut header = String::new();
        while reader.read_line(&mut header)? > 2 {
            if let Some((name, value)) = header.split_once(':') {
                let name = name.to_ascii_lowercase();
                match name.as_str() {
                    "sec-websocket-key" => ws_key = Some(value.trim().to_owned()),
                    "proxy-authorization" => proxy_auth = Some(value.trim().to_owned()),
                    "authorization" => auth = true,
                    _ => {}
                }
            }
            header.clear();
        }
        let tag = match (&proxy_auth, auth) {
            (Some(got), _) if Some(got.as_str()) == expected => "[proxy-auth] ",
            (Some(_), _) => "[bad-proxy-auth] ",
            (None, true) => "[auth] ",
            (None, false) => "",
        };
        log.lock().unwrap().push(format!("{tag}{}", line.trim_end()));
        if let Some(expected) = expected
            && !tunnelled
            && proxy_auth.as_deref() != Some(expected)
        {
            return (&stream).write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"silo\"\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
        if line.starts_with("CONNECT ") {
            (&stream).write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
            tunnelled = true;
            continue;
        }
        if let Some(key) = ws_key {
            return websocket(&stream, &key);
        }
        if line.contains("/private ") {
            return (&stream).write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"origin\"\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
        let mut extra = "";
        let (kind, body) = if line.contains("favicon") {
            ("image/png", data_encoding::BASE64.decode(ICON.as_bytes()).unwrap())
        } else if line.contains(".wav ") {
            ("audio/wav", wav())
        } else if line.contains(" /file") {
            extra = "Content-Disposition: attachment; filename=\"report.txt\"\r\n";
            ("application/octet-stream", DOWNLOAD.to_vec())
        } else {
            ("text/html", html.as_bytes().to_vec())
        };
        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        return (&stream).write_all(&[head.as_bytes(), &body].concat());
    }
}

/// The body of `/file*` downloads.
pub const DOWNLOAD: &[u8] = b"silo download\n";

/// Two seconds of silent 8 kHz 8-bit mono PCM.
fn wav() -> Vec<u8> {
    let samples = 16_000u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + samples).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&8000u32.to_le_bytes());
    out.extend_from_slice(&8000u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&samples.to_le_bytes());
    out.resize(out.len() + samples as usize, 0x80);
    out
}

/// Completes the upgrade, sends one unmasked text frame `hello`, then holds the socket briefly.
fn websocket(mut stream: &TcpStream, key: &str) -> std::io::Result<()> {
    let digest = sha1_smol::Sha1::from(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11")).digest();
    let accept = data_encoding::BASE64.encode(&digest.bytes());
    write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")?;
    stream.write_all(&[0x81, 5])?;
    stream.write_all(b"hello")?;
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
}
