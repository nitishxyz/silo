//! The Files block's logic, free of gpui: tabs, the tree's rows, which
//! reads a change calls for, positions and selection in a text, find in
//! file, go to line, the ⇧⌘P finder's ranking and what a file is shown as.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use silo_proto::{
    control::{FileEntry, FileKind},
    model::FileHighlight,
};
use unicode_width::UnicodeWidthChar as _;

/// Columns a tab advances to.
pub const TAB: usize = 4;

/// `path`'s last component.
#[must_use]
pub fn name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `path`'s folder; empty for the root's entries.
#[must_use]
pub fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// `name` in folder `dir` (empty: the root).
#[must_use]
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }
}

// ---------------------------------------------------------------- tabs

/// What each tab says: its file name, and its folder after it when another
/// open tab has the same name.
#[must_use]
pub fn tab_labels(open: &[String]) -> Vec<(String, Option<String>)> {
    open.iter()
        .map(|path| {
            let n = name(path);
            let twin = open.iter().filter(|p| name(p) == n).count() > 1;
            let dir = parent(path);
            (n.to_owned(), (twin && !dir.is_empty()).then(|| name(dir).to_owned()))
        })
        .collect()
}

/// Close tab `path`: the tabs left and the one shown next. Closing the
/// shown tab shows its right neighbour, else its left; closing another
/// keeps the shown one.
#[must_use]
pub fn close_tab(open: &[String], active: Option<&str>, path: &str) -> (Vec<String>, Option<String>) {
    let Some(at) = open.iter().position(|p| p == path) else { return (open.to_vec(), active.map(str::to_owned)) };
    let mut left = open.to_vec();
    left.remove(at);
    let next =
        if active == Some(path) { left.get(at).or_else(|| left.get(at.wrapping_sub(1))).or(left.last()).cloned() } else { active.map(str::to_owned) };
    (left, next)
}

/// The tab `delta` steps from the shown one, wrapping (⌃Tab, ⌃⇧Tab).
#[must_use]
pub fn step_tab(open: &[String], active: Option<&str>, delta: isize) -> Option<String> {
    if open.is_empty() {
        return None;
    }
    let at = active.and_then(|a| open.iter().position(|p| p == a)).unwrap_or(0);
    let len = open.len().cast_signed();
    open.get((at.cast_signed() + delta).rem_euclid(len).cast_unsigned()).cloned()
}

// ---------------------------------------------------------------- tree

/// Whether `entry` opens as a folder (a folder, or a link to one inside
/// the root).
#[must_use]
pub fn is_dir(entry: &FileEntry) -> bool {
    entry.kind == FileKind::Dir || (entry.kind == FileKind::Symlink && entry.target == Some(FileKind::Dir))
}

/// Names never listed: git's own folder.
#[must_use]
pub fn shown(name: &str) -> bool {
    name != ".git"
}

/// A row of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub dir: bool,
    pub link: bool,
    /// A dotfile: drawn dimmed.
    pub hidden: bool,
    pub expanded: bool,
    /// Expanded, its listing not here yet.
    pub loading: bool,
}

/// The tree's rows: the root's listing, and each expanded folder's under
/// it, depth first. `.git` is left out.
#[must_use]
pub fn tree_rows(listings: &HashMap<String, Vec<FileEntry>>, expanded: &HashSet<String>) -> Vec<TreeRow> {
    fn walk(dir: &str, depth: usize, listings: &HashMap<String, Vec<FileEntry>>, expanded: &HashSet<String>, out: &mut Vec<TreeRow>) {
        let Some(entries) = listings.get(dir) else { return };
        for entry in entries.iter().filter(|e| shown(&e.name)) {
            let path = join(dir, &entry.name);
            let dir = is_dir(entry);
            let open = dir && expanded.contains(&path);
            out.push(TreeRow {
                path: path.clone(),
                name: entry.name.clone(),
                depth,
                dir,
                link: entry.kind == FileKind::Symlink,
                hidden: entry.name.starts_with('.'),
                expanded: open,
                loading: open && !listings.contains_key(&path),
            });
            if open {
                walk(&path, depth + 1, listings, expanded, out);
            }
        }
    }
    let mut out = Vec::new();
    walk("", 0, listings, expanded, &mut out);
    out
}

/// A tree key: the row it moves to (by index), or a folder to open or
/// close (its path, and whether it opens).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeMove {
    Select(usize),
    Toggle(String, bool),
}

/// ↑ ↓ → ← on row `at` of `rows` (none selected: the first row).
#[must_use]
pub fn tree_key(rows: &[TreeRow], at: Option<usize>, key: &str) -> Option<TreeMove> {
    if rows.is_empty() {
        return None;
    }
    let Some(at) = at.filter(|&a| a < rows.len()) else { return Some(TreeMove::Select(0)) };
    let row = &rows[at];
    match key {
        "up" => Some(TreeMove::Select(at.saturating_sub(1))),
        "down" => Some(TreeMove::Select((at + 1).min(rows.len() - 1))),
        "right" if row.dir && !row.expanded => Some(TreeMove::Toggle(row.path.clone(), true)),
        "right" if row.dir => rows.get(at + 1).filter(|r| r.depth > row.depth).map(|_| TreeMove::Select(at + 1)),
        "left" if row.dir && row.expanded => Some(TreeMove::Toggle(row.path.clone(), false)),
        "left" => rows[..at].iter().rposition(|r| r.dir && r.depth + 1 == row.depth).map(TreeMove::Select),
        _ => None,
    }
}

/// Expanded folders without `dir` and everything under it (closing a
/// folder forgets which of its folders were open).
#[must_use]
pub fn collapse(expanded: &[String], dir: &str) -> Vec<String> {
    let under = format!("{dir}/");
    expanded.iter().filter(|p| *p != dir && !p.starts_with(&under)).cloned().collect()
}

/// `dir` and its folders up to the root, as expanded folders (revealing a
/// file opens the folders above it).
#[must_use]
pub fn reveal(expanded: &[String], file: &str) -> Vec<String> {
    let mut out = expanded.to_vec();
    let mut dir = parent(file);
    let mut add = Vec::new();
    while !dir.is_empty() {
        if !out.iter().any(|p| p == dir) {
            add.push(dir.to_owned());
        }
        dir = parent(dir);
    }
    add.reverse();
    out.extend(add);
    out
}

/// [`reveal`], keeping at most `max` expanded folders: the longest-open
/// ones that are not above `file` make way.
#[must_use]
pub fn reveal_within(expanded: &[String], file: &str, max: usize) -> Vec<String> {
    let mut out = reveal(expanded, file);
    let above = |dir: &str| file.starts_with(&format!("{dir}/"));
    while out.len() > max {
        let Some(oldest) = out.iter().position(|d| !above(d)) else { break };
        out.remove(oldest);
    }
    out
}

// ---------------------------------------------------------------- changes

/// What `files.changed` calls for: open files to read again, and listed
/// folders to list again. `truncated` (paths left out): all of them.
#[must_use]
pub fn affected<'a>(changed: &[String], truncated: bool, open: &[String], listed: impl Iterator<Item = &'a String>) -> (Vec<String>, Vec<String>) {
    let reread = open.iter().filter(|p| truncated || changed.contains(p)).cloned().collect();
    let relist = listed.filter(|dir| truncated || changed.iter().any(|c| parent(c) == dir.as_str() || c == *dir)).cloned().collect::<Vec<_>>();
    (reread, relist)
}

// ---------------------------------------------------------------- text

/// A text split into lines (`\n`, a `\r` before it dropped from view).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
    pub text: String,
    /// Each line's bytes, without its line break.
    pub lines: Vec<Range<usize>>,
    /// The line with the most columns (for the horizontal extent).
    pub widest: usize,
    pub widest_columns: usize,
    /// The text is ASCII (UTF-16 offsets are byte offsets); edits only
    /// ever clear it.
    pub ascii: bool,
}

impl Doc {
    #[must_use]
    pub fn new(text: String) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (i, _) in text.match_indices('\n') {
            lines.push(start..line_end(&text, start, i));
            start = i + 1;
        }
        let end = text.len();
        lines.push(start..end);
        let (mut widest, mut widest_columns) = (0, 0);
        for (i, range) in lines.iter().enumerate() {
            // A cheap bound first: no line has more columns than twice its bytes.
            if range.len() * 2 <= widest_columns {
                continue;
            }
            let columns = columns(&text[range.clone()]);
            if columns > widest_columns {
                (widest, widest_columns) = (i, columns);
            }
        }
        let ascii = text.is_ascii();
        Self { text, lines, widest, widest_columns, ascii }
    }

    #[must_use]
    pub fn line(&self, ix: usize) -> &str {
        self.lines.get(ix).map_or("", |r| &self.text[r.clone()])
    }

    /// Lines (an empty text has one).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The text between `a` and `b` (in order), lines joined with `\n`.
    #[must_use]
    pub fn slice(&self, a: Pos, b: Pos) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if a.line >= self.line_count() {
            return String::new();
        }
        let b = if b.line >= self.line_count() { Pos { line: self.line_count() - 1, col: self.line(self.line_count() - 1).len() } } else { b };
        let mut out = String::new();
        for ix in a.line..=b.line {
            let line = self.line(ix);
            let from = if ix == a.line { a.col.min(line.len()) } else { 0 };
            let to = if ix == b.line { b.col.min(line.len()) } else { line.len() };
            if ix > a.line {
                out.push('\n');
            }
            out.push_str(&line[from..to.max(from)]);
        }
        out
    }

    /// Where the text ends.
    #[must_use]
    pub fn end(&self) -> Pos {
        let line = self.line_count().saturating_sub(1);
        Pos { line, col: self.line(line).len() }
    }

    /// `pos` clamped into the text, on a character boundary.
    #[must_use]
    pub fn clamp(&self, pos: Pos) -> Pos {
        if pos.line >= self.line_count() {
            return self.end();
        }
        let line = self.line(pos.line);
        let mut col = pos.col.min(line.len());
        while !line.is_char_boundary(col) {
            col -= 1;
        }
        Pos { line: pos.line, col }
    }

    /// The byte offset of `pos` in [`Self::text`] (clamped).
    #[must_use]
    pub fn offset(&self, pos: Pos) -> usize {
        let pos = self.clamp(pos);
        self.lines[pos.line].start + pos.col
    }

    /// The line holding byte `offset` (its break counts as its own).
    #[must_use]
    pub fn line_of(&self, offset: usize) -> usize {
        self.lines.partition_point(|r| r.start <= offset).saturating_sub(1)
    }

    /// The position of byte `offset` (one inside a `\r\n` is the line's end).
    #[must_use]
    pub fn pos(&self, offset: usize) -> Pos {
        let line = self.line_of(offset.min(self.text.len()));
        let range = &self.lines[line];
        self.clamp(Pos { line, col: offset.saturating_sub(range.start).min(range.len()) })
    }

    /// Replace bytes `range` of the text with `text`, updating the line
    /// table in place (the lines around the edit are scanned again, the
    /// ones after it move). Returns the lines replaced: from `first`,
    /// `removed` old lines became `added` new ones.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Splice {
        let start = range.start.min(self.text.len());
        let range = start..range.end.clamp(start, self.text.len());
        let (first, last) = (self.line_of(range.start), self.line_of(range.end));
        let tail = last + 1 < self.lines.len();
        let delta = text.len().cast_signed() - range.len().cast_signed();
        let old_next = if tail { self.lines[last + 1].start } else { self.text.len() };
        self.ascii &= text.is_ascii();
        self.text.replace_range(range, text);
        let start = self.lines[first].start;
        let end = old_next.saturating_add_signed(delta);
        let mut fresh = Vec::new();
        let mut from = start;
        for (i, _) in self.text[start..end].match_indices('\n') {
            let i = start + i;
            fresh.push(from..line_end(&self.text, from, i));
            from = i + 1;
        }
        if !tail {
            fresh.push(from..self.text.len());
        }
        let added = fresh.len();
        let removed = last - first + 1;
        self.lines.splice(first..=last, fresh);
        for r in &mut self.lines[first + added..] {
            *r = r.start.saturating_add_signed(delta)..r.end.saturating_add_signed(delta);
        }
        // The widest line: kept when it is outside the edit and nothing new is wider.
        let widest_edited = (first..=last).contains(&self.widest);
        if self.widest > last {
            self.widest = (self.widest + added).saturating_sub(removed);
        }
        let (mut widest, mut columns) = if widest_edited { (first, 0) } else { (self.widest, self.widest_columns) };
        for ix in first..first + added {
            let c = self::columns(self.line(ix));
            if c > columns {
                (widest, columns) = (ix, c);
            }
        }
        if widest_edited && columns < self.widest_columns {
            let fresh = Self::new(std::mem::take(&mut self.text));
            *self = fresh;
        } else {
            (self.widest, self.widest_columns) = (widest, columns);
        }
        Splice { first, removed, added }
    }
}

/// Lines an edit replaced ([`Doc::replace`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Splice {
    pub first: usize,
    pub removed: usize,
    pub added: usize,
}

/// The end of the line from `start` broken at `\n` index `nl` (a `\r`
/// before it is not part of the line).
fn line_end(text: &str, start: usize, nl: usize) -> usize {
    if nl > start && text.as_bytes()[nl - 1] == b'\r' { nl - 1 } else { nl }
}

/// Columns `text` takes, tabs to the next multiple of [`TAB`], wide
/// characters two.
#[must_use]
pub fn columns(text: &str) -> usize {
    text.chars().fold(0, advance)
}

fn advance(at: usize, c: char) -> usize {
    if c == '\t' { (at / TAB + 1) * TAB } else { at + c.width().unwrap_or(0) }
}

/// `line` as drawn: tabs expanded to spaces.
#[must_use]
pub fn expand_tabs(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\t') {
        return line.into();
    }
    let mut out = String::with_capacity(line.len() + 8);
    let mut at = 0;
    for c in line.chars() {
        let next = advance(at, c);
        if c == '\t' {
            out.extend(std::iter::repeat_n(' ', next - at));
        } else {
            out.push(c);
        }
        at = next;
    }
    out.into()
}

/// Byte `offset` of `line` in its drawn form ([`expand_tabs`]).
#[must_use]
pub fn to_shown(line: &str, offset: usize) -> usize {
    let mut shown = 0;
    let mut at = 0;
    for (i, c) in line.char_indices() {
        if i >= offset {
            break;
        }
        let next = advance(at, c);
        shown += if c == '\t' { next - at } else { c.len_utf8() };
        at = next;
    }
    shown
}

/// The byte of `line` nearest column `column` (a character is entered past
/// its middle).
#[must_use]
pub fn col_at(line: &str, column: f32) -> usize {
    let mut at = 0;
    for (i, c) in line.char_indices() {
        let next = advance(at, c);
        #[allow(clippy::cast_precision_loss)]
        let middle = (at + next) as f32 / 2.0;
        if column < middle {
            return i;
        }
        at = next;
    }
    line.len()
}

/// A place in a text: a line and a byte in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// What a drag selects by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Char,
    Word,
    Line,
}

/// A selection, as the agent pane's: pressed at `anchor`, dragged to
/// `head`, by characters, words (double-click) or lines (triple-click).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Pos,
    pub head: Pos,
    pub unit: Unit,
}

impl Selection {
    #[must_use]
    pub fn new(at: Pos, unit: Unit) -> Self {
        Self { anchor: at, head: at, unit }
    }

    /// Its ends in order, grown to whole words or lines.
    #[must_use]
    pub fn bounds(&self, doc: &Doc) -> (Pos, Pos) {
        let (a, b) = if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) };
        match self.unit {
            Unit::Char => (a, b),
            Unit::Word => {
                let (wa, wb) = (word_at(doc.line(a.line), a.col), word_at(doc.line(b.line), b.col));
                (Pos { line: a.line, col: wa.start }, Pos { line: b.line, col: wb.end })
            }
            Unit::Line => {
                let end =
                    if b.line + 1 < doc.line_count() { Pos { line: b.line + 1, col: 0 } } else { Pos { line: b.line, col: doc.line(b.line).len() } };
                (Pos { line: a.line, col: 0 }, end)
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self, doc: &Doc) -> bool {
        let (a, b) = self.bounds(doc);
        a == b
    }

    /// The part of line `ix` selected, in its bytes; the line break counts
    /// as one byte past the end.
    #[must_use]
    pub fn on_line(&self, doc: &Doc, ix: usize) -> Option<Range<usize>> {
        let (a, b) = self.bounds(doc);
        if ix < a.line || ix > b.line || a == b {
            return None;
        }
        let len = doc.line(ix).len();
        let from = if ix == a.line { a.col.min(len) } else { 0 };
        let to = if ix == b.line { b.col.min(len) } else { len + 1 };
        (to > from).then_some(from..to)
    }

    #[must_use]
    pub fn text(&self, doc: &Doc) -> String {
        let (a, b) = self.bounds(doc);
        doc.slice(a, b)
    }
}

/// The word (letters, digits, `_`), run of spaces or single other
/// character at byte `col` of `line`.
#[must_use]
pub fn word_at(line: &str, col: usize) -> Range<usize> {
    let col = col.min(line.len());
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let Some(c) = line[col..].chars().next().or_else(|| line[..col].chars().next_back()) else { return col..col };
    let at = if col == line.len() { col - c.len_utf8() } else { col };
    let k = class(c);
    if k == 2 {
        return at..at + c.len_utf8();
    }
    let start = line[..at].char_indices().rev().take_while(|(_, c)| class(*c) == k).last().map_or(at, |(i, _)| i);
    let end = line[at..].char_indices().find(|(_, c)| class(*c) != k).map_or(line.len(), |(i, _)| at + i);
    start..end
}

// ---------------------------------------------------------------- find

/// Every match of `query` in `doc`, in order, per line. Smart case: a
/// query with no capital letter matches any case.
#[must_use]
pub fn find_all(doc: &Doc, query: &str) -> Vec<(usize, Range<usize>)> {
    if query.is_empty() {
        return Vec::new();
    }
    let fold = !query.chars().any(char::is_uppercase);
    let needle = if fold { query.to_lowercase() } else { query.to_owned() };
    let mut out = Vec::new();
    for ix in 0..doc.line_count() {
        let line = doc.line(ix);
        // Lowercasing may change lengths outside ASCII: match those as they are.
        let hay = if fold && line.is_ascii() { std::borrow::Cow::Owned(line.to_ascii_lowercase()) } else { line.into() };
        let mut from = 0;
        while let Some(i) = hay[from..].find(&needle) {
            let start = from + i;
            out.push((ix, start..start + needle.len()));
            from = start + needle.len().max(1);
            if out.len() >= 10_000 {
                return out;
            }
        }
    }
    out
}

/// The match to show first: the first at or after line `from`.
#[must_use]
pub fn first_from(matches: &[(usize, Range<usize>)], from: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.iter().position(|(line, _)| *line >= from).unwrap_or(0))
}

/// Go to line: `12` or `12:5` (1-based) as a 0-based line and column.
#[must_use]
pub fn parse_goto(text: &str) -> Option<(usize, Option<usize>)> {
    let text = text.trim();
    let (line, col) = match text.split_once(':') {
        Some((l, c)) => (l, Some(c)),
        None => (text, None),
    };
    let line = line.trim().parse::<usize>().ok().filter(|&l| l >= 1)?;
    let col = match col.map(str::trim) {
        Some(c) if !c.is_empty() => Some(c.parse::<usize>().ok().filter(|&c| c >= 1)? - 1),
        _ => None,
    };
    Some((line - 1, col))
}

// ---------------------------------------------------------------- finder

/// A finder result: the path, and the matched bytes of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub ranges: Vec<Range<usize>>,
}

/// `paths` (from `files.find`) ranked for `query` with the palette's
/// scoring: a match in the file name beats one across the path; open
/// tabs come first while nothing is typed.
#[must_use]
pub fn rank(query: &str, paths: Vec<String>, open: &[String]) -> Vec<Found> {
    let q = query.trim();
    if q.is_empty() {
        let mut out: Vec<Found> = open.iter().rev().map(|p| Found { path: p.clone(), ranges: Vec::new() }).collect();
        out.extend(paths.into_iter().filter(|p| !open.contains(p)).map(|path| Found { path, ranges: Vec::new() }));
        return out;
    }
    let mut scored: Vec<(u8, i32, usize, Found)> = paths
        .into_iter()
        .enumerate()
        .filter_map(|(i, path)| {
            let n = name(&path);
            let base = path.len() - n.len();
            let in_name = crate::finder::score(q, n, "").filter(|m| !q.contains('/') && (m.tier > 0 || !m.ranges.is_empty()));
            match in_name {
                Some(m) => {
                    let ranges = m.ranges.iter().map(|r| r.start + base..r.end + base).collect();
                    Some((m.tier + 1, m.score, i, Found { path, ranges }))
                }
                None => {
                    let m = crate::finder::score(q, &path, "")?;
                    (!m.ranges.is_empty()).then_some((0, m.score, i, Found { path, ranges: m.ranges }))
                }
            }
        })
        .collect();
    scored.sort_by_key(|(tier, score, i, _)| (std::cmp::Reverse(*tier), std::cmp::Reverse(*score), *i));
    scored.into_iter().map(|(_, _, _, f)| f).collect()
}

// ---------------------------------------------------------------- content

/// How a file is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    Text,
    Markdown,
    Image,
    Pdf,
    Binary,
}

/// Image types the viewer draws (and `files.read_image` sends).
pub const IMAGES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp", "image/svg+xml"];

/// How a file at `path` with type `mime` shows; `text`: it read as text.
#[must_use]
pub fn shown_as(path: &str, mime: &str, text: bool) -> Shown {
    if IMAGES.contains(&mime) {
        return Shown::Image;
    }
    if mime == "application/pdf" {
        return Shown::Pdf;
    }
    if !text {
        return Shown::Binary;
    }
    let ext = name(path).rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    if matches!(ext.as_deref(), Some("md" | "markdown" | "mdx")) { Shown::Markdown } else { Shown::Text }
}

/// `bytes` for people: `812 B`, `3.2 KB`, `14 MB`.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if value < 10.0 { format!("{value:.1} {}", UNITS[unit]) } else { format!("{value:.0} {}", UNITS[unit]) }
}

/// A binary file's summary line.
#[must_use]
pub fn binary_summary(mime: &str, size: u64) -> String {
    format!("binary · {} · {mime}", human_size(size))
}

/// The highlight covering 1-based line `line`, if any: its index, and
/// whether `line` is its first.
#[must_use]
pub fn mark_at(marks: &[FileHighlight], line: u64) -> Option<(usize, bool)> {
    marks.iter().position(|m| m.start <= line && line <= m.end).map(|i| (i, marks[i].start == line))
}

#[cfg(test)]
mod tests;
