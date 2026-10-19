//! Browser blocks: engine state held by the server, with no process behind
//! it. The owning client runs the engine and reports; commands edit the
//! state directly so the next client opens where the last one left off.

use silo_proto::{
    control::{BrowserCommand, OwnMode, RpcError},
    model::{BlockInfo, BlockKind, BrowserState, ClientId, HistoryEntry},
};

use crate::block::BlockHandle;

/// Longest URL or title accepted in a report.
pub const TEXT_MAX: usize = 8 * 1024;

/// A block in the registry.
pub enum Block {
    Terminal(BlockHandle),
    Browser(Box<BrowserBlock>),
}

impl Block {
    pub fn info(&self) -> Option<BlockInfo> {
        match self {
            Self::Terminal(handle) => handle.info.lock().ok().map(|i| i.clone()),
            Self::Browser(browser) => Some(browser.info()),
        }
    }

    pub fn terminal(&self) -> Option<&BlockHandle> {
        match self {
            Self::Terminal(handle) => Some(handle),
            Self::Browser(_) => None,
        }
    }

    pub fn into_terminal(self) -> Option<BlockHandle> {
        match self {
            Self::Terminal(handle) => Some(handle),
            Self::Browser(_) => None,
        }
    }
}

/// The engine's committed state, as reported by `browser.report`.
pub struct Report {
    pub url: String,
    pub title: String,
    pub favicon: Option<String>,
    pub loading: bool,
    pub history: Vec<HistoryEntry>,
    pub index: usize,
}

pub struct BrowserBlock {
    /// Block metadata; `browser` is `None` here and filled from `state`.
    base: BlockInfo,
    state: BrowserState,
}

impl BrowserBlock {
    /// A new browser block showing `state`.
    pub fn new(mut base: BlockInfo, state: BrowserState) -> Self {
        base.kind = BlockKind::Browser;
        base.browser = None;
        Self { base, state }
    }

    /// A browser block rebuilt from saved or transferred metadata; it has no
    /// owner until a client takes it.
    ///
    /// # Errors
    /// `info` carries no browser state.
    pub fn restore(mut info: BlockInfo) -> anyhow::Result<Self> {
        let state = info.browser.take().ok_or_else(|| anyhow::anyhow!("browser block {} has no state", info.id))?;
        info.owner = None;
        Ok(Self::new(info, state))
    }

    pub fn info(&self) -> BlockInfo {
        let title = if self.state.title.is_empty() { self.state.url.clone() } else { self.state.title.clone() };
        BlockInfo { title, browser: Some(self.state.clone()), ..self.base.clone() }
    }

    /// Apply a command to the stored state.
    ///
    /// # Errors
    /// `bad_request` at either end of history or for a non-finite zoom.
    pub fn command(&mut self, command: &BrowserCommand) -> Result<(), RpcError> {
        match command {
            BrowserCommand::Navigate { url } => self.state.push(url.clone()),
            BrowserCommand::Back if !self.state.go(-1) => return Err(RpcError::bad_request("no page to go back to")),
            BrowserCommand::Forward if !self.state.go(1) => return Err(RpcError::bad_request("no page to go forward to")),
            BrowserCommand::Back | BrowserCommand::Forward | BrowserCommand::Reload { .. } => {}
            BrowserCommand::Zoom { zoom } if !zoom.is_finite() => return Err(RpcError::bad_request("zoom must be finite")),
            BrowserCommand::Zoom { zoom } => self.state.zoom = *zoom,
        }
        Ok(())
    }

    /// Store the owner's engine state. History is trimmed to the
    /// [`BrowserState::HISTORY`] entries nearest the current one, oldest
    /// first, keeping `index` on the same entry.
    ///
    /// # Errors
    /// `unauthorized` unless `client` owns the block; `bad_request` for an
    /// index outside history or an oversized favicon.
    pub fn report(&mut self, client: ClientId, mut report: Report) -> Result<(), RpcError> {
        if self.base.owner != Some(client) {
            return Err(RpcError::unauthorized("only the block's owner may report browser state"));
        }
        if report.index >= report.history.len() {
            return Err(RpcError::bad_request("index must point into history"));
        }
        let texts = [&report.url, &report.title].into_iter().chain(report.history.iter().flat_map(|e| [&e.url, &e.title]));
        if texts.into_iter().any(|t| t.len() > TEXT_MAX) {
            return Err(RpcError::bad_request(format!("urls and titles must be at most {TEXT_MAX} bytes")));
        }
        let start = report.index.saturating_sub(BrowserState::HISTORY - 1).min(report.history.len().saturating_sub(BrowserState::HISTORY));
        report.history.drain(..start);
        report.history.truncate(BrowserState::HISTORY);
        report.index -= start;
        if report.favicon.as_ref().is_some_and(|f| f.len() > BrowserState::FAVICON || !f.starts_with("data:")) {
            return Err(RpcError::bad_request(format!("favicon must be a data: URL of at most {} bytes", BrowserState::FAVICON)));
        }
        let s = &mut self.state;
        (s.url, s.title, s.favicon, s.loading, s.history, s.index) =
            (report.url, report.title, report.favicon, report.loading, report.history, report.index);
        Ok(())
    }

    /// Take ownership; true if the owner changed.
    pub fn own(&mut self, client: ClientId, mode: OwnMode) -> bool {
        let take = match mode {
            OwnMode::Take => self.base.owner != Some(client),
            OwnMode::Request => self.base.owner.is_none(),
        };
        if take {
            self.base.owner = Some(client);
        }
        take
    }

    /// Release ownership held by `client`; true if the owner changed.
    pub fn disown(&mut self, client: ClientId) -> bool {
        let owned = self.base.owner == Some(client);
        if owned {
            self.base.owner = None;
        }
        owned
    }
}
