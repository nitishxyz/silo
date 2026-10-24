//! The drawing side: each selectable text drawn this frame registers its layout (for hit-testing) under the
//! row being drawn, and reads what of it is selected (painted as a highlight behind its glyphs).

use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc};

use gpui::{AnyElement, Bounds, Hsla, Pixels, Point, StyledText, TextLayout, div, point, prelude::*, px};

use super::{Anchor, text};

/// A text drawn this frame: `base` is where it starts in its leaf (a code line's start), `line` the source
/// of a code line (its tabs are drawn expanded), `bounds` its laid out box (window coordinates).
pub(in crate::agent) struct Entry {
    pub row: String,
    pub leaf: String,
    pub base: usize,
    pub line: Option<String>,
    pub layout: TextLayout,
    pub bounds: Bounds<Pixels>,
}

/// The texts drawn this frame, in paint (reading) order.
#[derive(Default)]
pub(in crate::agent) struct Registry {
    pub entries: Vec<Entry>,
}

impl Entry {
    fn anchor(&self, offset: usize) -> Anchor {
        let offset = match &self.line {
            Some(line) => text::to_source(line, offset).min(line.len()),
            None => offset.min(self.layout.len()),
        };
        Anchor { row: self.row.clone(), leaf: self.leaf.clone(), offset: self.base + offset }
    }

    fn end(&self) -> Anchor {
        self.anchor(self.line.as_ref().map_or(self.layout.len(), |l| text::to_shown(l, l.len())))
    }

    /// The char boundary nearest `p` (its wrapped line by height, clamped into the text).
    fn closest(&self, p: Point<Pixels>) -> Anchor {
        let lh = self.layout.line_height();
        let lines = self.layout.line_layouts();
        let (mut y, mut start) = (self.bounds.top(), 0);
        for (i, line) in lines.iter().enumerate() {
            let h = line.size(lh).height;
            if p.y < y + h || i + 1 == lines.len() {
                let local = point(p.x - self.bounds.left(), (p.y - y).clamp(px(0.0), (h - px(1.0)).max(px(0.0))));
                let (Ok(ix) | Err(ix)) = line.closest_index_for_position(local, lh);
                return self.anchor(start + ix.min(line.len()));
            }
            y += h;
            start += line.len() + 1;
        }
        self.anchor(0)
    }
}

impl Registry {
    /// The place at `p`: in a text, or (unless `strict`) the nearest place in reading order: the closest text
    /// level with it, else the end of the last text above it (the start of the first, above them all).
    pub fn hit(&self, p: Point<Pixels>, strict: bool) -> Option<Anchor> {
        if let Some(e) = self.entries.iter().find(|e| e.bounds.contains(&p)) {
            return Some(e.closest(p));
        }
        if strict {
            return None;
        }
        let level = self.entries.iter().filter(|e| e.bounds.top() <= p.y && p.y < e.bounds.bottom());
        let gap = |e: &&Entry| f32::from(if p.x < e.bounds.left() { e.bounds.left() - p.x } else { (p.x - e.bounds.right()).max(px(0.0)) });
        if let Some(e) = level.min_by(|a, b| gap(a).total_cmp(&gap(b))) {
            return Some(e.closest(p));
        }
        match self.entries.iter().rfind(|e| e.bounds.bottom() <= p.y) {
            Some(e) => Some(e.end()),
            None => self.entries.first().map(|e| e.anchor(0)),
        }
    }
}

/// The row being drawn: where its texts register, what of them is selected, and the highlight colour.
struct Scope {
    registry: Rc<RefCell<Registry>>,
    row: String,
    ranges: HashMap<String, Range<usize>>,
    bg: Hsla,
}

thread_local! {
    static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// While alive, texts drawn are row `row`'s (the previous scope comes back on drop).
pub(in crate::agent) struct Guard(Option<Scope>);

impl Drop for Guard {
    fn drop(&mut self) {
        SCOPE.set(self.0.take());
    }
}

pub(in crate::agent) fn scope(registry: Rc<RefCell<Registry>>, row: String, ranges: HashMap<String, Range<usize>>, bg: Hsla) -> Guard {
    Guard(SCOPE.replace(Some(Scope { registry, row, ranges, bg })))
}

/// What of leaf `leaf` of the row being drawn is selected, and the colour to paint it.
pub(in crate::agent) fn selected(leaf: &str) -> Option<(Range<usize>, Hsla)> {
    SCOPE.with_borrow(|s| s.as_ref().and_then(|s| s.ranges.get(leaf).map(|r| (r.clone(), s.bg))))
}

/// `el`, the drawing of `layout`, made selectable as part of leaf `leaf` (from byte `base`; `line`: a code
/// line's source): it registers once laid out and shows an I-beam. Outside a transcript row, `el` as it is.
pub(in crate::agent) fn register(leaf: &str, base: usize, line: Option<&str>, layout: &TextLayout, el: AnyElement) -> AnyElement {
    let Some((registry, row)) = SCOPE.with_borrow(|s| s.as_ref().map(|s| (s.registry.clone(), s.row.clone()))) else { return el };
    let (leaf, line, layout) = (leaf.to_owned(), line.map(str::to_owned), layout.clone());
    // After the text in paint order: its layout has its bounds by then.
    let probe = gpui::canvas(
        move |_, _, _| {
            let bounds = layout.bounds();
            registry.borrow_mut().entries.push(Entry { row, leaf, base, line, layout, bounds });
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_0();
    div().relative().cursor_text().child(el).child(probe).into_any_element()
}

/// Where each line of `text` (as `str::lines` splits it) starts.
pub(in crate::agent) fn line_starts(text: &str) -> Vec<usize> {
    let mut at = 0;
    text.split_inclusive('\n')
        .map(|l| {
            let start = at;
            at += l.len();
            start
        })
        .collect()
}

/// Code line `src` (starting at byte `base` of code block `leaf`) drawn as `shown` (tabs expanded; a space
/// for an empty line), its selected part highlighted.
pub(in crate::agent) fn code_line(leaf: &str, base: usize, src: &str, shown: String) -> AnyElement {
    let highlights = selected(leaf)
        .and_then(|(r, bg)| {
            // An empty line wholly inside shows its space selected.
            let range = if src.is_empty() {
                (r.start <= base && r.end > base).then_some(0..shown.len())?
            } else {
                let (a, b) = (r.start.clamp(base, base + src.len()) - base, r.end.clamp(base, base + src.len()) - base);
                (a < b).then(|| text::to_shown(src, a)..text::to_shown(src, b))?
            };
            Some(text::overlay(Vec::new(), range, bg))
        })
        .unwrap_or_default();
    let styled = StyledText::new(shown).with_highlights(highlights);
    let layout = styled.layout().clone();
    register(leaf, base, Some(src), &layout, styled.into_any_element())
}
