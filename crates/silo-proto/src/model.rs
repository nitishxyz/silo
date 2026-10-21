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

    /// Replace leaf `target` with a split of it and `new`.
    pub fn split(&mut self, target: &str, new: BlockId, axis: Axis) -> bool {
        match self {
            Self::Block { id } if id == target => {
                let old = Self::block(id.clone());
                *self = Self::Split { axis, ratio: 0.5, first: Box::new(old), second: Box::new(Self::block(new)) };
                true
            }
            Self::Block { .. } | Self::Unknown => false,
            Self::Split { first, second, .. } => first.split(target, new.clone(), axis) || second.split(target, new, axis),
        }
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    #[default]
    Terminal,
    Browser,
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
    #[serde(default)]
    pub pid: Option<i32>,
    /// Engine state; `Some` exactly when `kind` is [`BlockKind::Browser`].
    #[serde(default)]
    pub browser: Option<BrowserState>,
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
/// limited to `http`, `https`, `ws`, `wss`, `data:` and `about:blank`; any
/// other (`file:`, `chrome:`, `javascript:`…) gives `None`, as does empty
/// input. A bare host gets `http://` when server-private (localhost,
/// loopback, private or link-local IPs) and `https://` otherwise.
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
        assert_eq!(browser_url("example.com:8080/x").as_deref(), Some("https://example.com:8080/x"));
        for bad in [
            "file:///etc/passwd",
            "file:/etc/passwd",
            "chrome://settings",
            "devtools://devtools",
            "javascript:alert(1)",
            "about:config",
            "http:example.com",
            "view-source:https://x",
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
