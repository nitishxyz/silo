//! One handle for a block's pane, whatever its kind.
//!
//! The workspace keeps a map per kind (`panes`, `browsers`, `agents`,
//! `files`); everything that treats blocks alike (focus, focus navigation,
//! block info, rendering, the header) goes through [`BlockPane`] instead of
//! asking each map in turn. Its `match`es are exhaustive, so a new kind is
//! one variant here and the compiler names every place it must be handled.

use std::collections::HashMap;

use gpui::{AnyElement, App, Context, Entity, IntoElement, Subscription, Window};
use silo_proto::model::BlockInfo;

use crate::{
    agent::AgentPane,
    browser_pane::BrowserPane,
    files::FilesPane,
    pane::Pane,
    pane_header::{self, HeaderParts, HeaderStyle},
    workspace::Workspace,
};

#[derive(Clone)]
pub(crate) enum BlockPane {
    Terminal(Entity<Pane>),
    Browser(Entity<BrowserPane>),
    Agent(Entity<AgentPane>),
    Files(Entity<FilesPane>),
}

impl BlockPane {
    /// The browser pane, if this is one (handing a browser to the off-screen host).
    pub(crate) fn into_browser(self) -> Option<Entity<BrowserPane>> {
        match self {
            Self::Browser(p) => Some(p),
            _ => None,
        }
    }

    /// Give the pane keyboard focus (an agent: its composer).
    pub(crate) fn focus(&self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Terminal(p) => p.read(cx).focus.clone().focus(window, cx),
            Self::Browser(p) => p.read(cx).focus.clone().focus(window, cx),
            Self::Agent(p) => p.update(cx, |p, cx| p.focus_composer(window, cx)),
            Self::Files(p) => p.read(cx).focus.clone().focus(window, cx),
        }
    }

    /// Whether keyboard focus is in the pane (any of its own inputs).
    pub(crate) fn has_focus(&self, window: &Window, cx: &App) -> bool {
        match self {
            Self::Terminal(p) => p.read(cx).focus.is_focused(window),
            Self::Browser(p) => {
                let p = p.read(cx);
                p.focus.contains_focused(window, cx) || p.address.read(cx).focus.is_focused(window)
            }
            Self::Agent(p) => p.read(cx).focus.contains_focused(window, cx),
            Self::Files(p) => {
                let p = p.read(cx);
                p.focus.contains_focused(window, cx) || p.tree_focus.is_focused(window)
            }
        }
    }

    /// The server's latest info for the block.
    pub(crate) fn set_info(&self, info: BlockInfo, cx: &mut App) {
        match self {
            Self::Terminal(p) => p.update(cx, |p, cx| p.set_info(info, cx)),
            Self::Browser(p) => p.update(cx, |p, cx| p.set_info(info, cx)),
            Self::Agent(p) => {
                let session = info.agent.and_then(|a| a.session_id);
                p.update(cx, |p, cx| p.show(session, info.cwd, cx));
            }
            Self::Files(p) => p.update(cx, |p, cx| p.set_info(info, cx)),
        }
    }

    /// Content bounds the pane measured itself, if it does. Tiled blocks
    /// use the layout's cell instead ([`Workspace::block_bounds`]).
    pub(crate) fn content_bounds(&self, cx: &App) -> Option<gpui::Bounds<gpui::Pixels>> {
        match self {
            Self::Terminal(p) => p.read(cx).bounds(),
            Self::Browser(p) => p.read(cx).bounds(),
            Self::Agent(p) => p.read(cx).bounds(),
            Self::Files(p) => p.read(cx).bounds(),
        }
    }

    pub(crate) fn element(&self) -> AnyElement {
        match self {
            Self::Terminal(p) => p.clone().into_any_element(),
            Self::Browser(p) => p.clone().into_any_element(),
            Self::Agent(p) => p.clone().into_any_element(),
            Self::Files(p) => p.clone().into_any_element(),
        }
    }

    pub(crate) fn header_parts(&self, style: HeaderStyle, info: Option<&BlockInfo>, cx: &mut App) -> HeaderParts {
        match self {
            Self::Terminal(p) => pane_header::parts(p, style, info, cx),
            Self::Browser(p) => pane_header::parts(p, style, info, cx),
            Self::Agent(p) => pane_header::parts(p, style, info, cx),
            Self::Files(p) => pane_header::parts(p, style, info, cx),
        }
    }
}

/// Every block's pane in this window, keyed by block id: the one list the
/// workspace keeps. Code that treats blocks alike uses [`Blocks::get`] and
/// [`Blocks::iter`]; code for one kind asks for it by name
/// ([`Blocks::terminal`], [`Blocks::browser`], …).
#[derive(Default)]
pub(crate) struct Blocks {
    map: HashMap<String, (BlockPane, Subscription)>,
}

macro_rules! kind {
    ($one:ident, $all:ident, $variant:ident, $ty:ty) => {
        pub(crate) fn $one(&self, block: &str) -> Option<&Entity<$ty>> {
            match self.map.get(block) {
                Some((BlockPane::$variant(p), _)) => Some(p),
                _ => None,
            }
        }

        pub(crate) fn $all(&self) -> impl Iterator<Item = (&String, &Entity<$ty>)> {
            self.map.iter().filter_map(|(id, (pane, _))| match pane {
                BlockPane::$variant(p) => Some((id, p)),
                _ => None,
            })
        }
    };
}

impl Blocks {
    kind!(terminal, terminals, Terminal, Pane);
    kind!(browser, browsers, Browser, BrowserPane);
    kind!(agent, agents, Agent, AgentPane);
    kind!(files, files_panes, Files, FilesPane);

    pub(crate) fn get(&self, block: &str) -> Option<&BlockPane> {
        self.map.get(block).map(|(pane, _)| pane)
    }

    pub(crate) fn contains(&self, block: &str) -> bool {
        self.map.contains_key(block)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &BlockPane)> {
        self.map.iter().map(|(id, (pane, _))| (id, pane))
    }

    /// Show `block` with `pane`; `subscription` lives as long as it does.
    pub(crate) fn insert(&mut self, block: String, pane: BlockPane, subscription: Subscription) {
        self.map.insert(block, (pane, subscription));
    }

    pub(crate) fn remove(&mut self, block: &str) -> Option<BlockPane> {
        self.map.remove(block).map(|(pane, _)| pane)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str, &BlockPane) -> bool) {
        self.map.retain(|id, (pane, _)| keep(id, pane));
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }
}

impl Workspace {
    /// The pane showing `block` here, of any kind.
    pub(crate) fn block_pane(&self, block: &str) -> Option<BlockPane> {
        self.blocks.get(block).cloned()
    }

    /// The block whose pane holds keyboard focus.
    pub(crate) fn block_with_focus(&self, window: &Window, cx: &App) -> Option<String> {
        self.blocks.iter().find(|(_, p)| p.has_focus(window, cx)).map(|(id, _)| id.clone())
    }

    pub(crate) fn focus_block(&self, block: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.block_pane(block) {
            pane.focus(window, cx);
        }
    }

    /// Whether `block` has a pane of any kind here.
    pub(crate) fn has_pane(&self, block: &str) -> bool {
        self.block_pane(block).is_some()
    }

    /// Where a block is on screen. A tiled block of any kind has the cell
    /// the layout measured for it; a float falls back to its pane's
    /// content bounds.
    pub(crate) fn block_bounds(&self, block: &str, cx: &App) -> Option<gpui::Bounds<gpui::Pixels>> {
        let tiled = self.active_window().is_some_and(|w| w.layout.contains(block));
        if let Some(cell) = tiled.then(|| self.motion.measured.cells.borrow().get(block).copied()).flatten() {
            return Some(cell);
        }
        self.block_pane(block)?.content_bounds(cx)
    }
}
