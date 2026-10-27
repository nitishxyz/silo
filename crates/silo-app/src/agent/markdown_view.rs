//! The transcript's markdown renderer outside the transcript: the Files
//! block's Rendered view of a markdown file (tables, task lists, fenced
//! code highlighted by `crate::syntax`).

use std::rc::Rc;

use gpui::{AnyElement, ListAlignment, ListState, div, prelude::*, px};

use super::{
    markdown::{Block, Doc},
    scroll::Inner,
    view::render_settled,
};
/// All of `source` as blocks, complete (not streaming).
fn parse(source: &str) -> Vec<Block> {
    let mut doc = Doc::default();
    doc.update(source, false);
    doc.blocks().to_vec()
}

pub(crate) struct MarkdownView {
    blocks: Rc<Vec<Block>>,
    list: ListState,
    scroll: Rc<Inner>,
}

impl MarkdownView {
    pub(crate) fn new(source: &str) -> Self {
        let blocks = parse(source);
        let list = ListState::new(blocks.len(), ListAlignment::Top, px(800.0));
        let scroll = Inner::new(list.clone());
        Self { blocks: Rc::new(blocks), list, scroll }
    }

    /// The file changed: draw the new text, keeping the scroll position.
    pub(crate) fn set_source(&mut self, source: &str) {
        let top = self.list.logical_scroll_top();
        self.blocks = Rc::new(parse(source));
        self.list.reset(self.blocks.len());
        self.list.scroll_to(gpui::ListOffset { item_ix: top.item_ix.min(self.blocks.len().saturating_sub(1)), offset_in_item: top.offset_in_item });
    }

    /// The rendered document, scrolling, `pad` points in from each side.
    pub(crate) fn render(&self, pad: f32) -> AnyElement {
        let (blocks, scroll) = (self.blocks.clone(), self.scroll.clone());
        let count = blocks.len();
        let list = gpui::list(self.list.clone(), move |ix, _, cx| {
            let block = render_settled(&format!("md-{ix}"), &blocks[ix], &scroll, cx);
            div()
                .px(px(pad))
                .when(ix == 0, |el| el.pt(px(pad * 0.75)))
                .when(ix + 1 == count, |el| el.pb(px(pad * 2.0)))
                .child(block)
                .into_any_element()
        })
        .size_full();
        div().relative().size_full().child(list).child(self.scroll.router()).into_any_element()
    }
}
