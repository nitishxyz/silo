//! The Files block's browser (docs/FILES_BLOCK.md §8), free of gpui: its
//! rows (a folder's listing with folders expanded in place, or search
//! hits), sorting, kinds, dates, selection, type-to-select, history and the
//! path bar.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use silo_proto::control::{FileEntry, FileHit, FileKind};

use super::model::{is_dir, join, name, parent, shown};

/// A column the list sorts by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortKey {
    Name,
    Modified,
    Size,
    Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
    pub folders_first: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self { key: SortKey::Name, ascending: true, folders_first: true }
    }
}

impl Sort {
    /// A click on column `key`'s header: the same column turns round,
    /// another sorts ascending (dates and sizes newest and largest first,
    /// as Finder does).
    #[must_use]
    pub fn clicked(self, key: SortKey) -> Self {
        if self.key == key {
            Self { ascending: !self.ascending, ..self }
        } else {
            Self { key, ascending: matches!(key, SortKey::Name | SortKey::Kind), ..self }
        }
    }
}

/// A row of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub path: String,
    pub name: String,
    /// Levels below the folder shown (expanded folders' items).
    pub depth: usize,
    pub dir: bool,
    pub link: bool,
    pub hidden: bool,
    pub expanded: bool,
    /// Expanded, its listing not here yet.
    pub loading: bool,
    pub size: u64,
    pub mtime: u64,
    pub kind: &'static str,
    /// Search hits: the folder it is in (relative to the root).
    pub folder: Option<String>,
}

impl Row {
    fn of(path: String, entry: &FileEntry, depth: usize) -> Self {
        let dir = is_dir(entry);
        let link = entry.kind == FileKind::Symlink;
        Self {
            name: entry.name.clone(),
            depth,
            dir,
            link,
            hidden: entry.hidden || entry.name.starts_with('.'),
            expanded: false,
            loading: false,
            size: entry.size,
            mtime: entry.mtime,
            kind: kind_label(&entry.name, dir, link && entry.target.is_none()),
            folder: None,
            path,
        }
    }

    /// The glyph drawn before the name.
    #[must_use]
    pub fn icon(&self) -> &'static str {
        if self.dir {
            return "folder";
        }
        match self.kind {
            "PNG Image" | "JPEG Image" | "GIF Image" | "WebP Image" | "SVG Image" | "Image" => "image",
            "Shell Script" => "terminal",
            _ => "file-text",
        }
    }
}

/// Finder-like kind of a file named `name`.
#[must_use]
pub fn kind_label(name: &str, dir: bool, dangling: bool) -> &'static str {
    if dangling {
        return "Alias";
    }
    if dir {
        return "Folder";
    }
    let ext = name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("rs") => "Rust Source",
        Some("md" | "markdown") => "Markdown Document",
        Some("txt") => "Plain Text",
        Some("json") => "JSON",
        Some("toml") => "TOML Document",
        Some("yml" | "yaml") => "YAML Document",
        Some("png") => "PNG Image",
        Some("jpg" | "jpeg") => "JPEG Image",
        Some("gif") => "GIF Image",
        Some("webp") => "WebP Image",
        Some("svg") => "SVG Image",
        Some("ico" | "bmp" | "tiff" | "heic") => "Image",
        Some("pdf") => "PDF Document",
        Some("zip") => "ZIP Archive",
        Some("gz" | "tgz" | "tar" | "xz" | "bz2" | "zst") => "Archive",
        Some("js" | "mjs" | "cjs") => "JavaScript",
        Some("ts" | "tsx") => "TypeScript",
        Some("py") => "Python Script",
        Some("sh" | "bash" | "zsh" | "fish") => "Shell Script",
        Some("c" | "h") => "C Source",
        Some("cpp" | "cc" | "hpp") => "C++ Source",
        Some("go") => "Go Source",
        Some("swift") => "Swift Source",
        Some("html" | "htm") => "HTML Document",
        Some("css") => "CSS",
        Some("lock") => "Lockfile",
        Some("log") => "Log File",
        Some("csv") => "CSV Document",
        Some("xml" | "plist") => "XML Document",
        Some(_) => "Document",
        None => "File",
    }
}

/// Natural, case-insensitive name order (`a2` before `a10`).
#[must_use]
pub fn natural(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let (m, n) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                match m.len().cmp(&n.len()).then_with(|| m.cmp(n)) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(c), Some(d)) => {
                let (c, d) = (c.to_lowercase().next().unwrap_or(c), d.to_lowercase().next().unwrap_or(d));
                if c != d {
                    return c.cmp(&d);
                }
                x.next();
                y.next();
            }
        }
    }
}

fn compare(a: &Row, b: &Row, sort: Sort) -> Ordering {
    if sort.folders_first && a.dir != b.dir {
        return b.dir.cmp(&a.dir);
    }
    let by = match sort.key {
        SortKey::Name => Ordering::Equal,
        SortKey::Modified => a.mtime.cmp(&b.mtime),
        // Folders have no size: they sort as the smallest.
        SortKey::Size => (if a.dir { 0 } else { a.size + 1 }).cmp(&(if b.dir { 0 } else { b.size + 1 })),
        SortKey::Kind => a.kind.cmp(b.kind),
    };
    let by = by.then_with(|| natural(&a.name, &b.name));
    if sort.ascending { by } else { by.reverse() }
}

/// The list for folder `dir`: its items sorted, and each expanded folder's
/// under it; hidden items only when `hidden`; `.git` never.
#[must_use]
pub fn rows(listings: &HashMap<String, Vec<FileEntry>>, dir: &str, expanded: &HashSet<String>, sort: Sort, hidden: bool) -> Vec<Row> {
    fn walk(dir: &str, depth: usize, cx: &Walk<'_>, out: &mut Vec<Row>) {
        let Some(entries) = cx.listings.get(dir) else { return };
        let mut rows: Vec<Row> = entries
            .iter()
            .filter(|e| shown(&e.name) && (cx.hidden || !(e.hidden || e.name.starts_with('.'))))
            .map(|e| Row::of(join(dir, &e.name), e, depth))
            .collect();
        rows.sort_by(|a, b| compare(a, b, cx.sort));
        for mut row in rows {
            let open = row.dir && cx.expanded.contains(&row.path);
            row.expanded = open;
            row.loading = open && !cx.listings.contains_key(&row.path);
            let path = row.path.clone();
            out.push(row);
            if open {
                walk(&path, depth + 1, cx, out);
            }
        }
    }
    struct Walk<'a> {
        listings: &'a HashMap<String, Vec<FileEntry>>,
        expanded: &'a HashSet<String>,
        sort: Sort,
        hidden: bool,
    }
    let mut out = Vec::new();
    walk(dir, 0, &Walk { listings, expanded, sort, hidden }, &mut out);
    out
}

/// Search hits as rows, in the server's order unless sorted by another
/// column than the name.
#[must_use]
pub fn search_rows(hits: &[FileHit], sort: Sort, hidden: bool) -> Vec<Row> {
    let mut rows: Vec<Row> = hits
        .iter()
        .filter(|h| hidden || !(h.entry.hidden || h.path.split('/').any(|p| p.starts_with('.'))))
        .map(|h| Row { folder: Some(parent(&h.path).to_owned()), ..Row::of(h.path.clone(), &h.entry, 0) })
        .collect();
    if sort != Sort::default() {
        rows.sort_by(|a, b| compare(a, b, sort));
    }
    rows
}

/// `Today at 14:02`, `Yesterday at 09:30`, `3 Oct 2026 at 18:11`.
#[must_use]
pub fn date_label(ms: u64, now: chrono::DateTime<chrono::Local>) -> String {
    use chrono::TimeZone as _;
    if ms == 0 {
        return "—".into();
    }
    let Some(at) = i64::try_from(ms).ok().and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single()) else { return "—".into() };
    let time = at.format("%H:%M");
    let days = now.date_naive().signed_duration_since(at.date_naive()).num_days();
    match days {
        0 => format!("Today at {time}"),
        1 => format!("Yesterday at {time}"),
        _ => format!("{} at {time}", at.format("%-d %b %Y")),
    }
}

/// The size column: folders show `—`.
#[must_use]
pub fn size_label(row: &Row) -> String {
    if row.dir { "—".into() } else { super::model::human_size(row.size) }
}

// ---------------------------------------------------------------- selection

/// Which rows are selected (by path, so it survives a re-list or re-sort),
/// where a ⇧-click range starts and where the keyboard is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Picked {
    pub paths: Vec<String>,
    pub anchor: Option<String>,
    pub cursor: Option<String>,
}

impl Picked {
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        self.paths.iter().any(|p| p == path)
    }

    /// Only `path`.
    pub fn only(&mut self, path: &str) {
        self.paths = vec![path.to_owned()];
        self.anchor = Some(path.to_owned());
        self.cursor = Some(path.to_owned());
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// A click on row `at`: alone, ⌘ toggles it, ⇧ selects from the anchor
    /// (⌘⇧ adds that range).
    pub fn click(&mut self, rows: &[Row], at: usize, cmd: bool, shift: bool) {
        let Some(row) = rows.get(at) else { return };
        let path = row.path.clone();
        if shift && let Some(from) = self.anchor.as_ref().and_then(|a| rows.iter().position(|r| &r.path == a)) {
            let (lo, hi) = (from.min(at), from.max(at));
            let range = rows[lo..=hi].iter().map(|r| r.path.clone());
            if cmd {
                for p in range {
                    if !self.contains(&p) {
                        self.paths.push(p);
                    }
                }
            } else {
                self.paths = range.collect();
            }
            self.cursor = Some(path);
            return;
        }
        if cmd {
            if self.contains(&path) {
                self.paths.retain(|p| p != &path);
            } else {
                self.paths.push(path.clone());
            }
            self.anchor = Some(path.clone());
            self.cursor = Some(path);
            return;
        }
        self.only(&path);
    }

    /// ↑ (`delta` -1) or ↓ (1) from the cursor; ⇧ extends from the anchor.
    pub fn step(&mut self, rows: &[Row], delta: isize, extend: bool) -> Option<usize> {
        if rows.is_empty() {
            return None;
        }
        let at = self.cursor.as_ref().and_then(|c| rows.iter().position(|r| &r.path == c));
        let next = match at {
            None if delta < 0 => rows.len() - 1,
            None => 0,
            Some(at) => at.saturating_add_signed(delta).min(rows.len() - 1),
        };
        if extend && self.anchor.is_some() {
            self.click(rows, next, false, true);
        } else {
            self.only(&rows[next].path);
        }
        Some(next)
    }

    pub fn all(&mut self, rows: &[Row]) {
        self.paths = rows.iter().map(|r| r.path.clone()).collect();
        self.anchor = rows.first().map(|r| r.path.clone());
        self.cursor = rows.last().map(|r| r.path.clone());
    }

    /// Selected paths in the list's order (those still listed).
    #[must_use]
    pub fn ordered(&self, rows: &[Row]) -> Vec<String> {
        let picked = self.set();
        rows.iter().filter(|r| picked.contains(r.path.as_str())).map(|r| r.path.clone()).collect()
    }

    /// The selected paths, to look up many.
    #[must_use]
    pub fn set(&self) -> HashSet<&str> {
        self.paths.iter().map(String::as_str).collect()
    }

    /// Forget paths no longer listed.
    pub fn retain_listed(&mut self, rows: &[Row]) {
        let listed: HashSet<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        self.paths.retain(|p| listed.contains(p.as_str()));
        if self.anchor.as_ref().is_some_and(|a| !listed.contains(a.as_str())) {
            self.anchor = None;
        }
        if self.cursor.as_ref().is_some_and(|c| !listed.contains(c.as_str())) {
            self.cursor = None;
        }
    }
}

/// Type-to-select: the first row (from the top) whose name starts with
/// `prefix`, case-insensitively; failing that, the first one after it in
/// name order (Finder's nearest match).
#[must_use]
pub fn type_select(rows: &[Row], prefix: &str) -> Option<usize> {
    if prefix.is_empty() {
        return None;
    }
    let prefix = prefix.to_lowercase();
    if let Some(at) = rows.iter().position(|r| r.depth == 0 && r.name.to_lowercase().starts_with(&prefix)) {
        return Some(at);
    }
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.depth == 0 && natural(&r.name.to_lowercase(), &prefix) == Ordering::Greater)
        .min_by(|(_, a), (_, b)| natural(&a.name, &b.name))
        .map(|(i, _)| i)
}

// ---------------------------------------------------------------- navigation

/// Back and forward (⌘[ ⌘]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    pub back: Vec<String>,
    pub forward: Vec<String>,
}

impl History {
    /// Most folders remembered each way.
    const MAX: usize = 100;

    /// Going from `from` to another folder.
    pub fn went(&mut self, from: &str) {
        if self.back.last().map(String::as_str) != Some(from) {
            self.back.push(from.to_owned());
        }
        if self.back.len() > Self::MAX {
            self.back.remove(0);
        }
        self.forward.clear();
    }

    /// ⌘[: the folder to show, `current` remembered for ⌘].
    pub fn back(&mut self, current: &str) -> Option<String> {
        let to = self.back.pop()?;
        self.forward.push(current.to_owned());
        Some(to)
    }

    /// ⌘].
    pub fn forward(&mut self, current: &str) -> Option<String> {
        let to = self.forward.pop()?;
        self.back.push(current.to_owned());
        Some(to)
    }
}

/// The path bar: the root (by its name) then each folder down to `dir`,
/// with the path each opens.
#[must_use]
pub fn crumbs(root: &str, dir: &str) -> Vec<(String, String)> {
    let root_name = name(root.trim_end_matches('/'));
    let mut out = vec![(if root_name.is_empty() { root.to_owned() } else { root_name.to_owned() }, String::new())];
    let mut at = String::new();
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        at = join(&at, part);
        out.push((part.to_owned(), at.clone()));
    }
    out
}

/// `base` (`untitled.txt`), else `untitled 2.txt`… free among `taken`.
#[must_use]
pub fn free_name<'a>(base: &str, taken: impl Iterator<Item = &'a str>) -> String {
    let taken: HashSet<String> = taken.map(str::to_lowercase).collect();
    if !taken.contains(&base.to_lowercase()) {
        return base.to_owned();
    }
    let (stem, ext) = match base.rfind('.') {
        Some(at) if at > 0 => base.split_at(at),
        _ => (base, ""),
    };
    (2..).map(|n| format!("{stem} {n}{ext}")).find(|n| !taken.contains(&n.to_lowercase())).unwrap_or_else(|| base.to_owned())
}

/// What may go into folder `to_dir`: none of the items itself, nor a
/// folder into itself or below it.
#[must_use]
pub fn can_drop(paths: &[String], to_dir: &str) -> bool {
    !paths.is_empty() && paths.iter().all(|p| p != to_dir && !to_dir.starts_with(&format!("{p}/")) && parent(p) != to_dir)
}

/// A rename's new path: `name` beside `path`; `None` for a name with a
/// `/`, empty or unchanged.
#[must_use]
pub fn renamed(path: &str, new_name: &str) -> Option<String> {
    let new_name = new_name.trim();
    if new_name.is_empty() || new_name.contains('/') || new_name == "." || new_name == ".." || new_name == name(path) {
        return None;
    }
    Some(join(parent(path), new_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool, size: u64, mtime: u64) -> FileEntry {
        FileEntry {
            name: name.into(),
            kind: if dir { FileKind::Dir } else { FileKind::File },
            target: None,
            size,
            mtime,
            created: 0,
            hidden: name.starts_with('.'),
        }
    }

    fn listings() -> HashMap<String, Vec<FileEntry>> {
        HashMap::from([
            (
                String::new(),
                vec![
                    entry("b10.txt", false, 5, 3),
                    entry("b2.txt", false, 50, 1),
                    entry("src", true, 0, 2),
                    entry(".env", false, 1, 9),
                    entry(".git", true, 0, 0),
                    entry("A.md", false, 7, 4),
                ],
            ),
            ("src".to_owned(), vec![entry("main.rs", false, 10, 1)]),
        ])
    }

    fn names(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.path.as_str()).collect()
    }

    #[test]
    fn rows_sort_naturally_with_folders_first_and_expand_in_place() {
        let none = HashSet::new();
        let rows = rows(&listings(), "", &none, Sort::default(), false);
        assert_eq!(names(&rows), ["src", "A.md", "b2.txt", "b10.txt"]);
        let open = HashSet::from(["src".to_owned()]);
        let rows = super::rows(&listings(), "", &open, Sort::default(), true);
        assert_eq!(names(&rows), ["src", "src/main.rs", ".env", "A.md", "b2.txt", "b10.txt"]);
        assert!(rows[0].expanded && rows[1].depth == 1);
        let by_size = Sort { key: SortKey::Size, ascending: false, folders_first: false };
        assert_eq!(names(&super::rows(&listings(), "", &none, by_size, false)), ["b2.txt", "A.md", "b10.txt", "src"]);
        let by_date = Sort::default().clicked(SortKey::Modified);
        assert_eq!((by_date.key, by_date.ascending), (SortKey::Modified, false));
        assert_eq!(names(&super::rows(&listings(), "", &none, by_date, false)), ["src", "A.md", "b10.txt", "b2.txt"]);
        assert!(!by_date.clicked(SortKey::Modified).clicked(SortKey::Modified).ascending);
        assert_eq!(names(&super::rows(&listings(), "src", &none, Sort::default(), false)), ["src/main.rs"]);
    }

    #[test]
    fn kinds_dates_and_sizes_read_like_finder() {
        assert_eq!(kind_label("main.rs", false, false), "Rust Source");
        assert_eq!(kind_label("src", true, false), "Folder");
        assert_eq!(kind_label(".env", false, false), "File");
        assert_eq!(kind_label("x.PNG", false, false), "PNG Image");
        assert_eq!(kind_label("link", false, true), "Alias");
        use chrono::TimeZone as _;
        let now = chrono::Local.with_ymd_and_hms(2026, 10, 28, 15, 0, 0).unwrap();
        let ms = |d: u32, h: u32| u64::try_from(chrono::Local.with_ymd_and_hms(2026, 10, d, h, 5, 0).unwrap().timestamp_millis()).unwrap();
        assert_eq!(date_label(ms(28, 9), now), "Today at 09:05");
        assert_eq!(date_label(ms(27, 23), now), "Yesterday at 23:05");
        assert_eq!(date_label(ms(3, 18), now), "3 Oct 2026 at 18:05");
        assert_eq!(date_label(0, now), "—");
        let rows = rows(&listings(), "", &HashSet::new(), Sort::default(), false);
        assert_eq!(size_label(&rows[0]), "—");
        assert_eq!(rows[0].icon(), "folder");
        assert!(natural("a2", "a10").is_lt() && natural("B", "a").is_gt());
    }

    #[test]
    fn selection_follows_clicks_and_keys() {
        let rows = rows(&listings(), "", &HashSet::new(), Sort::default(), false);
        let mut picked = Picked::default();
        picked.click(&rows, 1, false, false);
        picked.click(&rows, 3, false, true);
        assert_eq!(picked.ordered(&rows), ["A.md", "b2.txt", "b10.txt"]);
        picked.click(&rows, 2, true, false);
        assert_eq!(picked.ordered(&rows), ["A.md", "b10.txt"]);
        picked.click(&rows, 0, true, true);
        assert_eq!(picked.ordered(&rows), ["src", "A.md", "b2.txt", "b10.txt"]);
        picked.only("A.md");
        assert_eq!(picked.step(&rows, 1, true), Some(2));
        assert_eq!(picked.ordered(&rows), ["A.md", "b2.txt"]);
        assert_eq!(picked.step(&rows, -5, false), Some(0));
        assert_eq!(picked.paths, ["src"]);
        picked.all(&rows);
        assert_eq!(picked.paths.len(), 4);
        picked.retain_listed(&rows[..2]);
        assert_eq!(picked.paths, ["src", "A.md"]);
        assert_eq!(type_select(&rows, "B1"), Some(3));
        assert_eq!(type_select(&rows, "a"), Some(1));
        assert_eq!(type_select(&rows, "c"), Some(0), "the nearest name after it");
        assert_eq!(type_select(&rows, "zz"), None);
    }

    #[test]
    fn history_paths_and_names() {
        let mut history = History::default();
        history.went("");
        history.went("src");
        assert_eq!(history.back("src/a"), Some("src".into()));
        assert_eq!(history.forward("src"), Some("src/a".into()));
        assert_eq!(history.back("src/a"), Some("src".into()));
        history.went("src");
        assert!(history.forward.is_empty());
        assert_eq!(crumbs("/home/me/proj", "src/a"), [("proj".into(), String::new()), ("src".into(), "src".into()), ("a".into(), "src/a".into())]);
        assert_eq!(crumbs("/", ""), [("/".to_owned(), String::new())]);
        assert_eq!(free_name("untitled folder", ["Untitled Folder", "untitled folder 2"].into_iter()), "untitled folder 3");
        assert_eq!(free_name("untitled.txt", ["a"].into_iter()), "untitled.txt");
        assert!(can_drop(&["a/b".into()], "c"));
        assert!(!can_drop(&["a".into()], "a/b"), "not into itself");
        assert!(!can_drop(&["a/b".into()], "a"), "already there");
        assert_eq!(renamed("src/a.rs", "b.rs").as_deref(), Some("src/b.rs"));
        assert_eq!(renamed("a.rs", " a.rs "), None);
        assert_eq!(renamed("a.rs", "x/y"), None);
    }
}
