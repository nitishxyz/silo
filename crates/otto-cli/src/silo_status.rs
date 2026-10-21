//! Otto's turn state as an OSC 7501 status record for the silo block it
//! runs in (`docs/AGENTS.md` §6), so agents driving otto in a terminal can
//! wait on it exactly.

use std::io::Write;

use base64::Engine;
use otto_client::SessionMirror;
use otto_proto::TurnStatus;

/// What the block shows: `state` and, when blocked, `kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub state: &'static str,
    pub kind: Option<&'static str>,
}

/// The state `mirror` shows; `None` before the first turn.
#[must_use]
pub fn of(mirror: &SessionMirror) -> Option<State> {
    if !mirror.pending_approvals.is_empty() {
        return Some(State { state: "blocked", kind: Some("permission") });
    }
    if !mirror.pending_questions.is_empty() {
        return Some(State { state: "blocked", kind: Some("question") });
    }
    let state = match mirror.turn?.1 {
        TurnStatus::Running => "working",
        TurnStatus::Done => "done",
        TurnStatus::Cancelled | TurnStatus::Interrupted => "idle",
        _ => "error",
    };
    Some(State { state, kind: None })
}

/// The OSC 7501 report for `state`.
#[must_use]
pub fn sequence(state: State, message: &str) -> String {
    let mut osc = format!("\x1b]7501;state={}:app=otto", state.state);
    if let Some(kind) = state.kind {
        osc.push_str(&format!(":kind={kind}"));
    }
    if !message.is_empty() {
        osc.push_str(&format!(":msg={}", base64::engine::general_purpose::STANDARD.encode(message)));
    }
    osc.push_str("\x1b\\");
    osc
}

/// Reports changes to the controlling terminal, only inside a silo block.
#[derive(Default)]
pub struct Reporter {
    last: Option<State>,
}

impl Reporter {
    pub fn report(&mut self, mirror: &SessionMirror) {
        let Some(state) = of(mirror).filter(|s| self.last != Some(*s)) else { return };
        self.last = Some(state);
        if std::env::var_os("SILO_BLOCK").is_none_or(|b| b.is_empty()) {
            return;
        }
        let message = if state.state == "error" { mirror.last_error.as_ref().map(|e| e.1.as_str()).unwrap_or_default() } else { "" };
        if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            let _ = tty.write_all(sequence(state, message).as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_follow_osc_7501() {
        assert_eq!(sequence(State { state: "working", kind: None }, ""), "\x1b]7501;state=working:app=otto\x1b\\");
        assert_eq!(
            sequence(State { state: "blocked", kind: Some("permission") }, "built"),
            "\x1b]7501;state=blocked:app=otto:kind=permission:msg=YnVpbHQ=\x1b\\"
        );
    }
}
