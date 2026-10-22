//! Agent blocks: an otto session shown by the app, with no process behind
//! it. The server holds which session is shown and its status records.

use silo_proto::{
    control::{AttentionReason, RpcError},
    model::{AgentState, BlockInfo, BlockKind, BlockStatus},
    text::{MAX_TITLE, untrusted},
};

use crate::status::{Pacer, Records, Report};

/// Longest otto session id accepted.
pub const SESSION_MAX: usize = 128;
/// Longest folder accepted.
pub const CWD_MAX: usize = 4096;

pub struct AgentBlock {
    /// Block metadata; `agent` is `None` here and filled from `state`.
    base: BlockInfo,
    state: AgentState,
    records: Records,
    /// Attention the block may raise (record transitions, notifications).
    pub attention: Pacer,
}

impl AgentBlock {
    pub fn new(mut base: BlockInfo, state: AgentState) -> Self {
        base.kind = BlockKind::Agent;
        base.agent = None;
        base.browser = None;
        Self { base, state, records: Records::default(), attention: Pacer::default() }
    }

    /// An agent block rebuilt from saved or transferred metadata.
    ///
    /// # Errors
    /// `info` carries no agent state.
    pub fn restore(mut info: BlockInfo) -> anyhow::Result<Self> {
        let state = info.agent.take().ok_or_else(|| anyhow::anyhow!("agent block {} has no state", info.id))?;
        info.owner = None;
        info.owner_name = None;
        Ok(Self::new(info, state))
    }

    pub fn info(&self) -> BlockInfo {
        let title = if self.state.title.is_empty() { "Agent".to_owned() } else { self.state.title.clone() };
        BlockInfo { title, agent: Some(self.state.clone()), ..self.base.clone() }
    }

    pub fn status(&self) -> BlockStatus {
        BlockStatus { block_id: self.base.id.clone(), records: self.records.published(), progress: None, busy: None }
    }

    /// Change the shown session, its title and/or the block's folder; true if
    /// anything changed. A new folder clears the session unless one is given.
    ///
    /// # Errors
    /// `bad_request` for an empty or oversized session id, or a folder that is
    /// not an absolute path.
    pub fn set(&mut self, session_id: Option<String>, title: Option<String>, cwd: Option<String>) -> Result<bool, RpcError> {
        if session_id.as_ref().is_some_and(|s| s.is_empty() || s.len() > SESSION_MAX || s.chars().any(char::is_control)) {
            return Err(RpcError::bad_request(format!("session_id must be 1 to {SESSION_MAX} printable bytes")));
        }
        if cwd.as_ref().is_some_and(|c| !c.starts_with('/') || c.len() > CWD_MAX || c.chars().any(char::is_control)) {
            return Err(RpcError::bad_request(format!("cwd must be an absolute path of at most {CWD_MAX} printable bytes")));
        }
        let before = (self.state.clone(), self.base.cwd.clone());
        if let Some(cwd) = cwd
            && self.base.cwd.as_ref() != Some(&cwd)
        {
            self.base.cwd = Some(cwd);
            self.state = AgentState::default();
        }
        if let Some(session_id) = session_id {
            if self.state.session_id.as_ref() != Some(&session_id) {
                self.state.title.clear();
            }
            self.state.session_id = Some(session_id);
        }
        if let Some(title) = title {
            self.state.title = untrusted(&title, MAX_TITLE);
        }
        Ok((&self.state, &self.base.cwd) != (&before.0, &before.1))
    }

    /// Apply a `block.report`: the attention it raises, if the pace allows,
    /// with its heading and body.
    pub fn report(&mut self, report: Report, now_ms: u64) -> Option<(AttentionReason, String, String)> {
        let heading = untrusted(if report.title.trim().is_empty() { &report.app } else { &report.title }, MAX_TITLE);
        let body = untrusted(&report.message, silo_proto::text::MAX_MESSAGE);
        let reason = self.records.report(report, now_ms)?;
        self.attention.allow(std::time::Instant::now()).then_some((reason, heading, body))
    }

    /// The user saw `ids` (all when `None`); true if records went.
    pub fn seen(&mut self, ids: Option<&[String]>) -> bool {
        self.records.seen(ids)
    }
}
