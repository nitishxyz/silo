//! Tiny HTTP server standing in for a tunnel's local proxy and for a plain origin: answers every
//! request (absolute-form proxy requests included) with `html`, or a 1x1 PNG for `*favicon*`, and
//! records the request lines.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub struct TestServer {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<String>>>,
}

const ICON: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// A full-viewport page with background `rgb`.
pub fn page(rgb: &str) -> String {
    format!("<html><body style='margin:0;height:100vh;background:rgb({rgb})'></body></html>")
}

/// Listens on `127.0.0.1:<random>`.
pub fn spawn(html: String) -> std::io::Result<TestServer> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut header = String::new();
            while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                header.clear();
            }
            log.lock().unwrap().push(line.trim_end().to_owned());
            let (kind, body) = if line.contains("favicon") {
                (
                    "image/png",
                    data_encoding::BASE64.decode(ICON.as_bytes()).unwrap(),
                )
            } else {
                ("text/html", html.clone().into_bytes())
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = (&stream).write_all(&[head.as_bytes(), &body].concat());
        }
    });
    Ok(TestServer { port, requests })
}
