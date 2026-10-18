//! Session switcher popover (opened from the titlebar pill): filter or
//! create, sessions grouped by host, new session, add remote host.

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, Subscription, Window, div, prelude::*, px, rgb,
};

use crate::theme::Continuous as _;
use crate::{
    add_host::AddHostSheet,
    assets::icon,
    hosts::HostState,
    text_field::{FieldEvent, TextField},
    theme,
    titlebar::TRAFFIC_LIGHTS,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

pub struct Switcher {
    pub field: Entity<TextField>,
    pub filter: String,
    pub selected: usize,
    /// Renaming this session instead of filtering.
    pub renaming: Option<String>,
    _sub: Subscription,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Session { host: usize, id: String },
    Create(String),
    Connect(usize),
    NewSession,
    AddHost,
}

enum Row {
    Header { host: usize },
    Item(Item),
    Divider,
}

impl Workspace {
    pub(crate) fn toggle_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() {
            self.close_switcher(window, cx);
        } else {
            self.open_switcher(None, window, cx);
        }
    }

    pub(crate) fn open_switcher_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = &self.view else { return };
        let (id, name) = (view.session.id.clone(), view.session.name.clone());
        self.open_switcher(Some((id, name)), window, cx);
    }

    fn open_switcher(&mut self, rename: Option<(String, String)>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        let placeholder = if rename.is_some() { "Rename session" } else { "Filter or create…" };
        let field = cx.new(|cx| TextField::new(placeholder, cx));
        if let Some((_, name)) = &rename {
            field.update(cx, |f, cx| f.set_text(name.clone(), cx));
        }
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.field_event(event, window, cx));
        let focus = field.read(cx).focus.clone();
        let filter = rename.as_ref().map(|r| r.1.clone()).unwrap_or_default();
        self.switcher = Some(Switcher { field, filter, selected: 0, renaming: rename.map(|r| r.0), _sub: sub });
        self.select_current();
        // Deferred: a mouse-down that opened us may still focus its target.
        focus.focus(window, cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn close_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.take().is_some() {
            self.refocus_terminal(window, cx);
            cx.notify();
        }
    }

    /// Highlight the current session.
    fn select_current(&mut self) {
        let current = self.view.as_ref().map(|v| v.session.id.clone());
        let host = self.active_host;
        let items = self.items();
        if let Some(switcher) = self.switcher.as_mut() {
            switcher.selected = items
                .iter()
                .position(|i| matches!(i, Item::Session { host: h, id } if *h == host && Some(id) == current.as_ref()))
                .unwrap_or(0);
        }
    }

    fn rows(&self) -> Vec<Row> {
        let filter = self.switcher.as_ref().map(|s| s.filter.trim().to_lowercase()).unwrap_or_default();
        let renaming = self.switcher.as_ref().is_some_and(|s| s.renaming.is_some());
        let mut rows = Vec::new();
        if renaming {
            return rows;
        }
        let mut exact = false;
        for (index, host) in self.hosts.iter().enumerate() {
            if host.removed {
                continue;
            }
            rows.push(Row::Header { host: index });
            if host.state == HostState::Connected {
                for s in &host.sessions {
                    if s.name.to_lowercase() == filter {
                        exact = true;
                    }
                    if filter.is_empty() || s.name.to_lowercase().contains(&filter) {
                        rows.push(Row::Item(Item::Session { host: index, id: s.id.clone() }));
                    }
                }
            } else if !host.info.local {
                rows.push(Row::Item(Item::Connect(index)));
            }
        }
        let text = self.switcher.as_ref().map(|s| s.filter.trim().to_owned()).unwrap_or_default();
        if !text.is_empty() && !exact {
            rows.push(Row::Divider);
            rows.push(Row::Item(Item::Create(text)));
        }
        rows.push(Row::Divider);
        rows.push(Row::Item(Item::NewSession));
        rows.push(Row::Divider);
        rows.push(Row::Item(Item::AddHost));
        rows
    }

    fn items(&self) -> Vec<Item> {
        self.rows().into_iter().filter_map(|r| if let Row::Item(i) = r { Some(i) } else { None }).collect()
    }

    /// Visible session rows in order; ⌘1–9 address the first nine.
    fn session_rows(&self) -> Vec<(usize, String)> {
        self.items()
            .into_iter()
            .filter_map(|i| if let Item::Session { host, id } = i { Some((host, id)) } else { None })
            .collect()
    }

    pub(crate) fn switch_to_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.as_ref().is_some_and(|s| s.renaming.is_some()) {
            return;
        }
        if let Some((host, id)) = self.session_rows().get(index).cloned() {
            self.activate(Item::Session { host, id }, window, cx);
        }
    }

    fn field_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.items().len();
        let Some(switcher) = self.switcher.as_mut() else { return };
        match event {
            FieldEvent::Changed(text) => {
                switcher.filter = text.clone();
                if switcher.renaming.is_none() {
                    // Prefer the first match; else the "create" row.
                    let filter = text.trim().to_lowercase();
                    let items = self.items();
                    let first = items
                        .iter()
                        .position(|i| match i {
                            Item::Session { .. } => !filter.is_empty(),
                            Item::Create(_) => true,
                            _ => false,
                        })
                        .unwrap_or(0);
                    if let Some(s) = self.switcher.as_mut() {
                        s.selected = first;
                    }
                    if filter.is_empty() {
                        self.select_current();
                    }
                }
            }
            FieldEvent::Up => switcher.selected = (switcher.selected + count.max(1) - 1) % count.max(1),
            FieldEvent::Down => switcher.selected = (switcher.selected + 1) % count.max(1),
            FieldEvent::Cancel => {
                self.close_switcher(window, cx);
                return;
            }
            FieldEvent::Submit => {
                if let Some(id) = switcher.renaming.clone() {
                    let name = switcher.filter.trim().to_owned();
                    if !name.is_empty() {
                        self.rename_session(id, name);
                    }
                    self.close_switcher(window, cx);
                    return;
                }
                let item = self.items().get(switcher_selected(self)).cloned();
                if let Some(item) = item {
                    self.activate(item, window, cx);
                }
                return;
            }
        }
        cx.notify();
    }

    fn activate(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        self.close_switcher(window, cx);
        match item {
            Item::Session { host, id } => self.open_session(host, id, window, cx),
            Item::Create(name) => self.create_session(Some(name), window, cx),
            Item::NewSession => self.create_session(None, window, cx),
            Item::AddHost => {
                self.add_host = Some(AddHostSheet::new(window, cx));
            }
            Item::Connect(host) => {
                let reason = match &self.hosts[host].state {
                    HostState::Unreachable(reason) => reason.clone(),
                    _ => "connecting…".into(),
                };
                self.status = Some(format!("{}: {reason}", self.hosts[host].info.name));
            }
        }
        cx.notify();
    }

    fn start_rename(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.hosts[self.active_host].sessions.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
        self.switcher = None;
        self.open_switcher(Some((id, name)), window, cx);
    }

    pub(crate) fn render_switcher(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let switcher = self.switcher.as_ref()?;
        let current = self.view.as_ref().map(|v| v.session.id.clone());
        let numbered = self.session_rows();
        let mut list = div().flex().flex_col().py(px(4.0));
        let mut index = 0usize;
        for row in self.rows() {
            match row {
                Row::Header { host } => {
                    let entry = &self.hosts[host];
                    let state = match &entry.state {
                        HostState::Connected => None,
                        HostState::Connecting => Some("connecting…".to_owned()),
                        HostState::Unreachable(_) if !entry.info.local => Some("not connected".to_owned()),
                        HostState::Unreachable(_) => Some("unavailable".to_owned()),
                    };
                    list = list.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(12.0))
                            .pt(px(6.0))
                            .pb(px(3.0))
                            .text_size(px(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::muted()))
                            .child(icon(if entry.info.local { "laptop" } else { "globe" }, 12.0, theme::muted()))
                            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(entry.info.name.clone()))
                            .children(state.map(|s| div().flex_none().text_size(px(10.5)).font_weight(FontWeight::NORMAL).child(s))),
                    );
                }
                Row::Divider => {
                    list = list.child(div().h(px(1.0)).mx(px(8.0)).my(px(4.0)).bg(rgb(theme::border())));
                }
                Row::Item(item) => {
                    let selected = index == switcher.selected;
                    let this_index = index;
                    index += 1;
                    let number = match &item {
                        Item::Session { host, id } => numbered.iter().position(|(h, i)| h == host && i == id).filter(|n| *n < 9),
                        _ => None,
                    };
                    list = list.child(self.render_item(item, selected, this_index, number, current.as_deref(), cx));
                }
            }
        }
        let renaming = switcher.renaming.is_some();
        let popover = div()
            .id("switcher")
            .occlude()
            .absolute()
            .top(px(TITLEBAR_HEIGHT - 6.0))
            .left(px(TRAFFIC_LIGHTS - 4.0))
            .w(px(280.0))
            .max_h(px((f32::from(window.viewport_size().height) - TITLEBAR_HEIGHT - 16.0).max(160.0)))
            .overflow_y_scroll()
            .continuous(10.0)
            .bg(rgb(theme::popover()))
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_lg()
            .font_family(theme::ui_font())
            .text_size(px(12.5))
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
                    .child(icon("filter", 13.0, theme::muted()))
                    .child(switcher.field.clone()),
            )
            .when(renaming, |el| {
                el.child(div().px(px(12.0)).py(px(6.0)).text_size(px(11.0)).text_color(rgb(theme::muted())).child("Enter to rename · Esc to cancel"))
            })
            .when(!renaming, |el| el.child(list));
        let backdrop = div()
            .id("switcher-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_switcher(window, cx)))
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(|this, _, window, cx| this.close_switcher(window, cx)));
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(popover).into_any_element())
    }

    fn render_item(&self, item: Item, selected: bool, index: usize, number: Option<usize>, current: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let (glyph, label, hint, checked): (Option<&'static str>, String, Option<String>, bool) = match &item {
            Item::Session { host, id } => {
                let sessions = &self.hosts[*host].sessions;
                let name = sessions.iter().find(|s| &s.id == id).map(|s| s.name.clone()).unwrap_or_default();
                let checked = *host == self.active_host && current == Some(id.as_str());
                let hint = number.map(|n| format!("⌘{}", n + 1));
                (None, name, hint, checked)
            }
            Item::Create(name) => (Some("plus"), format!("Create “{name}”"), Some("↩".into()), false),
            Item::Connect(_) => (Some("globe"), "Connect (remote hosts coming soon)".into(), None, false),
            Item::NewSession => (Some("new-session"), "New Session".into(), crate::commands::Command::NewSession.hint(), false),
            Item::AddHost => (Some("add-host"), "Add Remote Host…".into(), None, false),
        };
        let is_session = matches!(item, Item::Session { .. });
        let rename_id = if let Item::Session { id, .. } = &item { Some(id.clone()) } else { None };
        let muted = matches!(item, Item::Connect(_));
        div()
            .id(("switcher-item", index))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(26.0))
            .mx(px(4.0))
            .px(px(8.0))
            .continuous(crate::radius::MENU_ROW)
            .when(selected, |el| el.bg(rgb(theme::row_active())))
            .when(!selected, |el| el.hover(|s| s.bg(rgb(theme::tab_hover()))))
            .cursor_pointer()
            .child(div().w(px(14.0)).flex_none().flex().justify_center().map(|el| {
                if is_session {
                    el.when(checked, |el| el.child(icon("check", 12.0, theme::foreground())))
                } else {
                    el.children(glyph.map(|g| icon(g, 13.0, theme::muted())))
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .when(checked, |el| el.font_weight(FontWeight::MEDIUM))
                    .text_color(rgb(if muted { theme::muted() } else { theme::foreground() }))
                    .child(label),
            )
            .children(hint.map(|h| div().flex_none().text_size(px(11.0)).text_color(rgb(theme::muted())).child(h)))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if let Some(s) = this.switcher.as_mut()
                    && s.selected != index
                {
                    s.selected = index;
                    cx.notify();
                }
            }))
            // Right-click renames (a click switches immediately, so a
            // double-click could not).
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    if let Some(id) = rename_id.clone() {
                        cx.stop_propagation();
                        this.start_rename(id, window, cx);
                    }
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.activate(item.clone(), window, cx)))
            .into_any_element()
    }
}

fn switcher_selected(ws: &Workspace) -> usize {
    ws.switcher.as_ref().map_or(0, |s| s.selected)
}
