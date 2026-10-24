//! Editing a Files tab, free of gpui: what each key does to the text and
//! the selection, the file's indentation and line breaks, and undo grouped
//! the way people type (a run of typing until a pause, a space after a word
//! or a line break; a run of deletes).
//!
//! The text is a [`Doc`]; positions are [`Pos`] (line, byte) and every
//! edit is one byte range replaced ([`Edit`]), so an undo is one too.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use silo_proto::files::FileBase;

use super::model::{self, Doc, Pos, Selection, Splice, Unit};

/// A pause longer than this starts a new undo step.
pub const GROUP_PAUSE: Duration = Duration::from_millis(1000);
/// Undo steps kept.
const UNDO_MAX: usize = 10_000;

/// How the file indents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indent {
    Tab,
    Spaces(usize),
}

impl Indent {
    /// The file's indentation: tabs when more lines start with a tab than
    /// with spaces, else the most common step between space indents (2, 4
    /// or 8; 4 when nothing says).
    #[must_use]
    pub fn detect(doc: &Doc) -> Self {
        let (mut tabs, mut spaced) = (0usize, 0usize);
        let mut steps = [0usize; 9];
        let mut previous = 0usize;
        for ix in 0..doc.line_count().min(4000) {
            let line = doc.line(ix);
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with('\t') {
                tabs += 1;
                continue;
            }
            let width = line.len() - line.trim_start_matches(' ').len();
            if width > 0 {
                spaced += 1;
            }
            let step = width.abs_diff(previous);
            if (1..=8).contains(&step) {
                steps[step] += 1;
            }
            previous = width;
        }
        if tabs > spaced {
            return Self::Tab;
        }
        let best = [2, 4, 8].into_iter().max_by_key(|&n| (steps[n], n == 4)).unwrap_or(4);
        if steps[best] == 0 { Self::Spaces(4) } else { Self::Spaces(best) }
    }

    /// One level, as text.
    #[must_use]
    pub fn unit(self) -> String {
        match self {
            Self::Tab => "\t".into(),
            Self::Spaces(n) => " ".repeat(n),
        }
    }

    /// One level, in columns.
    #[must_use]
    pub fn width(self) -> usize {
        match self {
            Self::Tab => model::TAB,
            Self::Spaces(n) => n,
        }
    }
}

/// The file's line break: `\r\n` when its first line ends so.
#[must_use]
pub fn newline_of(doc: &Doc) -> &'static str {
    match doc.lines.first() {
        Some(r) if doc.text.as_bytes().get(r.end) == Some(&b'\r') => "\r\n",
        _ => "\n",
    }
}

/// `text` with its line breaks made `newline`.
#[must_use]
pub fn with_newlines(text: &str, newline: &str) -> String {
    let plain = text.replace("\r\n", "\n").replace('\r', "\n");
    if newline == "\n" { plain } else { plain.replace('\n', newline) }
}

// ---------------------------------------------------------------- motion

/// A caret at `pos`.
#[must_use]
pub fn caret(pos: Pos) -> Selection {
    Selection::new(pos, Unit::Char)
}

/// The selection as a plain range by characters (a word or line drag
/// grown to its words or lines), keeping its direction.
#[must_use]
pub fn plain(doc: &Doc, sel: Selection) -> Selection {
    if sel.unit == Unit::Char {
        return Selection { anchor: doc.clamp(sel.anchor), head: doc.clamp(sel.head), unit: Unit::Char };
    }
    let (a, b) = sel.bounds(doc);
    let (anchor, head) = if sel.head < sel.anchor { (b, a) } else { (a, b) };
    Selection { anchor, head, unit: Unit::Char }
}

/// The selected bytes.
#[must_use]
pub fn span(doc: &Doc, sel: Selection) -> Range<usize> {
    let (a, b) = sel.bounds(doc);
    doc.offset(a)..doc.offset(b)
}

/// The character before `p` (the end of the line above at a line's start).
#[must_use]
pub fn prev(doc: &Doc, p: Pos) -> Pos {
    if p.col > 0 {
        let line = doc.line(p.line);
        let col = line[..p.col.min(line.len())].char_indices().next_back().map_or(0, |(i, _)| i);
        Pos { line: p.line, col }
    } else if p.line > 0 {
        Pos { line: p.line - 1, col: doc.line(p.line - 1).len() }
    } else {
        p
    }
}

/// The character after `p` (the next line's start at a line's end).
#[must_use]
pub fn next(doc: &Doc, p: Pos) -> Pos {
    let line = doc.line(p.line);
    if p.col < line.len() {
        let col = line[p.col..].chars().next().map_or(line.len(), |c| p.col + c.len_utf8());
        Pos { line: p.line, col }
    } else if p.line + 1 < doc.line_count() {
        Pos { line: p.line + 1, col: 0 }
    } else {
        p
    }
}

fn class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

/// ⌥←: the start of the word (or run of punctuation) before `p`, spaces
/// skipped; at a line's start, the end of the line above.
#[must_use]
pub fn word_left(doc: &Doc, p: Pos) -> Pos {
    if p.col == 0 {
        return prev(doc, p);
    }
    let line = &doc.line(p.line)[..p.col.min(doc.line(p.line).len())];
    let mut chars = line.char_indices().rev().skip_while(|(_, c)| c.is_whitespace()).peekable();
    let Some(&(_, first)) = chars.peek() else { return Pos { line: p.line, col: 0 } };
    let k = class(first);
    let col = chars.take_while(|(_, c)| class(*c) == k).last().map_or(0, |(i, _)| i);
    Pos { line: p.line, col }
}

/// ⌥→: the end of the word (or run of punctuation) after `p`, spaces
/// skipped; at a line's end, the next line's start.
#[must_use]
pub fn word_right(doc: &Doc, p: Pos) -> Pos {
    let line = doc.line(p.line);
    if p.col >= line.len() {
        return next(doc, p);
    }
    let rest = &line[p.col..];
    let skip = rest.len() - rest.trim_start().len();
    let rest = &rest[skip..];
    let Some(first) = rest.chars().next() else { return Pos { line: p.line, col: line.len() } };
    let k = class(first);
    let len = rest.char_indices().find(|(_, c)| class(*c) != k).map_or(rest.len(), |(i, _)| i);
    Pos { line: p.line, col: p.col + skip + len }
}

/// Home, ⌘←: the line's first non-blank character, or its start when
/// already there.
#[must_use]
pub fn home(doc: &Doc, p: Pos) -> Pos {
    let line = doc.line(p.line);
    let first = line.len() - line.trim_start().len();
    Pos { line: p.line, col: if p.col == first { 0 } else { first } }
}

/// End, ⌘→.
#[must_use]
pub fn end(doc: &Doc, p: Pos) -> Pos {
    Pos { line: p.line, col: doc.line(p.line).len() }
}

/// ↑ ↓ (and pages): `delta` lines from `p`, at column `goal` (the one the
/// run of vertical moves started at; `None`: `p`'s). Past the first line
/// is its start, past the last its end. Returns the position and goal.
#[must_use]
pub fn vertical(doc: &Doc, p: Pos, delta: isize, goal: Option<usize>) -> (Pos, usize) {
    let goal = goal.unwrap_or_else(|| model::columns(&doc.line(p.line)[..p.col.min(doc.line(p.line).len())]));
    let target = p.line.cast_signed() + delta;
    if target < 0 {
        return (Pos::default(), goal);
    }
    let target = target.cast_unsigned();
    if target >= doc.line_count() {
        return (doc.end(), goal);
    }
    #[allow(clippy::cast_precision_loss)]
    let col = model::col_at(doc.line(target), goal as f32);
    (Pos { line: target, col }, goal)
}

// ---------------------------------------------------------------- edits

/// What an undo step groups with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Typing: one run until a pause or a space after a word.
    Type,
    /// Backspace: one run.
    Back,
    /// Forward delete: one run.
    Forward,
    /// Anything else is a step of its own.
    Other,
}

/// One edit: bytes `range` become `text`; the selection after it (a
/// caret after the new text when `None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
    pub after: Option<Selection>,
    pub kind: Kind,
}

impl Edit {
    fn new(range: Range<usize>, text: impl Into<String>, kind: Kind) -> Self {
        Self { range, text: text.into(), after: None, kind }
    }
}

/// Typing or pasting `text` over the selection.
#[must_use]
pub fn insert(doc: &Doc, sel: Selection, text: &str, newline: &str) -> Edit {
    let text = with_newlines(text, newline);
    let kind = if text.contains('\n') || text.chars().count() > 1 { Kind::Other } else { Kind::Type };
    Edit::new(span(doc, sel), text, kind)
}

/// How far a delete reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Char,
    Word,
    Line,
}

/// Backspace (⌥: a word, ⌘: to the line's start). In the spaces that
/// indent a line, a character goes back to the previous indent stop.
#[must_use]
pub fn backspace(doc: &Doc, sel: Selection, by: By, indent: Indent) -> Option<Edit> {
    let range = span(doc, sel);
    if !range.is_empty() {
        return Some(Edit::new(range, "", Kind::Back));
    }
    let p = doc.clamp(sel.head);
    let to = match by {
        By::Char => {
            let before = &doc.line(p.line)[..p.col];
            match indent {
                Indent::Spaces(n) if p.col > 0 && before.bytes().all(|b| b == b' ') => Pos { line: p.line, col: p.col - ((p.col - 1) % n + 1) },
                _ => prev(doc, p),
            }
        }
        By::Word => word_left(doc, p),
        By::Line if p.col == 0 => prev(doc, p),
        By::Line => Pos { line: p.line, col: 0 },
    };
    let from = doc.offset(to);
    (from < doc.offset(p)).then(|| Edit::new(from..doc.offset(p), "", Kind::Back))
}

/// Forward delete (⌥: a word, ⌘: to the line's end).
#[must_use]
pub fn delete(doc: &Doc, sel: Selection, by: By) -> Option<Edit> {
    let range = span(doc, sel);
    if !range.is_empty() {
        return Some(Edit::new(range, "", Kind::Forward));
    }
    let p = doc.clamp(sel.head);
    let to = match by {
        By::Char => next(doc, p),
        By::Word => word_right(doc, p),
        By::Line if p.col == doc.line(p.line).len() => next(doc, p),
        By::Line => end(doc, p),
    };
    let to = doc.offset(to);
    (to > doc.offset(p)).then(|| Edit::new(doc.offset(p)..to, "", Kind::Forward))
}

/// Return: a line break, then the line's indentation (as far as the
/// caret), one level more after an opening bracket (or a `:` ending a
/// Python line); between a bracket pair the closer goes on a line of its
/// own.
#[must_use]
pub fn newline(doc: &Doc, sel: Selection, indent: Indent, newline: &str, colon: bool) -> Edit {
    let range = span(doc, sel);
    let (a, b) = sel.bounds(doc);
    let line = doc.line(a.line);
    let lead = &line[..line.len() - line.trim_start().len()];
    let lead = &lead[..lead.len().min(a.col)];
    let before = line[..a.col].trim_end();
    let after = doc.line(b.line)[b.col.min(doc.line(b.line).len())..].trim_start();
    let opener = before.chars().next_back();
    let deeper = matches!(opener, Some('{' | '(' | '[')) || (colon && opener == Some(':'));
    let unit = if deeper { indent.unit() } else { String::new() };
    let mut text = format!("{newline}{lead}{unit}");
    let caret = range.start + text.len();
    let pair = matches!((opener, after.chars().next()), (Some('{'), Some('}')) | (Some('('), Some(')')) | (Some('['), Some(']')));
    if pair {
        text.push_str(newline);
        text.push_str(lead);
    }
    // Spaces left before the caret's old place go.
    let start = doc.offset(Pos { line: a.line, col: before.len().max(lead.len()) }).min(range.start);
    let removed = range.start - start;
    let mut edit = Edit::new(start..range.end, text, Kind::Other);
    let caret = caret - removed;
    edit.after = Some(caret_at(doc, &edit, caret));
    edit
}

/// The caret at byte `offset` of the text after `edit`, as a position.
fn caret_at(doc: &Doc, edit: &Edit, offset: usize) -> Selection {
    // Lines up to the edit are unchanged; count the new text's breaks.
    let start = doc.pos(edit.range.start);
    let inserted = &edit.text[..offset - edit.range.start];
    let breaks = inserted.matches('\n').count();
    let col = match inserted.rfind('\n') {
        Some(i) => inserted.len() - i - 1,
        None => start.col + inserted.len(),
    };
    caret(Pos { line: start.line + breaks, col })
}

/// The lines a selection covers for indenting: a selection ending at a
/// line's start leaves that line out.
fn covered(doc: &Doc, sel: Selection) -> (usize, usize) {
    let (a, b) = sel.bounds(doc);
    let last = if b.line > a.line && b.col == 0 { b.line - 1 } else { b.line };
    (a.line, last)
}

/// Tab: one level at the caret (spaces to the next stop), or over a
/// selection on one line; across lines, each line goes one level in.
#[must_use]
pub fn tab(doc: &Doc, sel: Selection, indent: Indent) -> Edit {
    let (a, b) = sel.bounds(doc);
    if a.line == b.line {
        let text = match indent {
            Indent::Tab => "\t".to_owned(),
            Indent::Spaces(n) => {
                let at = model::columns(&doc.line(a.line)[..a.col]);
                " ".repeat(n - at % n)
            }
        };
        return Edit::new(span(doc, sel), text, Kind::Other);
    }
    shift_lines(doc, sel, indent, false).unwrap_or_else(|| Edit::new(span(doc, sel), indent.unit(), Kind::Other))
}

/// ⇧Tab (or Tab across lines): each covered line one level out (or in);
/// blank lines stay as they are. `None` when nothing changes.
#[must_use]
pub fn shift_lines(doc: &Doc, sel: Selection, indent: Indent, out: bool) -> Option<Edit> {
    let (first, last) = covered(doc, sel);
    let unit = indent.unit();
    let mut text = String::new();
    let mut deltas = Vec::with_capacity(last - first + 1);
    for ix in first..=last {
        let line = doc.line(ix);
        let delta: isize = if out {
            let cut = if line.starts_with('\t') {
                1
            } else {
                let spaces = line.len() - line.trim_start_matches(' ').len();
                spaces.min(indent.width())
            };
            text.push_str(&line[cut..]);
            -cut.cast_signed()
        } else if line.trim().is_empty() {
            text.push_str(line);
            0
        } else {
            text.push_str(&unit);
            text.push_str(line);
            unit.len().cast_signed()
        };
        deltas.push(delta);
        if ix < last {
            text.push_str(&doc.text[doc.lines[ix].end..doc.lines[ix + 1].start]);
        }
    }
    if deltas.iter().all(|&d| d == 0) {
        return None;
    }
    let range = doc.lines[first].start..doc.lines[last].end;
    let moved = |p: Pos| match p.line.checked_sub(first).and_then(|i| deltas.get(i)) {
        Some(&d) => Pos { line: p.line, col: p.col.saturating_add_signed(d) },
        None => p,
    };
    let sel = plain(doc, sel);
    let after = Selection { anchor: moved(sel.anchor), head: moved(sel.head), unit: Unit::Char };
    Some(Edit { range, text, after: Some(after), kind: Kind::Other })
}

// ---------------------------------------------------------------- utf-16

/// Byte offset of UTF-16 offset `utf16` (input methods count in UTF-16).
#[must_use]
pub fn from_utf16(doc: &Doc, utf16: usize) -> usize {
    if doc.ascii {
        return utf16.min(doc.text.len());
    }
    let mut units = 0;
    for (i, c) in doc.text.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    doc.text.len()
}

/// UTF-16 offset of byte offset `offset`.
#[must_use]
pub fn to_utf16(doc: &Doc, offset: usize) -> usize {
    let offset = offset.min(doc.text.len());
    if doc.ascii { offset } else { doc.text[..offset].chars().map(char::len_utf16).sum() }
}

// ---------------------------------------------------------------- undo

/// One undo step: bytes from `at` were `old` and became `new`.
#[derive(Clone, Debug)]
pub struct Change {
    pub at: usize,
    pub old: String,
    pub new: String,
    /// The selection before and after.
    pub before: Selection,
    pub after: Selection,
    /// The text's state ids before and after.
    pub from: u64,
    pub to: u64,
    kind: Kind,
    time: Instant,
}

/// Undo and redo. Each state of the text has an id ([`History::current`]),
/// so a file is clean whenever its state is the one saved, undoing back to
/// it included.
#[derive(Debug, Default)]
pub struct History {
    undo: Vec<Change>,
    redo: Vec<Change>,
    current: u64,
    next: u64,
    /// The next change starts a step of its own.
    sealed: bool,
}

impl History {
    #[must_use]
    pub fn current(&self) -> u64 {
        self.current
    }

    /// The next change starts its own step (after a move, a click, a save).
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// Record bytes `at` going from `old` to `new`.
    #[allow(clippy::too_many_arguments)]
    pub fn record(&mut self, at: usize, old: String, new: String, before: Selection, after: Selection, kind: Kind, now: Instant) {
        self.redo.clear();
        self.next += 1;
        let id = self.next;
        if !self.sealed
            && let Some(last) = self.undo.last_mut()
            && last.kind == kind
            && last.to == self.current
            && now.saturating_duration_since(last.time) < GROUP_PAUSE
        {
            let merged = match kind {
                // A space after a word starts a new step.
                Kind::Type if old.is_empty() && at == last.at + last.new.len() => {
                    let word_ends = new.starts_with(char::is_whitespace) && !last.new.ends_with(char::is_whitespace);
                    !word_ends && {
                        last.new.push_str(&new);
                        true
                    }
                }
                Kind::Back if new.is_empty() && last.new.is_empty() && at + old.len() == last.at => {
                    last.at = at;
                    last.old.insert_str(0, &old);
                    true
                }
                Kind::Forward if new.is_empty() && last.new.is_empty() && at == last.at => {
                    last.old.push_str(&old);
                    true
                }
                _ => false,
            };
            if merged {
                (last.to, last.after, last.time) = (id, after, now);
                self.current = id;
                return;
            }
        }
        self.undo.push(Change { at, old, new, before, after, from: self.current, to: id, kind, time: now });
        if self.undo.len() > UNDO_MAX {
            self.undo.remove(0);
        }
        self.current = id;
        self.sealed = false;
    }

    /// Take the last step back: the change to revert.
    pub fn undo(&mut self) -> Option<Change> {
        let change = self.undo.pop()?;
        self.current = change.from;
        self.redo.push(change.clone());
        self.sealed = true;
        Some(change)
    }

    /// Take the last undone step again: the change to make.
    pub fn redo(&mut self) -> Option<Change> {
        let change = self.redo.pop()?;
        self.current = change.to;
        self.undo.push(change.clone());
        self.sealed = true;
        Some(change)
    }
}

// ---------------------------------------------------------------- state

/// The file on disk next to the edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disk {
    /// As the edits started from (or the last save left it).
    Same,
    /// Changed on disk while there are edits, or a save was refused: the
    /// bar asks to reload or keep them.
    Changed,
    /// Changed, and the edits were kept: the next save overwrites it.
    Kept,
}

/// A text tab's edits. Held by the app only: the server knows the file.
#[derive(Debug)]
pub struct EditState {
    /// The file the text came from (or was last saved as).
    pub base: FileBase,
    pub history: History,
    /// The state id saved (or read).
    pub saved: u64,
    pub disk: Disk,
    /// A save is on its way.
    pub saving: bool,
    /// The file changed while saving: look again once saved.
    pub recheck: bool,
    /// The column ↑ ↓ keep to.
    pub goal: Option<usize>,
    /// Text being composed (an input method's marked text), in bytes.
    pub marked: Option<Range<usize>>,
    pub indent: Indent,
    pub newline: &'static str,
}

impl EditState {
    #[must_use]
    pub fn new(base: FileBase, doc: &Doc) -> Self {
        Self {
            base,
            history: History::default(),
            saved: 0,
            disk: Disk::Same,
            saving: false,
            recheck: false,
            goal: None,
            marked: None,
            indent: Indent::detect(doc),
            newline: newline_of(doc),
        }
    }

    /// Edits not saved.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.history.current() != self.saved
    }

    /// Make `edit` on `doc` with `sel` selected: the lines it replaced and
    /// the selection after it.
    pub fn apply(&mut self, doc: &mut Doc, sel: Selection, edit: Edit, now: Instant) -> (Splice, Selection) {
        let range = edit.range.start.min(doc.text.len())..edit.range.end.min(doc.text.len());
        let before = plain(doc, sel);
        let old = doc.text[range.clone()].to_owned();
        let splice = doc.replace(range.clone(), &edit.text);
        let after = edit.after.map_or_else(|| caret(doc.pos(range.start + edit.text.len())), |s| plain(doc, s));
        self.history.record(range.start, old, edit.text, before, after, edit.kind, now);
        self.goal = None;
        (splice, after)
    }

    /// Undo (or redo, `again`) a step on `doc`.
    pub fn step(&mut self, doc: &mut Doc, again: bool) -> Option<(Splice, Selection)> {
        let change = if again { self.history.redo()? } else { self.history.undo()? };
        let (from, to, sel) = if again { (&change.old, &change.new, change.after) } else { (&change.new, &change.old, change.before) };
        let splice = doc.replace(change.at..change.at + from.len(), to);
        self.goal = None;
        self.marked = None;
        Some((splice, plain(doc, sel)))
    }

    /// Saved as `base` at state `id`.
    pub fn saved_as(&mut self, base: FileBase, id: u64) {
        self.base = base;
        self.saved = id;
        self.disk = Disk::Same;
        self.history.seal();
    }
}

/// Spans per line (`crate::syntax::Lines`' shape) moved along an edit:
/// a one-line edit at byte `col` replacing `old` bytes with `new` shifts
/// the spans after it; across lines, the first line keeps the spans
/// before the edit and the new lines have none (until highlighted again).
pub fn splice_spans<T: Clone>(spans: &mut Vec<Vec<(Range<usize>, T)>>, splice: Splice, col: usize, old: usize, new: usize) {
    if splice.first >= spans.len() {
        return;
    }
    let end = (splice.first + splice.removed).min(spans.len());
    if splice.removed == 1 && splice.added == 1 {
        let delta = new.cast_signed() - old.cast_signed();
        for (r, _) in &mut spans[splice.first] {
            if r.end <= col {
                continue;
            }
            if r.start >= col + old {
                *r = r.start.saturating_add_signed(delta)..r.end.saturating_add_signed(delta);
            } else {
                *r = r.start.min(col)..r.end.saturating_add_signed(delta).max(col + new);
            }
        }
        spans[splice.first].retain(|(r, _)| r.start < r.end);
        return;
    }
    let mut first: Vec<_> = spans[splice.first].iter().filter(|(r, _)| r.start < col).cloned().collect();
    for (r, _) in &mut first {
        r.end = r.end.min(col);
    }
    let fresh = std::iter::once(first).chain(std::iter::repeat_n(Vec::new(), splice.added.saturating_sub(1)));
    spans.splice(splice.first..end, fresh.take(splice.added));
}

#[cfg(test)]
mod tests;
