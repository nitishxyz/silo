//! ⌘P finder: a list of sources (commands, settings, sessions, machines,
//! tabs, every block of the session, recent pages, an address, files and
//! their text, agent sessions, changed files) that each turn the query into
//! scored entries, ranked and grouped under section labels. A category chip
//! (docs/FINDER.md §1) narrows it to one source. The palette (`palette.rs`)
//! shows and runs them.

use std::{ops::Range, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    block_list::BlockRow,
    commands::{Command, fuzzy},
    hosts::HostState,
    settings_panel::SettingsSnapshot,
    settings_table::{Kind, SETTINGS, SettingId},
    workspace::Workspace,
};

pub const PLACEHOLDER: &str = "Search commands, settings, sessions, blocks…";
/// The query Browse Blocks opens on: it turns into the Blocks chip.
pub const BLOCKS_QUERY: &str = "#";
/// Files shown without a chip, once the query has [`FILES_MIN`] characters.
pub const FILES_TOP: usize = 5;
pub const FILES_MIN: usize = 2;
/// Files asked for under the Files chip.
pub const FILES_LIMIT: usize = 50;
/// Matches asked for under the Grep chip.
pub const GREP_LIMIT: usize = 100;
/// Finder picks remembered for the empty query and the recency boost.
pub const RECENT_CAP: usize = 20;
/// Entries per group when searching everything.
const PER_GROUP: usize = 6;
/// Recent pages offered.
const PAGES: usize = 20;
/// Shown under the recent picks for an empty query.
const COMMON: [Command; 6] =
    [Command::NewTab, Command::SplitRight, Command::NewSession, Command::SwitchSession, Command::NewBrowserTab, Command::OpenSettings];

/// A section of the results, in display order for an empty query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// The `@` list.
    Categories,
    Open,
    Recent,
    Commands,
    Settings,
    Sessions,
    Machines,
    Tabs,
    /// Every block of the session: tiled, floating and background.
    Blocks,
    Pages,
    Files,
    Grep,
    Agents,
    Changes,
}

impl Group {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Categories => "Search in",
            Self::Open => "Open",
            Self::Recent => "Recent",
            Self::Commands => "Commands",
            Self::Settings => "Settings",
            Self::Sessions => "Sessions",
            Self::Machines => "Machines",
            Self::Tabs => "Tabs",
            Self::Blocks => "Blocks",
            Self::Pages => "Pages",
            Self::Files => "Files",
            Self::Grep => "Matches",
            Self::Agents => "Agents",
            Self::Changes => "Changes",
        }
    }
}

/// What a query searches: everything, or one category (a chip).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    All,
    Files,
    Grep,
    /// `#`; Browse Blocks.
    Blocks,
    /// Workspaces (sessions) and machines.
    Spaces,
    Tabs,
    Agents,
    /// `>`
    Commands,
    /// `:`
    Settings,
    Pages,
    Changes,
}

/// The categories of the `@` list, in order.
pub const CATEGORIES: [Scope; 10] = [
    Scope::Files,
    Scope::Grep,
    Scope::Blocks,
    Scope::Spaces,
    Scope::Tabs,
    Scope::Agents,
    Scope::Commands,
    Scope::Settings,
    Scope::Pages,
    Scope::Changes,
];

/// The server feature file and content search need.
pub const SEARCH_FEATURE: &str = "search";

impl Scope {
    #[must_use]
    pub fn includes(self, group: Group) -> bool {
        match self {
            Self::All => !matches!(group, Group::Categories | Group::Grep | Group::Agents | Group::Changes),
            Self::Files => group == Group::Files,
            Self::Grep => group == Group::Grep,
            Self::Commands => group == Group::Commands,
            Self::Settings => group == Group::Settings,
            Self::Spaces => matches!(group, Group::Sessions | Group::Machines),
            Self::Tabs => group == Group::Tabs,
            Self::Blocks => group == Group::Blocks,
            Self::Agents => group == Group::Agents,
            Self::Pages => group == Group::Pages,
            Self::Changes => group == Group::Changes,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Files => "Files",
            Self::Grep => "Grep",
            Self::Blocks => "Blocks",
            Self::Spaces => "Spaces",
            Self::Tabs => "Tabs",
            Self::Agents => "Agents",
            Self::Commands => "Commands",
            Self::Settings => "Settings",
            Self::Pages => "Pages",
            Self::Changes => "Changes",
        }
    }

    /// The chip's and the `@` row's glyph (`assets/icons`).
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::All => "settings-search",
            Self::Files => "file-text",
            Self::Grep => "search",
            Self::Blocks => "layers",
            Self::Spaces => "laptop",
            Self::Tabs => "split-right",
            Self::Agents => "agent",
            Self::Commands => "terminal",
            Self::Settings => "settings",
            Self::Pages => "globe",
            Self::Changes => "git-diff",
        }
    }

    /// The `@` row's one-line hint.
    #[must_use]
    pub fn hint(self) -> &'static str {
        match self {
            Self::All => "",
            Self::Files => "Files in this workspace's folder",
            Self::Grep => "Text inside files",
            Self::Blocks => "Every block: tiled, floating, parked",
            Self::Spaces => "Workspaces on this Mac and other machines",
            Self::Tabs => "Tabs of this workspace",
            Self::Agents => "Agent sessions of this project",
            Self::Commands => "Run a command",
            Self::Settings => "Change a setting",
            Self::Pages => "Recent browser pages",
            Self::Changes => "Changed files of this repo",
        }
    }

    /// The field's placeholder under the chip.
    #[must_use]
    pub fn placeholder(self) -> &'static str {
        match self {
            Self::All => PLACEHOLDER,
            Self::Files => "Search files (globs, !exclude, path:line)…",
            Self::Grep => "Search text in files…",
            Self::Blocks => "Search blocks…",
            Self::Spaces => "Search workspaces and machines…",
            Self::Tabs => "Search tabs…",
            Self::Agents => "Search agent sessions…",
            Self::Commands => "Search commands…",
            Self::Settings => "Search settings…",
            Self::Pages => "Search recent pages…",
            Self::Changes => "Search changed files…",
        }
    }

    /// The old one-character prefix that types this chip.
    #[must_use]
    pub fn prefix(self) -> Option<char> {
        match self {
            Self::Commands => Some('>'),
            Self::Settings => Some(':'),
            Self::Blocks => Some('#'),
            _ => None,
        }
    }

    /// The server feature the category needs.
    #[must_use]
    pub fn feature(self) -> Option<&'static str> {
        match self {
            Self::Files | Self::Grep => Some(SEARCH_FEATURE),
            Self::Changes => Some(silo_proto::compat::feature::CHANGES),
            _ => None,
        }
    }

    /// What ⌫ on an empty field puts back for this chip.
    #[must_use]
    pub fn typed(self) -> String {
        format!("@{}", self.label().to_lowercase())
    }
}

/// What the field shows: the `@` list (filtered by the text after `@`), or
/// results of a scope for the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode<'a> {
    Categories(&'a str),
    Search(Scope, &'a str),
}

/// The field's mode for `text` typed after `chip`.
#[must_use]
pub fn mode(chip: Option<Scope>, text: &str) -> Mode<'_> {
    match chip {
        Some(scope) => Mode::Search(scope, text.trim()),
        None => match text.trim_start().strip_prefix('@') {
            Some(filter) => Mode::Categories(filter.trim()),
            None => Mode::Search(Scope::All, text.trim()),
        },
    }
}

/// What Esc does in the finder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Escape {
    /// The chip goes; the text stays.
    Unchip,
    Close,
}

/// Esc with `chip`: the chip first, then the finder.
#[must_use]
pub fn escape(chip: Option<Scope>) -> Escape {
    if chip.is_some() { Escape::Unchip } else { Escape::Close }
}

/// ⌫ with `chip` on field `text`: on an empty field the chip goes and the
/// text it came from (`@files`) comes back, to be selected; otherwise
/// `None` (an ordinary ⌫).
#[must_use]
pub fn backspace(chip: Option<Scope>, text: &str) -> Option<String> {
    chip.filter(|_| text.is_empty()).map(Scope::typed)
}

/// An old prefix typed with no chip: the chip it turns into and the text
/// after it. `@` stays text (the category list).
#[must_use]
pub fn promote(chip: Option<Scope>, text: &str) -> Option<(Scope, String)> {
    if chip.is_some() {
        return None;
    }
    let t = text.trim_start();
    let first = t.chars().next()?;
    let scope = CATEGORIES.into_iter().find(|s| s.prefix() == Some(first))?;
    Some((scope, t[first.len_utf8()..].trim_start().to_owned()))
}

/// The `@` list for `filter`, among the categories `available`: prefix
/// matches of the label first, then fuzzy ones, else in list order.
#[must_use]
pub fn categories(filter: &str, available: impl Fn(Scope) -> bool) -> Vec<(Scope, Match)> {
    let mut out: Vec<(Scope, Match)> =
        CATEGORIES.into_iter().filter(|s| available(*s)).filter_map(|s| Some((s, score(filter, s.label(), s.hint())?))).collect();
    out.sort_by_key(|(_, m)| (std::cmp::Reverse(m.tier), std::cmp::Reverse(m.score)));
    out
}

/// Whether `scope` is offered: its feature is supported.
#[must_use]
pub fn offered(scope: Scope, supports: impl Fn(&str) -> bool) -> bool {
    scope.feature().is_none_or(supports)
}

/// A server search a query needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Files { query: String, limit: usize },
    Grep { query: String, limit: usize },
}

/// The server search for `text` under `scope` (`search`: the server has
/// feature `search`): the Files chip always (an empty query lists the
/// frecent files), Grep for some text, no chip the top files at
/// [`FILES_MIN`] characters.
#[must_use]
pub fn request(scope: Scope, text: &str, search: bool) -> Option<Request> {
    let query = text.trim().to_owned();
    if !search {
        return None;
    }
    match scope {
        Scope::Files => Some(Request::Files { query, limit: FILES_LIMIT }),
        Scope::Grep if !query.is_empty() => Some(Request::Grep { query, limit: GREP_LIMIT }),
        Scope::All if query.chars().count() >= FILES_MIN && address(&query).is_none() => Some(Request::Files { query, limit: FILES_TOP }),
        _ => None,
    }
}

/// The line a Files query asks for: `path:42` or `path:42:7`.
#[must_use]
pub fn query_line(query: &str) -> Option<u32> {
    let mut parts = query.trim().rsplit(':');
    let last = parts.next()?;
    let before = parts.next()?;
    let line = match (last.parse::<u32>(), before.parse::<u32>()) {
        // `path:line:col`, unless the path is a number itself.
        (Ok(_), Ok(line)) if parts.next().is_some() => line,
        (Ok(line), _) => line,
        _ => return None,
    };
    (line > 0).then_some(line)
}

/// How well an entry matches: `tier` 3 exact, 2 prefix, 1 a word's
/// prefix, 0 fuzzy or a keyword; `score` orders within a tier.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Match {
    pub tier: u8,
    pub score: i32,
    pub ranges: Vec<Range<usize>>,
}

/// Match `query` against an entry's `text` (highlighted) and `keywords`.
#[must_use]
pub fn score(query: &str, text: &str, keywords: &str) -> Option<Match> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(Match::default());
    }
    let t = text.to_lowercase();
    let tier = if t == q {
        3
    } else if t.starts_with(&q) {
        2
    } else if t.split(|c: char| !c.is_alphanumeric()).any(|w| !w.is_empty() && w.starts_with(&q)) {
        1
    } else {
        0
    };
    if let Some((score, ranges)) = fuzzy(&q, text) {
        return Some(Match { tier, score, ranges });
    }
    let keywords = keywords.to_lowercase();
    q.split_whitespace().all(|w| keywords.split_whitespace().any(|k| k.starts_with(w))).then(|| Match { tier: 0, score: -100, ranges: Vec::new() })
}

/// Where a pick goes.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Command(Command),
    Setting(SettingId),
    Session {
        host: usize,
        id: String,
    },
    Host(usize),
    RenameHost(usize),
    Upgrade(usize),
    Tab {
        index: usize,
    },
    /// A block shown in tab `index` (tiled or floating).
    Block {
        index: usize,
        block: String,
    },
    /// A background (parked) block: floated into the shown tab.
    Background(String),
    Page(String),
    OpenUrl(String),
    /// An `@` row: the chip it sets.
    Category(Scope),
    /// A file under `root` (root-relative `path`), at `line` (1-based).
    File {
        root: String,
        path: String,
        line: Option<u32>,
    },
    /// An otto session of the workspace's project.
    Agent {
        session: String,
        title: String,
    },
    /// A changed file, in Changes block `block` (`None`: no Changes block yet).
    Change {
        block: Option<String>,
        path: String,
    },
}

/// One result row.
#[derive(Clone, Debug)]
pub struct Item {
    pub target: Target,
    pub group: Group,
    /// Unhighlighted words before the matched text ("Switch to ").
    pub lead: &'static str,
    /// A glyph before it (a block's kind).
    pub icon: Option<&'static str>,
    /// The matched text.
    pub text: String,
    /// Matched too, never shown.
    pub keywords: String,
    pub detail: Option<String>,
    /// A shortcut (as a key cap) or a value, at the right.
    pub right: Option<Right>,
    pub check: bool,
    pub found: Match,
    /// Its identity in the recent picks.
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Right {
    Shortcut(String),
    Value(String),
}

impl Item {
    fn new(target: Target, group: Group, text: impl Into<String>, key: String) -> Self {
        Self {
            target,
            group,
            lead: "",
            icon: None,
            text: text.into(),
            keywords: String::new(),
            detail: None,
            right: None,
            check: false,
            found: Match::default(),
            key,
        }
    }
}

// ---- server search results ----

/// A file `search.files` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHit {
    /// Root-relative, `/`-separated.
    pub path: String,
    pub name: String,
    /// Its git status letter (M, A, ?, …).
    pub git: Option<char>,
}

/// A line `search.grep` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepHit {
    pub path: String,
    /// 1-based.
    pub line: u32,
    pub text: String,
    /// Byte ranges of the match in `text`.
    pub ranges: Vec<Range<usize>>,
}

/// A server search's answer for `query` under `root`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hits<T> {
    pub root: String,
    pub query: String,
    pub items: Vec<T>,
    pub total: usize,
    /// The server is still scanning the folder: more may come.
    pub indexing: bool,
}

/// The Files rows: the name (its matched letters highlighted), the folder
/// dimmed after it, the git letter at the right; `query`'s `:line` opens there.
#[must_use]
pub fn file_items(hits: &Hits<FileHit>) -> Vec<Item> {
    let line = query_line(&hits.query);
    let words = hits.query.split(':').next().unwrap_or_default();
    hits.items
        .iter()
        .map(|hit| {
            let target = Target::File { root: hits.root.clone(), path: hit.path.clone(), line };
            let mut item = Item::new(target, Group::Files, hit.name.clone(), format!("file:{}/{}", hits.root, hit.path));
            item.icon = Some("file-text");
            let folder = hit.path.rsplit_once('/').map(|(dir, _)| dir.to_owned());
            item.detail = folder.filter(|d| !d.is_empty());
            item.right = hit.git.map(|g| Right::Value(g.to_string()));
            item.found = Match { tier: 0, score: 0, ranges: fuzzy(&words.to_lowercase(), &hit.name).map(|(_, r)| r).unwrap_or_default() };
            item
        })
        .collect()
}

/// The Grep rows: the line, its match highlighted (leading blanks cut), with
/// `path:line` dimmed after it.
#[must_use]
pub fn grep_items(hits: &Hits<GrepHit>) -> Vec<Item> {
    hits.items
        .iter()
        .map(|hit| {
            let target = Target::File { root: hits.root.clone(), path: hit.path.clone(), line: Some(hit.line) };
            let cut = hit.text.len() - hit.text.trim_start().len();
            let text = hit.text.trim().to_owned();
            let ranges = hit
                .ranges
                .iter()
                .filter_map(|r| {
                    let r = r.start.saturating_sub(cut).min(text.len())..r.end.saturating_sub(cut).min(text.len());
                    (r.start < r.end && text.is_char_boundary(r.start) && text.is_char_boundary(r.end)).then_some(r)
                })
                .collect();
            let mut item = Item::new(target, Group::Grep, text, String::new());
            item.icon = Some("search");
            item.detail = Some(format!("{}:{}", hit.path, hit.line));
            item.found = Match { tier: 0, score: 0, ranges };
            item
        })
        .collect()
}

/// An otto session as the Agents rows show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub id: String,
    pub title: String,
    /// `agent · model`.
    pub detail: String,
    /// Shown in a block of this workspace now.
    pub open: bool,
}

/// The Agents rows, newest first as given.
#[must_use]
pub fn agent_items(rows: &[AgentRow]) -> Vec<Item> {
    rows.iter()
        .map(|row| {
            let title = if row.title.trim().is_empty() { "Untitled session".to_owned() } else { row.title.clone() };
            let target = Target::Agent { session: row.id.clone(), title: row.title.clone() };
            let mut item = Item::new(target, Group::Agents, title, format!("agent:{}", row.id));
            item.icon = Some(crate::agent_block::ICON);
            item.detail = Some(row.detail.clone());
            item.right = row.open.then(|| Right::Value("open".into()));
            item
        })
        .collect()
}

/// A changed file of the workspace's repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRow {
    pub path: String,
    /// M, A, D, R, ?, …
    pub status: char,
    pub added: u32,
    pub removed: u32,
}

/// What the Changes chip lists: the changed files read through Changes
/// block `block`, or none (no Changes block: one row opens one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
    pub block: Option<String>,
    pub files: Vec<ChangeRow>,
}

/// The Changes rows: the file name, its folder dimmed, the status letter
/// and line counts at the right; without a Changes block, New Changes Tab.
#[must_use]
pub fn change_items(changes: &Changes) -> Vec<Item> {
    let Some(block) = &changes.block else {
        let command = Command::NewChangesTab;
        let mut item = Item::new(Target::Command(command), Group::Changes, command.title(), command_key(command));
        item.icon = Some("git-diff");
        item.detail = Some("Lists the repo's changed files".into());
        return vec![item];
    };
    changes
        .files
        .iter()
        .map(|file| {
            let (folder, name) = file.path.rsplit_once('/').map_or((None, file.path.as_str()), |(d, n)| (Some(d.to_owned()), n));
            let target = Target::Change { block: Some(block.clone()), path: file.path.clone() };
            let mut item = Item::new(target, Group::Changes, name, String::new());
            item.icon = Some("git-diff");
            item.keywords.clone_from(&file.path);
            item.detail = folder;
            let counts = if file.added + file.removed > 0 { format!("+{} \u{2212}{}  ", file.added, file.removed) } else { String::new() };
            item.right = Some(Right::Value(format!("{counts}{}", file.status)));
            item
        })
        .collect()
}

/// The Agents rows of `sessions` (sub-agents left out), newest first;
/// those in `open` are shown in a block now.
#[must_use]
pub fn agent_rows(mut sessions: Vec<otto_proto::SessionInfo>, open: &[String]) -> Vec<AgentRow> {
    sessions.retain(|s| !s.is_subagent());
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    sessions
        .into_iter()
        .map(|s| AgentRow { open: open.contains(&s.id), detail: format!("{} \u{b7} {}", s.agent, s.model), id: s.id, title: s.title })
        .collect()
}

/// The Files block of the session to show file `full` (absolute) in: one
/// whose root holds it, the deepest root first; its id and `full` relative
/// to that root.
#[must_use]
pub fn files_block_for(blocks: &[silo_proto::model::BlockInfo], full: &Path) -> Option<(String, String)> {
    blocks
        .iter()
        .filter(|b| b.kind == silo_proto::model::BlockKind::Files)
        .filter_map(|b| Some((b.id.as_str(), Path::new(&b.files.as_ref()?.root))))
        .filter(|(_, root)| full.starts_with(root))
        .max_by_key(|(_, root)| root.components().count())
        .map(|(id, root)| {
            let rest = full.strip_prefix(root).unwrap_or(full);
            (id.to_owned(), rest.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
        })
}

/// The Changes block the Changes chip reads: the one whose root holds
/// `folder` (the deepest), else the first.
#[must_use]
pub fn changes_block(blocks: &[silo_proto::model::BlockInfo], folder: Option<&str>) -> Option<String> {
    let roots: Vec<(&str, &str)> = blocks
        .iter()
        .filter(|b| b.kind == silo_proto::model::BlockKind::Changes)
        .map(|b| (b.id.as_str(), b.changes.as_ref().map_or("", |c| c.root.as_str())))
        .collect();
    let holds = |root: &str| folder.is_some_and(|f| !root.is_empty() && Path::new(f).starts_with(root));
    roots.iter().filter(|(_, root)| holds(root)).max_by_key(|(_, root)| root.len()).or(roots.first()).map(|(id, _)| (*id).to_owned())
}

/// The Changes rows' files of a `changes.status`.
#[must_use]
pub fn change_rows(status: &silo_proto::changes::ChangesStatus) -> Vec<ChangeRow> {
    status.files.iter().map(|f| ChangeRow { path: f.path.clone(), status: status_letter(f.status), added: f.added, removed: f.removed }).collect()
}

/// A changed file's status letter.
#[must_use]
pub fn status_letter(status: silo_proto::changes::FileStatus) -> char {
    use silo_proto::changes::FileStatus as S;
    match status {
        S::Added => 'A',
        S::Modified | S::TypeChanged => 'M',
        S::Deleted => 'D',
        S::Renamed => 'R',
        S::Copied => 'C',
        S::Untracked => '?',
        S::Conflicted => 'U',
        S::Unknown => '\u{b7}',
    }
}

/// The `@` rows for `filter`.
#[must_use]
pub fn category_items(filter: &str, available: impl Fn(Scope) -> bool) -> Vec<Item> {
    categories(filter, available)
        .into_iter()
        .map(|(scope, found)| {
            let mut item = Item::new(Target::Category(scope), Group::Categories, scope.label(), String::new());
            item.icon = Some(scope.icon());
            item.detail = Some(scope.hint().to_owned());
            item.right = Some(Right::Value(scope.typed()));
            item.found = found;
            item
        })
        .collect()
}

/// Put the Files group (already in the server's order) among `ranked`
/// results: ahead of the first group whose best entry is no better than
/// the best file's match is, so a file named as typed leads.
#[must_use]
pub fn with_files(ranked: Vec<Item>, files: Vec<Item>, query: &str) -> Vec<Item> {
    if files.is_empty() {
        return ranked;
    }
    let words = query.split(':').next().unwrap_or_default();
    let best = files.iter().filter_map(|f| score(words, &f.text, "")).map(|m| m.tier).max().unwrap_or(0);
    // The first entry of each group is its best (`rank`).
    let at = ranked.iter().enumerate().find(|(i, item)| (*i == 0 || ranked[i - 1].group != item.group) && item.found.tier < best).map(|(i, _)| i);
    let mut ranked = ranked;
    let at = at.unwrap_or(ranked.len());
    ranked.splice(at..at, files);
    ranked
}

/// Sort by tier, then recency (`recent` newest first), then blocks last,
/// then score; then
/// group, groups ordered by their best entry, at most `per_group` each.
#[must_use]
pub fn rank(mut items: Vec<Item>, recent: &[String], per_group: Option<usize>) -> Vec<Item> {
    let boost = |key: &str| recent.iter().position(|k| k == key).map_or(0, |i| RECENT_CAP.saturating_sub(i));
    // Blocks go after commands and the rest as good: a block's name is
    // often a word a command starts with too.
    items.sort_by_cached_key(|i| {
        (std::cmp::Reverse(i.found.tier), std::cmp::Reverse(boost(&i.key)), i.group == Group::Blocks, std::cmp::Reverse(i.found.score))
    });
    let mut order: Vec<Group> = Vec::new();
    for item in &items {
        if !order.contains(&item.group) {
            order.push(item.group);
        }
    }
    let cap = per_group.unwrap_or(usize::MAX);
    let mut out = Vec::with_capacity(items.len());
    for group in order {
        out.extend(items.iter().filter(|i| i.group == group).take(cap).cloned());
    }
    out
}

/// The list row of entry `selected` when a label precedes each group.
#[must_use]
pub fn row_index(groups: &[Group], selected: usize) -> usize {
    let labels = (0..=selected.min(groups.len().saturating_sub(1))).filter(|&i| i == 0 || groups[i] != groups[i - 1]).count();
    selected + labels
}

/// Whether a query reads as a web address rather than a search.
#[must_use]
pub fn address(query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() || q.contains(char::is_whitespace) {
        return None;
    }
    let schemed = q.starts_with("http://") || q.starts_with("https://");
    let host = q.split(['/', '?', '#']).next().unwrap_or_default();
    let dotted = host.split('.').count() > 1
        && host.split('.').all(|p| !p.is_empty())
        && host.rsplit('.').next().is_some_and(|tld| tld.chars().all(char::is_alphanumeric));
    let local = host == "localhost" || host.starts_with("localhost:");
    (schemed || dotted || local).then(|| silo_proto::model::browser_url(q)).flatten()
}

// ---- recent picks ----

#[derive(Default, Serialize, Deserialize)]
struct RecentFile {
    #[serde(default)]
    recent: Vec<String>,
}

const FILE: &str = "finder.json";

/// A pick of `key`: it moves first, once; the newest [`RECENT_CAP`] stay.
pub fn remember(list: &mut Vec<String>, key: &str) {
    list.retain(|k| k != key);
    list.insert(0, key.to_owned());
    list.truncate(RECENT_CAP);
}

#[must_use]
pub fn load_recent(home: &Path) -> Vec<String> {
    std::fs::read(home.join(FILE)).ok().and_then(|b| serde_json::from_slice::<RecentFile>(&b).ok()).map(|f| f.recent).unwrap_or_default()
}

pub fn save_recent(home: &Path, recent: &[String]) {
    let file = RecentFile { recent: recent.to_vec() };
    let written = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other).and_then(|b| {
        let tmp = home.join(format!(".{FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, b)?;
        std::fs::rename(tmp, home.join(FILE))
    });
    if let Err(error) = written {
        tracing::debug!("save finder picks: {error}");
    }
}

/// A value shown live while choosing, kept or put back.
#[derive(Clone, Debug)]
pub struct Preview<T> {
    original: T,
    shown: T,
}

impl<T: Clone + PartialEq> Preview<T> {
    pub fn new(original: T) -> Self {
        Self { shown: original.clone(), original }
    }

    /// Show `value`; whether that changed what is shown.
    pub fn show(&mut self, value: T) -> bool {
        if self.shown == value {
            return false;
        }
        self.shown = value;
        true
    }

    /// Escape: the value to put back, if another is shown.
    #[must_use]
    pub fn cancel(self) -> Option<T> {
        (self.shown != self.original).then_some(self.original)
    }
}

// ---- sources ----

/// What the server answered so far for the open finder.
#[derive(Clone, Copy, Default)]
pub(crate) struct Live<'a> {
    pub files: Option<&'a Hits<FileHit>>,
    pub grep: Option<&'a Hits<GrepHit>>,
    pub agents: Option<&'a [AgentRow]>,
    pub changes: Option<&'a Changes>,
}

impl Workspace {
    /// The finder's entries for `text` typed after `chip`; `live` holds
    /// what the server answered so far.
    pub(crate) fn finder_items(
        &self,
        chip: Option<Scope>,
        text: &str,
        snapshot: &SettingsSnapshot,
        recent: &[String],
        live: &Live<'_>,
        cx: &gpui::App,
    ) -> Vec<Item> {
        let (scope, q) = match mode(chip, text) {
            Mode::Categories(filter) => {
                let agents = self.agent_target(self.active_host).is_some();
                return category_items(filter, |s| offered(s, |f| self.host_supports(f)) && (s != Scope::Agents || agents));
            }
            Mode::Search(scope, q) => (scope, q),
        };
        match scope {
            Scope::Files => return live.files.map(file_items).unwrap_or_default(),
            Scope::Grep => return live.grep.map(grep_items).unwrap_or_default(),
            _ => {}
        }
        let mut all = self.finder_sources(scope, snapshot, cx);
        if scope == Scope::Agents {
            all.extend(live.agents.map(agent_items).unwrap_or_default());
        }
        if scope == Scope::Changes {
            all.extend(live.changes.map(change_items).unwrap_or_default());
        }
        if q.is_empty() {
            if scope != Scope::All {
                return all;
            }
            // Recent picks, then common commands.
            let mut out: Vec<Item> =
                recent.iter().filter_map(|key| all.iter().find(|i| i.key == *key)).map(|i| Item { group: Group::Recent, ..i.clone() }).collect();
            for command in COMMON {
                let key = command_key(command);
                if !out.iter().any(|i| i.key == key)
                    && let Some(item) = all.iter_mut().find(|i| i.key == key)
                {
                    out.push(item.clone());
                }
            }
            return out;
        }
        let mut found: Vec<Item> = all
            .into_iter()
            .filter_map(|mut item| {
                item.found = score(q, &item.text, &item.keywords)?;
                Some(item)
            })
            .collect();
        if scope == Scope::All
            && let Some(url) = address(q)
        {
            let mut open = Item::new(Target::OpenUrl(url.clone()), Group::Open, url, String::new());
            open.lead = "Open ";
            open.detail = Some("New browser tab".into());
            open.found.tier = 4;
            found.push(open);
        }
        let ranked = rank(found, recent, (scope == Scope::All).then_some(PER_GROUP));
        match live.files.filter(|_| scope == Scope::All && q.chars().count() >= FILES_MIN) {
            Some(files) => with_files(ranked, file_items(files).into_iter().take(FILES_TOP).collect(), q),
            None => ranked,
        }
    }

    fn finder_sources(&self, scope: Scope, snapshot: &SettingsSnapshot, cx: &gpui::App) -> Vec<Item> {
        let mut out = Vec::new();
        if scope.includes(Group::Commands) {
            for &command in Command::ALL.iter().filter(|c| !matches!(c, Command::AddHost | Command::RenameHost) && self.available(**c, cx)) {
                let mut item = Item::new(Target::Command(command), Group::Commands, command.title(), command_key(command));
                item.detail = Some(self.subtitle(command).map_or_else(|| command.group().to_owned(), |s| format!("{} · {s}", command.group())));
                item.right = command.hint().map(Right::Shortcut);
                item.check = self.checked(command);
                out.push(item);
            }
        }
        // Renaming the shown host is a command; the others' sit after
        // their Go to entries, so a bare name still goes there first.
        let remotes: Vec<(usize, String, String)> = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, h)| !h.removed)
            .filter_map(|(index, h)| Some((index, h.info.transport.remote_id()?, h.info.name.clone())))
            .collect();
        let (rename_shown, rename_others): (Vec<Item>, Vec<Item>) =
            rename_items(&remotes, self.active_host).into_iter().filter(|i| scope.includes(i.group)).partition(|i| i.group == Group::Commands);
        out.extend(rename_shown);
        if scope.includes(Group::Settings) {
            for setting in SETTINGS.iter().filter(|s| s.id.visible(snapshot)) {
                let mut item = Item::new(Target::Setting(setting.id), Group::Settings, setting.title, setting_key(setting.id));
                item.keywords = format!("{} {}", setting.label, setting.keywords);
                item.detail = Some(setting.section.label().to_owned());
                item.right = Some(Right::Value(match setting.kind {
                    Kind::Open => "Open in Settings ›".into(),
                    Kind::Choice => format!("{} ›", setting.id.value(snapshot).unwrap_or_default()),
                    Kind::Toggle | Kind::Number => setting.id.value(snapshot).unwrap_or_default(),
                }));
                out.push(item);
            }
        }
        if scope.includes(Group::Sessions) {
            let current = self.view.as_ref().map(|v| v.session.id.as_str());
            for (host, entry) in self.hosts.iter().enumerate().filter(|(_, h)| !h.removed && h.state == HostState::Connected) {
                let machine = host_key(entry);
                for s in entry.sessions.iter().filter(|s| !(host == self.active_host && Some(s.id.as_str()) == current)) {
                    let mut item =
                        Item::new(Target::Session { host, id: s.id.clone() }, Group::Sessions, s.name.clone(), session_key(&machine, &s.id));
                    item.lead = "Switch to ";
                    item.detail = Some(entry.info.name.clone());
                    out.push(item);
                }
            }
        }
        if scope.includes(Group::Machines) {
            for (index, host) in self.hosts.iter().enumerate().filter(|(_, h)| !h.removed) {
                let id = host_key(host);
                if index != self.active_host {
                    let mut item = Item::new(Target::Host(index), Group::Machines, host.info.name.clone(), format!("host:{id}"));
                    item.lead = "Go to ";
                    item.detail = crate::switcher::state_label(&host.state, host.info.local).map(str::to_owned);
                    out.push(item);
                }
                if host.incompatible.is_some() || host.state == HostState::Incompatible {
                    let mut item = Item::new(Target::Upgrade(index), Group::Machines, host.info.name.clone(), format!("upgrade:{id}"));
                    item.lead = "Upgrade ";
                    item.detail = Some("Incompatible".into());
                    out.push(item);
                }
            }
            out.extend(rename_others);
            let mut add = Item::new(Target::Command(Command::AddHost), Group::Machines, Command::AddHost.title(), command_key(Command::AddHost));
            add.detail = Command::AddHost.subtitle().map(str::to_owned);
            out.push(add);
        }
        if scope.includes(Group::Tabs) {
            out.extend(self.tab_items(scope == Scope::Tabs, cx));
        }
        if scope.includes(Group::Blocks) {
            out.extend(block_items(self.block_rows(cx)));
        }
        if scope.includes(Group::Pages) {
            out.extend(self.page_items(cx));
        }
        out
    }

    /// Tabs of the shown session: with more than one block, or `every` one
    /// (the Tabs chip; a single block's tab is under Blocks too).
    fn tab_items(&self, every: bool, cx: &gpui::App) -> Vec<Item> {
        let Some(view) = &self.view else { return Vec::new() };
        let mut out = Vec::new();
        for (index, w) in view.session.windows.iter().enumerate() {
            if every || crate::floats::window_blocks(w).len() > 1 {
                let mut item = Item::new(Target::Tab { index }, Group::Tabs, self.window_title(w, cx), format!("tab:{}", w.id));
                item.icon = Some(self.tab_icon(w));
                item.detail = Some(format!("Tab {}", index + 1));
                out.push(item);
            }
        }
        out
    }

    /// Recent pages of the shown host.
    fn page_items(&self, cx: &gpui::App) -> Vec<Item> {
        let Some(host) = self.hosts.get(self.active_host) else { return Vec::new() };
        let key = if host.info.local { "local".to_owned() } else { host.info.transport.remote_id().unwrap_or_default() };
        let pages = &crate::hosts_state::HostsState::global(cx).read(cx).app_state().recent_pages;
        pages
            .iter()
            .filter(|p| p.host == key)
            .take(PAGES)
            .map(|p| {
                let title = if p.title.is_empty() { p.url.clone() } else { p.title.clone() };
                let mut item = Item::new(Target::Page(p.url.clone()), Group::Pages, title, format!("page:{}", p.url));
                item.keywords.clone_from(&p.url);
                item.detail = Some(p.url.clone());
                item
            })
            .collect()
    }
}

/// The Blocks entries ([`crate::block_list`]): the kind's glyph and title,
/// where it is after it.
#[must_use]
pub(crate) fn block_items(rows: Vec<BlockRow>) -> Vec<Item> {
    rows.into_iter()
        .map(|row| {
            // The same key wherever the block is, so a pick stays recent
            // as it moves between the background and a tab.
            let mut item = Item::new(row.target(), Group::Blocks, row.title.clone(), format!("pane:{}", row.block));
            item.icon = Some(row.icon());
            item.detail = Some(row.detail());
            item
        })
        .collect()
}

/// A host's identity in the recent picks: its EndpointId ("local" for this
/// Mac), which survives other hosts being added or removed.
fn host_key(host: &crate::hosts_state::HostEntry) -> String {
    host.info.transport.remote_id().unwrap_or_else(|| "local".into())
}

/// Rename entries of the remote hosts, given as (index, EndpointId, name);
/// this Mac is not renamed. The shown host's comes first, as the Rename
/// Host… command; the others' are under Machines.
#[must_use]
pub fn rename_items(remotes: &[(usize, String, String)], active: usize) -> Vec<Item> {
    let mut out: Vec<Item> = remotes
        .iter()
        .map(|(index, id, name)| {
            let mut item = if *index == active {
                let command = Command::RenameHost;
                let mut item = Item::new(Target::RenameHost(*index), Group::Commands, command.title(), command_key(command));
                item.detail = Some(format!("{} · {name}", command.group()));
                item
            } else {
                let mut item = Item::new(Target::RenameHost(*index), Group::Machines, name.clone(), format!("rename:{id}"));
                item.lead = "Rename ";
                item
            };
            item.keywords = format!("rename host machine {name}");
            item
        })
        .collect();
    out.sort_by_key(|i| i.target != Target::RenameHost(active));
    out
}

#[must_use]
pub fn session_key(host: &str, session: &str) -> String {
    format!("session:{host}:{session}")
}

#[must_use]
pub fn command_key(command: Command) -> String {
    format!("cmd:{command:?}")
}

#[must_use]
pub fn setting_key(id: SettingId) -> String {
    format!("set:{id:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, group: Group, key: &str) -> Item {
        Item::new(Target::Page(text.into()), group, text, key.into())
    }

    fn found(query: &str, items: Vec<Item>, recent: &[String]) -> Vec<String> {
        let items = items
            .into_iter()
            .filter_map(|mut i| {
                i.found = score(query, &i.text, &i.keywords)?;
                Some(i)
            })
            .collect();
        rank(items, recent, None).into_iter().map(|i| i.text).collect()
    }

    #[test]
    fn old_prefixes_become_chips() {
        assert_eq!(promote(None, ">split"), Some((Scope::Commands, "split".into())));
        assert_eq!(promote(None, ": font"), Some((Scope::Settings, "font".into())));
        assert_eq!(promote(None, "#btop"), Some((Scope::Blocks, "btop".into())));
        assert_eq!(promote(None, BLOCKS_QUERY), Some((Scope::Blocks, String::new())), "Browse Blocks opens with the Blocks chip");
        assert_eq!(promote(None, "  >"), Some((Scope::Commands, String::new())));
        assert_eq!(promote(None, "@files"), None, "@ is the category list");
        assert_eq!(promote(None, "theme"), None);
        assert_eq!(promote(Some(Scope::Files), ">x"), None, "after a chip a prefix is text");
        for scope in CATEGORIES {
            if let Some(c) = scope.prefix() {
                assert_eq!(promote(None, &c.to_string()).map(|p| p.0), Some(scope));
            }
        }
    }

    #[test]
    fn backspace_and_escape_take_the_chip_off_first() {
        assert_eq!(backspace(Some(Scope::Files), ""), Some("@files".into()), "the chip's text comes back");
        assert_eq!(backspace(Some(Scope::Files), "ma"), None, "an ordinary ⌫");
        assert_eq!(backspace(None, ""), None);
        // What comes back lists that category again, first.
        let typed = backspace(Some(Scope::Grep), "").unwrap();
        let Mode::Categories(filter) = mode(None, &typed) else { panic!("{typed}") };
        assert_eq!(categories(filter, |_| true)[0].0, Scope::Grep);
        assert_eq!(escape(Some(Scope::Blocks)), Escape::Unchip);
        assert_eq!(escape(None), Escape::Close);
    }

    #[test]
    fn the_field_lists_categories_after_at_and_searches_a_chip() {
        assert_eq!(mode(None, "  theme "), Mode::Search(Scope::All, "theme"));
        assert_eq!(mode(None, "@"), Mode::Categories(""));
        assert_eq!(mode(None, "@fi"), Mode::Categories("fi"));
        assert_eq!(mode(Some(Scope::Files), " main.rs "), Mode::Search(Scope::Files, "main.rs"));
        assert_eq!(mode(Some(Scope::Grep), "@x"), Mode::Search(Scope::Grep, "@x"), "a chip's text is never the @ list");
        assert_eq!(Scope::Files.typed(), "@files", "what ⌫ puts back");
        assert_eq!(Scope::Spaces.typed(), "@spaces");
        assert!(Scope::Spaces.includes(Group::Machines) && Scope::Spaces.includes(Group::Sessions) && !Scope::Spaces.includes(Group::Blocks));
        assert!(Scope::Blocks.includes(Group::Blocks) && !Scope::Blocks.includes(Group::Tabs) && !Scope::Blocks.includes(Group::Commands));
        assert!(Scope::All.includes(Group::Pages) && Scope::All.includes(Group::Files) && !Scope::Commands.includes(Group::Settings));
        assert!(!Scope::All.includes(Group::Grep) && !Scope::All.includes(Group::Agents) && !Scope::All.includes(Group::Changes));
    }

    #[test]
    fn the_category_list_filters_as_typed() {
        let all = |_: Scope| true;
        let labels = |filter: &str, available: &dyn Fn(Scope) -> bool| -> Vec<&str> {
            categories(filter, available).into_iter().map(|(s, _)| s.label()).collect()
        };
        assert_eq!(labels("", &all), CATEGORIES.map(Scope::label), "every category, in order");
        assert_eq!(labels("fi", &all)[0], "Files");
        assert_eq!(labels("g", &all)[0], "Grep");
        assert_eq!(labels("ag", &all)[0], "Agents");
        assert_eq!(labels("cha", &all)[0], "Changes");
        assert_eq!(labels("text", &all)[0], "Grep", "hints match too, after labels");
        assert_eq!(labels("sp", &all)[0], "Spaces");
        assert!(labels("zzz", &all).is_empty());
        // Rows: icon, label, hint; the pick is the chip.
        let rows = category_items("fil", all);
        assert_eq!(rows[0].target, Target::Category(Scope::Files));
        assert_eq!((rows[0].icon, rows[0].text.as_str(), rows[0].group), (Some("file-text"), "Files", Group::Categories));
        assert_eq!(rows[0].detail.as_deref(), Some(Scope::Files.hint()));
        let icons: Vec<&str> = CATEGORIES.map(Scope::icon).to_vec();
        assert_eq!(icons, ["file-text", "search", "layers", "laptop", "split-right", "agent", "terminal", "settings", "globe", "git-diff"]);
        for scope in CATEGORIES {
            assert!(!scope.hint().is_empty() && scope.placeholder() != PLACEHOLDER, "{scope:?}");
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/");
            assert!(Path::new(&format!("{path}{}.svg", scope.icon())).exists(), "{scope:?}");
        }
    }

    #[test]
    fn files_grep_and_changes_are_hidden_without_their_feature() {
        let old = |f: &str| f != SEARCH_FEATURE && f != silo_proto::compat::feature::CHANGES;
        let shown: Vec<Scope> = categories("", |s| offered(s, old)).into_iter().map(|(s, _)| s).collect();
        assert!(!shown.contains(&Scope::Files) && !shown.contains(&Scope::Grep) && !shown.contains(&Scope::Changes));
        assert_eq!(shown.len(), CATEGORIES.len() - 3);
        assert!(categories("fi", |s| offered(s, old)).iter().all(|(s, _)| *s != Scope::Files));
        assert!(offered(Scope::Files, |_| true) && offered(Scope::Blocks, |_| false));
        // No server search: no files without a chip either.
        assert_eq!(request(Scope::All, "main", false), None);
        assert_eq!(request(Scope::Files, "main", false), None);
    }

    #[test]
    fn the_query_sent_per_chip() {
        let files = |q: &str, limit| Some(Request::Files { query: q.into(), limit });
        assert_eq!(request(Scope::Files, " main.rs ", true), files("main.rs", FILES_LIMIT));
        assert_eq!(request(Scope::Files, "", true), files("", FILES_LIMIT), "frecent files for an empty query");
        assert_eq!(request(Scope::Files, "src/main.rs:42", true), files("src/main.rs:42", FILES_LIMIT));
        assert_eq!(request(Scope::Grep, "fn main", true), Some(Request::Grep { query: "fn main".into(), limit: GREP_LIMIT }));
        assert_eq!(request(Scope::Grep, "  ", true), None);
        // No chip: the top files at two characters, not for an address.
        assert_eq!(request(Scope::All, "m", true), None);
        assert_eq!(request(Scope::All, "ma", true), files("ma", FILES_TOP));
        assert_eq!(request(Scope::All, "github.com", true), None);
        for scope in [Scope::Blocks, Scope::Spaces, Scope::Tabs, Scope::Agents, Scope::Commands, Scope::Settings, Scope::Pages, Scope::Changes] {
            assert_eq!(request(scope, "main", true), None, "{scope:?} searches here");
        }
    }

    #[test]
    fn a_files_query_can_name_a_line() {
        assert_eq!(query_line("src/main.rs:42"), Some(42));
        assert_eq!(query_line("main.rs:42:7"), Some(42));
        assert_eq!(query_line("main.rs"), None);
        assert_eq!(query_line("main.rs:"), None);
        assert_eq!(query_line("main.rs:0"), None);
        assert_eq!(query_line("a:b"), None);
    }

    fn hits<T>(query: &str, items: Vec<T>) -> Hits<T> {
        Hits { root: "/work/app".into(), query: query.into(), total: items.len(), items, indexing: false }
    }

    #[test]
    fn file_rows_show_the_folder_and_git_status_and_open_at_a_line() {
        let found = vec![
            FileHit { path: "src/main.rs".into(), name: "main.rs".into(), git: Some('M') },
            FileHit { path: "Cargo.toml".into(), name: "Cargo.toml".into(), git: None },
        ];
        let rows = file_items(&hits("main:12", found.clone()));
        assert_eq!(rows[0].target, Target::File { root: "/work/app".into(), path: "src/main.rs".into(), line: Some(12) });
        assert_eq!(
            (rows[0].text.as_str(), rows[0].detail.as_deref(), rows[0].right.clone()),
            ("main.rs", Some("src"), Some(Right::Value("M".into())))
        );
        assert_eq!(rows[0].found.ranges.first(), Some(&(0..4)), "the matched letters of the name");
        assert_eq!((rows[1].detail.as_deref(), rows[1].right.clone()), (None, None));
        assert!(rows.iter().all(|r| r.group == Group::Files && r.icon == Some("file-text")));
        assert_eq!(file_items(&hits("main", found))[0].target, Target::File { root: "/work/app".into(), path: "src/main.rs".into(), line: None });
    }

    #[test]
    fn grep_rows_show_the_line_with_its_match() {
        let found =
            vec![GrepHit { path: "src/main.rs".into(), line: 7, text: "    let total = 1;".into(), ranges: std::iter::once(8..13).collect() }];
        let rows = grep_items(&hits("total", found));
        assert_eq!(rows[0].target, Target::File { root: "/work/app".into(), path: "src/main.rs".into(), line: Some(7) });
        assert_eq!(rows[0].text, "let total = 1;");
        assert_eq!(&rows[0].text[rows[0].found.ranges[0].clone()], "total", "ranges follow the cut indent");
        assert_eq!(rows[0].detail.as_deref(), Some("src/main.rs:7"));
        assert_eq!(rows[0].group, Group::Grep);
        // A range past the text (or inside a character) is dropped, never panics.
        let odd = vec![GrepHit { path: "a".into(), line: 1, text: "é".into(), ranges: vec![1..2, 5..9] }];
        assert!(grep_items(&hits("x", odd))[0].found.ranges.is_empty());
    }

    #[test]
    fn the_files_group_goes_by_its_best_match() {
        let found = |text: &str, group, query: &str| {
            let mut i = item(text, group, text);
            i.found = score(query, text, "").unwrap();
            i
        };
        let files = |names: &[&str]| -> Vec<Item> {
            file_items(&hits("", names.iter().map(|n| FileHit { path: (*n).into(), name: (*n).into(), git: None }).collect()))
        };
        let groups = |items: &[Item]| items.iter().map(|i| i.group).collect::<Vec<_>>();
        // A file named as typed leads a fuzzy command.
        let ranked = vec![found("Split Right", Group::Commands, "sr")];
        assert_eq!(groups(&with_files(ranked, files(&["sr"]), "sr")), [Group::Files, Group::Commands]);
        // An equally good command stays first.
        let ranked = vec![found("Split Right", Group::Commands, "split")];
        assert_eq!(groups(&with_files(ranked, files(&["split.rs"]), "split")), [Group::Commands, Group::Files]);
        assert_eq!(with_files(Vec::new(), files(&["a.rs"]), "a").len(), 1);
        assert!(with_files(Vec::new(), Vec::new(), "a").is_empty());
    }

    #[test]
    fn agent_rows_open_their_session() {
        let rows = [
            AgentRow { id: "s1".into(), title: "Fix login".into(), detail: "general · opus".into(), open: true },
            AgentRow { id: "s2".into(), title: " ".into(), detail: "general · opus".into(), open: false },
        ];
        let items = agent_items(&rows);
        assert_eq!(items[0].target, Target::Agent { session: "s1".into(), title: "Fix login".into() });
        assert_eq!((items[0].text.as_str(), items[0].right.clone()), ("Fix login", Some(Right::Value("open".into()))));
        assert_eq!((items[1].text.as_str(), items[1].right.clone()), ("Untitled session", None));
        assert!(items.iter().all(|i| i.group == Group::Agents && i.icon == Some(crate::agent_block::ICON)));
    }

    fn session(id: &str, parent: Option<&str>, updated_at: i64) -> otto_proto::SessionInfo {
        otto_proto::SessionInfo {
            id: id.into(),
            project_id: "proj".into(),
            parent_id: parent.map(Into::into),
            forked_from: None,
            agent: "general".into(),
            approval_mode: otto_proto::ApprovalMode::default(),
            provider: "test".into(),
            model: "m1".into(),
            effort: None,
            title: format!("task {id}"),
            cwd: String::new(),
            status: otto_proto::SessionStatus::Idle,
            created_at: 0,
            updated_at,
            last_seq: 0,
            usage: otto_proto::UsageTotals::default(),
            context: otto_proto::ContextUse::default(),
            report_held: false,
            plan: None,
        }
    }

    #[test]
    fn agents_list_top_level_sessions_newest_first() {
        let mut fork = session("f", Some("a"), 3);
        fork.forked_from = Some(otto_proto::ForkOrigin { session_id: "a".into(), at_seq: 1 });
        let rows = agent_rows(vec![session("a", None, 1), session("sub", Some("a"), 9), session("b", None, 2), fork], &["b".to_owned()]);
        let shown: Vec<(&str, bool)> = rows.iter().map(|r| (r.id.as_str(), r.open)).collect();
        assert_eq!(shown, [("f", false), ("b", true), ("a", false)], "no sub-agents; a fork is listed");
        assert_eq!(rows[0].detail, "general · m1");
    }

    fn block(id: &str, kind: silo_proto::model::BlockKind, root: &str) -> silo_proto::model::BlockInfo {
        use silo_proto::model::{BlockKind, ChangesState, FilesState};
        let mut info: silo_proto::model::BlockInfo = serde_json::from_value(serde_json::json!({
            "id": id, "session_id": "s", "kind": kind, "title": "", "size": {"columns": 80, "rows": 24}
        }))
        .unwrap();
        match kind {
            BlockKind::Files => info.files = Some(Box::new(FilesState::new(root.into()))),
            BlockKind::Changes => info.changes = Some(Box::new(ChangesState::new(root.into()))),
            _ => {}
        }
        info
    }

    #[test]
    fn a_found_file_opens_in_the_files_block_holding_it() {
        use silo_proto::model::BlockKind;
        let blocks = [block("t", BlockKind::Terminal, ""), block("f1", BlockKind::Files, "/work"), block("f2", BlockKind::Files, "/work/app")];
        let pick = |p: &str| files_block_for(&blocks, Path::new(p));
        assert_eq!(pick("/work/app/src/main.rs"), Some(("f2".into(), "src/main.rs".into())), "the deepest root");
        assert_eq!(pick("/work/other/a.rs"), Some(("f1".into(), "other/a.rs".into())));
        assert_eq!(pick("/etc/hosts"), None, "none holds it: a new Files tab");
    }

    #[test]
    fn changes_rows_come_from_the_workspace_repo() {
        use silo_proto::model::BlockKind;
        let blocks = [block("c1", BlockKind::Changes, "/other"), block("c2", BlockKind::Changes, "/work/app")];
        assert_eq!(changes_block(&blocks, Some("/work/app/src")).as_deref(), Some("c2"));
        assert_eq!(changes_block(&blocks, Some("/elsewhere")).as_deref(), Some("c1"), "else the first");
        assert_eq!(changes_block(&blocks[..0], Some("/work/app")), None);
        let changes = Changes {
            block: Some("c2".into()),
            files: vec![
                ChangeRow { path: "src/main.rs".into(), status: 'M', added: 3, removed: 1 },
                ChangeRow { path: "NEW.md".into(), status: '?', added: 0, removed: 0 },
            ],
        };
        let items = change_items(&changes);
        assert_eq!(items[0].target, Target::Change { block: Some("c2".into()), path: "src/main.rs".into() });
        assert_eq!((items[0].text.as_str(), items[0].detail.as_deref()), ("main.rs", Some("src")));
        assert_eq!(items[0].right, Some(Right::Value("+3 \u{2212}1  M".into())));
        assert_eq!(items[1].right, Some(Right::Value("?".into())));
        // The path is matched too: `src` finds main.rs.
        assert!(score("src", &items[0].text, &items[0].keywords).is_some());
        // No Changes block: one row opens one.
        let none = change_items(&Changes { block: None, files: Vec::new() });
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].target, Target::Command(Command::NewChangesTab));
        assert_eq!(status_letter(silo_proto::changes::FileStatus::Untracked), '?');
        assert_eq!(status_letter(silo_proto::changes::FileStatus::Added), 'A');
    }

    #[test]
    fn exact_and_prefix_first_then_recent_then_fuzzy() {
        let items =
            || vec![item("Split Right", Group::Commands, "a"), item("Show Pane Headers", Group::Commands, "b"), item("Sp", Group::Settings, "c")];
        assert_eq!(found("sp", items(), &[]), ["Sp", "Split Right", "Show Pane Headers"]);
        // A recent pick goes ahead of equally good matches, not of better ones.
        let recent = ["b".to_owned()];
        assert_eq!(found("s", items(), &recent), ["Show Pane Headers", "Split Right", "Sp"]);
        assert_eq!(found("sp", items(), &recent), ["Sp", "Split Right", "Show Pane Headers"]);
        // Keywords find what the title does not, last.
        let mut theme = item("Theme", Group::Settings, "t");
        theme.keywords = "colors dark light".into();
        assert_eq!(found("dark", vec![theme, item("Dark Mode Thing", Group::Commands, "d")], &[]), ["Dark Mode Thing", "Theme"]);
        assert!(score("zzz", "Theme", "colors").is_none());
        assert_eq!(score("theme", "Theme", "").unwrap().tier, 3);
        assert_eq!(score("size", "Font size", "").unwrap().tier, 1);
    }

    #[test]
    fn groups_follow_their_best_entry_and_are_capped() {
        let items = vec![
            item("alpha one", Group::Commands, "1"),
            item("beta", Group::Blocks, "2"),
            item("alpha", Group::Blocks, "3"),
            item("alpha two", Group::Commands, "4"),
            item("alpha three", Group::Commands, "5"),
        ];
        let ranked: Vec<Item> = items
            .into_iter()
            .filter_map(|mut i| {
                i.found = score("alpha", &i.text, "")?;
                Some(i)
            })
            .collect();
        let ranked = rank(ranked, &[], Some(2));
        let shown: Vec<(&str, Group)> = ranked.iter().map(|i| (i.text.as_str(), i.group)).collect();
        assert_eq!(shown, [("alpha", Group::Blocks), ("alpha one", Group::Commands), ("alpha two", Group::Commands)]);
        let groups: Vec<Group> = ranked.iter().map(|i| i.group).collect();
        assert_eq!([0, 1, 2].map(|s| row_index(&groups, s)), [1, 3, 4], "a label row before each group");
    }

    #[test]
    fn recent_picks_move_first_once_and_are_capped() {
        let mut list = Vec::new();
        for i in 0..RECENT_CAP + 5 {
            remember(&mut list, &format!("k{i}"));
        }
        assert_eq!(list.len(), RECENT_CAP);
        assert_eq!(list[0], format!("k{}", RECENT_CAP + 4));
        remember(&mut list, "k10");
        assert_eq!(list[0], "k10");
        assert_eq!(list.iter().filter(|k| *k == "k10").count(), 1);
        assert_eq!(list.len(), RECENT_CAP);

        let home = std::env::temp_dir().join(format!("silo-finder-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        save_recent(&home, &list);
        assert_eq!(load_recent(&home), list);
        let _ = std::fs::remove_dir_all(&home);
        assert!(load_recent(&home).is_empty());
    }

    #[test]
    fn theme_preview_is_put_back_on_escape() {
        let mut preview = Preview::new("silo-dark");
        assert!(preview.clone().cancel().is_none(), "nothing shown, nothing to put back");
        assert!(preview.show("silo-light"));
        assert!(!preview.show("silo-light"));
        assert!(preview.show("nord"));
        assert_eq!(preview.clone().cancel(), Some("silo-dark"));
        preview.show("silo-dark");
        assert_eq!(preview.cancel(), None, "back on the original");
    }

    #[test]
    fn addresses_open_pages() {
        assert_eq!(address("github.com").as_deref(), Some("https://github.com"));
        assert_eq!(address("https://example.org/a b"), None);
        assert!(address("localhost:3000").is_some());
        assert!(address("http://x").is_some());
        for query in ["theme", "font size", "a.", "split.", ".com", ""] {
            assert_eq!(address(query), None, "{query}");
        }
    }

    #[test]
    fn session_picks_are_keyed_by_machine_not_position() {
        assert_eq!(session_key("local", "s1"), "session:local:s1");
        assert_ne!(session_key("1a33ca7b", "s1"), session_key("local", "s1"));
        assert!(!session_key("1a33ca7b", "s1").contains(":0:") && !session_key("1a33ca7b", "s1").contains(":1:"));
    }

    #[test]
    fn the_shown_host_is_renamed_first() {
        let remotes = [(1, "aaa".to_owned(), "mini".to_owned()), (2, "bbb".to_owned(), "studio".to_owned())];
        let items = rename_items(&remotes, 2);
        assert_eq!(items[0].target, Target::RenameHost(2));
        assert_eq!((items[0].group, items[0].text.as_str()), (Group::Commands, "Rename Host…"));
        assert_eq!(items[0].detail.as_deref(), Some("Hosts · studio"));
        assert_eq!(items[1].target, Target::RenameHost(1));
        assert_eq!((items[1].group, items[1].lead, items[1].text.as_str()), (Group::Machines, "Rename ", "mini"));
        // This Mac (index 0, not a remote) is the shown host: no command.
        let items = rename_items(&remotes, 0);
        assert!(items.iter().all(|i| i.group == Group::Machines && i.target != Target::RenameHost(0)));
        assert!(rename_items(&[], 0).is_empty());
    }

    #[test]
    fn host_renames_match_rename_host_and_the_name() {
        let remotes = [(1, "aaa".to_owned(), "mini".to_owned()), (2, "bbb".to_owned(), "studio".to_owned())];
        let targets = |query: &str| -> Vec<Target> {
            let items: Vec<Item> = rename_items(&remotes, 2)
                .into_iter()
                .filter_map(|mut i| {
                    i.found = score(query, &i.text, &i.keywords)?;
                    Some(i)
                })
                .collect();
            rank(items, &[], None).into_iter().map(|i| i.target).collect()
        };
        assert_eq!(targets("rename"), [Target::RenameHost(2), Target::RenameHost(1)]);
        assert_eq!(targets("host"), [Target::RenameHost(2), Target::RenameHost(1)]);
        assert_eq!(targets("studio"), [Target::RenameHost(2)]);
        assert_eq!(targets("rename studio"), [Target::RenameHost(2)]);
        assert_eq!(targets("mini"), [Target::RenameHost(1)]);
        assert!(targets("zzz").is_empty());
    }

    fn blocks() -> Vec<BlockRow> {
        use crate::{block_list::Place, blocks::Kind};
        let tab = |index, name: &str, float| Place::Tab { index, name: name.into(), float };
        let row = |block: &str, title: &str, kind: Kind, state: &str, by: Option<&str>, place: Place| BlockRow {
            block: block.into(),
            kind,
            title: title.into(),
            state: state.into(),
            by: by.map(Into::into),
            command: None,
            busy: false,
            place,
        };
        vec![
            row("b1", "api", Kind::Terminal, "idle", None, tab(0, "api", false)),
            row("b2", "GitHub", Kind::Browser, "idle", None, tab(0, "api", true)),
            row("b3", "otto", Kind::Agent, "idle", None, tab(1, "review", false)),
            row("b4", "sonic", Kind::Terminal, "running cargo", Some("otto"), Place::Background),
            row("b5", "logs", Kind::Terminal, "exited · code 1", None, Place::Background),
        ]
    }

    #[test]
    fn blocks_show_their_kind_tab_or_background() {
        let items = block_items(blocks());
        let shown: Vec<(&str, Option<&str>, &str)> = items.iter().map(|i| (i.text.as_str(), i.icon, i.detail.as_deref().unwrap_or(""))).collect();
        assert_eq!(
            shown,
            [
                ("api", Some("terminal"), "Tab 1"),
                ("GitHub", Some("globe"), "Tab 1 · api · floating"),
                ("otto", Some(crate::agent_block::ICON), "Tab 2 · review"),
                ("sonic", Some("terminal"), "background · running cargo · started by otto"),
                ("logs", Some("terminal"), "background · exited · code 1"),
            ]
        );
        assert!(items.iter().all(|i| i.group == Group::Blocks));
        assert_eq!(items[2].target, Target::Block { index: 1, block: "b3".into() });
        assert_eq!(items[3].target, Target::Background("b4".into()));
        assert_eq!(items[3].key, "pane:b4", "one key for a block wherever it is");
    }

    #[test]
    fn blocks_match_by_title_and_rank_like_everything_else() {
        let targets = |query: &str, recent: &[String]| -> Vec<String> {
            let items: Vec<Item> = block_items(blocks())
                .into_iter()
                .filter_map(|mut i| {
                    i.found = score(query, &i.text, &i.keywords)?;
                    Some(i)
                })
                .collect();
            rank(items, recent, None).into_iter().map(|i| i.text).collect()
        };
        assert_eq!(targets("sonic", &[]), ["sonic"], "a background block is found");
        assert_eq!(targets("o", &[]), ["otto", "sonic", "logs"], "a prefix first, then the rest in session order");
        assert_eq!(targets("gh", &[]), ["GitHub"]);
        assert!(targets("zzz", &[]).is_empty());
        // A recent pick goes ahead of equally good matches.
        assert_eq!(targets("o", &["pane:b5".to_owned()]), ["otto", "logs", "sonic"]);
    }

    #[test]
    fn blocks_keep_their_own_section_next_to_commands() {
        let mut command = item("Split Right", Group::Commands, "c");
        command.found = score("s", &command.text, "").unwrap();
        let mut found: Vec<Item> = block_items(blocks())
            .into_iter()
            .filter_map(|mut i| {
                i.found = score("s", &i.text, "")?;
                Some(i)
            })
            .collect();
        found.push(command);
        let ranked = rank(found, &[], Some(PER_GROUP));
        let shown: Vec<(&str, Group)> = ranked.iter().map(|i| (i.text.as_str(), i.group)).collect();
        // Blocks are one section of their own: never mixed into Commands.
        assert_eq!(shown, [("Split Right", Group::Commands), ("sonic", Group::Blocks), ("logs", Group::Blocks)]);
        // A block named exactly as typed still leads.
        let mut command = item("Split Right", Group::Commands, "c");
        command.found = score("sonic", &command.text, "").unwrap_or_default();
        let mut sonic = block_items(blocks()).remove(3);
        sonic.found = score("sonic", &sonic.text, "").unwrap();
        assert_eq!(rank(vec![command, sonic], &[], None)[0].text, "sonic");
    }

    #[test]
    fn every_setting_has_a_key() {
        let keys: Vec<String> = SETTINGS.iter().map(|s| setting_key(s.id)).collect();
        for (i, key) in keys.iter().enumerate() {
            assert!(!keys[i + 1..].contains(key));
        }
    }
}
