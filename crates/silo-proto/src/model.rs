//! Session tree: sessions contain windows (tabs); a window's layout is a
//! binary split tree whose leaves are blocks.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type SessionId = String;
pub type WindowId = String;
pub type BlockId = String;
pub type ClientId = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TermSize {
    pub columns: u16,
    pub rows: u16,
    #[serde(default)]
    pub cell_width: u16,
    #[serde(default)]
    pub cell_height: u16,
}

impl TermSize {
    #[must_use]
    pub fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows, cell_width: 0, cell_height: 0 }
    }
}

/// Cursor shape a DECSCUSR reset restores.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
    Hollow,
    /// A newer peer's shape: drawn as a block.
    #[serde(other)]
    Unknown,
}

/// Effective terminal configuration of a block: the defaults the
/// authoritative terminal answers color queries with (OSC 4/10/11/12,
/// `CSI ? 996 n`) and every client renders with. Program overrides
/// (OSC 4/10/11/12, DECSCUSR) still win. Carried by value, never as a path
/// to a local file, so remote clients see the same state.
/// Colors are `0x00RRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalConfig {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    /// ANSI 0-15.
    pub palette: [u32; 16],
    #[serde(default)]
    pub cursor_shape: CursorShape,
    #[serde(default = "yes")]
    pub cursor_blink: bool,
    #[serde(default = "default_scrollback")]
    pub scrollback_lines: u64,
}

fn yes() -> bool {
    true
}

fn default_scrollback() -> u64 {
    TerminalConfig::DEFAULT_SCROLLBACK
}

impl TerminalConfig {
    pub const DEFAULT_SCROLLBACK: u64 = 10_000;
    pub const MIN_SCROLLBACK: u64 = 100;
    pub const MAX_SCROLLBACK: u64 = 1_000_000;

    /// Byte bound handed to the engine next to the line bound, generous
    /// enough that the line count is what limits history in practice.
    #[must_use]
    pub fn scrollback_bytes(&self) -> u64 {
        (self.scrollback_lines * 4096).clamp(16 * 1024 * 1024, 1024 * 1024 * 1024)
    }

    /// Light themes answer `CSI ? 996 n` with "light".
    #[must_use]
    pub fn is_light(&self) -> bool {
        let c = |s: u32| f64::from((self.background >> s) & 0xff);
        (0.2126 * c(16) + 0.7152 * c(8) + 0.0722 * c(0)) / 255.0 > 0.5
    }

    /// Clamp out-of-range values (colors to 24 bits, scrollback to bounds).
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        for c in [&mut self.foreground, &mut self.background, &mut self.cursor].into_iter().chain(self.palette.iter_mut()) {
            *c &= 0x00ff_ffff;
        }
        self.scrollback_lines = self.scrollback_lines.clamp(Self::MIN_SCROLLBACK, Self::MAX_SCROLLBACK);
        self
    }
}

impl Default for TerminalConfig {
    /// Silo's built-in dark theme.
    fn default() -> Self {
        Self {
            foreground: 0x00d6_d8dc,
            background: 0x0016_1719,
            cursor: 0x00e8_e9ec,
            palette: [
                0x001d_1f21,
                0x00e0_6c75,
                0x0098_c379,
                0x00e5_c07b,
                0x0061_afef,
                0x00c6_78dd,
                0x0056_b6c2,
                0x00ab_b2bf,
                0x005c_6370,
                0x00ef_7b84,
                0x00a9_d48a,
                0x00f0_cd8c,
                0x0074_bdf8,
                0x00d5_8ae8,
                0x0066_c7d3,
                0x00e6_e9ef,
            ],
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            scrollback_lines: Self::DEFAULT_SCROLLBACK,
        }
    }
}

/// `Horizontal` places children side by side; `Vertical` stacks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Layout {
    Block {
        id: BlockId,
    },
    Split {
        axis: Axis,
        /// Fraction of the space given to `first`, in (0, 1).
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
    /// A newer server's node: shown as unsupported, left as is.
    #[serde(other)]
    Unknown,
}

impl Layout {
    #[must_use]
    pub fn block(id: impl Into<BlockId>) -> Self {
        Self::Block { id: id.into() }
    }

    /// Block ids in reading order (left-to-right, top-to-bottom).
    #[must_use]
    pub fn blocks(&self) -> Vec<BlockId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<BlockId>) {
        match self {
            Self::Block { id } => out.push(id.clone()),
            Self::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
            Self::Unknown => {}
        }
    }

    #[must_use]
    pub fn contains(&self, block: &str) -> bool {
        match self {
            Self::Block { id } => id == block,
            Self::Split { first, second, .. } => first.contains(block) || second.contains(block),
            Self::Unknown => false,
        }
    }

    /// Replace leaf `target` with a split of it and `new`, `new` second.
    pub fn split(&mut self, target: &str, new: BlockId, axis: Axis) -> bool {
        self.split_at(target, new, axis, false)
    }

    /// Replace leaf `target` with an even split of it and `new`; `before`
    /// puts `new` first (left or above), else second (right or below).
    pub fn split_at(&mut self, target: &str, new: BlockId, axis: Axis, before: bool) -> bool {
        match self {
            Self::Block { id } if id == target => {
                let (old, new) = (Box::new(Self::block(id.clone())), Box::new(Self::block(new)));
                let (first, second) = if before { (new, old) } else { (old, new) };
                *self = Self::Split { axis, ratio: 0.5, first, second };
                true
            }
            Self::Block { .. } | Self::Unknown => false,
            Self::Split { first, second, .. } => first.split_at(target, new.clone(), axis, before) || second.split_at(target, new, axis, before),
        }
    }

    /// Move leaf `block` next to leaf `target`: out of its place (its
    /// parent split collapses) and into an even split of `target` along
    /// `axis`, first when `before`. False, leaving the tree as is, unless
    /// both are distinct leaves of this tree.
    pub fn dock(&mut self, block: &str, target: &str, axis: Axis, before: bool) -> bool {
        if block == target || !self.contains(block) || !self.contains(target) {
            return false;
        }
        // `target` stays, so the tree never empties.
        let mut rest = std::mem::replace(self, Self::Unknown).remove(block).expect("target remains");
        let docked = rest.split_at(target, block.to_owned(), axis, before);
        *self = rest;
        docked
    }

    /// Remove leaf `target`, collapsing its parent split. Returns `None` if
    /// the tree becomes empty.
    #[must_use]
    pub fn remove(self, target: &str) -> Option<Self> {
        match self {
            Self::Block { ref id } if id == target => None,
            Self::Block { .. } | Self::Unknown => Some(self),
            Self::Split { axis, ratio, first, second } => match ((*first).remove(target), (*second).remove(target)) {
                (Some(a), Some(b)) => Some(Self::Split { axis, ratio, first: Box::new(a), second: Box::new(b) }),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            },
        }
    }

    /// Exchange the positions of leaves `a` and `b`; splits (axes, ratios)
    /// are untouched. False unless both are distinct leaves of this tree.
    pub fn swap(&mut self, a: &str, b: &str) -> bool {
        if a == b || !self.contains(a) || !self.contains(b) {
            return false;
        }
        fn walk(layout: &mut Layout, a: &str, b: &str) {
            match layout {
                Layout::Block { id } if id == a => b.clone_into(id),
                Layout::Block { id } if id == b => a.clone_into(id),
                Layout::Block { .. } | Layout::Unknown => {}
                Layout::Split { first, second, .. } => {
                    walk(first, a, b);
                    walk(second, a, b);
                }
            }
        }
        walk(self, a, b);
        true
    }

    /// Set the ratio of the split reached by `path` (0 = first child,
    /// 1 = second child at each level; empty = this node).
    pub fn set_ratio(&mut self, path: &[usize], ratio: f32) -> bool {
        match (self, path.split_first()) {
            (Self::Split { ratio: r, .. }, None) => {
                *r = ratio.clamp(0.05, 0.95);
                true
            }
            (Self::Split { first, second, .. }, Some((&index, rest))) => {
                if index == 0 {
                    first.set_ratio(rest, ratio)
                } else {
                    second.set_ratio(rest, ratio)
                }
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WindowInfo {
    pub id: WindowId,
    #[serde(default)]
    pub name: Option<String>,
    pub layout: Layout,
    /// A tiled block or a float.
    pub focused: BlockId,
    /// Tiled blocks only.
    #[serde(default)]
    pub zoomed: bool,
    /// Back to front; the last is topmost.
    pub floats: Vec<Float>,
}

/// A block floating above a window's layout; the block id is the float id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Float {
    pub id: BlockId,
    pub rect: FloatRect,
}

/// Fractions of the window body, 0..1.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FloatRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl FloatRect {
    pub const MIN: f32 = 0.15;

    /// `w,h` within `[MIN, 1]` and the rect inside `0..1`; non-finite
    /// values take the default's.
    #[must_use]
    pub fn clamped(self) -> Self {
        let d = Self::default();
        let fix = |v: f32, d: f32| if v.is_finite() { v } else { d };
        let (w, h) = (fix(self.w, d.w).clamp(Self::MIN, 1.0), fix(self.h, d.h).clamp(Self::MIN, 1.0));
        Self { x: fix(self.x, d.x).clamp(0.0, 1.0 - w), y: fix(self.y, d.y).clamp(0.0, 1.0 - h), w, h }
    }
}

impl Default for FloatRect {
    /// Centred, 60% × 60%.
    fn default() -> Self {
        Self { x: 0.2, y: 0.2, w: 0.6, h: 0.6 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub windows: Vec<WindowInfo>,
    pub active_window: WindowId,
    pub revision: u64,
    pub created_at: u64,
    /// Blocks of the session in no layout (`block.spawn`, `block.park`),
    /// oldest first; `block.reveal` shows one.
    #[serde(default)]
    pub parked: Vec<Parked>,
}

/// A parked block and the block that owns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Parked {
    pub id: BlockId,
    /// The block that started it (e.g. an agent block): closing that block
    /// closes this one.
    #[serde(default)]
    pub owner: Option<BlockId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    #[default]
    Terminal,
    Browser,
    /// An otto session shown as a chat; no process.
    Agent,
    /// A read-only view of a directory on the server (feature `files`);
    /// no process.
    Files,
    /// A review of code changes in a git repo on the server (feature
    /// `changes`, docs/CHANGES_BLOCK.md); no process.
    Changes,
    /// A block kind of a newer server: shown as unsupported.
    #[serde(other)]
    Unknown,
}

/// Whether a block still has a process behind it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    /// A live PTY and program; input is delivered.
    #[default]
    Running,
    /// The program exited; contents stay readable, input is ignored.
    Exited,
    /// Rebuilt from a saved snapshot (crash, reboot); no process, input is
    /// ignored, exit status unknown.
    Restored,
    /// A newer server's state: treated as not running.
    #[serde(other)]
    Unknown,
}

impl Lifecycle {
    #[must_use]
    pub fn is_running(self) -> bool {
        self == Self::Running
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BlockInfo {
    pub id: BlockId,
    pub session_id: SessionId,
    pub kind: BlockKind,
    pub title: String,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Exit code once the process has exited; `None` while running, when
    /// killed by a signal, or for restored blocks whose exit status is
    /// unknown. Use `lifecycle` to tell whether a process is running.
    #[serde(default)]
    pub exited: Option<i32>,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    pub size: TermSize,
    #[serde(default)]
    pub owner: Option<ClientId>,
    /// The owner's short client name (`Silo`, `silo cli`), with the
    /// paired device's name appended for remote owners.
    pub owner_name: Option<String>,
    /// The block that started this one with `block.spawn` (e.g. an agent
    /// block running a command), for the block's whole life, shown or
    /// parked: closing that block closes this one, and apps park rather
    /// than close a running owned block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_block: Option<BlockId>,
    #[serde(default)]
    pub pid: Option<i32>,
    /// Engine state; `Some` exactly when `kind` is [`BlockKind::Browser`].
    #[serde(default)]
    pub browser: Option<BrowserState>,
    /// Shown otto session; `Some` exactly when `kind` is [`BlockKind::Agent`].
    #[serde(default)]
    pub agent: Option<AgentState>,
    /// View state; `Some` exactly when `kind` is [`BlockKind::Files`]
    /// (boxed: most blocks have none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Box<FilesState>>,
    /// View state; `Some` exactly when `kind` is [`BlockKind::Changes`]
    /// (boxed: most blocks have none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Box<ChangesState>>,
}

/// Server-held state of a changes block (docs/CHANGES_BLOCK.md §1). Paths
/// are relative to `root`, `/`-separated, without `.` or `..` components.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangesState {
    /// The repo's top level (absolute, canonical); the block never reads
    /// outside it. Not a git repo: the folder it was opened in
    /// (`changes.status` answers `repo: false`).
    pub root: String,
    /// What is compared.
    #[serde(default)]
    pub scope: ChangesScope,
    /// Open file tabs, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<String>,
    /// The file shown; `None`: the summary of every file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// The file list is shown.
    #[serde(default = "shown")]
    pub list: bool,
    /// Side by side; false: unified.
    #[serde(default)]
    pub split: bool,
    /// Context lines around each hunk.
    #[serde(default = "ChangesState::default_context")]
    pub context: u32,
    /// Review comments not sent yet (docs/CHANGES_BLOCK.md §5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
    /// The agent the review goes back to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentLink>,
    /// Lines an agent marked with `changes_show`, with notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<ChangeMark>,
}

impl ChangesState {
    /// Most open tabs.
    pub const OPEN_MAX: usize = 100;
    /// Most review comments.
    pub const COMMENTS_MAX: usize = 500;
    /// Longest comment, in characters.
    pub const COMMENT_MAX: usize = 10_000;
    /// Most context lines.
    pub const CONTEXT_MAX: u32 = 10_000;

    #[must_use]
    pub const fn default_context() -> u32 {
        3
    }

    /// A fresh state on `root`: the working tree, no tabs, the file list
    /// hidden (the diff gets the width; the toolbar shows it).
    #[must_use]
    pub fn new(root: String) -> Self {
        Self {
            root,
            scope: ChangesScope::default(),
            open: Vec::new(),
            active: None,
            list: false,
            split: false,
            context: Self::default_context(),
            comments: Vec::new(),
            agent: None,
            marks: Vec::new(),
        }
    }
}

/// What a changes block compares.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangesScope {
    /// Work tree and index against `HEAD`, untracked files included.
    #[default]
    Working,
    /// The index against `HEAD`.
    Staged,
    /// The work tree against the index, untracked files included.
    Unstaged,
    /// Between two tree ids (otto's turn snapshots); `head` `None`: the
    /// work tree now (untracked files included). Read-only with a `head`.
    Tree {
        base: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<String>,
        /// "Turn 4", "This session".
        #[serde(default)]
        label: String,
    },
    /// Between revisions (`base` "main", `head` "otto"); `head` `None`:
    /// the work tree (untracked files included). Read-only with a `head`.
    Range {
        base: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<String>,
    },
    /// A scope of a newer peer.
    #[serde(other)]
    Unknown,
}

impl ChangesScope {
    /// Stage, unstage and revert are refused (`bad_request`).
    #[must_use]
    pub fn read_only(&self) -> bool {
        match self {
            Self::Tree { head, .. } | Self::Range { head, .. } => head.is_some(),
            Self::Unknown => true,
            Self::Working | Self::Staged | Self::Unstaged => false,
        }
    }
}

/// A side of a diff.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    #[default]
    New,
}

/// A review comment on line `line` (to `end_line`) of `path`'s `side`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewComment {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub side: Side,
    /// 1-based.
    pub line: u32,
    /// The range's last line (inclusive), when more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub text: String,
    /// Unix ms.
    #[serde(default)]
    pub created_at: u64,
}

/// The agent a changes block's review goes back to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLink {
    /// The otto session.
    pub otto_session: String,
    /// The agent block that shows the session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<BlockId>,
}

/// A line (new side, 1-based) of a file marked by an agent's
/// `changes_show`, with an optional short note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangeMark {
    pub path: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ChangeMark {
    /// Most marks per block.
    pub const MAX: usize = 50;
    /// Longest note, in characters.
    pub const NOTE_MAX: usize = 300;
}

/// Server-held state of a files block (docs/FILES_BLOCK.md §1). Paths are
/// relative to `root`, `/`-separated, without `.` or `..` components.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FilesState {
    /// Absolute, canonical path; the block never reads outside it.
    pub root: String,
    /// Open tabs, in order.
    #[serde(default)]
    pub open: Vec<String>,
    /// The tab shown.
    #[serde(default)]
    pub active: Option<String>,
    /// The tree is shown.
    #[serde(default = "shown")]
    pub tree: bool,
    /// Expanded directories in the tree.
    #[serde(default)]
    pub expanded: Vec<String>,
    /// Top line per open tab.
    #[serde(default)]
    pub scroll: std::collections::BTreeMap<String, u64>,
    /// Marked line ranges per open tab (an agent's `files_show`); they go
    /// with the tab when it closes. Additive: older peers ignore it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub highlights: std::collections::BTreeMap<String, Vec<FileHighlight>>,
    /// Which view the block shows: the editor (tabs, tree) or the
    /// Finder-like browser. Additive: older peers ignore it (and show the
    /// editor).
    #[serde(default, skip_serializing_if = "FilesView::is_edit")]
    pub view: FilesView,
    /// The folder the browser shows (empty: the root); watched like the
    /// expanded folders. Additive.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub browse: String,
}

/// A files block's view (`FilesState::view`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FilesView {
    /// A Finder-like list of one folder, with file operations.
    Browse,
    /// Tabs of files beside the folder tree; also any view of a newer peer.
    #[default]
    #[serde(other)]
    Edit,
}

impl FilesView {
    #[must_use]
    pub fn is_edit(&self) -> bool {
        *self == Self::Edit
    }
}

/// Lines `start..=end` (1-based) of a file marked in a files block, with
/// an optional short note shown beside them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileHighlight {
    pub start: u64,
    pub end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl FileHighlight {
    /// Most highlights per file.
    pub const PER_FILE: usize = 50;
    /// Longest note, in characters.
    pub const NOTE_MAX: usize = 300;
}

fn shown() -> bool {
    true
}

impl FilesState {
    /// Most open tabs.
    pub const OPEN_MAX: usize = 100;
    /// Most expanded directories.
    pub const EXPANDED_MAX: usize = 1000;
    /// Longest path accepted.
    pub const PATH_MAX: usize = 4096;

    /// A fresh state showing `root`: no tabs, the tree shown.
    #[must_use]
    pub fn new(root: String) -> Self {
        Self {
            root,
            open: Vec::new(),
            active: None,
            tree: true,
            expanded: Vec::new(),
            scroll: std::collections::BTreeMap::new(),
            highlights: std::collections::BTreeMap::new(),
            view: FilesView::Edit,
            browse: String::new(),
        }
    }
}

/// Server-held state of an agent block; the app sets it with `agent.set`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentState {
    /// The shown otto session; `None` until the first prompt.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The otto session title; empty before the first prompt.
    #[serde(default)]
    pub title: String,
}

/// A client name up to its first ` (`, without a trailing version:
/// `Silo 0.1.0 (macOS, pid 7)` gives `Silo`; `silo cli on mini` stays.
#[must_use]
pub fn short_client_name(name: &str) -> &str {
    let name = name.split(" (").next().unwrap_or_default().trim();
    match name.rsplit_once(' ') {
        Some((head, version)) if version.starts_with(|c: char| c.is_ascii_digit()) => head,
        _ => name,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryEntry {
    pub url: String,
    #[serde(default)]
    pub title: String,
}

/// Server-held state of a browser block. The owning client's engine reports
/// it; commands edit it directly so the next client opens there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BrowserState {
    /// Committed URL of the current entry.
    pub url: String,
    /// Back/forward list, at most [`BrowserState::HISTORY`] entries.
    pub history: Vec<HistoryEntry>,
    pub index: usize,
    pub title: String,
    /// `data:` URL, at most [`BrowserState::FAVICON`] bytes.
    pub favicon: Option<String>,
    /// CEF zoom level; 0.0 = 100%.
    pub zoom: f32,
    /// `default` or a name; selects the client-side profile directory.
    pub profile: String,
    pub loading: bool,
}

impl BrowserState {
    pub const HISTORY: usize = 50;
    pub const FAVICON: usize = 32 * 1024;
    pub const PROFILE: &str = "default";

    /// A fresh state showing `url` (already normalized).
    #[must_use]
    pub fn new(url: String) -> Self {
        let history = vec![HistoryEntry { url: url.clone(), title: String::new() }];
        Self { url, history, index: 0, title: String::new(), favicon: None, zoom: 0.0, profile: Self::PROFILE.into(), loading: false }
    }

    /// Navigate: drop forward entries, push `url`, keep the newest
    /// [`Self::HISTORY`].
    pub fn push(&mut self, url: String) {
        self.history.truncate(self.index + 1);
        self.history.push(HistoryEntry { url: url.clone(), title: String::new() });
        let excess = self.history.len().saturating_sub(Self::HISTORY);
        self.history.drain(..excess);
        self.index = self.history.len() - 1;
        self.url = url;
        self.title.clear();
        self.favicon = None;
    }

    /// Move `delta` entries through history; false at either end.
    pub fn go(&mut self, delta: isize) -> bool {
        let Some(index) = self.index.checked_add_signed(delta).filter(|&i| i < self.history.len()) else {
            return false;
        };
        self.index = index;
        let entry = &self.history[index];
        (self.url, self.title) = (entry.url.clone(), entry.title.clone());
        self.favicon = None;
        true
    }
}

/// Normalize typed input to a URL a browser block may load. Schemes are
/// limited to `http`, `https`, `ws`, `wss`, `data:`, `about:blank` and
/// `view-source:` of an `http(s)` URL; any other (`file:`, `chrome:`,
/// `javascript:`…) gives `None`, as does empty input. A bare host gets
/// `http://` when server-private (localhost, loopback, private or
/// link-local IPs) and `https://` otherwise.
#[must_use]
pub fn browser_url(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some((scheme, rest)) = input.split_once(':') {
        let token = scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c));
        // `host:port` is not a scheme.
        if token && !rest.starts_with(|c: char| c.is_ascii_digit()) {
            let scheme = scheme.to_ascii_lowercase();
            let allowed = match scheme.as_str() {
                "http" | "https" | "ws" | "wss" => rest.starts_with("//"),
                "data" => true,
                "about" => rest == "blank",
                "view-source" => browser_url(rest).is_some_and(|page| {
                    let scheme = page.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
                    page == rest && matches!(scheme.as_deref(), Some("http" | "https"))
                }),
                _ => false,
            };
            return allowed.then(|| input.to_owned());
        }
    }
    let authority = input.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    let scheme = if private_host(host) { "http" } else { "https" };
    Some(format!("{scheme}://{input}"))
}

/// Whether `host` names the server machine or its private network.
#[must_use]
pub fn private_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
        Err(_) => false,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    pub session: SessionInfo,
    pub blocks: Vec<BlockInfo>,
    /// Status of the session's blocks that have any (never bumps
    /// `revision`).
    #[serde(default)]
    pub status: Vec<BlockStatus>,
    /// Clients with a data stream on a block of the session.
    #[serde(default)]
    pub watchers: Vec<Watcher>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSummary {
    pub id: SessionId,
    pub name: String,
    pub windows: usize,
    pub blocks: usize,
    pub attached_clients: usize,
    pub created_at: u64,
    #[serde(default)]
    pub attention: AttentionCounts,
}

/// A program's status report (OSC 7501), as published.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProgramStatus {
    /// `""` is the root record; `a/b` lies beneath `a`.
    pub id: String,
    pub state: StatusState,
    #[serde(default)]
    pub kind: Option<StatusKind>,
    /// Percent, 0..=100.
    #[serde(default)]
    pub progress: Option<u8>,
    /// Effective (inherited from the nearest ancestor).
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub message: String,
    /// Unix ms.
    pub updated_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StatusState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    #[serde(other)]
    Unknown,
}

/// What a blocked program waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StatusKind {
    Permission,
    Question,
    Auth,
    #[serde(other)]
    Unknown,
}

/// OSC 9;4 progress of a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Progress {
    pub state: ProgressState,
    #[serde(default)]
    pub percent: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProgressState {
    Set,
    Error,
    Indeterminate,
    Pause,
    #[serde(other)]
    Unknown,
}

/// A running program a close would end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Busy {
    /// The foreground program's name, e.g. `vim`.
    pub process: String,
    /// The command line, when the shell reported one (OSC 133).
    #[serde(default)]
    pub command: Option<String>,
    /// Unix ms when this busy period began.
    pub since: u64,
}

/// Everything a block reports about its programs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BlockStatus {
    pub block_id: BlockId,
    #[serde(default)]
    pub records: Vec<ProgramStatus>,
    #[serde(default)]
    pub progress: Option<Progress>,
    #[serde(default)]
    pub busy: Option<Busy>,
}

/// Records per state across a session's blocks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttentionCounts {
    #[serde(default)]
    pub working: u32,
    #[serde(default)]
    pub blocked: u32,
    #[serde(default)]
    pub done: u32,
    #[serde(default)]
    pub error: u32,
}

/// One command a shell ran (OSC 133 C..D).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandRecord {
    /// Monotonic per block.
    pub index: u64,
    #[serde(default)]
    pub command: Option<String>,
    /// `None` while running or when the shell sent none.
    #[serde(default)]
    pub exit: Option<i32>,
    /// Unix ms; `None` without an output-start mark.
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// How a watcher is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WatcherKind {
    Owner,
    Device,
    Token,
    Link,
    #[serde(other)]
    Unknown,
}

/// A client with a data stream on blocks of a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Watcher {
    pub client_id: ClientId,
    pub label: String,
    pub principal: WatcherKind,
    pub rights: crate::access::Rights,
    pub blocks: Vec<BlockId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_url_picks_scheme() {
        assert_eq!(browser_url(" example.com/a ").as_deref(), Some("https://example.com/a"));
        assert_eq!(browser_url("localhost:3000").as_deref(), Some("http://localhost:3000"));
        assert_eq!(browser_url("app.localhost").as_deref(), Some("http://app.localhost"));
        assert_eq!(browser_url("192.168.1.4:8080/x").as_deref(), Some("http://192.168.1.4:8080/x"));
        assert_eq!(browser_url("[::1]:5173").as_deref(), Some("http://[::1]:5173"));
        assert_eq!(browser_url("8.8.8.8").as_deref(), Some("https://8.8.8.8"));
        assert_eq!(browser_url("http://example.com").as_deref(), Some("http://example.com"));
        assert_eq!(browser_url("about:blank").as_deref(), Some("about:blank"));
        assert_eq!(browser_url("  "), None);
        assert_eq!(browser_url("WSS://h/s").as_deref(), Some("WSS://h/s"));
        assert_eq!(browser_url("data:text/html,hi").as_deref(), Some("data:text/html,hi"));
        assert_eq!(browser_url("view-source:https://x/a").as_deref(), Some("view-source:https://x/a"));
        assert_eq!(browser_url("view-source:http://localhost:3000").as_deref(), Some("view-source:http://localhost:3000"));
        assert_eq!(browser_url("example.com:8080/x").as_deref(), Some("https://example.com:8080/x"));
        for bad in [
            "file:///etc/passwd",
            "file:/etc/passwd",
            "chrome://settings",
            "devtools://devtools",
            "javascript:alert(1)",
            "about:config",
            "http:example.com",
            "view-source:example.com",
            "view-source:file:///etc/passwd",
            "view-source:view-source:https://x",
            "view-source:data:text/html,hi",
            "view-source:javascript:alert(1)",
        ] {
            assert_eq!(browser_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn browser_history_push_go_and_cap() {
        let mut state = BrowserState::new("a".into());
        state.push("b".into());
        state.push("c".into());
        assert!(state.go(-2));
        assert_eq!((state.url.as_str(), state.index), ("a", 0));
        assert!(!state.go(-1));
        state.push("d".into());
        let urls: Vec<_> = state.history.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(urls, ["a", "d"], "forward entries dropped");
        for i in 0..100 {
            state.push(format!("u{i}"));
        }
        assert_eq!(state.history.len(), BrowserState::HISTORY);
        assert_eq!((state.index, state.url.as_str()), (BrowserState::HISTORY - 1, "u99"));
        assert!(!state.go(1));
    }

    #[test]
    fn short_client_name_drops_details_and_version() {
        assert_eq!(short_client_name("Silo 0.1.0 (macOS, pid 7)"), "Silo");
        assert_eq!(short_client_name("Silo 0.1.0"), "Silo");
        assert_eq!(short_client_name("silo cli"), "silo cli");
        assert_eq!(short_client_name("silo cli on mini"), "silo cli on mini");
        assert_eq!(short_client_name(""), "");
    }

    #[test]
    fn float_rect_clamped_keeps_size_and_bounds() {
        let r = |x, y, w, h| FloatRect { x, y, w, h };
        assert_eq!(FloatRect::default().clamped(), FloatRect::default());
        assert_eq!(r(0.9, -0.5, 0.5, 0.05).clamped(), r(0.5, 0.0, 0.5, FloatRect::MIN));
        assert_eq!(r(-1.0, 2.0, 3.0, 1.0).clamped(), r(0.0, 0.0, 1.0, 1.0));
        assert_eq!(r(f32::NAN, 0.1, f32::INFINITY, 0.3).clamped(), r(0.2, 0.1, 0.6, 0.3));
    }

    #[test]
    fn split_and_remove_collapse() {
        let mut layout = Layout::block("a");
        assert!(layout.split("a", "b".into(), Axis::Horizontal));
        assert!(layout.split("b", "c".into(), Axis::Vertical));
        assert_eq!(layout.blocks(), ["a", "b", "c"]);
        let layout = layout.remove("b").unwrap();
        assert_eq!(layout.blocks(), ["a", "c"]);
        let layout = layout.remove("a").unwrap();
        assert_eq!(layout, Layout::block("c"));
        assert!(layout.remove("c").is_none());
    }

    #[test]
    fn swap_exchanges_leaves_and_keeps_splits() {
        let mut layout = Layout::block("a");
        layout.split("a", "b".into(), Axis::Horizontal);
        layout.split("b", "c".into(), Axis::Vertical);
        layout.set_ratio(&[], 0.3);
        layout.set_ratio(&[1], 0.7);
        let before = layout.clone();
        assert!(layout.swap("a", "c"));
        assert_eq!(layout.blocks(), ["c", "b", "a"]);
        let Layout::Split { axis, ratio, second, .. } = &layout else { panic!() };
        assert_eq!((*axis, *ratio), (Axis::Horizontal, 0.3));
        assert!(matches!(**second, Layout::Split { axis: Axis::Vertical, ratio, .. } if ratio == 0.7));
        assert!(!layout.swap("a", "a"));
        assert!(!layout.swap("a", "x"));
        assert!(layout.swap("c", "a"));
        assert_eq!(layout, before);
    }

    #[test]
    fn split_at_places_the_new_leaf_on_either_side() {
        let mut layout = Layout::block("a");
        assert!(layout.split_at("a", "b".into(), Axis::Horizontal, true));
        assert_eq!(layout.blocks(), ["b", "a"]);
        assert!(layout.split_at("a", "c".into(), Axis::Vertical, false));
        assert_eq!(layout.blocks(), ["b", "a", "c"]);
        assert!(!layout.split_at("x", "d".into(), Axis::Vertical, true));
    }

    fn split(axis: Axis, ratio: f32, first: Layout, second: Layout) -> Layout {
        Layout::Split { axis, ratio, first: Box::new(first), second: Box::new(second) }
    }

    #[test]
    fn dock_moves_a_leaf_to_each_side_of_another() {
        let (a, b, c) = (Layout::block("a"), Layout::block("b"), Layout::block("c"));
        // a | b | c (b and c nested); dock c on each side of a.
        let start = split(Axis::Horizontal, 0.3, a.clone(), split(Axis::Horizontal, 0.6, b.clone(), c.clone()));
        let cases = [
            (Axis::Horizontal, true, split(Axis::Horizontal, 0.5, c.clone(), a.clone())),
            (Axis::Horizontal, false, split(Axis::Horizontal, 0.5, a.clone(), c.clone())),
            (Axis::Vertical, true, split(Axis::Vertical, 0.5, c.clone(), a.clone())),
            (Axis::Vertical, false, split(Axis::Vertical, 0.5, a.clone(), c.clone())),
        ];
        for (axis, before, docked) in cases {
            let mut layout = start.clone();
            assert!(layout.dock("c", "a", axis, before), "{axis:?} {before}");
            // c's old split collapsed into b; a's slot keeps the root ratio.
            assert_eq!(layout, split(Axis::Horizontal, 0.3, docked, b.clone()), "{axis:?} {before}");
        }
    }

    #[test]
    fn dock_under_a_leaf_in_a_nested_split() {
        // (a | b) with c below both: drag c under a, giving (a / c) | b.
        let (a, b, c) = (Layout::block("a"), Layout::block("b"), Layout::block("c"));
        let mut layout = split(Axis::Vertical, 0.7, split(Axis::Horizontal, 0.4, a.clone(), b.clone()), c.clone());
        assert!(layout.dock("c", "a", Axis::Vertical, false));
        assert_eq!(layout, split(Axis::Horizontal, 0.4, split(Axis::Vertical, 0.5, a.clone(), c.clone()), b.clone()));
        // And back up above b, deep in the other branch.
        assert!(layout.dock("c", "b", Axis::Vertical, true));
        assert_eq!(layout, split(Axis::Horizontal, 0.4, a.clone(), split(Axis::Vertical, 0.5, c.clone(), b.clone())));
        // Two leaves: docking just rearranges them.
        let mut two = split(Axis::Horizontal, 0.3, a.clone(), b.clone());
        assert!(two.dock("a", "b", Axis::Vertical, false));
        assert_eq!(two, split(Axis::Vertical, 0.5, b, a));
    }

    #[test]
    fn dock_refuses_itself_and_strangers() {
        let mut layout = Layout::block("a");
        layout.split("a", "b".into(), Axis::Horizontal);
        let before = layout.clone();
        assert!(!layout.dock("a", "a", Axis::Vertical, false));
        assert!(!layout.dock("x", "a", Axis::Vertical, false));
        assert!(!layout.dock("a", "x", Axis::Vertical, false));
        assert_eq!(layout, before);
    }

    #[test]
    fn newer_layout_nodes_and_states_parse() {
        let json = serde_json::json!({"type": "split", "axis": "horizontal", "ratio": 0.5,
            "first": {"type": "block", "id": "a"}, "second": {"type": "stack", "ids": ["b"]}});
        let mut layout: Layout = serde_json::from_value(json).unwrap();
        assert_eq!(layout.blocks(), ["a"]);
        assert!(!layout.split("b", "c".into(), Axis::Vertical));
        assert_eq!(layout.clone().remove("a"), Some(Layout::Unknown));
        let state: Lifecycle = serde_json::from_value(serde_json::json!("paused")).unwrap();
        assert!(!state.is_running());
    }
}
