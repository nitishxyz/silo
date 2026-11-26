//! `@` mentions in the canvas's prompt editors (docs/STUDIO.md §4.2): a card's inline prompt and the canvas
//! prompt. As the text or the caret changes, the editor's chips are found again in its text
//! ([`crate::studio::mentions::chips`]) and the popup opens, filters or closes on the `@query` at the caret; ↑ / ↓
//! / Tab / ⏎ / esc are the popup's while it shows. A pick puts the token in the text as a chip. The rows are the
//! composer's ([`crate::agent::mention::list`]), from the board's canvas and the world.

use std::ops::Range;

use gpui::{AnyElement, Context, Entity, MouseButton, ScrollHandle, Window, div, prelude::*, px};
use silo_proto::studio::NodeBody;

use super::CanvasView;
use crate::{
    agent::{
        editor::{Editor, EditorEvent, Nav},
        mention::{self as chips, Choice},
    },
    studio::{board, mentions},
    surface::Glassy as _,
};

/// The popup's width.
const W: f32 = 300.0;
/// Room it wants under a card's prompt before it opens above it instead.
const ROOM: f64 = 280.0;

/// The `@` popup of one editor.
pub(crate) struct Popup {
    pub editor: Entity<Editor>,
    /// The card whose prompt it is (`None`: the canvas prompt).
    pub card: Option<String>,
    /// The `@query`'s range in the text (to the word's end) and the query before the caret.
    pub range: Range<usize>,
    pub query: String,
    pub selected: usize,
    pub scroll: ScrollHandle,
}

impl CanvasView {
    /// Editor `editor` (a prompt of `card`; `None`: the canvas prompt) changed or its caret moved: its chips found
    /// again in its text, the popup opened, filtered or closed.
    pub(crate) fn sync_mentions(&mut self, editor: &Entity<Editor>, card: Option<&str>, cx: &mut Context<Self>) {
        let canvas = self.board.canvas();
        // Quiet (unwired) chips: only tokens already saved, once the server's wires are in.
        let settled = card.and_then(|c| match canvas.node(c).map(|n| &n.body) {
            Some(NodeBody::Gen { prompt, .. }) if self.board.settled() => Some(prompt.as_str()),
            _ => None,
        });
        let (found, list) = {
            let e = editor.read(cx);
            // An entity's chip with `#…` typed right after it is a query again, for its references.
            let list = mentions::drilled(e.text(), e.caret(), mentions::chips(e.text(), canvas, card, self.board.sections(), settled));
            (chips::query(e.text(), e.caret(), &list).map(|(range, query)| (range, query.to_owned())), list)
        };
        editor.update(cx, |e, cx| e.set_chips(list, cx));
        let id = editor.entity_id();
        if self.mention_dismissed.is_some_and(|(ed, at)| ed != id || found.as_ref().map(|f| f.0.start) != Some(at)) {
            self.mention_dismissed = None;
        }
        match found.filter(|_| self.mention_dismissed.is_none()) {
            Some((range, query)) => {
                let same = self.mention.take().filter(|p| p.editor == *editor);
                let fresh = same.as_ref().is_none_or(|p| p.query != query);
                let (selected, scroll) = same.map_or_else(|| (0, ScrollHandle::new()), |p| (p.selected, p.scroll));
                let selected = if fresh { 0 } else { selected };
                if fresh {
                    scroll.scroll_to_item(0);
                }
                self.mention = Some(Popup { editor: editor.clone(), card: card.map(str::to_owned), range, query, selected, scroll });
            }
            None => {
                if self.mention.as_ref().is_some_and(|p| p.editor == *editor) {
                    self.mention = None;
                }
            }
        }
        let open = self.mention.as_ref().is_some_and(|p| p.editor == *editor);
        editor.update(cx, |e, _| e.nav = open);
        cx.notify();
    }

    /// The chips of the editors open, found again (the canvas or the world changed under them).
    pub(crate) fn resync_mentions(&mut self, cx: &mut Context<Self>) {
        if let Some((editor, node)) = self.inline.as_ref().filter(|i| i.kind == super::InlineKind::Prompt).map(|i| (i.editor.clone(), i.node.clone()))
        {
            self.sync_mentions(&editor, Some(&node), cx);
        }
        if let Some(editor) = self.prompt.as_ref().map(|p| p.editor.clone()) {
            self.sync_mentions(&editor, None, cx);
        }
    }

    /// The popup's rows now.
    pub(crate) fn mention_choices(&self) -> Vec<Choice> {
        let Some(p) = &self.mention else { return Vec::new() };
        mentions::choices(self.board.canvas(), p.card.as_deref(), self.board.sections(), &p.query)
    }

    /// An editor's key event while its popup shows: picks (⏎, Tab), moves the selection (↑ / ↓) or closes it (esc).
    /// `false`: not the popup's (none shows for `editor`).
    pub(crate) fn mention_event(&mut self, editor: &Entity<Editor>, event: &EditorEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(p) = self.mention.as_mut().filter(|p| p.editor == *editor) else { return false };
        match event {
            EditorEvent::Submit | EditorEvent::Nav(Nav::Tab) => {
                let at = p.selected;
                self.pick_mention(at, window, cx);
            }
            EditorEvent::Escape => {
                self.mention_dismissed = Some((editor.entity_id(), p.range.start));
                self.close_mention(cx);
            }
            EditorEvent::Nav(nav) => {
                let count = mentions::choices(self.board.canvas(), p.card.as_deref(), self.board.sections(), &p.query).len();
                p.selected = match nav {
                    Nav::Up => p.selected.checked_sub(1).unwrap_or(count.saturating_sub(1)),
                    _ => {
                        if count == 0 {
                            0
                        } else {
                            (p.selected + 1) % count
                        }
                    }
                };
                let at = p.selected;
                let choices = self.mention_choices();
                if let Some(p) = &self.mention {
                    p.scroll.scroll_to_item(chips::child(&choices, at));
                }
                cx.notify();
            }
            _ => return false,
        }
        true
    }

    /// Row `index` picked: its token replaces the `@query` at the caret, as a chip, the caret after it.
    pub(crate) fn pick_mention(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.mention.as_ref().map(|p| p.editor.clone()) else { return };
        let Some((kind, target)) = self.mention_choices().get(index).and_then(Choice::mention) else { return };
        // The `@word` at the caret now (it may have moved since the popup opened).
        let range = {
            let e = editor.read(cx);
            chips::query(e.text(), e.caret(), e.chips()).map(|(range, _)| range)
        };
        if let Some(range) = range {
            editor.update(cx, |e, cx| e.insert_chip(range, kind, target, cx));
        }
        editor.read(cx).focus.clone().focus(window, cx);
        cx.notify();
    }

    /// The popup closes (its editor stops passing ↑ / ↓ / Tab to it).
    pub(crate) fn close_mention(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.mention.take() {
            p.editor.update(cx, |e, _| e.nav = false);
        }
        cx.notify();
    }

    /// The popup's list for editor `editor`, when it shows one.
    pub(crate) fn mention_list(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.mention.as_ref().filter(|p| p.editor == *editor)?;
        let list = chips::list(
            "studio-mention",
            self.mention_choices(),
            p.selected,
            &p.scroll,
            "",
            cx,
            |this, index, cx| {
                if let Some(p) = this.mention.as_mut()
                    && p.selected != index
                {
                    p.selected = index;
                    cx.notify();
                }
            },
            |this, index, window, cx| this.pick_mention(index, window, cx),
        );
        Some(list)
    }
}

/// A press in the popup stays there: the editor keeps the focus (an inline prompt would save and close).
fn keep_focus(el: gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div> {
    el.on_mouse_down(MouseButton::Left, |_, window, cx| {
        window.prevent_default();
        cx.stop_propagation();
    })
}

/// The popup in the canvas prompt's card, under its field.
pub(super) fn in_prompt(view: &CanvasView, editor: &Entity<Editor>, cx: &mut Context<CanvasView>) -> Option<AnyElement> {
    let list = view.mention_list(editor, cx)?;
    Some(keep_focus(div().id("studio-prompt-mention").flex().flex_col().child(list)).into_any_element())
}

/// The popup of the card prompt being edited, floating under the prompt (over it when the view has no room
/// below).
pub(super) fn floating(view: &CanvasView, cx: &mut Context<CanvasView>) -> Option<AnyElement> {
    let inline = view.inline.as_ref().filter(|i| i.kind == super::InlineKind::Prompt)?;
    let parts = view.board.laid(&inline.node)?.card?;
    let pr = view.board.camera().rect_to_view(parts.prompt);
    let list = view.mention_list(&inline.editor, cx)?;
    let size = view.bounds.get().size;
    let (width, height) = (f64::from(f32::from(size.width)), f64::from(f32::from(size.height)));
    let left = pr.x.clamp(8.0, (width - f64::from(W) - 8.0).max(8.0));
    let below = pr.bottom() + 4.0;
    let card = keep_focus(div().id("studio-mention").occlude().absolute().left(px(left as f32)))
        .map(|el| if below + ROOM <= height || pr.y < ROOM { el.top(px(below as f32)) } else { el.bottom(px((height - pr.y + 4.0) as f32)) })
        .w(px(W))
        .p(px(chips::PAD))
        .glass(chips::RADIUS)
        .text_size(px(13.0))
        .flex()
        .flex_col()
        .child(list);
    Some(card.into_any_element())
}

/// A chip's tooltip region in the resting face of a card's prompt: view points from the text's top-left.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tip {
    pub rect: board::Rect,
    pub text: String,
}
