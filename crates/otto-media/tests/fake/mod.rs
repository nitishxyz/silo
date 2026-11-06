//! A local HTTP server answering every request through a handler, recording what it got.
#![allow(dead_code)]
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug)]
pub struct Got {
    pub method: String,
    pub path: String,
    /// Lowercased names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Got {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub struct Reply {
    pub status: u16,
    pub mime: &'static str,
    pub body: Vec<u8>,
}

pub fn json(status: u16, value: serde_json::Value) -> Reply {
    Reply { status, mime: "application/json", body: value.to_string().into_bytes() }
}

pub fn bytes(mime: &'static str, body: Vec<u8>) -> Reply {
    Reply { status: 200, mime, body }
}

type Handler = Arc<dyn Fn(&Got, &str) -> Reply + Send + Sync>;

/// Its base URL (`http://127.0.0.1:port`) and what it got so far. The handler gets the base too.
pub async fn serve(handler: impl Fn(&Got, &str) -> Reply + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<Got>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let got: Arc<Mutex<Vec<Got>>> = Arc::default();
    let handler: Handler = Arc::new(handler);
    let (seen, url) = (got.clone(), base.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let (seen, handler, url) = (seen.clone(), handler.clone(), url.clone());
            tokio::spawn(async move {
                let Some(request) = read(&mut socket).await else { return };
                let reply = handler(&request, &url);
                seen.lock().unwrap().push(request);
                let head = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.status,
                    reply.mime,
                    reply.body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&reply.body).await;
            });
        }
    });
    (base, got)
}

async fn read(socket: &mut tokio::net::TcpStream) -> Option<Got> {
    let mut bytes = Vec::new();
    let end = loop {
        let mut buf = [0; 8192];
        let n = socket.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_lowercase(), v.trim().to_owned())).collect();
    let length: usize = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = bytes[end..].to_vec();
    while body.len() < length {
        let mut buf = [0; 65536];
        let n = socket.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    Some(Got { method, path, headers, body })
}

/// A real 2×1 PNG.
pub fn png() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255])).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

pub fn b64(bytes: &[u8]) -> String {
    otto_tools::attachments::encode(bytes)
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(body.len() + 8).unwrap().to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// A tiny MP4: `ftyp`, then `moov` with an `mvhd` (8 s) and a 1280×720 video `tkhd`.
pub fn mp4() -> Vec<u8> {
    let mut mvhd = vec![0; 12];
    mvhd.extend(1000u32.to_be_bytes());
    mvhd.extend(8000u32.to_be_bytes());
    mvhd.resize(100, 0);
    let mut tkhd = vec![0; 76];
    tkhd.extend((1280u32 << 16).to_be_bytes());
    tkhd.extend((720u32 << 16).to_be_bytes());
    let moov = boxed(b"moov", &[boxed(b"mvhd", &mvhd), boxed(b"trak", &boxed(b"tkhd", &tkhd))].concat());
    [boxed(b"ftyp", b"isom\0\0\0\0isom"), moov].concat()
}

/// A tiny MP3: 383 MPEG 1 Layer III frames at 44.1 kHz, 10.0 s.
pub fn mp3() -> Vec<u8> {
    let mut frame = vec![0xFF, 0xFB, 0x90, 0x00];
    frame.resize(417, 0);
    frame.repeat(383)
}

/// A real 2×1 GIF.
pub fn gif() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(2, 1, image::Rgba([0, 0, 255, 255])).write_to(&mut out, image::ImageFormat::Gif).unwrap();
    out.into_inner()
}
