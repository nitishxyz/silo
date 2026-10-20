//! One bench session: a data stream, its mirror and a real `Predictor`,
//! driven key by key with `key→pred` / `key→echo` stamps.

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use silo_client::{
    Control, DataStream, Mirror, Predictor,
    proto::{
        control::{Created, Method, SpawnSpec},
        data::{ClientData, ServerData},
        model::{BlockKind, TermSize},
    },
    transport::{Conn, RemoteConn},
    vt::Frame,
};
use tokio::time::timeout;

/// One measured keystroke, as offsets from sending it.
#[derive(Clone, Debug)]
pub struct Key {
    /// The mirror first held the expected cell.
    pub echo: Duration,
    /// The cell was first visible, from the overlay or the mirror.
    pub visible: Duration,
    /// The predictor showed it before the echo.
    pub predicted: bool,
    /// It was shown and the mirror disagreed.
    pub wrong: bool,
    /// When each of [`crate::stages::STAGES`] was first reached.
    pub stages: Vec<Option<Duration>>,
}

pub struct Session {
    stream: DataStream,
    /// The connection whose path RTT the predictor follows (remote only).
    path: Option<RemoteConn>,
    mirror: Mirror,
    pub predictor: Predictor,
    /// Most cells the overlay ever showed at once.
    pub max_shown: usize,
    /// Server frames received, for [`TypedAhead::log`].
    frames: Vec<String>,
    /// The frame painted and the mirror offset it shows.
    painted: Option<(Frame, u64)>,
}

impl Session {
    /// Spawn `command` and settle on its first screen.
    pub async fn open(control: &Control, command: &[&str], predictor: Predictor) -> anyhow::Result<Self> {
        let command = command.iter().map(|s| (*s).to_owned()).collect();
        let spawn = SpawnSpec { command, cwd: Some("/tmp".into()), size: Some(TermSize::new(200, 24)) };
        let method = Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None };
        let created: Created = control.call(method).await?;
        let stream = DataStream::open(control, &created.block_id, None).await?;
        let path = match control.conn() {
            Conn::Iroh(conn) => Some(conn.clone()),
            Conn::Unix(_) => None,
        };
        let mut session = Self { stream, path, mirror: Mirror::new(), predictor, max_shown: 0, frames: Vec::new(), painted: None };
        session.mirror.begin(&session.stream.welcome)?;
        session.predictor.begin(&session.stream.welcome);
        session.recv(crate::bench::T).await?;
        session.settle(Duration::from_millis(300)).await?;
        Ok(session)
    }

    /// Apply frames until the stream is quiet for `quiet`.
    pub async fn settle(&mut self, quiet: Duration) -> anyhow::Result<()> {
        while let Ok(applied) = timeout(quiet, self.recv(crate::bench::T)).await {
            applied?;
        }
        Ok(())
    }

    pub fn frame(&mut self) -> anyhow::Result<Frame> {
        Ok(self.mirror.terminal().context("no terminal")?.frame()?)
    }

    /// Apply the next server frame, then paint and judge as the app does.
    async fn recv(&mut self, wait: Duration) -> anyhow::Result<()> {
        let data: ServerData =
            timeout(wait, self.stream.rx.recv()).await.context("timed out waiting for output")?.ok_or_else(|| anyhow!("data stream closed"))?;
        match &data {
            ServerData::Output { bytes, .. } => self.frames.push(format!("out {:?}", String::from_utf8_lossy(bytes))),
            ServerData::InputAck { .. } => self.frames.push(format!("{data:?}")),
            _ => {}
        }
        self.predictor.server(&data);
        self.mirror.apply(data)?;
        self.paint()?;
        self.stream.ack(&mut self.mirror, true);
        Ok(())
    }

    /// Paint the mirror's frame unless the predictor holds it back, and
    /// judge against what is painted (`Pane::present`).
    fn paint(&mut self) -> anyhow::Result<()> {
        let Some(vt) = self.mirror.terminal() else { return Ok(()) };
        let frame = vt.frame()?;
        if self.predictor.present(&frame) {
            self.painted = Some((frame, self.mirror.next_offset()));
        }
        if let Some((frame, offset)) = &self.painted {
            self.predictor.judge(frame, *offset);
        }
        Ok(())
    }
}

impl Session {
    /// Send one key (a printable byte or backspace) and wait for its echo.
    pub async fn key(&mut self, byte: u8) -> anyhow::Result<Key> {
        let frame = self.frame()?;
        let (row, column) = self.predictor.overlay().1.unwrap_or((frame.cursor.row, frame.cursor.column));
        let (column, text) = match byte {
            0x7f | 0x08 => (column.saturating_sub(1), String::new()),
            _ => (column, char::from(byte).to_string()),
        };
        let index = usize::from(row) * usize::from(frame.size.columns) + usize::from(column);
        let mark = crate::stages::mark();
        let sent = Instant::now();
        self.stream.send(&ClientData::Input(vec![byte]));
        self.follow_path();
        self.predictor.input(&[byte], &frame);
        let (mut shown, mut wrong) = (None, false);
        loop {
            let overlay = self.predictor.overlay().0;
            self.max_shown = self.max_shown.max(overlay.len());
            let in_overlay = overlay.iter().any(|c| (c.row, c.column) == (row, column) && c.text == text);
            let frame = self.frame()?;
            let actual = frame.cells.get(index).map_or("", |c| c.text.as_str());
            let echoed = if text.is_empty() { actual.trim().is_empty() } else { actual == text };
            if in_overlay && shown.is_none() {
                shown = Some(sent.elapsed());
            }
            wrong |= shown.is_some() && !in_overlay && !echoed;
            if echoed {
                let (echo, now) = (sent.elapsed(), Instant::now());
                let stages = crate::stages::since(mark, sent, now, crate::stages::STAGES);
                return Ok(Key { echo, visible: shown.unwrap_or(echo), predicted: shown.is_some(), wrong, stages });
            }
            let left = (sent + crate::bench::T).saturating_duration_since(Instant::now());
            self.recv(left).await?;
        }
    }
}

/// What a typist saw while typing ahead of the echo ([`Session::type_ahead`]).
#[derive(Clone, Debug, Default)]
pub struct TypedAhead {
    /// Painted states (mirror plus overlay) after each server frame.
    pub states: usize,
    /// States that showed fewer typed characters, or the cursor further
    /// left, than the state before: visible flicker.
    pub regressions: Vec<String>,
    /// The final painted line equals the typed text.
    pub exact: bool,
    pub log: Vec<String>,
}

impl Session {
    /// Type printable `text` one key every `gaps` (uniform) without
    /// waiting for echoes, as a person does, and watch every painted state.
    pub async fn type_ahead(&mut self, text: &[u8], gaps: (Duration, Duration)) -> anyhow::Result<TypedAhead> {
        let frame = self.frame()?;
        let (row, start) = (frame.cursor.row, frame.cursor.column);
        let started = Instant::now();
        let mut out = TypedAhead::default();
        let mut last: Option<(usize, u16)> = None;
        let mut typed = 0;
        let mut next_key = Instant::now();
        let settle_until = |typed: usize| typed == text.len();
        loop {
            if typed < text.len() && Instant::now() >= next_key {
                let frame = self.frame()?;
                self.stream.send(&ClientData::Input(vec![text[typed]]));
                self.follow_path();
                self.predictor.input(&[text[typed]], &frame);
                typed += 1;
                next_key += gaps.0 + (gaps.1.saturating_sub(gaps.0)).mul_f64(rand::random::<f64>());
                out.log.push(format!("{:7.1} key {:?}", started.elapsed().as_secs_f64() * 1e3, char::from(text[typed - 1])));
            } else {
                let wait = if settle_until(typed) { Duration::from_millis(400) } else { next_key.saturating_duration_since(Instant::now()) };
                match timeout(wait, self.recv(crate::bench::T)).await {
                    Ok(applied) => {
                        applied?;
                        out.log.extend(self.frames.drain(..).map(|f| format!("        {f}")));
                    }
                    // The app's deadline timer.
                    Err(_) if settle_until(typed) => {
                        self.paint()?;
                        break;
                    }
                    Err(_) => self.paint()?,
                }
            }
            let mirror = self.frame()?.cursor.column;
            let frame = self.painted.as_ref().map(|(frame, _)| frame.clone()).context("nothing painted")?;
            let (cells, cursor) = self.predictor.overlay();
            let painted_cursor = cursor.unwrap_or((frame.cursor.row, frame.cursor.column)).1;
            let cell = |column: u16| {
                cells.iter().find(|c| (c.row, c.column) == (row, column)).map(|c| c.text.clone()).unwrap_or_else(|| {
                    frame
                        .cells
                        .get(usize::from(row) * usize::from(frame.size.columns) + usize::from(column))
                        .map_or_else(String::new, |c| c.text.clone())
                })
            };
            let shown = (0..typed).take_while(|&i| cell(start + i as u16) == char::from(text[i]).to_string()).count();
            out.states += 1;
            let line: String = (0..typed as u16 + 1).map(|i| cell(start + i)).map(|t| if t.is_empty() { " ".into() } else { t }).collect();
            let state = format!(
                "{:7.1} shown {shown}/{typed} cursor {} (mirror {}) overlay {} |{line}|",
                started.elapsed().as_secs_f64() * 1e3,
                painted_cursor.saturating_sub(start),
                mirror.saturating_sub(start),
                cells.len()
            );
            if let Some((was_shown, was_cursor)) = last
                && (shown < was_shown || painted_cursor < was_cursor)
            {
                out.regressions.push(state.clone());
            }
            out.log.push(state);
            last = Some((shown, painted_cursor));
        }
        let frame = self.painted.as_ref().map(|(frame, _)| frame.clone()).context("nothing painted")?;
        let line: String = (0..text.len() as u16)
            .map(|i| frame.cells[usize::from(row) * usize::from(frame.size.columns) + usize::from(start + i)].text.clone())
            .collect();
        out.exact = line.as_bytes() == text && frame.cursor.column == start + text.len() as u16;
        Ok(out)
    }
}

impl Session {
    /// Type `bytes` one by one where nothing echoes, watching the overlay
    /// for `wait` after each; returns whether prediction was ever enabled.
    pub async fn blind(&mut self, bytes: &[u8], wait: Duration) -> anyhow::Result<bool> {
        let mut enabled = false;
        for &byte in bytes {
            let frame = self.frame()?;
            self.stream.send(&ClientData::Input(vec![byte]));
            self.follow_path();
            self.predictor.input(&[byte], &frame);
            let until = Instant::now() + wait;
            loop {
                let frame = self.frame()?;
                enabled |= self.predictor.enabled(&frame);
                self.max_shown = self.max_shown.max(self.predictor.overlay().0.len());
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match timeout(left, self.recv(crate::bench::T)).await {
                    Ok(applied) => applied?,
                    Err(_) => break,
                }
            }
        }
        Ok(enabled)
    }

    /// Send Enter (an epoch boundary) and let the program's output settle.
    pub async fn enter(&mut self) -> anyhow::Result<()> {
        let frame = self.frame()?;
        self.stream.send(&ClientData::Input(b"\r".to_vec()));
        self.follow_path();
        self.predictor.input(b"\r", &frame);
        self.settle(Duration::from_millis(300)).await
    }

    /// Hand the predictor the path RTT, as the app does with every input.
    fn follow_path(&mut self) {
        if let Some(rtt) = self.path.as_ref().and_then(RemoteConn::rtt) {
            self.predictor.path_rtt(rtt);
        }
    }

    /// Kill the line and send EOF (ends `cat`, `zsh -f` and `read`).
    pub fn close(&self) {
        self.stream.send(&ClientData::Input(b"\x15\x04".to_vec()));
    }
}
