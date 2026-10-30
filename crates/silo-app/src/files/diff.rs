//! A tab's diff against `HEAD` (the header's diff toggle): the Changes
//! block's unified rows (`changes::model::rows`, folds and word changes)
//! and syntax colours (`changes::syntax`, worked out off the UI thread),
//! drawn in the viewer's font.

use std::{collections::HashSet, rc::Rc};

use gpui::{AnyElement, Context, Hsla, SharedString, UniformListScrollHandle, Window, div, prelude::*, px, rgb};
use silo_proto::changes::{FileDiff, LineKind};

use super::FilesPane;
use crate::{
    changes::{
        model::{self as diffs, FoldId, Layout, Row},
        syntax::{self, DiffSyntax},
    },
    theme,
};

/// A tab's diff: loading, failed, or its rows.
#[derive(Default)]
pub(crate) struct DiffView {
    loaded: Option<Result<Rc<Loaded>, String>>,
    expanded: HashSet<FoldId>,
    pub scroll: UniformListScrollHandle,
}

#[derive(Clone)]
struct Loaded {
    diff: FileDiff,
    words: Vec<Vec<diffs::Spans>>,
    rows: Vec<Row>,
    /// Per hunk and line, once worked out.
    syntax: Option<DiffSyntax>,
}

impl DiffView {
    /// The diff came (again): the colouring to run when it changed.
    pub(super) fn set(&mut self, diff: Result<FileDiff, String>) -> Option<syntax::Job> {
        if let (Ok(new), Some(Ok(old))) = (&diff, &self.loaded)
            && old.diff == *new
        {
            return None;
        }
        let job = diff.as_ref().ok().and_then(syntax::job);
        self.loaded = Some(diff.map(|diff| Rc::new(Loaded { words: diffs::words(&diff), rows: Vec::new(), diff, syntax: None })));
        self.lay_out();
        job
    }

    /// Colours for `diff`, unless another diff came meanwhile.
    pub(super) fn coloured(&mut self, diff: &FileDiff, colours: DiffSyntax) {
        if let Some(Ok(loaded)) = &mut self.loaded
            && loaded.diff == *diff
        {
            Rc::make_mut(loaded).syntax = Some(colours);
        }
    }

    fn lay_out(&mut self) {
        if let Some(Ok(loaded)) = &mut self.loaded {
            let rows = diffs::rows(&[&loaded.diff], Layout { split: false, headers: false }, &self.expanded, &[]);
            Rc::make_mut(loaded).rows = rows;
        }
    }
}

fn alpha(c: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a = a;
    h
}

/// `n` unchanged lines.
fn unchanged(n: usize) -> String {
    if n == 1 { "1 unchanged line".into() } else { format!("{n} unchanged lines") }
}

impl FilesPane {
    /// The shown tab's diff, when toggled on.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn render_diff(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let m = self.measure(window);
        let Some(view) = self.diffs.get(path) else { return div().into_any_element() };
        let loaded = match &view.loaded {
            None => return super::view::centered_hint("Loading…"),
            Some(Err(error)) => return super::view::centered_hint(error.clone()),
            Some(Ok(loaded)) => loaded,
        };
        if loaded.rows.is_empty() || (loaded.diff.hunks.is_empty() && !loaded.diff.binary && !loaded.diff.truncated) {
            return super::view::centered_hint("No changes against HEAD");
        }
        let (family, card) = (SharedString::from(theme::summary().1), self.surface.color());
        // Tints over the card (its veil shows through them when frosted).
        let (added, removed) = (alpha(theme::success(), 0.12), alpha(theme::danger(), 0.12));
        let (added_word, removed_word) = (alpha(theme::success(), 0.32), alpha(theme::danger(), 0.32));
        let numbers = (m.cell * 5.0).max(m.cell * loaded.diff.hunks.last().map_or(3, |h| (h.new_start + h.new_lines).to_string().len() + 1) as f32);
        let loaded = loaded.clone();
        let pane = cx.entity();
        let count = loaded.rows.len();
        let list = gpui::uniform_list("files-diff", count, move |range, _, _| {
            range.map(|ix| diff_row(&loaded, ix, m, numbers, Tints { card, added, removed, added_word, removed_word }, &pane)).collect()
        })
        .with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&view.scroll)
        .size_full();
        div()
            .size_full()
            .py(px(4.0))
            .font_family(family)
            .text_size(px(m.size))
            .line_height(px(m.line))
            .on_pinch(cx.listener(|this, e: &gpui::PinchEvent, _, cx| this.pinch(e, cx)))
            .child(list)
            .into_any_element()
    }

    fn unfold(&mut self, fold: &FoldId, cx: &mut Context<Self>) {
        let Some(path) = self.state.active.clone() else { return };
        if let Some(view) = self.diffs.get_mut(&path) {
            view.expanded.insert(*fold);
            view.lay_out();
            cx.notify();
        }
    }
}

#[derive(Clone, Copy)]
struct Tints {
    /// The card's colour: syntax colours stay readable on it.
    card: u32,
    added: Hsla,
    removed: Hsla,
    added_word: Hsla,
    removed_word: Hsla,
}

/// Row `ix` of a diff.
fn diff_row(loaded: &Loaded, ix: usize, m: super::text::Metrics, numbers: f32, t: Tints, pane: &gpui::Entity<FilesPane>) -> AnyElement {
    let muted = rgb(theme::muted());
    let note = |text: String| div().h(px(m.line)).pl(px(10.0)).flex().items_center().text_color(muted).child(text).into_any_element();
    match loaded.rows[ix] {
        Row::Line { hunk, at, .. } => {
            let line = &loaded.diff.hunks[hunk].lines[at];
            let (tint, word, sign) = match line.kind {
                LineKind::Added => (Some(t.added), t.added_word, "+"),
                LineKind::Removed => (Some(t.removed), t.removed_word, "\u{2212}"),
                _ => (None, t.added_word, " "),
            };
            let colours = loaded.syntax.as_ref().and_then(|s| s.get(hunk)?.get(at)).map(Vec::as_slice);
            let words = loaded.words.get(hunk).and_then(|h| h.get(at)).map_or(&[][..], Vec::as_slice);
            let number = |n: Option<u32>| {
                div().flex_none().w(px(numbers)).pr(px(6.0)).flex().justify_end().text_color(muted).children(n.map(|n| n.to_string()))
            };
            div()
                .h(px(m.line))
                .flex()
                .whitespace_nowrap()
                .when_some(tint, |el, c| el.bg(c))
                .child(number(line.old.filter(|_| line.kind != LineKind::Added)))
                .child(number(line.new.filter(|_| line.kind != LineKind::Removed)))
                .child(div().flex_none().w(px(m.cell + 6.0)).text_color(muted).child(sign))
                .child(syntax::styled(&line.text, colours, words, word, t.card))
                .pr(px(10.0))
                .into_any_element()
        }
        Row::Hunk { hunk, .. } => {
            let h = &loaded.diff.hunks[hunk];
            let title = format!("@@ \u{2212}{},{} +{},{} @@ {}", h.old_start, h.old_lines, h.new_start, h.new_lines, h.header);
            div().h(px(m.line)).pl(px(10.0)).flex().items_center().bg(alpha(theme::accent(), 0.07)).text_color(muted).child(title).into_any_element()
        }
        Row::Fold { file, hunk, at, lines } => {
            let pane = pane.clone();
            div()
                .id(("files-diff-fold", ix))
                .h(px(m.line))
                .pl(px(numbers * 2.0 + m.cell + 6.0))
                .text_color(muted)
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
                .child(format!("\u{22ef} {}", unchanged(lines)))
                .on_click(move |_, _, cx| pane.update(cx, |this, cx| this.unfold(&(file, hunk, at), cx)))
                .into_any_element()
        }
        Row::Gap { lines, .. } => note(unchanged(lines as usize)),
        Row::Note { ref text, .. } => note(text.clone()),
        _ => div().h(px(m.line)).into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::changes::{DiffLine, FileStatus, Hunk};

    use super::*;

    fn diff(new: &str) -> FileDiff {
        let line = |kind, text: &str, old, new| DiffLine { kind, old, new, text: text.into() };
        let hunk = Hunk {
            id: "h".into(),
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            header: String::new(),
            lines: vec![line(LineKind::Removed, "fn a() {}", Some(1), None), line(LineKind::Added, new, None, Some(1))],
        };
        FileDiff { path: "src/a.rs".into(), old_path: None, status: FileStatus::Modified, binary: false, hunks: vec![hunk], truncated: false }
    }

    #[test]
    fn colours_come_once_per_diff_and_never_onto_a_newer_one() {
        let mut view = DiffView::default();
        let job = view.set(Ok(diff("fn b() {}"))).expect("rust is coloured");
        assert!(view.set(Ok(diff("fn b() {}"))).is_none(), "the same diff again keeps its colours");
        let colours = job.run().into_shared();
        let syntax = |v: &DiffView| matches!(&v.loaded, Some(Ok(l)) if l.syntax.is_some());
        // A newer diff came while colouring: the old colours are dropped.
        assert!(view.set(Ok(diff("fn c() {}"))).is_some());
        view.coloured(&diff("fn b() {}"), colours.clone());
        assert!(!syntax(&view));
        view.coloured(&diff("fn c() {}"), colours);
        assert!(syntax(&view));
        assert!(view.set(Err("gone".into())).is_none());
    }
}
