//! The block core: what every block kind shares (see `docs/BLOCKS.md`).
//!
//! [`Kind`] is the client's kind of a block (from its server info), with
//! what is fixed per kind (its glyph). [`BlockPane`] is one handle for a
//! block's pane, whatever its kind; [`Blocks`] is the workspace's one list
//! of them. Everything that treats blocks alike (focus, focus navigation,
//! block info, rendering, the header, close motion) goes through these
//! instead of asking for one kind. Their `match`es are exhaustive (no `_`
//! arms), so a new kind is one variant here and the compiler names every
//! place it must be handled.

use std::collections::HashMap;

use gpui::{AnyElement, App, Context, Entity, IntoElement, Subscription, Window};
use silo_proto::model::{BlockInfo, BlockKind};

use crate::{
    agent::AgentPane,
    browser_pane::{BrowserPane, Icon},
    files::FilesPane,
    motion::{Exit, GhostBody},
    pane::Pane,
    pane_header::{self, HeaderParts, HeaderStyle},
    workspace::Workspace,
};

/// Which pane a block gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Terminal,
    Browser,
    /// An otto session; a "Not supported" card on remote hosts.
    Agent,
    /// A folder's files.
    Files,
    /// A newer server's block kind: no pane, a "Not supported" card.
    Unsupported,
}

impl Kind {
    /// Every kind, for tests that must cover each one.
    #[cfg(test)]
    pub const ALL: [Self; 5] = [Self::Terminal, Self::Browser, Self::Agent, Self::Files, Self::Unsupported];

    /// The kind's glyph: lists, tabs, headers and the drag pill.
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Browser => "globe",
            Self::Agent => crate::agent_block::ICON,
            Self::Files => crate::files_block::ICON,
            Self::Unsupported => "info",
        }
    }

    /// Whether it works in a folder, which a block split off it starts in.
    #[must_use]
    pub fn has_folder(self) -> bool {
        match self {
            Self::Terminal | Self::Agent | Self::Files => true,
            Self::Browser | Self::Unsupported => false,
        }
    }

    /// How its pane leaves the tiled layout; `alive`: the block is still in
    /// the session (floated, parked, moved), so its pane may be drawn there.
    #[must_use]
    pub fn exit(self, alive: bool) -> Exit {
        match self {
            Self::Terminal => Exit::Frame,
            Self::Browser | Self::Agent | Self::Files if alive => Exit::Card,
            Self::Browser | Self::Agent | Self::Files => Exit::Pane,
            Self::Unsupported => Exit::Card,
        }
    }
}

/// The pane kind of a block the server describes as `info`; a block not
/// (yet) in the view is a terminal.
#[must_use]
pub fn kind_of(info: Option<&BlockInfo>) -> Kind {
    match info.map(|i| i.kind) {
        None | Some(BlockKind::Terminal) => Kind::Terminal,
        Some(BlockKind::Browser) => Kind::Browser,
        Some(BlockKind::Agent) => Kind::Agent,
        Some(BlockKind::Files) => Kind::Files,
        Some(BlockKind::Unknown) => Kind::Unsupported,
    }
}

/// The card a pane is drawn in this frame ([`BlockPane::set_card`]).
pub(crate) struct Card {
    pub palette: crate::adaptive::Chrome,
    pub corners: crate::radius::CardCorners,
    /// A page's bottom corners inside the card (0 when flush).
    pub page_corners: (f32, f32),
    /// How a terminal paints its cells on the card's material.
    pub translucency: Option<silo_render::Translucency>,
}

#[derive(Clone)]
pub(crate) enum BlockPane {
    Terminal(Entity<Pane>),
    Browser(Entity<BrowserPane>),
    Agent(Entity<AgentPane>),
    Files(Entity<FilesPane>),
}

impl BlockPane {
    /// The title the pane knows better than the server (a terminal's
    /// program, a page's title); `None`: the server's.
    pub(crate) fn title(&self, cx: &App) -> Option<String> {
        match self {
            Self::Terminal(p) => Some(p.read(cx).title()),
            Self::Browser(p) => Some(p.read(cx).title()),
            Self::Agent(_) | Self::Files(_) => None,
        }
    }

    /// What fills its icon slot (header, tab): a page's spinner or favicon,
    /// else the kind's glyph.
    pub(crate) fn icon(&self, cx: &App) -> Icon {
        match self {
            Self::Browser(p) => p.read(cx).icon(),
            Self::Terminal(_) => Icon::Glyph(Kind::Terminal.icon()),
            Self::Agent(_) => Icon::Glyph(Kind::Agent.icon()),
            Self::Files(_) => Icon::Glyph(Kind::Files.icon()),
        }
    }

    /// The image behind [`Icon::Favicon`].
    pub(crate) fn favicon(&self, cx: &App) -> Option<std::sync::Arc<gpui::Image>> {
        match self {
            Self::Browser(p) => p.read(cx).favicon_image(),
            Self::Terminal(_) | Self::Agent(_) | Self::Files(_) => None,
        }
    }

    /// The colour its program or page paints behind itself, if it reports one.
    pub(crate) fn backdrop(&self, cx: &App) -> Option<u32> {
        match self {
            Self::Terminal(p) => p.read(cx).backdrop(),
            Self::Browser(p) => p.read(cx).backdrop(),
            Self::Agent(_) | Self::Files(_) => None,
        }
    }

    /// How dim it is while another pane is active (a page dims less).
    pub(crate) fn inactive_dim(&self) -> f32 {
        match self {
            Self::Browser(_) => crate::theme::INACTIVE_DIM_PAGE,
            Self::Terminal(_) | Self::Agent(_) | Self::Files(_) => crate::theme::INACTIVE_DIM,
        }
    }

    /// The card it is drawn in this frame. A terminal paints its cells for
    /// the card's material and corner; a page clips to the card and tints
    /// its own controls with the card's palette.
    pub(crate) fn set_card(&self, card: &Card, cx: &mut App) {
        match self {
            Self::Terminal(p) => p.update(cx, |p, cx| {
                p.set_translucency(card.translucency, cx);
                p.set_card_corner(card.corners.bottom_right, cx);
            }),
            Self::Browser(p) => p.update(cx, |p, cx| {
                p.set_palette(card.palette, cx);
                p.set_corners(card.page_corners, cx);
            }),
            Self::Agent(_) | Self::Files(_) => {}
        }
    }

    /// The host's connection is back. Agents reach otto on their own
    /// ([`Workspace::agents_host_up`]).
    pub(crate) fn set_control(&self, control: &silo_client::Control, cx: &mut App) {
        match self {
            Self::Terminal(p) => p.update(cx, |p, cx| p.set_control(control.clone(), cx)),
            Self::Browser(p) => p.update(cx, |p, cx| p.set_control(control.clone(), cx)),
            Self::Files(p) => p.update(cx, |p, cx| p.set_control(control.clone(), cx)),
            Self::Agent(_) => {}
        }
    }

    /// What a close would lose: its unsaved files (the close guard asks first).
    pub(crate) fn unsaved(&self, cx: &App) -> Vec<String> {
        match self {
            Self::Files(p) => p.read(cx).unsaved(),
            Self::Terminal(_) | Self::Browser(_) | Self::Agent(_) => Vec::new(),
        }
    }

    /// The last frame it painted, for a pane that leaves as one ([`Exit::Frame`]).
    pub(crate) fn last_frame(&self, cx: &App) -> Option<std::sync::Arc<silo_render::PreparedFrame>> {
        match self {
            Self::Terminal(p) => p.read(cx).last_frame(),
            Self::Browser(_) | Self::Agent(_) | Self::Files(_) => None,
        }
    }

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
    /// The kind of `block`, from the server's view of it.
    pub(crate) fn block_kind(&self, block: &str) -> Kind {
        kind_of(self.block_info(block))
    }

    /// A block's glyph by kind.
    pub(crate) fn block_icon(&self, block: &str) -> &'static str {
        self.block_kind(block).icon()
    }

    /// A block's title wherever it is named (lists, tabs, the background
    /// menu): its pane's when it knows better, else the server's.
    pub(crate) fn block_label(&self, block: &str, cx: &App) -> String {
        if let Some(title) = self.block_pane(block).and_then(|p| p.title(cx)) {
            return title;
        }
        self.block_info(block).map_or_else(|| "shell".into(), crate::pane::block_title)
    }

    /// What block `block`'s ghost shows as it leaves the layout
    /// ([`Kind::exit`]); `alive`: it is still in the session.
    pub(crate) fn exit_body(&self, block: &str, alive: bool, cx: &App) -> GhostBody {
        let pane = self.block_pane(block);
        match self.block_kind(block).exit(alive) {
            Exit::Frame => pane.and_then(|p| p.last_frame(cx)).map_or(GhostBody::Card, GhostBody::Frame),
            Exit::Pane => pane.map_or(GhostBody::Card, GhostBody::Pane),
            Exit::Card => GhostBody::Card,
        }
    }

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

#[cfg(test)]
mod tests {
    use silo_proto::model::{Lifecycle, TermSize};

    use super::*;

    fn info(id: &str, kind: BlockKind) -> BlockInfo {
        BlockInfo {
            id: id.into(),
            session_id: "s".into(),
            kind,
            title: String::new(),
            cwd: None,
            exited: None,
            lifecycle: Lifecycle::Running,
            size: TermSize::new(80, 24),
            owner: None,
            owner_name: None,
            owner_block: None,
            pid: None,
            browser: None,
            agent: None,
            files: None,
        }
    }

    /// Position of `kind` in [`Kind::ALL`]; exhaustive, so a new kind
    /// can't be left out of it.
    fn index(kind: Kind) -> usize {
        match kind {
            Kind::Terminal => 0,
            Kind::Browser => 1,
            Kind::Agent => 2,
            Kind::Files => 3,
            Kind::Unsupported => 4,
        }
    }

    #[test]
    fn all_lists_every_kind_once() {
        let at: Vec<usize> = Kind::ALL.iter().map(|k| index(*k)).collect();
        assert_eq!(at, (0..Kind::ALL.len()).collect::<Vec<_>>());
    }

    #[test]
    fn pane_kind_follows_block_kind() {
        assert_eq!(kind_of(Some(&info("b", BlockKind::Browser))), Kind::Browser);
        assert_eq!(kind_of(Some(&info("b", BlockKind::Agent))), Kind::Agent);
        assert_eq!(kind_of(Some(&info("b", BlockKind::Files))), Kind::Files);
        assert_eq!(kind_of(Some(&info("b", BlockKind::Terminal))), Kind::Terminal);
        assert_eq!(kind_of(Some(&info("b", BlockKind::Unknown))), Kind::Unsupported);
        assert_eq!(kind_of(None), Kind::Terminal);
    }

    #[test]
    fn splits_start_in_the_folder_of_a_kind_that_has_one() {
        let with: Vec<Kind> = Kind::ALL.into_iter().filter(|k| k.has_folder()).collect();
        assert_eq!(with, [Kind::Terminal, Kind::Agent, Kind::Files]);
    }

    #[test]
    fn every_kind_has_its_own_glyph() {
        let glyphs: Vec<&str> = Kind::ALL.iter().map(|k| k.icon()).collect();
        assert_eq!(glyphs, ["terminal", "globe", crate::agent_block::ICON, crate::files_block::ICON, "info"]);
        let mut unique = glyphs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), glyphs.len(), "two kinds share a glyph");
    }
}
