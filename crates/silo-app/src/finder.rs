//! ⌘P finder: a list of sources (commands, settings, sessions, machines,
//! tabs, every block of the session, recent pages, an address) that each turn the query into
//! scored entries, ranked and grouped under section labels. The palette
//! (`palette.rs`) shows and runs them.

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
/// The query Browse Blocks opens on: the Blocks scope.
pub const BLOCKS_QUERY: &str = "#";
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
}

impl Group {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Recent => "Recent",
            Self::Commands => "Commands",
            Self::Settings => "Settings",
            Self::Sessions => "Sessions",
            Self::Machines => "Machines",
            Self::Tabs => "Tabs",
            Self::Blocks => "Blocks",
            Self::Pages => "Pages",
        }
    }
}

/// What a query searches: everything, or one kind after a prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    /// `>`
    Commands,
    /// `:`
    Settings,
    /// `@`: sessions and machines.
    Sessions,
    /// `#`: blocks (Browse Blocks).
    Blocks,
}

impl Scope {
    #[must_use]
    pub fn includes(self, group: Group) -> bool {
        match self {
            Self::All => true,
            Self::Commands => group == Group::Commands,
            Self::Settings => group == Group::Settings,
            Self::Sessions => matches!(group, Group::Sessions | Group::Machines),
            Self::Blocks => group == Group::Blocks,
        }
    }
}

/// The scope a query's prefix picks and the rest of the query.
#[must_use]
pub fn parse(query: &str) -> (Scope, &str) {
    let query = query.trim_start();
    let scope = match query.chars().next() {
        Some('>') => Scope::Commands,
        Some(':') => Scope::Settings,
        Some('@') => Scope::Sessions,
        Some('#') => Scope::Blocks,
        _ => return (Scope::All, query.trim()),
    };
    (scope, query[1..].trim())
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

impl Workspace {
    /// The finder's entries for `query`.
    pub(crate) fn finder_items(&self, query: &str, snapshot: &SettingsSnapshot, recent: &[String], cx: &gpui::App) -> Vec<Item> {
        let (scope, q) = parse(query);
        let mut all = self.finder_sources(scope, snapshot, cx);
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
        rank(found, recent, (scope == Scope::All).then_some(PER_GROUP))
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
            out.extend(self.tab_items(cx));
        }
        if scope.includes(Group::Blocks) {
            out.extend(block_items(self.block_rows(cx)));
        }
        if scope.includes(Group::Pages) {
            out.extend(self.page_items(cx));
        }
        out
    }

    /// Tabs with more than one block, of the shown session.
    fn tab_items(&self, cx: &gpui::App) -> Vec<Item> {
        let Some(view) = &self.view else { return Vec::new() };
        let mut out = Vec::new();
        for (index, w) in view.session.windows.iter().enumerate() {
            if crate::floats::window_blocks(w).len() > 1 {
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
    fn prefixes_pick_a_scope() {
        assert_eq!(parse("  theme "), (Scope::All, "theme"));
        assert_eq!(parse(">split"), (Scope::Commands, "split"));
        assert_eq!(parse(": font"), (Scope::Settings, "font"));
        assert_eq!(parse("@work"), (Scope::Sessions, "work"));
        assert_eq!(parse("#btop"), (Scope::Blocks, "btop"));
        assert_eq!(parse(BLOCKS_QUERY), (Scope::Blocks, ""));
        assert!(Scope::Sessions.includes(Group::Machines) && !Scope::Sessions.includes(Group::Blocks));
        assert!(Scope::Blocks.includes(Group::Blocks) && !Scope::Blocks.includes(Group::Tabs) && !Scope::Blocks.includes(Group::Commands));
        assert!(Scope::All.includes(Group::Pages) && !Scope::Commands.includes(Group::Settings));
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
