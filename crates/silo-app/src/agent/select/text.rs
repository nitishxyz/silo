//! Plain-string work: word and block boundaries (unicode), tab expansion offsets, highlight overlays.

use std::ops::Range;

use gpui::{HighlightStyle, Hsla};

use super::{Kind, Leaf, Unit};
use crate::agent::reveal;

/// `at` moved back onto a char boundary of `text`, within it.
pub(super) fn floor(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Other,
}

/// Marks, joiners, variation selectors and skin tones: part of the character before them.
fn joins(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0x200D | 0x1F3FB..=0x1F3FF | 0xE0100..=0xE01EF)
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Other
    }
}

/// The word at `at` (a double click): a run of letters, digits and `_` (marks included), a run of spaces,
/// or one other character (with what joins it: an emoji sequence). At the end, the word before.
pub(super) fn word_at(text: &str, at: usize) -> Range<usize> {
    // Each char's start, class, and whether it continues the one before.
    let mut chars: Vec<(usize, Class, bool)> = Vec::new();
    let mut after_zwj = false;
    for (i, c) in text.char_indices() {
        let joined = !chars.is_empty() && (joins(c) || after_zwj);
        let cls = if joined { chars.last().map_or(Class::Other, |p| p.1) } else { class(c) };
        chars.push((i, cls, joined));
        after_zwj = c == '\u{200D}';
    }
    if chars.is_empty() {
        return 0..0;
    }
    let at = floor(text, at);
    let k = chars.iter().rposition(|(i, _, _)| *i <= at).unwrap_or(0);
    // At a word's end (or the text's), the word before is meant.
    let k = if (at == text.len() || chars[k].1 == Class::Space) && k > 0 && at == chars[k].0 && chars[k - 1].1 == Class::Word {
        k - 1
    } else if at == text.len() {
        chars.len() - 1
    } else {
        k
    };
    let end_of = |j: usize| chars.get(j + 1).map_or(text.len(), |c| c.0);
    let cls = chars[k].1;
    let (mut a, mut b) = (k, k);
    if cls == Class::Other {
        while a > 0 && chars[a].2 {
            a -= 1;
        }
        while b + 1 < chars.len() && chars[b + 1].2 {
            b += 1;
        }
    } else {
        while a > 0 && chars[a - 1].1 == cls {
            a -= 1;
        }
        while b + 1 < chars.len() && chars[b + 1].1 == cls {
            b += 1;
        }
    }
    chars[a].0..end_of(b)
}

/// The line at `at`, without its newline.
pub(super) fn line_at(text: &str, at: usize) -> Range<usize> {
    let at = floor(text, at);
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    start..end
}

/// What a click at `at` in `leaf` takes by `unit`.
pub(super) fn unit_at(leaf: &Leaf, at: usize, unit: Unit) -> Range<usize> {
    let at = floor(&leaf.text, at);
    match (unit, &leaf.kind) {
        (Unit::Char, _) => at..at,
        (Unit::Word, _) => word_at(&leaf.text, at),
        (Unit::Block, Kind::Lines | Kind::Code) => line_at(&leaf.text, at),
        (Unit::Block, Kind::Text | Kind::Cell { .. }) => 0..leaf.text.len(),
    }
}

/// Where byte `at` of a code line is in the line as drawn (tabs expanded).
pub(super) fn to_shown(line: &str, at: usize) -> usize {
    let at = floor(line, at);
    if line.contains('\t') { reveal::expand_tabs(&line[..at]).len() } else { at }
}

/// The byte of a code line at byte `at` of it as drawn (in a tab's spaces: before it).
pub(super) fn to_source(line: &str, at: usize) -> usize {
    if !line.contains('\t') {
        return floor(line, at);
    }
    line.char_indices().map(|(i, _)| i).chain([line.len()]).take_while(|i| reveal::expand_tabs(&line[..*i]).len() <= at).last().unwrap_or(0)
}

/// `highlights` (sorted, not overlapping) with `selected` given background `bg` over whatever else they set.
pub(in crate::agent) fn overlay(
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    selected: Range<usize>,
    bg: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if selected.is_empty() {
        return highlights;
    }
    let mut cuts: Vec<usize> = highlights.iter().flat_map(|(r, _)| [r.start, r.end]).chain([selected.start, selected.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mut style = highlights.iter().find(|(r, _)| r.start <= a && b <= r.end).map(|(_, s)| *s).unwrap_or_default();
        if selected.start <= a && b <= selected.end {
            style.background_color = Some(bg);
        }
        if style != HighlightStyle::default() {
            out.push((a..b, style));
        }
    }
    out
}
