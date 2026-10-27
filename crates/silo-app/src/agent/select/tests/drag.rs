//! Drawn in a window (gpui's test text system): texts register, and a drag over them selects.

use std::rc::Rc;

use gpui::{
    Context, IntoElement, ListAlignment, ListState, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render,
    TestAppContext, VisualTestContext, Window, div, point, prelude::*, px, size,
};

use super::{super::view, Doc};
use crate::agent::{
    markdown::Doc as Markdown,
    scroll::Inner,
    select::{Select, copy_text, ranges},
    view::render_settled,
};

const TEXT: &str = "First paragraph here.\n\nSecond one follows it.\n\n| A | B |\n|---|---|\n| 1 | 2 |";

/// One assistant text drawn as the pane draws a row, wired as the pane wires the transcript.
struct Transcript {
    md: Markdown,
    doc: Doc,
    select: Select,
    scroll: Rc<Inner>,
}

impl Render for Transcript {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.select.registry.borrow_mut().entries.clear();
        let frame = self.select.sel.as_ref().and_then(|s| s.bounds(&self.doc));
        let ranges = frame.map(|f| ranges(&self.doc, f, 0)).unwrap_or_default();
        let _scope = view::scope(self.select.registry.clone(), "r".into(), ranges, super::super::wire::colour());
        let blocks = self.md.blocks().iter().enumerate().map(|(i, b)| render_settled(&format!("r-{i}"), b, &self.scroll, cx));
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(blocks.collect::<Vec<_>>())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, _| this.select.down(&this.doc, e.position, e.click_count, e.modifiers.shift)),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.select.drag_to(&this.doc, e.position) {
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.select.up(&this.doc);
                    cx.notify();
                }),
            )
    }
}

fn open(cx: &mut TestAppContext) -> (gpui::Entity<Transcript>, &mut VisualTestContext) {
    let scroll = Inner::new(ListState::new(0, ListAlignment::Top, px(100.0)));
    let (view, cx) = cx.add_window_view(move |_, cx| {
        let mut md = Markdown::default();
        md.update(TEXT, false);
        Transcript { md, doc: Doc(vec![super::md("r", TEXT)]), select: Select::new(cx.focus_handle()), scroll }
    });
    cx.simulate_resize(size(px(600.0), px(400.0)));
    cx.run_until_parked();
    (view, cx)
}

/// A point at byte `ix` of drawn leaf `leaf` (just right of where its glyph starts, inside its line).
fn inside(view: &gpui::Entity<Transcript>, cx: &mut VisualTestContext, leaf: &str, ix: usize) -> Point<Pixels> {
    view.read_with(cx, |t, _| {
        let reg = t.select.registry.borrow();
        let e = reg.entries.iter().find(|e| e.leaf == leaf).unwrap_or_else(|| panic!("{leaf} registered"));
        let at = e.layout.position_for_index(ix).expect("in the text");
        point(at.x + px(0.5), at.y + px(4.0))
    })
}

fn copied(view: &gpui::Entity<Transcript>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |t, _| t.select.sel.as_ref().and_then(|s| copy_text(&t.doc, s)))
}

#[gpui::test]
fn dragging_across_two_paragraphs_copies_both_parts(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    let leaves: Vec<String> = view.read_with(cx, |t, _| t.select.registry.borrow().entries.iter().map(|e| e.leaf.clone()).collect());
    assert_eq!(leaves, ["r-0", "r-1", "r-2-r0-c0", "r-2-r0-c1", "r-2-r1-c0", "r-2-r1-c1"], "every drawn text registers, in reading order");
    let none = Modifiers::none();
    // From the very start of the first paragraph to the end of the second.
    let (from, to) = (inside(&view, cx, "r-0", 0), inside(&view, cx, "r-1", 22));
    cx.simulate_mouse_down(from, MouseButton::Left, none);
    cx.simulate_mouse_move(point(from.x + px(20.0), from.y), MouseButton::Left, none);
    cx.simulate_mouse_move(to, MouseButton::Left, none);
    cx.simulate_mouse_up(to, MouseButton::Left, none);
    cx.run_until_parked();
    assert_eq!(copied(&view, cx).as_deref(), Some("First paragraph here.\nSecond one follows it."));
    // A click without a drag clears it.
    cx.simulate_click(from, none);
    cx.run_until_parked();
    assert_eq!(copied(&view, cx), None);
    // Into a table: cells tab-separated.
    let cell = inside(&view, cx, "r-2-r1-c1", 1);
    let start = inside(&view, cx, "r-1", 0);
    cx.simulate_mouse_down(start, MouseButton::Left, none);
    cx.simulate_mouse_move(cell, MouseButton::Left, none);
    cx.simulate_mouse_up(cell, MouseButton::Left, none);
    assert_eq!(copied(&view, cx).as_deref(), Some("Second one follows it.\nA\tB\n1\t2"));
}

#[gpui::test]
fn a_double_click_takes_a_word_and_paints_it(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    // Inside "paragraph" (after "First ").
    let p = inside(&view, cx, "r-0", 9);
    let down = MouseDownEvent { position: p, modifiers: Modifiers::none(), button: MouseButton::Left, click_count: 2, first_mouse: false };
    cx.simulate_event(down);
    cx.simulate_event(MouseUpEvent { position: p, modifiers: Modifiers::none(), button: MouseButton::Left, click_count: 2 });
    cx.run_until_parked();
    assert_eq!(copied(&view, cx).as_deref(), Some("paragraph"));
    let frame = view.read_with(cx, |t, _| t.select.frame);
    assert_eq!(frame, None, "the test view keeps no frame; the row's ranges come from the selection");
    let painted = view.read_with(cx, |t, _| ranges(&t.doc, t.select.sel.as_ref().and_then(|s| s.bounds(&t.doc)).expect("selected"), 0));
    assert_eq!(painted, [("r-0".to_owned(), 6..15)].into());
}
