//! The block launcher: a dialog over the whole window choosing what a new
//! tab (⌃⌘T, the tab strip's +) or pane (⌃⌘↩) holds. Type to filter the
//! kinds; an address opens a browser there, `>cmd` a terminal running
//! `cmd`, `@name` an agent, a path (`/…`, `~/…`) Files there; 1–9 pick a
//! row while nothing is typed. A new
//! pane's side (right or down: splits always put the new block second)
//! starts where ⌘↩ would put it; tab/⇧tab or ←/→ change it, ↑↓ stay rows.
//! Its destination shows as a ghost in the layout until a choice is made.
//! ⌘T, ⌘D, ⇧⌘D and ⌘↩ skip it.

mod actions;
#[cfg(test)]
mod tests;
mod view;

use std::ops::Range;

use gpui::{Entity, Subscription};
use silo_proto::model::{Axis, BlockInfo, BlockKind};

use crate::{
    browser_home::RecentPage,
    commands::{Command, fuzzy},
    text_field::TextField,
};

/// What a launcher row creates. A new kind gets a variant here, its label,
/// icon and command below, a place in [`SHOWN`], and its creation in
/// `Workspace::launch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Terminal,
    Browser,
    Agent,
    Files,
}

/// The kinds listed, in order (their digits are 1, 2, 3…).
pub const SHOWN: &[Kind] = &[Kind::Terminal, Kind::Browser, Kind::Agent, Kind::Files];

/// Recent pages listed under the kinds.
const RECENTS: usize = 3;

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Browser => "Browser",
            Self::Agent => "Agent",
            Self::Files => "Files",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Browser => "globe",
            Self::Agent => crate::agent_block::ICON,
            Self::Files => crate::files_block::ICON,
        }
    }

    pub fn block_kind(self) -> BlockKind {
        match self {
            Self::Terminal => BlockKind::Terminal,
            Self::Browser => BlockKind::Browser,
            Self::Agent => BlockKind::Agent,
            Self::Files => BlockKind::Files,
        }
    }

    /// The command opening a tab of this kind without the launcher.
    pub fn tab_command(self) -> Command {
        match self {
            Self::Terminal => Command::NewTerminalTab,
            Self::Browser => Command::NewBrowserTab,
            Self::Agent => Command::NewAgentTab,
            Self::Files => Command::NewFilesTab,
        }
    }

    /// The command splitting beside a pane with this kind: its host gate.
    pub fn split_command(self) -> Command {
        match self {
            Self::Terminal => Command::NewPane,
            Self::Browser => Command::NewBrowser,
            Self::Agent => Command::NewAgent,
            Self::Files => Command::NewFiles,
        }
    }
}

/// Which side of the focused pane a new pane takes. A split always puts
/// the new block second (`Layout::split`, `block.split` takes only an
/// axis), so there is no left or up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Right,
    Down,
}

/// The sides offered, in tab order.
pub const SIDES: &[Side] = &[Side::Right, Side::Down];

impl Side {
    /// Where ⌘↩ splits along `axis` (its automatic one).
    pub fn of_axis(axis: Axis) -> Self {
        match axis {
            Axis::Horizontal => Self::Right,
            Axis::Vertical => Self::Down,
        }
    }

    pub fn axis(self) -> Axis {
        match self {
            Self::Right => Axis::Horizontal,
            Self::Down => Axis::Vertical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Right => "Right",
            Self::Down => "Down",
        }
    }

    /// The instant command splitting a terminal to this side.
    pub fn command(self) -> Command {
        match self {
            Self::Right => Command::SplitRight,
            Self::Down => Command::SplitDown,
        }
    }

    /// The side `delta` steps on through [`SIDES`], wrapping.
    #[must_use]
    pub fn step(self, delta: isize) -> Self {
        let at = SIDES.iter().position(|s| *s == self).unwrap_or(0);
        SIDES[step(at, SIDES.len(), delta)]
    }
}

/// How a key changes a new pane's side: tab / → forward, ⇧tab / ← back.
/// ↑↓ stay row navigation; the field has no caret to move, so ←→ are free.
pub fn side_key(key: &str, shift: bool) -> Option<isize> {
    match key {
        "tab" => Some(if shift { -1 } else { 1 }),
        "right" if !shift => Some(1),
        "left" if !shift => Some(-1),
        _ => None,
    }
}

/// Where a launch goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A new tab.
    Tab,
    /// A split of `block` to `side`; `auto` is the side ⌘↩ would take
    /// (where the dialog started).
    Split { block: String, side: Side, auto: Side },
}

/// The target a chooser opens: New Pane… splits `block` (if any), starting
/// on `auto`; anything else is a tab.
pub fn chooser_target(chooser: Command, block: Option<String>, auto: impl FnOnce(&str) -> Axis) -> Option<Target> {
    match chooser {
        Command::NewPaneChooser => {
            let block = block?;
            let side = Side::of_axis(auto(&block));
            Some(Target::Split { block, side, auto: side })
        }
        _ => Some(Target::Tab),
    }
}

/// The `block.split` creating `row` beside `block` to `side`, as the
/// instant commands make it (`cwd`: the split block's, for shells, agents
/// and Files; browsers start with no spawn; a typed path is Files there).
pub fn split_method(block: &str, side: Side, row: &Row, cwd: Option<String>) -> silo_proto::control::Method {
    use silo_proto::control::{Method, SpawnSpec};
    let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
    let (kind, spawn, url) = match row {
        Row::Kind(Kind::Terminal, _) | Row::Run(_) => (BlockKind::Terminal, spawn, None),
        Row::Kind(Kind::Browser, _) => (BlockKind::Browser, SpawnSpec::default(), None),
        Row::Kind(Kind::Agent, _) => (BlockKind::Agent, spawn, None),
        Row::Kind(Kind::Files, _) => (BlockKind::Files, spawn, None),
        Row::Path(path) => (BlockKind::Files, SpawnSpec { cwd: Some(path.clone()), ..SpawnSpec::default() }, None),
        Row::Open { url, .. } => (BlockKind::Browser, SpawnSpec::default(), Some(url.clone())),
    };
    Method::BlockSplit { block_id: block.to_owned(), axis: side.axis(), spawn, kind, url, agent: None }
}

/// What the typed text asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Filter the kinds (and recent pages) by this text, trimmed.
    Filter(String),
    /// A browser at this (normalized) address.
    Open(String),
    /// A terminal running this command (`>cmd`), typed into its shell.
    Run(String),
    /// An agent (`@name`); the name, possibly empty.
    Agent(String),
    /// Files at this path (`/…`, `~`, `~/…`): a folder, or a file in one.
    Path(String),
}

/// Read `text`: `>cmd` runs, `@name` is an agent, a path opens Files, an
/// address (scheme, dotted host or localhost, as the palette reads one)
/// opens a browser, else filter.
pub fn classify(text: &str) -> Intent {
    let t = text.trim();
    if t.starts_with('/') || t == "~" || t.starts_with("~/") {
        return Intent::Path(t.to_owned());
    }
    if let Some(command) = t.strip_prefix('>') {
        return Intent::Run(command.trim().to_owned());
    }
    if let Some(name) = t.strip_prefix('@') {
        return Intent::Agent(name.trim().to_owned());
    }
    match crate::finder::address(t) {
        Some(url) => Intent::Open(url),
        None => Intent::Filter(t.to_owned()),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// A kind and its label's matched ranges.
    Kind(Kind, Vec<Range<usize>>),
    /// A browser at `url` (typed, or a recent page with its title).
    Open { url: String, title: String },
    /// A terminal running this command.
    Run(String),
    /// Files at this path.
    Path(String),
}

impl Row {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Kind(kind, _) => *kind,
            Self::Open { .. } => Kind::Browser,
            Self::Run(_) => Kind::Terminal,
            Self::Path(_) => Kind::Files,
        }
    }
}

/// The rows for `text`: every kind then the newest recent pages while it
/// is empty; the matching kinds best first, then matching recent pages,
/// while filtering; the one thing an address, `>cmd` or `@name` asks for.
pub fn rows(text: &str, recents: &[RecentPage]) -> Vec<Row> {
    let open = |p: &RecentPage| Row::Open { url: p.url.clone(), title: p.title.clone() };
    match classify(text) {
        Intent::Filter(q) if q.is_empty() => SHOWN.iter().map(|k| Row::Kind(*k, Vec::new())).chain(recents.iter().take(RECENTS).map(open)).collect(),
        Intent::Filter(q) => {
            let mut found: Vec<(i32, usize, Vec<Range<usize>>)> =
                SHOWN.iter().enumerate().filter_map(|(i, k)| fuzzy(&q, k.label()).map(|(score, ranges)| (score, i, ranges))).collect();
            found.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
            let pages = recents.iter().filter(|p| fuzzy(&q, &p.title).is_some() || fuzzy(&q, &p.url).is_some()).take(RECENTS).map(open);
            found.into_iter().map(|(_, i, ranges)| Row::Kind(SHOWN[i], ranges)).chain(pages).collect()
        }
        Intent::Open(url) => vec![Row::Open { url, title: String::new() }],
        Intent::Run(command) if command.is_empty() => vec![Row::Kind(Kind::Terminal, Vec::new())],
        Intent::Run(command) => vec![Row::Run(command)],
        Intent::Agent(_) => vec![Row::Kind(Kind::Agent, Vec::new())],
        Intent::Path(path) => vec![Row::Path(path)],
    }
}

/// What the dialog says about `text` beyond its rows.
pub fn intent_note(text: &str) -> Option<String> {
    match classify(text) {
        Intent::Agent(name) if !name.is_empty() => Some(format!("Opens the default agent; pick “{name}” in its composer")),
        _ => None,
    }
}

/// The row selected for `text`: the kind the instant shortcut opens until
/// something is typed (so ⌃⌘T ↩ is ⌘T and ⌃⌘↩ ↩ is ⌘↩), then the best.
pub fn preselect(text: &str, rows: &[Row], same: Kind) -> usize {
    if text.trim().is_empty() { rows.iter().position(|r| matches!(r, Row::Kind(k, _) if *k == same)).unwrap_or(0) } else { 0 }
}

/// `selected` moved by `delta` through `len` rows, wrapping.
pub fn step(selected: usize, len: usize, delta: isize) -> usize {
    let len = len.max(1).cast_signed();
    (selected.cast_signed() + delta).rem_euclid(len).cast_unsigned()
}

/// The row a digit key picks: 1–9 while nothing is typed (once something
/// is, digits are text: an address may start with one).
pub fn digit_pick(key: &str, text: &str, len: usize) -> Option<usize> {
    if !text.is_empty() {
        return None;
    }
    let digit = key.parse::<usize>().ok().filter(|d| (1..=9).contains(d))?;
    (digit <= len).then(|| digit - 1)
}

/// The kind of `focused` (any block of the session, floats included),
/// else a terminal: what ⌘T opens.
pub fn same_kind(focused: Option<&str>, blocks: &[BlockInfo]) -> Kind {
    let kind = focused.and_then(|id| blocks.iter().find(|b| b.id == id)).map(|b| b.kind);
    kind.and_then(|kind| SHOWN.iter().copied().find(|k| k.block_kind() == kind)).unwrap_or(Kind::Terminal)
}

/// A row's shortcut: the instant command doing the same thing, if any.
pub fn row_hint(kind: Kind, target: &Target, same: Kind) -> Option<String> {
    match target {
        Target::Tab => kind.tab_command().hint().or_else(|| (kind == same).then(|| Command::NewTab.hint()).flatten()),
        // ⌘D and ⇧⌘D split right and down with a terminal; ⇧⌘O an agent
        // on ⌘↩'s side.
        Target::Split { side, auto, .. } => match kind {
            Kind::Terminal => side.command().hint(),
            Kind::Agent if side == auto => Command::NewAgent.hint(),
            _ => None,
        },
    }
}

/// The open dialog.
pub(crate) struct Dialog {
    field: Entity<TextField>,
    rows: Vec<Row>,
    selected: usize,
    target: Target,
    /// The kind the instant shortcut opens, from when the dialog opened.
    same: Kind,
    recents: Vec<RecentPage>,
    /// Why ↩ did nothing (a host without the row's feature), or what the
    /// typed text does beyond its rows.
    note: Option<String>,
    _sub: Subscription,
}

#[derive(Default)]
pub struct Launcher {
    dialog: Option<Dialog>,
}

impl Launcher {
    pub fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// Open for a new tab (the + button shows pressed).
    pub fn is_open_for_tab(&self) -> bool {
        self.dialog.as_ref().is_some_and(|d| d.target == Target::Tab)
    }

    /// The split being chosen: its block, axis and the selected row's kind,
    /// for the ghost at its destination.
    pub fn ghost(&self) -> Option<(&str, Axis, Option<Kind>)> {
        let dialog = self.dialog.as_ref()?;
        let Target::Split { block, side, .. } = &dialog.target else { return None };
        Some((block.as_str(), side.axis(), dialog.rows.get(dialog.selected).map(Row::kind)))
    }

    /// Move a new pane's side by `delta`; false for a tab or a closed dialog.
    pub fn step_side(&mut self, delta: isize) -> bool {
        match self.dialog.as_mut().map(|d| &mut d.target) {
            Some(Target::Split { side, .. }) => {
                *side = side.step(delta);
                true
            }
            _ => false,
        }
    }
}
