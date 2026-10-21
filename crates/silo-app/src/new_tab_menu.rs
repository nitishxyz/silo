//! The block kinds a new tab can start with, and the menu listing them
//! behind the tab strip's + button (⌃⌘T). ⌘T skips it and opens the
//! focused block's kind.

use std::{cell::Cell, rc::Rc};

use gpui::{AnyElement, Bounds, Context, FocusHandle, FontWeight, IntoElement, Pixels, Subscription, Window, canvas, div, prelude::*, px, rgb};
use silo_proto::model::{BlockInfo, BlockKind};

use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    commands::Command,
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

/// A block kind a tab can start with: a row of the menu, and what ⌘T
/// opens when a block of that kind is focused.
pub struct TabKind {
    pub kind: BlockKind,
    pub label: &'static str,
    pub icon: &'static str,
    /// Opens a tab with this kind.
    pub command: Command,
}

pub const TAB_KINDS: [TabKind; 2] = [
    TabKind { kind: BlockKind::Terminal, label: "Terminal", icon: "terminal", command: Command::NewTerminalTab },
    TabKind { kind: BlockKind::Browser, label: "Browser", icon: "globe", command: Command::NewBrowserTab },
];

/// The terminal row: ⌘T's kind when nothing (or no known kind) is focused.
const TERMINAL: usize = 0;

/// The row ⌘T opens: the kind of `focused` (any block of the session,
/// floats included), else a terminal.
fn same_kind(focused: Option<&str>, blocks: &[BlockInfo]) -> usize {
    let kind = focused.and_then(|id| blocks.iter().find(|b| b.id == id)).map(|b| b.kind);
    kind.and_then(|kind| TAB_KINDS.iter().position(|t| t.kind == kind)).unwrap_or(TERMINAL)
}

/// A row's shortcut: its own, or ⌘T's when it is the kind ⌘T opens.
fn row_hint(index: usize, same: usize) -> Option<String> {
    TAB_KINDS[index].command.hint().or_else(|| (index == same).then(|| Command::NewTab.hint()).flatten())
}

pub struct NewTabMenu {
    pub open: bool,
    selected: usize,
    /// The row ⌘T would open, from when the menu opened.
    same: usize,
    /// The + button's bounds, from its last paint: the menu hangs below
    /// it, and presses on it are the button's to toggle.
    button: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Focused while open, so keys reach the menu, not the terminal.
    focus: FocusHandle,
    _blur: Subscription,
}

impl NewTabMenu {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |this, window, cx| {
            if !this.new_tab_menu.over_button(window) {
                this.close_new_tab_menu(cx);
            }
        });
        Self { open: false, selected: TERMINAL, same: TERMINAL, button: Rc::default(), focus, _blur: blur }
    }

    fn over_button(&self, window: &Window) -> bool {
        self.button.get().is_some_and(|b| b.contains(&window.mouse_position()))
    }

    /// Records the + button's bounds; a child of the button.
    pub fn button_probe(&self) -> impl IntoElement {
        let button = self.button.clone();
        canvas(move |bounds, _, _| button.set(Some(bounds)), |_, (), _, _| {}).absolute().top_0().left_0().size_full()
    }
}

/// The row after `selected` moved by `step`, wrapping.
fn step(selected: usize, step: isize) -> usize {
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    let next = (selected as isize + step).rem_euclid(TAB_KINDS.len() as isize) as usize;
    next
}

impl Workspace {
    /// The row ⌘T opens for the focused block of the active window.
    fn same_kind_row(&self, window: &Window, cx: &gpui::App) -> usize {
        let focused = self.focused_block(window, cx);
        same_kind(focused.as_deref(), self.view.as_ref().map_or(&[], |v| v.blocks.as_slice()))
    }

    /// ⌘T: a tab with the focused block's kind, gated like the row's own
    /// command.
    pub(crate) fn new_tab_like_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.same_kind_row(window, cx);
        self.run(TAB_KINDS[row].command, window, cx);
    }

    fn close_new_tab_menu(&mut self, cx: &mut Context<Self>) {
        if self.new_tab_menu.open {
            self.new_tab_menu.open = false;
            cx.notify();
        }
    }

    pub(crate) fn toggle_new_tab_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let same = self.same_kind_row(window, cx);
        let menu = &mut self.new_tab_menu;
        menu.open = !menu.open;
        if menu.open {
            menu.same = same;
            menu.selected = same;
            let focus = menu.focus.clone();
            // One overlay at a time: ⌃⌘T over the switcher replaces it.
            self.close_switcher(window, cx);
            focus.focus(window, cx);
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        } else {
            self.refocus_terminal(window, cx);
        }
        cx.notify();
    }

    fn choose_new_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_new_tab_menu(cx);
        self.refocus_terminal(window, cx);
        self.run(TAB_KINDS[index].command, window, cx);
    }

    pub(crate) fn render_new_tab_menu(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = &self.new_tab_menu;
        if !menu.open {
            return None;
        }
        // Hangs below the + button, right edges aligned; at the titlebar's
        // right end before the button has been painted.
        let (top, right) = match menu.button.get() {
            Some(button) => (button.bottom() + px(4.0), window.viewport_size().width - button.right()),
            None => (px(TITLEBAR_HEIGHT - 4.0), px(10.0)),
        };
        let mut list = div().flex().flex_col();
        for (index, row) in TAB_KINDS.iter().enumerate() {
            let selected = index == menu.selected;
            // A host without the feature: muted, saying so instead of the shortcut.
            let supported = row.command.feature().is_none_or(|f| self.host_supports(f));
            let hint = if supported { row_hint(index, menu.same) } else { Some(self.not_supported()) };
            list = list.child(
                div()
                    .id(("new-tab-item", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(28.0))
                    .px(px(8.0))
                    .continuous(crate::radius::MENU_ROW)
                    .when(supported, |el| el.cursor_pointer())
                    .when(selected, |el| el.bg(theme::row_fill()))
                    .child(icon(row.icon, 13.0, theme::muted()))
                    .child(div().flex_1().when(!supported, |el| el.text_color(rgb(theme::muted()))).child(row.label))
                    .children(hint.map(|h| div().flex_none().text_size(px(11.0)).text_color(rgb(theme::muted())).child(h)))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.new_tab_menu.selected != index {
                            this.new_tab_menu.selected = index;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| this.choose_new_tab(index, window, cx))),
            );
        }
        Some(
            div()
                .id("new-tab-menu")
                .track_focus(&menu.focus)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    let menu = &mut this.new_tab_menu;
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            this.close_new_tab_menu(cx);
                            this.refocus_terminal(window, cx);
                        }
                        "up" => menu.selected = step(menu.selected, -1),
                        "down" => menu.selected = step(menu.selected, 1),
                        "enter" => {
                            let index = menu.selected;
                            this.choose_new_tab(index, window, cx);
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                    cx.notify();
                }))
                .absolute()
                .top(top)
                .right(right)
                .min_w(px(200.0))
                .p(px(4.0))
                .continuous(10.0)
                .bg(theme::overlay_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_lg()
                .occlude()
                .font_family(theme::ui_font())
                .font_weight(FontWeight::NORMAL)
                .text_size(px(12.5))
                .text_color(rgb(theme::foreground()))
                .child(list)
                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                    if !this.new_tab_menu.over_button(window) {
                        this.close_new_tab_menu(cx);
                        this.refocus_terminal(window, cx);
                    }
                }))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::model::{Lifecycle, TermSize};

    use super::*;

    fn block(id: &str, kind: BlockKind) -> BlockInfo {
        BlockInfo {
            id: id.into(),
            session_id: "s".into(),
            kind,
            title: String::new(),
            cwd: None,
            exited: None,
            lifecycle: Lifecycle::Running,
            size: TermSize::new(80, 24),
            owner: None,
            owner_name: None,
            pid: None,
            browser: None,
        }
    }

    fn row(focused: Option<&str>, blocks: &[BlockInfo]) -> Command {
        TAB_KINDS[same_kind(focused, blocks)].command
    }

    #[test]
    fn selection_wraps() {
        assert_eq!(step(0, -1), 1);
        assert_eq!(step(1, 1), 0);
        assert_eq!(step(0, 1), 1);
    }

    #[test]
    fn new_tab_opens_the_focused_kind() {
        let blocks = [block("t", BlockKind::Terminal), block("b", BlockKind::Browser), block("u", BlockKind::Unknown)];
        assert_eq!(row(Some("t"), &blocks), Command::NewTerminalTab);
        assert_eq!(row(Some("b"), &blocks), Command::NewBrowserTab);
        assert_eq!(row(None, &blocks), Command::NewTerminalTab, "nothing focused");
        assert_eq!(row(Some("t"), &[]), Command::NewTerminalTab, "no window");
        assert_eq!(row(Some("u"), &blocks), Command::NewTerminalTab, "a kind no tab starts with");
    }

    #[test]
    fn every_kind_is_a_row_opening_it() {
        assert_eq!(TAB_KINDS[TERMINAL].kind, BlockKind::Terminal);
        let blocks: Vec<BlockInfo> = TAB_KINDS.iter().map(|t| block(t.label, t.kind)).collect();
        for (index, t) in TAB_KINDS.iter().enumerate() {
            // ⌘T on a block of this kind and Enter on the preselected row
            // both run the row's command.
            assert_eq!(same_kind(Some(t.label), &blocks), index, "{}", t.label);
            assert_ne!(t.command, Command::NewTab);
            assert_eq!(TAB_KINDS.iter().filter(|o| o.kind == t.kind).count(), 1);
        }
    }

    #[test]
    fn rows_show_true_shortcuts() {
        let browser = TAB_KINDS.iter().position(|t| t.kind == BlockKind::Browser).unwrap();
        assert_eq!(row_hint(browser, TERMINAL).as_deref(), Some("⌥⌘T"));
        assert_eq!(row_hint(browser, browser).as_deref(), Some("⌥⌘T"));
        assert_eq!(row_hint(TERMINAL, TERMINAL).as_deref(), Some("⌘T"), "⌘T opens a terminal here");
        assert_eq!(row_hint(TERMINAL, browser), None, "⌘T opens a browser here");
    }
}
