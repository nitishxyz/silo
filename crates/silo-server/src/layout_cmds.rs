//! Server-side layout commands and `server.restart`/`server.log`
//! (Phase 7 §4.1): the math the app does locally, over the same
//! [`silo_proto::layout_ops`].

use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use serde_json::{Value, json};
use silo_proto::{
    api::method_name,
    control::{Direction, LogText, Method, RpcError},
    layout_ops::{self, RatioChange},
    model::BlockId,
};

use crate::{Server, registry::Registry};

/// `server.log`'s default and largest line counts.
const LOG_LINES: u32 = 200;
const MAX_LOG_LINES: u32 = 10_000;
/// How much of the log's end `server.log` reads at most.
const LOG_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// Set by `server.restart`: once the server has stopped, the process
/// starts its binary again (see [`restart_requested`]).
static RESTART: AtomicBool = AtomicBool::new(false);

/// Whether a `server.restart` stopped the server in this process.
pub fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

pub async fn handle(server: &Server, _client_id: u64, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::WindowStep { session_id, delta } => window_step(&mut server.registry.lock().unwrap(), &session_id, delta)?,
        Method::BlockFocusStep { block_id, delta } => focus_step(&mut server.registry.lock().unwrap(), &block_id, delta)?,
        Method::BlockFocusDir { block_id, direction } => focus_dir(&mut server.registry.lock().unwrap(), &block_id, direction)?,
        Method::LayoutGrow { block_id, delta } => {
            if !delta.is_finite() || delta.abs() >= 1.0 {
                return Err(RpcError::bad_request("delta must be between -1 and 1"));
            }
            resize(&mut server.registry.lock().unwrap(), &block_id, |layout| {
                layout_ops::resize_share(layout, &block_id, delta).into_iter().collect()
            })?;
        }
        Method::LayoutEdge { block_id, direction } => {
            resize(&mut server.registry.lock().unwrap(), &block_id, |layout| {
                layout_ops::move_edge(layout, &block_id, direction).into_iter().collect()
            })?;
        }
        Method::LayoutEqualize { window_id } => equalize(&mut server.registry.lock().unwrap(), &window_id)?,
        // The control loop stops the server once this is answered.
        Method::ServerRestart { .. } => {
            tracing::info!("restart requested");
            RESTART.store(true, Ordering::SeqCst);
        }
        Method::ServerLog { lines } => {
            let lines = lines.unwrap_or(LOG_LINES).clamp(1, MAX_LOG_LINES);
            let path = silo_proto::paths::log(&server.home);
            let text = tokio::task::spawn_blocking(move || tail(&path, lines as usize)).await.map_err(RpcError::internal)?;
            return serde_json::to_value(LogText { text: text.map_err(RpcError::internal)? }).map_err(RpcError::internal);
        }
        other => return Err(RpcError::method_not_found(&method_name(&other))),
    }
    Ok(json!({}))
}

/// The last `lines` lines of the file at `path` (none if it is missing).
fn tail(path: &Path, lines: usize) -> std::io::Result<String> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // A read from the middle starts mid-line: drop that partial line.
    let text = if start > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    let body = text.strip_suffix('\n').unwrap_or(text);
    let kept: Vec<&str> = body.rsplit('\n').take(lines).collect();
    let mut out = kept.into_iter().rev().collect::<Vec<_>>().join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Indexes of the session and window holding block `block`.
fn locate(reg: &Registry, block: &str) -> Result<(usize, usize), RpcError> {
    reg.sessions
        .iter()
        .enumerate()
        .find_map(|(si, s)| s.windows.iter().position(|w| w.contains(block)).map(|wi| (si, wi)))
        .ok_or_else(|| RpcError::not_found(format!("block {block}")))
}

fn window_step(reg: &mut Registry, session: &str, delta: i32) -> Result<(), RpcError> {
    let sid = reg.session_id(session).ok_or_else(|| RpcError::not_found(format!("session {session}")))?;
    let s = reg.sessions.iter().find(|s| s.id == sid).ok_or_else(|| RpcError::not_found(format!("session {session}")))?;
    let current = s.windows.iter().position(|w| w.id == s.active_window).unwrap_or(0);
    let next = layout_ops::step(s.windows.len(), current, delta);
    if next == current {
        return Ok(());
    }
    let window = s.windows[next].id.clone();
    reg.select_window(&sid, &window)
}

fn focus_step(reg: &mut Registry, block: &str, delta: i32) -> Result<(), RpcError> {
    let (si, wi) = locate(reg, block)?;
    let blocks = reg.sessions[si].windows[wi].blocks();
    let current = blocks.iter().position(|b| b == block).unwrap_or(0);
    let next = blocks[layout_ops::step(blocks.len(), current, delta)].clone();
    reg.focus(&next)
}

fn focus_dir(reg: &mut Registry, block: &str, direction: Direction) -> Result<(), RpcError> {
    let (si, wi) = locate(reg, block)?;
    let next: Option<BlockId> = layout_ops::neighbour_in(&reg.sessions[si].windows[wi].layout, block, direction);
    match next {
        Some(next) => reg.focus(&next),
        None => Ok(()),
    }
}

/// Apply `changes` of the layout holding `block`, unless its window is
/// zoomed (as in the app).
fn resize(reg: &mut Registry, block: &str, changes: impl FnOnce(&silo_proto::model::Layout) -> Vec<RatioChange>) -> Result<(), RpcError> {
    let (si, wi) = locate(reg, block)?;
    let window = &reg.sessions[si].windows[wi];
    if window.zoomed {
        return Ok(());
    }
    let id = window.id.clone();
    apply(reg, &id, changes(&window.layout))
}

fn equalize(reg: &mut Registry, window: &str) -> Result<(), RpcError> {
    let layout = reg
        .sessions
        .iter()
        .flat_map(|s| &s.windows)
        .find(|w| w.id == window)
        .map(|w| w.layout.clone())
        .ok_or_else(|| RpcError::not_found(format!("window {window}")))?;
    apply(reg, window, layout_ops::equalize(&layout))
}

fn apply(reg: &mut Registry, window: &str, changes: Vec<RatioChange>) -> Result<(), RpcError> {
    for (path, ratio) in changes {
        reg.set_ratio(window, &path, ratio)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use silo_proto::model::{Axis, Layout};

    use super::*;
    use crate::registry::{Session, Window};

    /// Session `s`: window `w` holding `(a / c) | (b / d)` with float `f`,
    /// and window `x` holding `e`.
    fn registry() -> Registry {
        let (notes, _) = tokio::sync::mpsc::unbounded_channel();
        let mut reg = Registry::new(notes);
        let column = |top: &str, bottom: &str| Layout::Split {
            axis: Axis::Vertical,
            ratio: 0.5,
            first: Box::new(Layout::block(top)),
            second: Box::new(Layout::block(bottom)),
        };
        let layout = Layout::Split { axis: Axis::Horizontal, ratio: 0.5, first: Box::new(column("a", "c")), second: Box::new(column("b", "d")) };
        let mut w = Window { layout, ..Window::new("w".into(), "a") };
        w.floats.push(silo_proto::model::Float { id: "f".into(), rect: Default::default() });
        reg.sessions.push(Session {
            id: "s".into(),
            name: "work".into(),
            active_window: "w".into(),
            windows: vec![w, Window::new("x".into(), "e")],
            revision: 1,
            created_at: 0,
        });
        reg
    }

    fn window(reg: &Registry, id: &str) -> Window {
        reg.sessions[0].windows.iter().find(|w| w.id == id).unwrap().clone()
    }

    #[test]
    fn window_step_wraps_and_accepts_session_names() {
        let mut reg = registry();
        window_step(&mut reg, "s", 1).unwrap();
        assert_eq!(reg.sessions[0].active_window, "x");
        window_step(&mut reg, "work", 1).unwrap();
        assert_eq!(reg.sessions[0].active_window, "w");
        window_step(&mut reg, "s", -1).unwrap();
        assert_eq!(reg.sessions[0].active_window, "x");
        assert_eq!(window_step(&mut reg, "nope", 1).unwrap_err().code, "not_found");
    }

    #[test]
    fn focus_steps_in_reading_order_then_floats() {
        let mut reg = registry();
        focus_step(&mut reg, "a", 1).unwrap();
        assert_eq!(window(&reg, "w").focused, "c");
        focus_step(&mut reg, "d", 1).unwrap();
        assert_eq!(window(&reg, "w").focused, "f");
        focus_step(&mut reg, "a", -1).unwrap();
        assert_eq!(window(&reg, "w").focused, "f");
        assert_eq!(focus_step(&mut reg, "zz", 1).unwrap_err().code, "not_found");
    }

    /// The same neighbours as the app's pixel layout of this window.
    #[test]
    fn focus_dir_matches_the_app_on_a_grid() {
        let mut reg = registry();
        focus_dir(&mut reg, "a", Direction::Right).unwrap();
        assert_eq!(window(&reg, "w").focused, "b");
        focus_dir(&mut reg, "b", Direction::Down).unwrap();
        assert_eq!(window(&reg, "w").focused, "d");
        focus_dir(&mut reg, "d", Direction::Left).unwrap();
        assert_eq!(window(&reg, "w").focused, "c");
        focus_dir(&mut reg, "c", Direction::Left).unwrap();
        assert_eq!(window(&reg, "w").focused, "c", "nothing that way");
    }

    #[test]
    fn grow_edge_and_equalize_match_the_layout_ops() {
        let mut reg = registry();
        let before = window(&reg, "w").layout;
        resize(&mut reg, "a", |l| layout_ops::resize_share(l, "a", 0.1).into_iter().collect()).unwrap();
        let mut expected = before.clone();
        let (path, ratio) = layout_ops::resize_share(&before, "a", 0.1).unwrap();
        expected.set_ratio(&path, ratio);
        assert_eq!(window(&reg, "w").layout, expected);
        assert_eq!(layout_ops::split_ratio(&window(&reg, "w").layout, &[0]).map(|(_, r)| (r * 100.0).round()), Some(60.0));

        resize(&mut reg, "a", |l| layout_ops::move_edge(l, "a", Direction::Right).into_iter().collect()).unwrap();
        assert_eq!(layout_ops::split_ratio(&window(&reg, "w").layout, &[]).map(|(_, r)| (r * 100.0).round()), Some(55.0));

        equalize(&mut reg, "w").unwrap();
        assert!(layout_ops::equalize(&window(&reg, "w").layout).is_empty());
        assert_eq!(equalize(&mut reg, "nope").unwrap_err().code, "not_found");

        // A zoomed window keeps its splits.
        reg.sessions[0].windows[0].zoomed = true;
        let zoomed = window(&reg, "w").layout;
        resize(&mut reg, "a", |l| layout_ops::move_edge(l, "a", Direction::Right).into_iter().collect()).unwrap();
        assert_eq!(window(&reg, "w").layout, zoomed);
    }

    #[test]
    fn the_log_tail_is_its_last_lines() {
        let dir = std::env::temp_dir().join(format!("silo-log-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.log");
        assert_eq!(tail(&path, 3).unwrap(), "", "no log yet");
        std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(tail(&path, 2).unwrap(), "three\nfour\n");
        assert_eq!(tail(&path, 10).unwrap(), "one\ntwo\nthree\nfour\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
