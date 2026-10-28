//! A context menu at a window point: rows of actions with their shortcut
//! hints and separators, in the Files menu's style. Its owner keeps a
//! [`Menu`], feeds it keys while open (↑↓ move, ↩ picks, esc closes) and
//! renders it last; a click picks a row, a click outside closes it.

use gpui::{AnyElement, Context, IntoElement, MouseButton, Pixels, Point, SharedString, Window, anchored, deferred, div, prelude::*, px, rgb};

use crate::theme::{self, Continuous as _};

pub enum Item<A> {
    Action { label: SharedString, keys: &'static str, enabled: bool, action: A },
    Separator,
}

impl<A> Item<A> {
    pub fn action(label: impl Into<SharedString>, keys: &'static str, action: A) -> Self {
        Self::Action { label: label.into(), keys, enabled: true, action }
    }

    #[must_use]
    pub fn enabled(mut self, on: bool) -> Self {
        if let Self::Action { enabled, .. } = &mut self {
            *enabled = on;
        }
        self
    }

    fn pickable(&self) -> bool {
        matches!(self, Self::Action { enabled: true, .. })
    }
}

/// What a key did to an open menu.
#[derive(Debug, PartialEq, Eq)]
pub enum Key<A> {
    Pick(A),
    Close,
    /// Moved the selection or did nothing: the menu stays.
    Stay,
}

pub struct Menu<A> {
    /// Window coordinates of its top-left corner (snapped inside the window).
    pub at: Point<Pixels>,
    items: Vec<Item<A>>,
    selected: Option<usize>,
}

impl<A: Clone + 'static> Menu<A> {
    /// `items` without leading, trailing or doubled separators.
    pub fn new(at: Point<Pixels>, items: Vec<Item<A>>) -> Self {
        let mut tidy: Vec<Item<A>> = Vec::with_capacity(items.len());
        for item in items {
            if matches!(item, Item::Separator) && tidy.last().is_none_or(|last| matches!(last, Item::Separator)) {
                continue;
            }
            tidy.push(item);
        }
        while tidy.last().is_some_and(|last| matches!(last, Item::Separator)) {
            tidy.pop();
        }
        Self { at, items: tidy, selected: None }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub fn items(&self) -> &[Item<A>] {
        &self.items
    }

    pub fn key(&mut self, key: &str) -> Key<A> {
        match key {
            "up" | "down" => {
                let picks: Vec<usize> = (0..self.items.len()).filter(|&i| self.items[i].pickable()).collect();
                let (Some(&first), Some(&last)) = (picks.first(), picks.last()) else { return Key::Stay };
                self.selected = Some(match (self.selected, key) {
                    (None, "down") => first,
                    (None, _) => last,
                    (Some(at), "down") => picks.iter().copied().find(|&i| i > at).unwrap_or(first),
                    (Some(at), _) => picks.iter().copied().rev().find(|&i| i < at).unwrap_or(last),
                });
                Key::Stay
            }
            "enter" | "space" => match self.selected.and_then(|i| self.items.get(i)) {
                Some(Item::Action { enabled: true, action, .. }) => Key::Pick(action.clone()),
                _ => Key::Close,
            },
            "escape" => Key::Close,
            _ => Key::Stay,
        }
    }

    /// The menu over everything; `pick` gets the clicked row's action, or
    /// `None` for a click outside.
    pub fn render<V: 'static>(
        &self,
        id: &'static str,
        cx: &mut Context<V>,
        pick: impl Fn(&mut V, Option<A>, &mut Window, &mut Context<V>) + 'static,
    ) -> AnyElement {
        let pick = std::rc::Rc::new(pick);
        let rows = self.items.iter().enumerate().map(|(index, item)| match item {
            Item::Separator => div().h(px(1.0)).my(px(4.0)).mx(px(6.0)).bg(rgb(theme::border())).into_any_element(),
            Item::Action { label, keys, enabled, action } => {
                let row = div()
                    .id((id, index))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(18.0))
                    .h(px(24.0))
                    .px(px(10.0))
                    .continuous(5.0)
                    .whitespace_nowrap()
                    .child(label.clone())
                    .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(*keys))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation());
                if *enabled {
                    let (pick, action) = (pick.clone(), action.clone());
                    row.cursor_pointer()
                        .when(self.selected == Some(index), |el| el.bg(rgb(theme::row_active())))
                        .hover(|s| s.bg(rgb(theme::row_active())))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            pick(this, Some(action.clone()), window, cx);
                            cx.notify();
                        }))
                        .into_any_element()
                } else {
                    row.text_color(rgb(theme::muted())).into_any_element()
                }
            }
        });
        let card = div()
            .id(id)
            .occlude()
            .min_w(px(210.0))
            .max_w(px(360.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .continuous(8.0)
            .bg(rgb(theme::popover()))
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_lg()
            .font_family(theme::ui_font())
            .text_size(px(12.5))
            .text_color(rgb(theme::foreground()))
            .children(rows)
            .on_mouse_down_out(cx.listener(move |this, _, window, cx| {
                pick(this, None, window, cx);
                cx.notify();
            }));
        deferred(anchored().position(self.at).snap_to_window_with_margin(px(8.0)).child(card)).with_priority(1).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(items: Vec<Item<u8>>) -> Menu<u8> {
        Menu::new(gpui::point(px(0.0), px(0.0)), items)
    }

    #[test]
    fn separators_only_divide_rows() {
        let m = menu(vec![Item::Separator, Item::action("a", "", 1), Item::Separator, Item::Separator, Item::action("b", "", 2), Item::Separator]);
        let kinds: Vec<bool> = m.items().iter().map(|i| matches!(i, Item::Separator)).collect();
        assert_eq!(kinds, [false, true, false]);
        assert!(menu(vec![Item::Separator]).is_empty());
    }

    #[test]
    fn arrows_skip_disabled_rows_and_wrap() {
        let mut m = menu(vec![Item::action("a", "", 1), Item::action("b", "", 2).enabled(false), Item::Separator, Item::action("c", "", 3)]);
        assert_eq!(m.key("enter"), Key::Close, "nothing selected yet");
        assert_eq!(m.key("down"), Key::Stay);
        assert_eq!(m.key("enter"), Key::Pick(1));
        m.key("down");
        assert_eq!(m.key("enter"), Key::Pick(3), "the disabled row and the separator are skipped");
        m.key("down");
        assert_eq!(m.key("enter"), Key::Pick(1), "wraps to the top");
        m.key("up");
        assert_eq!(m.key("enter"), Key::Pick(3), "and to the bottom");
        assert_eq!(m.key("escape"), Key::Close);
        assert_eq!(m.key("x"), Key::Stay);
        let mut up = menu(vec![Item::action("a", "", 1), Item::action("b", "", 2)]);
        up.key("up");
        assert_eq!(up.key("enter"), Key::Pick(2), "↑ first selects the last row");
    }
}
