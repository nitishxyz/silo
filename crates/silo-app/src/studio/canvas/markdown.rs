//! Markdown drawn on the board at the zoom: a note's text and a document card's preview. The transcript's
//! parser (`crate::agent::markdown`), drawn small and scaled: bold, italics, strikes, inline code, links as
//! text, headings at modest sizes, lists, quotes, code blocks, tables as rows. Nothing scrolls or takes the
//! pointer; the caller clips it to the node.

use std::{collections::HashMap, rc::Rc};

use gpui::{AnyElement, FontStyle, FontWeight, HighlightStyle, SharedString, StrikethroughStyle, StyledText, div, prelude::*, px, rgb};

use crate::{
    agent::markdown::{Block, Doc, Inline},
    studio::tiles::ink,
    theme,
};

/// At most this many top-level blocks are drawn (a long script's preview is clipped long before).
const MAX_BLOCKS: usize = 60;

/// Parsed sources by key (a node id, a document path), parsed again when the text changes.
#[derive(Default)]
pub(crate) struct Cache(HashMap<String, (Rc<str>, Rc<Vec<Block>>)>);

impl Cache {
    pub(crate) fn blocks(&mut self, key: &str, source: &str) -> Rc<Vec<Block>> {
        if let Some((s, b)) = self.0.get(key)
            && &**s == source
        {
            return b.clone();
        }
        let mut doc = Doc::default();
        doc.update(source, false);
        let blocks: Rc<Vec<Block>> = Rc::new(doc.blocks().iter().take(MAX_BLOCKS).cloned().collect());
        self.0.insert(key.to_owned(), (source.into(), blocks.clone()));
        blocks
    }
}

/// `blocks` at zoom `z` (body text 12.5 pt at 100 %), in the caller's text colour.
pub(super) fn render(blocks: &[Block], z: f32) -> AnyElement {
    div().flex().flex_col().text_size(px(12.5 * z)).line_height(px(18.0 * z)).children(blocks.iter().map(|b| block(b, z, 0))).into_any_element()
}

fn block(b: &Block, z: f32, depth: usize) -> AnyElement {
    let s = |v: f32| px(v * z);
    let gap = if depth > 0 { 2.0 } else { 6.0 };
    match b {
        Block::Para(i) => div().pb(s(gap)).child(text(i)).into_any_element(),
        Block::Heading(level, i) => {
            let size = match level {
                1 => 15.0,
                2 => 14.0,
                _ => 13.0,
            };
            div()
                .pt(s(2.0))
                .pb(s(4.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(s(size))
                .line_height(s(size * 1.35))
                .child(text(i))
                .into_any_element()
        }
        Block::Code { text: code, .. } => div()
            .mb(s(gap))
            .px(s(6.0))
            .py(s(4.0))
            .rounded(s(4.0))
            .bg(rgb(ink(0.06)))
            .font_family(mono())
            .text_size(s(11.5))
            .line_height(s(16.0))
            .child(code.trim_end().to_owned())
            .into_any_element(),
        Block::List { start, items } => div()
            .pb(s(gap))
            .flex()
            .flex_col()
            .children(items.iter().enumerate().map(|(k, item)| {
                let marker = match (item.task, start) {
                    (Some(true), _) => "\u{2611}".to_owned(),
                    (Some(false), _) => "\u{2610}".to_owned(),
                    (None, Some(n)) => format!("{}.", n + k as u64),
                    (None, None) => "\u{2022}".to_owned(),
                };
                div()
                    .flex()
                    .gap(s(5.0))
                    .child(div().flex_none().text_color(rgb(theme::muted())).child(marker))
                    .child(div().flex_1().min_w_0().flex().flex_col().children(item.blocks.iter().map(|b| block(b, z, depth + 1))))
            }))
            .into_any_element(),
        Block::Quote(blocks) => div()
            .mb(s(gap))
            .pl(s(8.0))
            .border_l_2()
            .border_color(rgb(ink(0.2)))
            .text_color(rgb(theme::muted()))
            .flex()
            .flex_col()
            .children(blocks.iter().map(|b| block(b, z, depth + 1)))
            .into_any_element(),
        Block::Rule => div().my(s(4.0)).h(px(1.0)).bg(rgb(ink(0.15))).into_any_element(),
        Block::Table { head, rows, .. } => {
            let row = |cells: &[Inline], bold: bool| {
                div()
                    .flex()
                    .gap(s(8.0))
                    .py(s(1.0))
                    .when(bold, |el| el.font_weight(FontWeight::SEMIBOLD))
                    .children(cells.iter().map(|c| div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(text(c))))
            };
            div()
                .pb(s(gap))
                .flex()
                .flex_col()
                .when(!head.is_empty(), |el| el.child(row(head, true)).child(div().h(px(1.0)).bg(rgb(ink(0.15)))))
                .children(rows.iter().map(|r| row(r, false)))
                .into_any_element()
        }
    }
}

fn mono() -> SharedString {
    theme::summary().1.into()
}

/// An inline's styled runs (links drawn as their text).
fn text(inline: &Inline) -> StyledText {
    let mut highlights = Vec::new();
    let mut code = Vec::new();
    for (range, style) in &inline.spans {
        let mut h = HighlightStyle::default();
        if style.bold {
            h.font_weight = Some(FontWeight::SEMIBOLD);
        }
        if style.italic {
            h.font_style = Some(FontStyle::Italic);
        }
        if style.strike {
            h.strikethrough = Some(StrikethroughStyle { thickness: px(1.0), color: None });
        }
        if style.code {
            h.background_color = Some(rgb(ink(0.08)).into());
            code.push((range.clone(), mono()));
        }
        if h != HighlightStyle::default() {
            highlights.push((range.clone(), h));
        }
    }
    StyledText::new(inline.text.clone()).with_highlights(highlights).with_font_family_overrides(code)
}
