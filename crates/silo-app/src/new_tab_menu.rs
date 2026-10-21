//! The block kinds a new tab can start with, and the chooser listing them
//! (⌃⌘T, or the tab strip's + button): type to filter, ↩ opens the
//! selected row. ⌘T skips it and opens the focused block's kind.

use std::{cell::Cell, ops::Range, rc::Rc};

use gpui::{
    AnyElement, Bounds, Context, Entity, FontWeight, HighlightStyle, IntoElement, Pixels, StyledText, Subscription, Window, canvas, div, prelude::*,
    px, rgb,
};
use silo_proto::model::{BlockInfo, BlockKind};

use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    commands::{Command, fuzzy},
    text_field::{FieldEvent, TextField},
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

/// A block kind a tab can start with: a row of the chooser, and what ⌘T
/// opens when a block of that kind is focused.
pub struct TabKind {
    pub kind: BlockKind,
    pub label: &'static str,
    pub icon: &'static str,
    /// Opens a tab with this kind.
    pub command: Command,
}

pub const TAB_KINDS: [TabKind; 3] = [
    TabKind { kind: BlockKind::Terminal, label: "Terminal", icon: "terminal", command: Command::NewTerminalTab },
    TabKind { kind: BlockKind::Browser, label: "Browser", icon: "globe", command: Command::NewBrowserTab },
    TabKind { kind: BlockKind::Agent, label: "Agent", icon: crate::agent_block::ICON, command: Command::NewAgentTab },
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

#[derive(Clone, Debug, PartialEq)]
enum Row {
    /// A `TAB_KINDS` index and its label's matched ranges.
    Kind(usize, Vec<Range<usize>>),
    /// A browser tab at this address.
    Open(String),
}

impl Row {
    /// What opening the row runs (its host feature gates the row).
    fn command(&self) -> Command {
        match self {
            Self::Kind(index, _) => TAB_KINDS[*index].command,
            Self::Open(_) => Command::NewBrowserTab,
        }
    }
}

/// The rows for `query`: every kind while it is empty, else the matching
/// kinds best first, then the address it reads as, if any.
fn rows(query: &str) -> Vec<Row> {
    let q = query.trim();
    if q.is_empty() {
        return (0..TAB_KINDS.len()).map(|index| Row::Kind(index, Vec::new())).collect();
    }
    let mut found: Vec<(i32, usize, Vec<Range<usize>>)> =
        TAB_KINDS.iter().enumerate().filter_map(|(index, t)| fuzzy(q, t.label).map(|(score, ranges)| (score, index, ranges))).collect();
    found.sort_by_key(|(score, index, _)| (std::cmp::Reverse(*score), *index));
    let mut out: Vec<Row> = found.into_iter().map(|(_, index, ranges)| Row::Kind(index, ranges)).collect();
    out.extend(crate::finder::address(q).map(Row::Open));
    out
}

/// The row selected for `query`: the kind ⌘T opens until something is
/// typed (so ⌃⌘T ↩ is ⌘T), then the best match.
fn preselect(query: &str, rows: &[Row], same: usize) -> usize {
    if query.trim().is_empty() { rows.iter().position(|r| matches!(r, Row::Kind(index, _) if *index == same)).unwrap_or(0) } else { 0 }
}

struct Chooser {
    field: Entity<TextField>,
    rows: Vec<Row>,
    selected: usize,
    /// The row ⌘T would open, from when the chooser opened.
    same: usize,
    /// Why ↩ did nothing (a host without the row's feature).
    note: Option<String>,
    _sub: Subscription,
}

#[derive(Default)]
pub struct NewTabMenu {
    chooser: Option<Chooser>,
    /// The + button's bounds, from its last paint: the chooser hangs
    /// below it.
    button: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl NewTabMenu {
    pub fn is_open(&self) -> bool {
        self.chooser.is_some()
    }

    /// Records the + button's bounds; a child of the button.
    pub fn button_probe(&self) -> impl IntoElement {
        let button = self.button.clone();
        canvas(move |bounds, _, _| button.set(Some(bounds)), |_, (), _, _| {}).absolute().top_0().left_0().size_full()
    }
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

    pub(crate) fn close_new_tab_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_tab_menu.chooser.take().is_some() {
            self.refocus_terminal(window, cx);
            cx.notify();
        }
    }

    pub(crate) fn toggle_new_tab_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_tab_menu.is_open() {
            return self.close_new_tab_menu(window, cx);
        }
        let same = self.same_kind_row(window, cx);
        // One dialog at a time.
        self.close_switcher(window, cx);
        let field = cx.new(|cx| TextField::new("New tab…", cx));
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.new_tab_event(event, window, cx));
        let focus = field.read(cx).focus.clone();
        let rows = rows("");
        let selected = preselect("", &rows, same);
        self.new_tab_menu.chooser = Some(Chooser { field, rows, selected, same, note: None, _sub: sub });
        // Deferred: a mouse-down that opened us may still focus its target.
        focus.focus(window, cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    fn new_tab_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chooser) = self.new_tab_menu.chooser.as_mut() else { return };
        let count = chooser.rows.len().max(1);
        match event {
            FieldEvent::Changed(text) => {
                chooser.rows = rows(text);
                chooser.selected = preselect(text, &chooser.rows, chooser.same);
                chooser.note = None;
            }
            FieldEvent::Up => chooser.selected = (chooser.selected + count - 1) % count,
            FieldEvent::Down => chooser.selected = (chooser.selected + 1) % count,
            FieldEvent::Cancel => return self.close_new_tab_menu(window, cx),
            FieldEvent::Submit => {
                let index = chooser.selected;
                return self.choose_new_tab(index, window, cx);
            }
        }
        cx.notify();
    }

    fn row_supported(&self, row: &Row) -> bool {
        let command = row.command();
        command.feature().is_none_or(|f| self.host_supports(f)) && (command != Command::NewAgentTab || self.agents_here())
    }

    /// Open row `index`; a row the host can't open stays, saying so.
    fn choose_new_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.new_tab_menu.chooser.as_ref().and_then(|c| c.rows.get(index).cloned()) else { return };
        if !self.row_supported(&row) {
            let note = self.not_supported();
            if let Some(chooser) = self.new_tab_menu.chooser.as_mut() {
                chooser.note = Some(note);
            }
            return cx.notify();
        }
        self.close_new_tab_menu(window, cx);
        match row {
            Row::Kind(index, _) => self.run(TAB_KINDS[index].command, window, cx),
            Row::Open(url) => self.open_page_tab(url),
        }
    }

    pub(crate) fn render_new_tab_menu(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = &self.new_tab_menu;
        let chooser = menu.chooser.as_ref()?;
        // Hangs below the + button, right edges aligned; at the titlebar's
        // right end before the button has been painted.
        let right = menu.button.get().map_or(px(10.0), |button| window.viewport_size().width - button.right());
        let highlight = HighlightStyle { color: Some(rgb(theme::accent()).into()), font_weight: Some(FontWeight::BOLD), ..Default::default() };
        let mut list = div().flex().flex_col().py(px(4.0));
        for (index, row) in chooser.rows.iter().enumerate() {
            let selected = index == chooser.selected;
            let supported = self.row_supported(row);
            let (glyph, label, ranges, hint) = match row {
                Row::Kind(kind, ranges) => (TAB_KINDS[*kind].icon, TAB_KINDS[*kind].label.to_owned(), ranges.clone(), row_hint(*kind, chooser.same)),
                Row::Open(url) => ("globe", format!("Open {url}"), Vec::new(), None),
            };
            // A host without the feature: muted, saying so instead of the shortcut.
            let hint = if supported { hint } else { Some(self.not_supported()) };
            let label = StyledText::new(label).with_highlights(ranges.into_iter().map(|r| (r, highlight)).collect::<Vec<_>>());
            list = list.child(
                div()
                    .id(("new-tab-item", index))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(26.0))
                    .mx(px(4.0))
                    .px(px(8.0))
                    .continuous(crate::radius::MENU_ROW)
                    .when(selected, |el| el.bg(theme::row_fill()))
                    .when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill())))
                    .when(supported, |el| el.cursor_pointer())
                    .child(div().w(px(14.0)).flex_none().flex().justify_center().child(icon(glyph, 13.0, theme::muted())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(rgb(if supported { theme::foreground() } else { theme::muted() }))
                            .child(label),
                    )
                    .children(hint.map(|h| div().flex_none().text_size(px(11.0)).text_color(rgb(theme::muted())).child(h)))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if let Some(c) = this.new_tab_menu.chooser.as_mut()
                            && c.selected != index
                        {
                            c.selected = index;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| this.choose_new_tab(index, window, cx))),
            );
        }
        let empty = chooser.rows.is_empty();
        let popover = div()
            .id("new-tab-menu")
            .occlude()
            .absolute()
            .top(px(TITLEBAR_HEIGHT - 6.0))
            .right(right)
            .w(px(280.0))
            .continuous(10.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_lg()
            .font_family(theme::ui_font())
            .text_size(px(12.5))
            .text_color(rgb(theme::foreground()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(34.0))
                    .px(px(12.0))
                    .border_b_1()
                    .border_color(rgb(theme::border()))
                    .child(icon("plus", 13.0, theme::muted()))
                    .child(chooser.field.clone()),
            )
            .children(chooser.note.clone().map(|n| div().px(px(12.0)).pt(px(6.0)).text_size(px(11.0)).text_color(rgb(theme::muted())).child(n)))
            .when(empty, |el| el.child(div().px(px(12.0)).py(px(8.0)).text_color(rgb(theme::muted())).child("No matches")))
            .when(!empty, |el| el.child(list));
        let backdrop = div()
            .id("new-tab-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_new_tab_menu(window, cx)))
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(|this, _, window, cx| this.close_new_tab_menu(window, cx)));
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(popover).into_any_element())
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
            agent: None,
        }
    }

    fn row(focused: Option<&str>, blocks: &[BlockInfo]) -> Command {
        TAB_KINDS[same_kind(focused, blocks)].command
    }

    const BROWSER: usize = 1;
    const AGENT: usize = 2;

    /// The commands of `query`'s rows, in order.
    fn commands(query: &str) -> Vec<Command> {
        rows(query).iter().map(Row::command).collect()
    }

    #[test]
    fn new_tab_opens_the_focused_kind() {
        let blocks = [block("t", BlockKind::Terminal), block("b", BlockKind::Browser), block("a", BlockKind::Agent), block("u", BlockKind::Unknown)];
        assert_eq!(row(Some("t"), &blocks), Command::NewTerminalTab);
        assert_eq!(row(Some("b"), &blocks), Command::NewBrowserTab);
        assert_eq!(row(Some("a"), &blocks), Command::NewAgentTab);
        assert_eq!(row(None, &blocks), Command::NewTerminalTab, "nothing focused");
        assert_eq!(row(Some("t"), &[]), Command::NewTerminalTab, "no window");
        assert_eq!(row(Some("u"), &blocks), Command::NewTerminalTab, "a kind no tab starts with");
    }

    #[test]
    fn every_kind_is_a_row_opening_it() {
        assert_eq!(TAB_KINDS[TERMINAL].kind, BlockKind::Terminal);
        assert_eq!(TAB_KINDS[BROWSER].kind, BlockKind::Browser);
        assert_eq!(TAB_KINDS[AGENT].kind, BlockKind::Agent);
        let blocks: Vec<BlockInfo> = TAB_KINDS.iter().map(|t| block(t.label, t.kind)).collect();
        for (index, t) in TAB_KINDS.iter().enumerate() {
            // ⌘T on a block of this kind and ↩ on the preselected row
            // both run the row's command.
            let same = same_kind(Some(t.label), &blocks);
            assert_eq!(same, index, "{}", t.label);
            let all = rows("");
            assert_eq!(all[preselect("", &all, same)].command(), t.command);
            assert_ne!(t.command, Command::NewTab);
            assert_eq!(TAB_KINDS.iter().filter(|o| o.kind == t.kind).count(), 1);
        }
    }

    #[test]
    fn rows_show_true_shortcuts() {
        assert_eq!(row_hint(BROWSER, TERMINAL).as_deref(), Some("⌥⌘T"));
        assert_eq!(row_hint(BROWSER, BROWSER).as_deref(), Some("⌥⌘T"));
        assert_eq!(row_hint(TERMINAL, TERMINAL).as_deref(), Some("⌘T"), "⌘T opens a terminal here");
        assert_eq!(row_hint(TERMINAL, BROWSER), None, "⌘T opens a browser here");
    }

    #[test]
    fn typing_filters_and_selects_the_best_match() {
        assert_eq!(rows(""), vec![Row::Kind(TERMINAL, Vec::new()), Row::Kind(BROWSER, Vec::new()), Row::Kind(AGENT, Vec::new())]);
        assert_eq!(commands("age"), vec![Command::NewAgentTab]);
        #[allow(clippy::single_range_in_vec_init, reason = "one highlighted run")]
        let highlighted = vec![0..2];
        assert_eq!(rows("br"), vec![Row::Kind(BROWSER, highlighted)], "matched range highlighted");
        assert_eq!(commands("term"), vec![Command::NewTerminalTab]);
        assert_eq!(commands("  BRO "), vec![Command::NewBrowserTab], "trimmed, any case");
        // Both match: best first, and typing selects it whatever was preselected.
        let both = rows("r");
        assert_eq!(both.len(), 2);
        let score = |r: &Row| match r {
            Row::Kind(index, _) => fuzzy("r", TAB_KINDS[*index].label).unwrap().0,
            Row::Open(_) => unreachable!(),
        };
        assert!(score(&both[0]) >= score(&both[1]));
        assert_eq!(preselect("r", &both, TERMINAL), 0);
        assert_eq!(preselect("r", &both, BROWSER), 0);
    }

    #[test]
    fn nothing_matching_is_empty() {
        assert!(rows("zzz").is_empty());
        assert!(rows("terminalx").is_empty());
    }

    #[test]
    fn an_address_offers_a_browser_tab() {
        let found = rows("example.com/a");
        assert!(matches!(found.as_slice(), [Row::Open(url)] if url.contains("example.com/a")), "{found:?}");
        assert_eq!(found[0].command(), Command::NewBrowserTab, "gated like a browser tab");
        assert!(!rows("br").iter().any(|r| matches!(r, Row::Open(_))), "a word is not an address");
    }
}
