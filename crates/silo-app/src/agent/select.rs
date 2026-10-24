//! Text selection in the transcript: a pure model (anchors in document order, word and block expansion, the
//! copied text) and, in `view`/`pane`, the thin layer that registers each drawn text's layout for hit-testing
//! and paints the selection behind its glyphs.
//!
//! A selection is kept as anchors (row key, leaf id, byte offset), never pixels: it survives streaming
//! re-renders and scrolling. A *leaf* is one selectable text of a row: a paragraph, heading, list item's
//! paragraph, code block, table cell or user bubble. Leaf ids are the ids `render_block` gives its texts, so
//! what is drawn and what is copied agree. Tool rows, markers and reports have no leaves: they're skipped.

mod leaves;
mod pane;
mod text;
mod view;
mod wire;

#[cfg(test)]
mod tests;

use std::{collections::HashMap, ops::Range};

use leaves::{block_leaves, user_leaf};
pub(super) use pane::Select;
pub(super) use text::overlay;
pub(super) use view::{code_line, line_starts, register, selected};

/// The leaf id of a user bubble's text.
pub(super) const USER: &str = "user";

/// A place in the transcript: a byte `offset` into leaf `leaf` of row `row` (keys, stable across renders).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Anchor {
    pub row: String,
    pub leaf: String,
    pub offset: usize,
}

/// An anchor resolved against the transcript as it is now: ordered as the document reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Pos {
    pub row: usize,
    pub leaf: usize,
    pub offset: usize,
}

/// What a click selects and a drag then extends by: characters, words (double click), blocks (triple).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Unit {
    #[default]
    Char,
    Word,
    Block,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// A markdown paragraph or heading: a triple click takes all of it.
    Text,
    /// A user bubble: lines are its paragraphs.
    Lines,
    /// A code block, copied verbatim; a triple click takes a line.
    Code,
    /// A table cell: cells of one row copy tab-separated.
    Cell { table: String, row: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Leaf {
    pub id: String,
    pub text: String,
    pub kind: Kind,
}

/// The transcript as selection sees it: rows by key, each with its leaves in reading order.
pub(super) trait Source {
    fn len(&self) -> usize;
    fn key(&self, row: usize) -> &str;
    fn leaves(&self, row: usize) -> Vec<Leaf>;
    fn index(&self, key: &str) -> Option<usize> {
        (0..self.len()).find(|&r| self.key(r) == key)
    }
}

/// `start..end` is what the first click took (a word, a block, or a caret); `head` follows the pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Selection {
    pub start: Anchor,
    pub end: Anchor,
    pub head: Anchor,
    pub unit: Unit,
}

impl Selection {
    pub fn new(src: &dyn Source, at: Anchor, unit: Unit) -> Self {
        let (start, end) = match find(src, &at) {
            Some((pos, leaf)) if unit != Unit::Char => {
                let r = text::unit_at(&leaf, pos.offset, unit);
                (Anchor { offset: r.start, ..at.clone() }, Anchor { offset: r.end, ..at.clone() })
            }
            _ => (at.clone(), at.clone()),
        };
        Self { start, end, head: at, unit }
    }

    /// Where it starts and ends now, in order (`None` once an anchor's row or leaf is gone).
    pub fn bounds(&self, src: &dyn Source) -> Option<(Pos, Pos)> {
        let (s, _) = find(src, &self.start)?;
        let (e, _) = find(src, &self.end)?;
        let (h, leaf) = find(src, &self.head)?;
        let r = text::unit_at(&leaf, h.offset, self.unit);
        Some(if h < s { (Pos { offset: r.start, ..h }, e) } else { (s, Pos { offset: r.end, ..h }.max(e)) })
    }
}

/// Anchor `a` resolved: its position (offset on a char boundary, within the leaf) and its leaf.
fn find(src: &dyn Source, a: &Anchor) -> Option<(Pos, Leaf)> {
    let row = src.index(&a.row)?;
    let (leaf, l) = src.leaves(row).into_iter().enumerate().find(|(_, l)| l.id == a.leaf)?;
    let offset = text::floor(&l.text, a.offset);
    Some((Pos { row, leaf, offset }, l))
}

/// Each leaf from `lo` to `hi` (only row `only`'s, if given) with the part of it selected.
fn walk(src: &dyn Source, lo: Pos, hi: Pos, only: Option<usize>, mut f: impl FnMut(usize, &Leaf, Range<usize>)) {
    let rows = only.map_or(lo.row..=hi.row, |r| r..=r);
    for row in rows.filter(|r| (lo.row..=hi.row).contains(r) && *r < src.len()) {
        for (i, leaf) in src.leaves(row).iter().enumerate() {
            let at = (row, i);
            if at < (lo.row, lo.leaf) || at > (hi.row, hi.leaf) {
                continue;
            }
            let a = if at == (lo.row, lo.leaf) { text::floor(&leaf.text, lo.offset) } else { 0 };
            let b = if at == (hi.row, hi.leaf) { text::floor(&leaf.text, hi.offset) } else { leaf.text.len() };
            f(row, leaf, a..b.max(a));
        }
    }
}

/// What row `row` has selected, by leaf id (non-empty ranges only).
pub(super) fn ranges(src: &dyn Source, (lo, hi): (Pos, Pos), row: usize) -> HashMap<String, Range<usize>> {
    let mut out = HashMap::new();
    walk(src, lo, hi, Some(row), |_, leaf, r| {
        if !r.is_empty() {
            out.insert(leaf.id.clone(), r);
        }
    });
    out
}

/// The selection as plain text: a newline between blocks, a blank line between messages, a tab between a
/// table row's cells, code verbatim. `None` when nothing is selected.
pub(super) fn copy_text(src: &dyn Source, sel: &Selection) -> Option<String> {
    let (lo, hi) = sel.bounds(src)?;
    if lo == hi {
        return None;
    }
    let mut out = String::new();
    let mut prev: Option<(usize, Kind)> = None;
    walk(src, lo, hi, None, |row, leaf, r| {
        if let Some((prow, pkind)) = &prev {
            out.push_str(match (pkind, &leaf.kind) {
                _ if *prow != row => "\n\n",
                (Kind::Cell { table: a, row: x }, Kind::Cell { table: b, row: y }) if a == b && x == y => "\t",
                _ => "\n",
            });
        }
        out.push_str(&leaf.text[r]);
        prev = Some((row, leaf.kind.clone()));
    });
    (!out.is_empty()).then_some(out)
}

/// The whole transcript selected (`None`: nothing in it to select).
pub(super) fn everything(src: &dyn Source) -> Option<Selection> {
    let first = (0..src.len()).find_map(|r| src.leaves(r).into_iter().next().map(|l| (r, l)))?;
    let last = (0..src.len()).rev().find_map(|r| src.leaves(r).pop().map(|l| (r, l)))?;
    let start = Anchor { row: src.key(first.0).to_owned(), leaf: first.1.id, offset: 0 };
    let head = Anchor { row: src.key(last.0).to_owned(), leaf: last.1.id, offset: last.1.text.len() };
    Some(Selection { end: start.clone(), start, head, unit: Unit::Char })
}
