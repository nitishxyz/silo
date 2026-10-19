//! Interactive attach: a local terminal mirror rendered onto the host
//! terminal (row diffs with truecolor SGR), raw stdin forwarded as input.

use std::{fmt::Write as _, io::Write, os::fd::AsFd, time::Duration};

use anyhow::Context;
use silo_client::{Applied, Control, DataStream, Mirror};
use silo_proto::{
    control::{Method, OwnMode},
    data::ClientData,
    model::TermSize,
};
use silo_vt::{Cell, CursorShape, Frame};
use tokio::io::AsyncReadExt;

/// Ctrl-\ detaches.
const DETACH: u8 = 0x1c;

/// Columns and rows of the controlling terminal.
pub fn host_size() -> Option<(u16, u16)> {
    let size = rustix::termios::tcgetwinsize(std::io::stdout().as_fd()).ok()?;
    (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

struct RawMode {
    saved: rustix::termios::Termios,
}

impl RawMode {
    fn enter() -> anyhow::Result<Self> {
        let stdin = std::io::stdin();
        let saved = rustix::termios::tcgetattr(stdin.as_fd()).context("tcgetattr")?;
        let mut raw = saved.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(stdin.as_fd(), rustix::termios::OptionalActions::Flush, &raw)?;
        // Alt screen, clear, keep the host cursor hidden until drawn.
        print!("\x1b[?1049h\x1b[H\x1b[2J");
        let _ = std::io::stdout().flush();
        Ok(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        print!("\x1b[0m\x1b[0 q\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        let _ = rustix::termios::tcsetattr(std::io::stdin().as_fd(), rustix::termios::OptionalActions::Flush, &self.saved);
    }
}

/// Attach until detach, exit or a lost server; a server that stops
/// answering pings ends it with exit code 1.
pub async fn run(control: &Control, block_id: &str, watch: bool) -> anyhow::Result<i32> {
    // Decided before reporting, so the host terminal is restored first.
    let outcome = tokio::select! {
        code = session(control, block_id, watch) => Some(code),
        () = control.clone().heartbeat() => None,
    };
    outcome.unwrap_or_else(|| {
        eprintln!("silo: server not responding");
        Ok(1)
    })
}

async fn session(control: &Control, block_id: &str, watch: bool) -> anyhow::Result<i32> {
    let mode = if watch { OwnMode::Request } else { OwnMode::Take };
    let report_size = |control: &Control| {
        let control = control.clone();
        let block_id = block_id.to_owned();
        async move {
            if let Some((columns, rows)) = host_size() {
                let _ = control.call_value(Method::TerminalResize { block_id, size: TermSize::new(columns, rows) }).await;
            }
        }
    };
    report_size(control).await;
    control.call_value(Method::OwnBlock { block_ids: vec![block_id.to_owned()], mode }).await?;

    let mut mirror = Mirror::new();
    let mut stream = DataStream::open(control, block_id, None).await?;
    mirror.begin(&stream.welcome)?;

    let raw = RawMode::enter()?;
    let mut renderer = Renderer::default();
    let mut stdin = tokio::io::stdin();
    let mut input = [0u8; 4096];
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut tick = tokio::time::interval(Duration::from_millis(8));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut dirty = true;

    let outcome = loop {
        tokio::select! {
            data = stream.rx.recv() => {
                let Some(data) = data else { break Outcome::Lost };
                match mirror.apply(data) {
                    Ok(Applied::Exited(code)) => {
                        renderer.draw(&mut mirror);
                        break Outcome::Exited(code);
                    }
                    Ok(Applied::Redraw) => {
                        dirty = true;
                        stream.ack(&mut mirror, stream.rx.is_empty());
                    }
                    Ok(Applied::None) => {}
                    Err(e) => {
                        // Out of sync: re-attach from a fresh snapshot.
                        tracing::debug!("re-sync: {e}");
                        match DataStream::open(control, block_id, None).await {
                            Ok(s) => {
                                stream = s;
                                mirror.begin(&stream.welcome)?;
                            }
                            Err(_) => break Outcome::Lost,
                        }
                    }
                }
            }
            read = stdin.read(&mut input) => {
                let n = match read {
                    Ok(0) | Err(_) => break Outcome::Detached,
                    Ok(n) => n,
                };
                let bytes = &input[..n];
                if let Some(pos) = bytes.iter().position(|b| *b == DETACH) {
                    if pos > 0 {
                        stream.send(&ClientData::Input(bytes[..pos].to_vec()));
                    }
                    break Outcome::Detached;
                }
                if !watch || mirror.exited().is_none() {
                    stream.send(&ClientData::Input(bytes.to_vec()));
                }
            }
            _ = winch.recv() => {
                if let Some((columns, rows)) = host_size() {
                    stream.send(&ClientData::Resize(TermSize::new(columns, rows)));
                }
                renderer.invalidate();
                dirty = true;
            }
            _ = tick.tick(), if dirty => {
                dirty = false;
                renderer.draw(&mut mirror);
            }
        }
    };
    drop(raw);
    let _ = control.call_value(Method::DisownBlock { block_ids: vec![block_id.to_owned()] }).await;
    Ok(match outcome {
        Outcome::Detached => {
            println!("[detached]");
            0
        }
        Outcome::Exited(Some(code)) => {
            println!("[exited {code}]");
            0
        }
        Outcome::Exited(None) => {
            println!("[exited]");
            0
        }
        Outcome::Lost => {
            eprintln!("[lost connection to server]");
            1
        }
    })
}

enum Outcome {
    Detached,
    Exited(Option<i32>),
    Lost,
}

#[derive(Default)]
struct Renderer {
    rows: Vec<String>,
    host: (u16, u16),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: u32,
    bg: Option<u32>,
    bold: bool,
    faint: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

impl Style {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.foreground,
            bg: (cell.background_explicit || cell.selected).then_some(cell.background),
            bold: cell.bold,
            faint: cell.faint,
            italic: cell.italic,
            underline: cell.underline,
            strike: cell.strikethrough,
        }
    }

    fn sgr(self, out: &mut String) {
        out.push_str("\x1b[0");
        if self.bold {
            out.push_str(";1");
        }
        if self.faint {
            out.push_str(";2");
        }
        if self.italic {
            out.push_str(";3");
        }
        if self.underline {
            out.push_str(";4");
        }
        if self.strike {
            out.push_str(";9");
        }
        let _ = write!(out, ";38;2;{};{};{}", (self.fg >> 16) & 0xff, (self.fg >> 8) & 0xff, self.fg & 0xff);
        if let Some(bg) = self.bg {
            let _ = write!(out, ";48;2;{};{};{}", (bg >> 16) & 0xff, (bg >> 8) & 0xff, bg & 0xff);
        }
        out.push('m');
    }
}

impl Renderer {
    fn invalidate(&mut self) {
        self.rows.clear();
    }

    fn draw(&mut self, mirror: &mut Mirror) {
        let Some(vt) = mirror.terminal() else { return };
        let Ok(frame) = vt.frame() else { return };
        let host = host_size().unwrap_or((frame.size.columns, frame.size.rows));
        let mut out = String::new();
        if host != self.host {
            self.host = host;
            self.rows.clear();
            out.push_str("\x1b[0m\x1b[2J");
        }
        out.push_str("\x1b[?25l");
        let columns = usize::from(frame.size.columns).max(1);
        let visible_rows = usize::from(frame.size.rows.min(host.1));
        let visible_cols = usize::from(frame.size.columns.min(host.0));
        self.rows.resize(visible_rows, String::new());
        for (y, cells) in frame.cells.chunks(columns).take(visible_rows).enumerate() {
            let line = render_row(&cells[..visible_cols.min(cells.len())]);
            if self.rows[y] != line {
                let _ = write!(out, "\x1b[{};1H{line}\x1b[0m\x1b[K", y + 1);
                self.rows[y] = line;
            }
        }
        cursor(&frame, host, &mut out);
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
}

fn render_row(cells: &[Cell]) -> String {
    let mut out = String::new();
    let mut style: Option<Style> = None;
    let mut column = 0;
    for cell in cells {
        if cell.width == 0 {
            continue;
        }
        if column + usize::from(cell.width) > cells.len() {
            break;
        }
        let next = Style::of(cell);
        if style != Some(next) {
            next.sgr(&mut out);
            style = Some(next);
        }
        out.push_str(if cell.text.is_empty() { " " } else { &cell.text });
        column += usize::from(cell.width);
    }
    out
}

fn cursor(frame: &Frame, host: (u16, u16), out: &mut String) {
    let c = frame.cursor;
    if !c.visible || !frame.scroll.at_bottom() || c.row >= host.1 || c.column >= host.0 {
        return;
    }
    let shape = match (c.shape, c.blinking) {
        (CursorShape::Block | CursorShape::Hollow, true) => 1,
        (CursorShape::Block | CursorShape::Hollow, false) => 2,
        (CursorShape::Underline, true) => 3,
        (CursorShape::Underline, false) => 4,
        (CursorShape::Bar, true) => 5,
        (CursorShape::Bar, false) => 6,
    };
    let _ = write!(out, "\x1b[{} q\x1b[{};{}H\x1b[?25h", shape, c.row + 1, c.column + 1);
}
