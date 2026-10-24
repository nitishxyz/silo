//! Highlighting: parse, run the highlights query, paint captures, recurse
//! into injected languages.

use std::ops::Range;

use streaming_iterator::StreamingIterator as _;
use tree_sitter::{Node, Parser, Query, QueryCursor, Range as TsRange};

use super::{
    capture::{Capture, NONE},
    config,
    language::Language,
};

/// Files larger than this get no highlighting.
pub const MAX_BYTES: usize = 1 << 20;
/// Nor do files with a line longer than this many characters (minified code).
pub const MAX_LINE_CHARS: usize = 10_000;
/// How deep injections nest (markdown → html → javascript).
const MAX_DEPTH: usize = 3;

/// Whether `source` is small enough to highlight (see [`MAX_BYTES`] and
/// [`MAX_LINE_CHARS`]).
#[must_use]
pub fn highlightable(source: &str) -> bool {
    source.len() <= MAX_BYTES && source.split('\n').all(|line| line.len() <= MAX_LINE_CHARS || line.chars().count() <= MAX_LINE_CHARS)
}

/// Highlight `source` as `lang`: byte ranges with the capture that colours
/// them, sorted and non-overlapping, adjacent equal captures merged. The
/// innermost capture wins; for one node, the later pattern in the query
/// wins (as `tree-sitter highlight` and Zed do; Go's query is written the
/// other way round, see `Config::first_wins`). Injected languages (markdown
/// fences, html `<script>`/`<style>`, rust macros…) are highlighted over
/// their host. Empty past the size guards ([`highlightable`]).
///
/// Pure and synchronous: it parses the whole file, so callers run it off
/// the UI thread.
#[must_use]
pub fn highlight(lang: Language, source: &str) -> Vec<(Range<usize>, Capture)> {
    if !highlightable(source) {
        return Vec::new();
    }
    let mut painter = Painter { ids: vec![0; source.len()], captures: Vec::new() };
    let mut parser = Parser::new();
    layer(&mut parser, &mut painter, lang, source, None, 0);
    painter.finish()
}

/// [`highlight`]'s spans split per line (`\n`), with ranges relative to
/// each line's start; one entry per line of `source`.
#[must_use]
pub fn by_line(source: &str, spans: &[(Range<usize>, Capture)]) -> Vec<Vec<(Range<usize>, Capture)>> {
    let starts: Vec<usize> = std::iter::once(0).chain(source.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let mut lines = vec![Vec::new(); starts.len()];
    for (range, capture) in spans {
        let mut row = starts.partition_point(|&s| s <= range.start).saturating_sub(1);
        while row < starts.len() && starts[row] < range.end {
            let line_end = starts.get(row + 1).map_or(source.len(), |&next| next - 1);
            let (start, end) = (range.start.max(starts[row]), range.end.min(line_end));
            if start < end {
                lines[row].push((start - starts[row]..end - starts[row], *capture));
            }
            row += 1;
        }
    }
    lines
}

/// Capture per byte, then runs of them.
struct Painter {
    /// 0 is unpainted, else 1 + an index into `captures`.
    ids: Vec<u16>,
    captures: Vec<Capture>,
}

impl Painter {
    fn id(&mut self, capture: Capture) -> u16 {
        let index = self.captures.iter().position(|c| *c == capture).unwrap_or_else(|| {
            self.captures.push(capture);
            self.captures.len() - 1
        });
        u16::try_from(index + 1).unwrap_or(0)
    }

    fn paint(&mut self, range: Range<usize>, id: u16) {
        if let Some(bytes) = self.ids.get_mut(range) {
            bytes.fill(id);
        }
    }

    fn finish(self) -> Vec<(Range<usize>, Capture)> {
        let mut out: Vec<(Range<usize>, Capture)> = Vec::new();
        let mut start = 0;
        while start < self.ids.len() {
            let id = self.ids[start];
            let end = self.ids[start..].iter().position(|&i| i != id).map_or(self.ids.len(), |n| start + n);
            if id != 0 {
                let capture = self.captures[usize::from(id) - 1];
                if capture.key() != Some(NONE) {
                    out.push((start..end, capture));
                }
            }
            start = end;
        }
        out
    }
}

/// Highlight one layer: `lang` over `ranges` of `source` (all of it when
/// `None`), then its injections.
fn layer(parser: &mut Parser, painter: &mut Painter, lang: Language, source: &str, ranges: Option<&[TsRange]>, depth: usize) {
    let Some(config) = config::config(lang) else { return };
    if parser.set_language(&config.language).is_err() || parser.set_included_ranges(ranges.unwrap_or(&[])).is_err() {
        return;
    }
    let Some(tree) = parser.parse(source, None) else { return };
    let root = tree.root_node();

    let names = config.highlights.capture_names();
    let mut found = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&config.highlights, root, source.as_bytes());
    while let Some(m) = matches.next() {
        for c in m.captures {
            if config.paints[c.index as usize] {
                let rank = if config.first_wins { usize::MAX - m.pattern_index } else { m.pattern_index };
                found.push((c.node.start_byte(), c.node.end_byte(), rank, c.index as usize));
            }
        }
    }
    // Outer before inner, so inner paints over; for one node, the winning
    // pattern (by rank) last.
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    for (start, end, _, index) in found {
        let id = painter.id(Capture(names[index]));
        match ranges {
            None => painter.paint(start..end, id),
            Some(ranges) => {
                // An injected tree's nodes can span the host text between
                // its ranges (a `>` quote marker); paint only inside them.
                let first = ranges.partition_point(|r| r.end_byte <= start);
                for r in ranges[first..].iter().take_while(|r| r.start_byte < end) {
                    painter.paint(start.max(r.start_byte)..end.min(r.end_byte), id);
                }
            }
        }
    }

    if depth >= MAX_DEPTH {
        return;
    }
    if let Some(query) = &config.injections {
        for (lang, ranges) in injections(query, root, source, ranges) {
            layer(parser, painter, lang, source, Some(&ranges), depth + 1);
        }
    }
}

/// The languages `query` injects into `root`'s tree, with the ranges each
/// covers (within `parent`, the host layer's own ranges).
fn injections(query: &Query, root: Node, source: &str, parent: Option<&[TsRange]>) -> Vec<(Language, Vec<TsRange>)> {
    let names = query.capture_names();
    let mut out = Vec::new();
    let mut combined: Vec<(usize, Language, Vec<TsRange>)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source.as_bytes());
    while let Some(m) = matches.next() {
        let (mut name, mut content) = (None, None);
        for c in m.captures {
            match names[c.index as usize] {
                "injection.language" => name = source.get(c.node.byte_range()),
                "injection.content" => content = Some(c.node),
                _ => {}
            }
        }
        let (mut include_children, mut is_combined) = (false, false);
        for property in query.property_settings(m.pattern_index) {
            match &*property.key {
                "injection.language" if name.is_none() => name = property.value.as_deref(),
                "injection.include-children" => include_children = true,
                "injection.combined" => is_combined = true,
                _ => {}
            }
        }
        let (Some(lang), Some(node)) = (name.and_then(Language::from_name), content) else { continue };
        let mut ranges = content_ranges(node, include_children);
        if let Some(parent) = parent {
            ranges = intersect(&ranges, parent);
        }
        if ranges.is_empty() {
            continue;
        }
        if is_combined {
            match combined.iter_mut().find(|(p, l, _)| *p == m.pattern_index && *l == lang) {
                Some((_, _, all)) => all.extend(ranges),
                None => combined.push((m.pattern_index, lang, ranges)),
            }
        } else {
            out.push((lang, ranges));
        }
    }
    for (_, lang, mut ranges) in combined {
        ranges.sort_by_key(|r| r.start_byte);
        ranges.dedup_by(|b, a| b.start_byte < a.end_byte);
        out.push((lang, ranges));
    }
    out
}

/// `node`'s text, without its named children's unless `include_children`
/// (a fence's code without the `>` markers of an enclosing quote).
fn content_ranges(node: Node, include_children: bool) -> Vec<TsRange> {
    let full = node.range();
    if include_children {
        return vec![full];
    }
    let mut out = Vec::new();
    let (mut start_byte, mut start_point) = (full.start_byte, full.start_point);
    let mut cursor = node.walk();
    // Named children only: markdown's block grammar has its anonymous
    // tokens (`(`, `*`) as children of the text it injects.
    for child in node.named_children(&mut cursor) {
        if child.start_byte() > start_byte {
            out.push(TsRange { start_byte, end_byte: child.start_byte(), start_point, end_point: child.start_position() });
        }
        if child.end_byte() > start_byte {
            (start_byte, start_point) = (child.end_byte(), child.end_position());
        }
    }
    if full.end_byte > start_byte {
        out.push(TsRange { start_byte, end_byte: full.end_byte, start_point, end_point: full.end_point });
    }
    out
}

/// The parts of sorted, disjoint `a` inside sorted, disjoint `b`.
fn intersect(a: &[TsRange], b: &[TsRange]) -> Vec<TsRange> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (&a[i], &b[j]);
        let (start_byte, start_point) = if x.start_byte >= y.start_byte { (x.start_byte, x.start_point) } else { (y.start_byte, y.start_point) };
        let (end_byte, end_point) = if x.end_byte <= y.end_byte { (x.end_byte, x.end_point) } else { (y.end_byte, y.end_point) };
        if start_byte < end_byte {
            out.push(TsRange { start_byte, end_byte, start_point, end_point });
        }
        if x.end_byte <= y.end_byte {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}
