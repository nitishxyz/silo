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
    vt::Frame,
};
use tokio::time::timeout;

/// One measured keystroke, as offsets from sending it.
#[derive(Clone, Copy, Debug)]
pub struct Key {
    /// The mirror first held the expected cell.
    pub echo: Duration,
    /// The cell was first visible, from the overlay or the mirror.
    pub visible: Duration,
    /// The predictor showed it before the echo.
    pub predicted: bool,
    /// It was shown and the mirror disagreed.
    pub wrong: bool,
}

pub struct Session {
    stream: DataStream,
    mirror: Mirror,
    pub predictor: Predictor,
    /// Most cells the overlay ever showed at once.
    pub max_shown: usize,
}

impl Session {
    /// Spawn `command` and settle on its first screen.
    pub async fn open(control: &Control, command: &[&str], predictor: Predictor) -> anyhow::Result<Self> {
        let command = command.iter().map(|s| (*s).to_owned()).collect();
        let spawn = SpawnSpec { command, cwd: Some("/tmp".into()), size: Some(TermSize::new(200, 24)) };
        let method = Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None };
        let created: Created = control.call(method).await?;
        let stream = DataStream::open(control, &created.block_id, None).await?;
        let mut session = Self { stream, mirror: Mirror::new(), predictor, max_shown: 0 };
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

    /// Apply the next server frame and judge predictions against it.
    async fn recv(&mut self, wait: Duration) -> anyhow::Result<()> {
        let data: ServerData =
            timeout(wait, self.stream.rx.recv()).await.context("timed out waiting for output")?.ok_or_else(|| anyhow!("data stream closed"))?;
        self.predictor.server(&data);
        self.mirror.apply(data)?;
        let next_offset = self.mirror.next_offset();
        if let Some(vt) = self.mirror.terminal() {
            self.predictor.judge(&vt.frame()?, next_offset);
        }
        self.stream.ack(&mut self.mirror, true);
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
        let sent = Instant::now();
        self.stream.send(&ClientData::Input(vec![byte]));
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
                let echo = sent.elapsed();
                return Ok(Key { echo, visible: shown.unwrap_or(echo), predicted: shown.is_some(), wrong });
            }
            let left = (sent + crate::bench::T).saturating_duration_since(Instant::now());
            self.recv(left).await?;
        }
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
        self.predictor.input(b"\r", &frame);
        self.settle(Duration::from_millis(300)).await
    }

    /// Kill the line and send EOF (ends `cat`, `zsh -f` and `read`).
    pub fn close(&self) {
        self.stream.send(&ClientData::Input(b"\x15\x04".to_vec()));
    }
}
