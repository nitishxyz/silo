//! The Changes block's logic, free of gpui (docs/CHANGES_BLOCK.md §3, §5):
//! a `FileDiff` as rows (unified or side by side), long unchanged runs
//! folded, word-level highlights inside changed lines, the file list's
//! groups, the scope picker's rows, the review message sent back to an
//! agent and the thread's turn card.

use std::{collections::HashSet, ops::Range};

use super::wire::{ChangedFile, DiffLine, FileDiff, FileStatus, Hunk, LineKind, ReviewComment, Side};

/// Unchanged lines kept on each side of a change when a run is folded.
pub const KEEP: usize = 3;
/// A run hides at least this many lines when folded (fewer: shown).
pub const FOLD_MIN: usize = 4;
/// Word highlights are skipped past this many tokens a line.
const WORDS_MAX: usize = 400;
/// Lines of code quoted under a comment's place in a review.
const QUOTE_MAX: usize = 6;

// ---------------------------------------------------------------- rows

/// A fold: file, hunk and the first hidden line's index in the hunk.
pub type FoldId = (usize, usize, usize);

/// One row of the diff view. `file` indexes the diffs given to [`rows`],
/// `hunk` its hunks, `at` / `old` / `new` the hunk's lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A file's header (the summary of every file).
    File { file: usize },
    /// Unchanged lines git left out before hunk `hunk` (from the file's start
    /// or the previous hunk's end).
    Gap { file: usize, hunk: usize, lines: u32 },
    /// The hunk's `@@ … @@` header and its actions.
    Hunk { file: usize, hunk: usize },
    /// Unchanged lines folded away; a click shows them.
    Fold { file: usize, hunk: usize, at: usize, lines: usize },
    /// One line (unified), or a line drawn across both sides (split: no
    /// newline at the end).
    Line { file: usize, hunk: usize, at: usize },
    /// Side by side: the old line left, the new right.
    Pair { file: usize, hunk: usize, old: Option<usize>, new: Option<usize> },
    /// A review comment under the line it is on (`index` into the comments).
    Comment { file: usize, index: usize },
    /// Nothing to show for a file: binary, truncated or no hunks.
    Note { file: usize, text: String },
}

impl Row {
    #[must_use]
    pub fn file(&self) -> usize {
        match self {
            Row::File { file }
            | Row::Gap { file, .. }
            | Row::Hunk { file, .. }
            | Row::Fold { file, .. }
            | Row::Line { file, .. }
            | Row::Pair { file, .. }
            | Row::Comment { file, .. }
            | Row::Note { file, .. } => *file,
        }
    }

    /// The hunk it belongs to, if any.
    #[must_use]
    pub fn hunk(&self) -> Option<usize> {
        match self {
            Row::Gap { hunk, .. } | Row::Hunk { hunk, .. } | Row::Fold { hunk, .. } | Row::Line { hunk, .. } | Row::Pair { hunk, .. } => Some(*hunk),
            Row::File { .. } | Row::Comment { .. } | Row::Note { .. } => None,
        }
    }
}

/// How the rows are laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub split: bool,
    /// Every file gets its header row (the summary).
    pub headers: bool,
}

/// Which context lines of `hunk` stay shown: runs longer than
/// `KEEP` on each side of a change (and `FOLD_MIN` more) fold, unless
/// expanded. Returns `(start, len)` of each fold.
#[must_use]
pub fn folds(hunk: &Hunk) -> Vec<(usize, usize)> {
    let lines = &hunk.lines;
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Context {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && lines[i].kind == LineKind::Context {
            i += 1;
        }
        let (lead, tail) = (if start == 0 { 0 } else { KEEP }, if i == lines.len() { 0 } else { KEEP });
        let len = i - start;
        if len >= lead + tail + FOLD_MIN {
            out.push((start + lead, len - lead - tail));
        }
    }
    out
}

/// The diff view's rows for `diffs`, folds closed unless in `expanded`;
/// `comments` show under the line they end on.
#[must_use]
pub fn rows(diffs: &[&FileDiff], layout: Layout, expanded: &HashSet<FoldId>, comments: &[ReviewComment]) -> Vec<Row> {
    let mut out = Vec::new();
    for (f, diff) in diffs.iter().enumerate() {
        if layout.headers {
            out.push(Row::File { file: f });
        }
        let mine: Vec<usize> = comments.iter().enumerate().filter(|(_, c)| c.path == diff.path).map(|(i, _)| i).collect();
        let mut placed = vec![false; mine.len()];
        let start = out.len();
        if diff.binary {
            out.push(Row::Note { file: f, text: "Binary file".into() });
        } else if diff.hunks.is_empty() {
            out.push(Row::Note { file: f, text: empty_note(diff.status).into() });
        }
        let mut prev_end = 1u32;
        for (h, hunk) in diff.hunks.iter().enumerate() {
            let from = if hunk.old_lines == 0 { hunk.old_start + 1 } else { hunk.old_start };
            let gap = from.saturating_sub(prev_end);
            if gap > 0 {
                out.push(Row::Gap { file: f, hunk: h, lines: gap });
            }
            // A hunk that only adds has `old_start` the line it adds after.
            prev_end = hunk.old_start + hunk.old_lines.max(1);
            out.push(Row::Hunk { file: f, hunk: h });
            let folded: Vec<(usize, usize)> = folds(hunk).into_iter().filter(|(at, _)| !expanded.contains(&(f, h, *at))).collect();
            let hidden = |i: usize| folded.iter().find(|(at, n)| (*at..at + n).contains(&i)).copied();
            let mut body = Vec::new();
            if layout.split {
                for (old, new) in pairs(hunk) {
                    let at = old.or(new).unwrap_or(0);
                    if let Some((fold, n)) = hidden(at) {
                        if fold == at {
                            body.push(Row::Fold { file: f, hunk: h, at: fold, lines: n });
                        }
                        continue;
                    }
                    if old.is_some() && old == new && hunk.lines[at].kind != LineKind::Context {
                        body.push(Row::Line { file: f, hunk: h, at });
                    } else {
                        body.push(Row::Pair { file: f, hunk: h, old, new });
                    }
                }
            } else {
                for at in 0..hunk.lines.len() {
                    if let Some((fold, n)) = hidden(at) {
                        if fold == at {
                            body.push(Row::Fold { file: f, hunk: h, at: fold, lines: n });
                        }
                        continue;
                    }
                    body.push(Row::Line { file: f, hunk: h, at });
                }
            }
            for row in body {
                let lines = row_lines(hunk, &row);
                out.push(row);
                for (k, &c) in mine.iter().enumerate() {
                    let comment = &comments[c];
                    let end = comment.end_line.unwrap_or(comment.line);
                    if !placed[k] && lines.iter().any(|(side, n)| *side == comment.side && *n == end) {
                        placed[k] = true;
                        out.push(Row::Comment { file: f, index: c });
                    }
                }
            }
        }
        if diff.truncated {
            out.push(Row::Note { file: f, text: "The diff is too large: showing its start".into() });
        }
        // Comments on lines not shown (folded, or the file changed since): at the file's top.
        let rest: Vec<Row> = mine.iter().zip(&placed).filter(|(_, p)| !**p).map(|(c, _)| Row::Comment { file: f, index: *c }).collect();
        out.splice(start..start, rest);
    }
    out
}

fn empty_note(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added | FileStatus::Untracked => "Empty file",
        FileStatus::Deleted => "Deleted (empty)",
        FileStatus::Renamed | FileStatus::Copied => "Moved without changes",
        FileStatus::TypeChanged => "Its type changed",
        FileStatus::Conflicted => "Conflicted: resolve it in the file",
        FileStatus::Modified | FileStatus::Unknown => "Only its mode changed",
    }
}

/// The (side, number) of each line a row shows.
fn row_lines(hunk: &Hunk, row: &Row) -> Vec<(Side, u32)> {
    let of = |at: usize| {
        let l = &hunk.lines[at];
        l.old.map(|n| (Side::Old, n)).into_iter().chain(l.new.map(|n| (Side::New, n)))
    };
    match row {
        Row::Line { at, .. } => of(*at).collect(),
        Row::Pair { old, new, .. } => {
            let mut v: Vec<(Side, u32)> = old.map(of).into_iter().flatten().filter(|(s, _)| *s == Side::Old).collect();
            v.extend(new.map(of).into_iter().flatten().filter(|(s, _)| *s == Side::New));
            v
        }
        _ => Vec::new(),
    }
}

/// Side by side: context on both sides, each run of removed lines next to
/// the added lines after it (the longer run's rest against nothing). A
/// "no newline" marker is `(Some(i), Some(i))`.
#[must_use]
pub fn pairs(hunk: &Hunk) -> Vec<(Option<usize>, Option<usize>)> {
    let lines = &hunk.lines;
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        match lines[i].kind {
            LineKind::Context => {
                out.push((Some(i), Some(i)));
                i += 1;
            }
            LineKind::NoNewline | LineKind::Unknown => {
                out.push((Some(i), Some(i)));
                i += 1;
            }
            LineKind::Removed | LineKind::Added => {
                let (mut removed, mut added, mut marks) = (Vec::new(), Vec::new(), Vec::new());
                while i < lines.len() && lines[i].kind == LineKind::Removed {
                    removed.push(i);
                    i += 1;
                    while i < lines.len() && lines[i].kind == LineKind::NoNewline {
                        marks.push(i);
                        i += 1;
                    }
                }
                while i < lines.len() && lines[i].kind == LineKind::Added {
                    added.push(i);
                    i += 1;
                    while i < lines.len() && lines[i].kind == LineKind::NoNewline {
                        marks.push(i);
                        i += 1;
                    }
                }
                for k in 0..removed.len().max(added.len()) {
                    out.push((removed.get(k).copied(), added.get(k).copied()));
                }
                out.extend(marks.into_iter().map(|m| (Some(m), Some(m))));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- words

/// Each line's changed bytes, per hunk and line: a removed line against the
/// added line it pairs with. Lines with too little in common get none (the
/// line's tint says it).
#[must_use]
pub fn words(diff: &FileDiff) -> Vec<Vec<Spans>> {
    diff.hunks
        .iter()
        .map(|hunk| {
            let mut out = vec![Vec::new(); hunk.lines.len()];
            for (old, new) in pairs(hunk) {
                if let (Some(o), Some(n)) = (old, new)
                    && hunk.lines[o].kind == LineKind::Removed
                    && hunk.lines[n].kind == LineKind::Added
                    && let Some((a, b)) = word_diff(&hunk.lines[o].text, &hunk.lines[n].text)
                {
                    out[o] = a;
                    out[n] = b;
                }
            }
            out
        })
        .collect()
}

/// Byte ranges in a line.
pub type Spans = Vec<Range<usize>>;

/// Words, runs of spaces and single other characters.
fn tokens(text: &str) -> Vec<Range<usize>> {
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut last = None;
    for (i, c) in text.char_indices() {
        let k = class(c);
        match out.last_mut() {
            Some(r) if last == Some(k) && k != 2 => r.end = i + c.len_utf8(),
            _ => out.push(i..i + c.len_utf8()),
        }
        last = Some(k);
    }
    out
}

/// The bytes of `old` and `new` that differ, by words; `None` when the two
/// have too little in common to be worth marking (or are too long).
#[must_use]
pub fn word_diff(old: &str, new: &str) -> Option<(Spans, Spans)> {
    let (a, b) = (tokens(old), tokens(new));
    if a.len() > WORDS_MAX || b.len() > WORDS_MAX || old == new {
        return None;
    }
    // Longest common subsequence of tokens.
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![0u16; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if old[a[i].clone()] == new[b[j].clone()] { dp[at(i + 1, j + 1)] + 1 } else { dp[at(i + 1, j)].max(dp[at(i, j + 1)]) };
        }
    }
    let (mut same_a, mut same_b) = (vec![false; n], vec![false; m]);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[a[i].clone()] == new[b[j].clone()] {
            same_a[i] = true;
            same_b[j] = true;
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let solid = |s: &str, toks: &[Range<usize>], same: &[bool]| -> usize {
        toks.iter().zip(same).filter(|(r, k)| **k && !s[(*r).clone()].trim().is_empty()).map(|(r, _)| r.len()).sum()
    };
    let common = solid(old, &a, &same_a).max(solid(new, &b, &same_b));
    let longest = old.trim().len().max(new.trim().len());
    if common * 4 < longest {
        return None;
    }
    Some((changed(old, &a, &same_a), changed(new, &b, &same_b)))
}

/// The changed tokens as ranges, joined across spaces between them.
fn changed(text: &str, toks: &[Range<usize>], same: &[bool]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for (k, r) in toks.iter().enumerate() {
        if same[k] {
            continue;
        }
        match out.last_mut() {
            Some(last) if text[last.end..r.start].trim().is_empty() => last.end = r.end,
            _ => out.push(r.clone()),
        }
    }
    out
}

// ---------------------------------------------------------------- list

/// The file list's groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Staged,
    Changes,
    Untracked,
    Conflicts,
}

impl Section {
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Section::Staged => "Staged",
            Section::Changes => "Changes",
            Section::Untracked => "Untracked",
            Section::Conflicts => "Conflicts",
        }
    }
}

/// What the list can do in a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListMode {
    /// Work tree and index: every group, checkboxes stage and unstage.
    Working,
    /// The index: staged files, checked (a click unstages).
    Staged,
    /// The work tree: unstaged files, unchecked (a click stages).
    Unstaged,
    /// Trees and revisions: one group, read-only.
    ReadOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListRow {
    Section {
        section: Section,
        count: usize,
    },
    /// `file` indexes the status's files; `checked`: its checkbox (none: no box).
    File {
        file: usize,
        section: Section,
        checked: Option<bool>,
    },
}

/// The list's rows: Staged, Changes, Untracked, Conflicts in that order,
/// empty groups left out. A file both staged and changed after shows in
/// both groups.
#[must_use]
pub fn group(files: &[ChangedFile], mode: ListMode) -> Vec<ListRow> {
    type Files = Vec<(usize, Option<bool>)>;
    let mut sections: Vec<(Section, Files)> =
        [Section::Staged, Section::Changes, Section::Untracked, Section::Conflicts].into_iter().map(|s| (s, Vec::new())).collect();
    let mut put = |s: Section, i: usize, checked: Option<bool>| {
        if let Some((_, v)) = sections.iter_mut().find(|(k, _)| *k == s) {
            v.push((i, checked));
        }
    };
    for (i, f) in files.iter().enumerate() {
        match mode {
            ListMode::ReadOnly => put(Section::Changes, i, None),
            _ if f.status == FileStatus::Conflicted => put(Section::Conflicts, i, None),
            ListMode::Staged => put(Section::Staged, i, Some(true)),
            ListMode::Unstaged if f.status == FileStatus::Untracked => put(Section::Untracked, i, Some(false)),
            ListMode::Unstaged => put(Section::Changes, i, Some(false)),
            ListMode::Working if f.status == FileStatus::Untracked => put(Section::Untracked, i, Some(false)),
            ListMode::Working => {
                if f.staged {
                    put(Section::Staged, i, Some(true));
                }
                if f.unstaged || !f.staged {
                    put(Section::Changes, i, Some(false));
                }
            }
        }
    }
    let mut out = Vec::new();
    for (section, files) in sections {
        if files.is_empty() {
            continue;
        }
        out.push(ListRow::Section { section, count: files.len() });
        out.extend(files.into_iter().map(|(file, checked)| ListRow::File { file, section, checked }));
    }
    out
}

/// A file's counts in the list: `+12 −3`, `new`, `deleted`, `binary`.
#[must_use]
pub fn file_counts(f: &ChangedFile) -> String {
    match f.status {
        _ if f.binary => "binary".into(),
        FileStatus::Untracked if f.added == 0 => "new".into(),
        FileStatus::Conflicted => "conflict".into(),
        _ => totals(f.added, f.removed),
    }
}

/// `+142 −37`.
#[must_use]
pub fn totals(added: u32, removed: u32) -> String {
    format!("+{added} \u{2212}{removed}")
}

/// `5 files`, `1 file`.
#[must_use]
pub fn files_count(n: usize) -> String {
    if n == 1 { "1 file".into() } else { format!("{n} files") }
}

// ---------------------------------------------------------------- navigation

/// The row index of the hunk header `delta` hunks from `top` (j / k).
#[must_use]
pub fn step_hunk(rows: &[Row], top: usize, delta: isize) -> Option<usize> {
    let hunks: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| matches!(r, Row::Hunk { .. })).map(|(i, _)| i).collect();
    if delta > 0 { hunks.into_iter().find(|&i| i > top) } else { hunks.into_iter().rev().find(|&i| i < top) }
}

/// The hunk shown at row `top` (or the next one under it): what the view
/// keeps in place across a refresh, and what s / u / r act on.
#[must_use]
pub fn hunk_at(rows: &[Row], top: usize) -> Option<(usize, usize)> {
    let row = rows.get(top.min(rows.len().saturating_sub(1)))?;
    match row.hunk() {
        Some(h) => Some((row.file(), h)),
        None => rows[top..].iter().find_map(|r| r.hunk().map(|h| (r.file(), h))),
    }
}

/// Where hunk `hunk` of file `file` starts in `rows` (its header).
#[must_use]
pub fn hunk_row(rows: &[Row], file: usize, hunk: usize) -> Option<usize> {
    rows.iter().position(|r| matches!(r, Row::Hunk { file: f, hunk: h } if *f == file && *h == hunk))
}

/// How many lines a hunk's revert discards: its added and removed lines.
#[must_use]
pub fn changed_lines(hunks: &[&Hunk]) -> usize {
    hunks.iter().flat_map(|h| &h.lines).filter(|l| matches!(l.kind, LineKind::Added | LineKind::Removed)).count()
}

/// The revert confirm's question: "Discard 3 changed lines in view.rs?".
#[must_use]
pub fn revert_question(path: &str, lines: Option<usize>) -> String {
    let name = crate::files::model::name(path);
    match lines {
        Some(1) => format!("Discard 1 changed line in {name}?"),
        Some(n) => format!("Discard {n} changed lines in {name}?"),
        None => format!("Discard every change to {name}?"),
    }
}

/// A comment's place on a hunk: its new lines (the old ones for a hunk that
/// only removes).
#[must_use]
pub fn hunk_place(hunk: &Hunk) -> (Side, u32, Option<u32>) {
    let side_lines = |side: Side| -> Vec<u32> {
        hunk.lines
            .iter()
            .filter(|l| match side {
                Side::New => l.kind == LineKind::Added,
                Side::Old => l.kind == LineKind::Removed,
            })
            .filter_map(|l| if side == Side::New { l.new } else { l.old })
            .collect()
    };
    let (side, lines) = match side_lines(Side::New) {
        v if v.is_empty() => (Side::Old, side_lines(Side::Old)),
        v => (Side::New, v),
    };
    let first = lines.first().copied().unwrap_or(if side == Side::New { hunk.new_start } else { hunk.old_start });
    let last = lines.last().copied().unwrap_or(first);
    (side, first, (last != first).then_some(last))
}

// ---------------------------------------------------------------- review

/// A line's text on one side of a diff, if the diff shows it.
#[must_use]
pub fn line_text(diff: &FileDiff, side: Side, line: u32) -> Option<&str> {
    let is = |l: &DiffLine| match side {
        Side::Old => l.old == Some(line) && l.kind != LineKind::Added,
        Side::New => l.new == Some(line) && l.kind != LineKind::Removed,
    };
    diff.hunks.iter().flat_map(|h| &h.lines).find(|l| is(l)).map(|l| l.text.as_str())
}

/// The message Send review queues on the agent (§5):
///
/// ```text
/// Review of your changes (Turn 4):
///
/// crates/silo-app/src/agent/view.rs:812 (new)
/// > div().child(text)
/// why not truncate at 80?
/// ```
///
/// `quote` gives a line's text when known; at most `QUOTE_MAX` lines are
/// quoted per comment.
#[must_use]
pub fn review_message(label: &str, comments: &[ReviewComment], quote: impl Fn(&str, Side, u32) -> Option<String>) -> String {
    let mut out = if label.is_empty() { "Review of your changes:\n".to_owned() } else { format!("Review of your changes ({label}):\n") };
    for c in comments {
        out.push('\n');
        let end = c.end_line.filter(|e| *e > c.line);
        let place = match end {
            Some(e) => format!("{}:{}-{e}", c.path, c.line),
            None => format!("{}:{}", c.path, c.line),
        };
        let side = match c.side {
            Side::New => "new",
            Side::Old => "old",
        };
        out.push_str(&format!("{place} ({side})\n"));
        let last = end.unwrap_or(c.line);
        let quoted: Vec<String> = (c.line..=last).take(QUOTE_MAX).filter_map(|n| quote(&c.path, c.side, n)).collect();
        for q in &quoted {
            out.push_str(&format!("> {q}\n"));
        }
        if !quoted.is_empty() && (last - c.line) as usize >= QUOTE_MAX {
            out.push_str("> …\n");
        }
        out.push_str(c.text.trim());
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// The ask for a commit message (§3 footer).
pub const COMMIT_ASK: &str = "Write a commit message for the staged changes in this repo's style. Reply with only the message in one fenced block.";

/// A commit message from the agent's reply: its first fenced block, else
/// the text.
#[must_use]
pub fn commit_message_from(reply: &str) -> String {
    let mut lines = reply.lines();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("```") {
            let body: Vec<&str> = lines.by_ref().take_while(|l| !l.trim_start().starts_with("```")).collect();
            return body.join("\n").trim().to_owned();
        }
    }
    reply.trim().to_owned()
}

// ---------------------------------------------------------------- scope picker

/// A turn of the linked agent that has changes to review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnRef {
    pub turn: i64,
    pub base_tree: String,
    pub end_tree: String,
    pub files: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    Working,
    Staged,
    Unstaged,
    Turn(TurnRef),
    /// From the first turn's start to the last one's end.
    Session {
        base: String,
        head: String,
    },
    /// A revision against another (asks for them).
    Branch,
}

impl Pick {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Pick::Working => "Working".into(),
            Pick::Staged => "Staged".into(),
            Pick::Unstaged => "Unstaged".into(),
            Pick::Turn(t) => turn_label(t.turn),
            Pick::Session { .. } => SESSION_LABEL.into(),
            Pick::Branch => "Branch\u{2026}".into(),
        }
    }

    /// The detail at the row's right: a turn's file count.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        match self {
            Pick::Turn(t) => Some(files_count(t.files)),
            _ => None,
        }
    }
}

pub const SESSION_LABEL: &str = "This session";

#[must_use]
pub fn turn_label(turn: i64) -> String {
    format!("Turn {turn}")
}

/// The scope picker's rows: the git scopes (in a repo), the agent's turns
/// newest first, This session (with any turn), Branch… (in a repo).
#[must_use]
pub fn picker_rows(turns: &[TurnRef], repo: bool) -> Vec<Pick> {
    let mut out = Vec::new();
    if repo {
        out.extend([Pick::Working, Pick::Staged, Pick::Unstaged]);
    }
    let mut sorted: Vec<&TurnRef> = turns.iter().collect();
    sorted.sort_by_key(|t| std::cmp::Reverse(t.turn));
    out.extend(sorted.iter().map(|t| Pick::Turn((*t).clone())));
    if let (Some(first), Some(last)) = (turns.iter().min_by_key(|t| t.turn), turns.iter().max_by_key(|t| t.turn)) {
        out.push(Pick::Session { base: first.base_tree.clone(), head: last.end_tree.clone() });
    }
    if repo {
        out.push(Pick::Branch);
    }
    out
}

/// A typed branch scope: `main` (against the work tree), `main..otto`,
/// `main...otto`.
#[must_use]
pub fn parse_range(text: &str) -> Option<(String, Option<String>)> {
    let text = text.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let (base, head) = match text.split_once("...").or_else(|| text.split_once("..")) {
        Some((a, b)) => (a, Some(b)),
        None => (text, None),
    };
    if base.is_empty() {
        return None;
    }
    Some((base.to_owned(), head.filter(|h| !h.is_empty()).map(str::to_owned)))
}

#[cfg(test)]
mod tests;
