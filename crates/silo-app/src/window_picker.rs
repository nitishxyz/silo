//! ⌘N window picker: choose (or create) a session to open in a new window.
//! A session already shown in another window focuses that window instead.

use gpui::{AnyElement, AnyWindowHandle, App, Context, Entity, FontWeight, IntoElement, Subscription, Window, div, prelude::*, px, rgb};
use silo_proto::control::{Created, Method, SpawnSpec};
use silo_proto::model::BlockKind;

use crate::surface::Glassy as _;
use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    commands::Command,
    switcher::{older, state_color, state_label},
    text_field::{FieldEvent, TextField},
    theme,
    workspace::Workspace,
};

pub struct WindowPicker {
    field: Entity<TextField>,
    filter: String,
    selected: usize,
    /// The other windows' sessions, read when the picker opened: never
    /// while drawing (`other_windows`).
    others: Vec<(usize, String, AnyWindowHandle)>,
    _sub: Subscription,
}

#[derive(Clone, Debug, PartialEq)]
enum Item {
    Session { host: usize, id: String },
    Create(String),
    NewSession,
}

enum Row {
    Header(usize),
    Item(Item),
    Divider,
}

/// Where a session is shown: this window, or another one.
#[derive(Clone, Copy)]
enum Shown {
    Here,
    Elsewhere(AnyWindowHandle),
}

/// Commands that still work while the picker is open.
pub(crate) fn allowed_while_open(command: Command) -> bool {
    matches!(
        command,
        Command::NewWindow
            | Command::Quit
            | Command::HideSilo
            | Command::CloseWindow
            | Command::HideOthers
            | Command::ShowAll
            | Command::ToggleFullScreen
            | Command::CycleWindows
    )
}

/// ⌘`: bring the next Silo window forward.
pub(crate) fn cycle_windows(window: &Window, cx: &mut App) {
    let windows: Vec<AnyWindowHandle> = cx.windows().into_iter().filter(|w| w.downcast::<Workspace>().is_some()).collect();
    let current = window.window_handle().window_id();
    let Some(pos) = windows.iter().position(|w| w.window_id() == current) else { return };
    let next = windows[(pos + 1) % windows.len()];
    if next.window_id() != current {
        cx.defer(move |cx| {
            let _ = next.update(cx, |_, window, _| window.activate_window());
        });
    }
}

/// Open `id` of `host` in a new window (deferred: not inside an update).
fn open_in_new_window(host: usize, id: String, cx: &mut App) {
    cx.defer(move |cx| {
        if let Err(error) = crate::open_window(Some((host, id)), cx) {
            tracing::warn!("open window: {error:#}");
        }
    });
}

impl Workspace {
    /// The session each other Silo window shows (host, session id), front
    /// to back. Not while drawing: an entity read in a window's draw becomes
    /// that window's in gpui (its callbacks get that `window`), and the
    /// other window's own requests then mistake which window is theirs.
    fn other_windows(&self, window: &Window, cx: &Context<Self>) -> Vec<(usize, String, AnyWindowHandle)> {
        self.window_sessions(window, cx).into_iter().filter_map(|(w, shown)| shown.and_then(|(host, id)| Some((host, id?, w)))).collect()
    }

    pub(crate) fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.close_picker(window, cx);
            return;
        }
        self.close_palette(window, cx);
        self.close_switcher(window, cx);
        let field = cx.new(|cx| TextField::new("Open or create a session…", cx).on_glass());
        let sub = cx.subscribe_in(&field, window, |this, _, event: &FieldEvent, window, cx| this.picker_event(event, window, cx));
        let focus = field.read(cx).focus.clone();
        let others = self.other_windows(window, cx);
        self.picker = Some(WindowPicker { field, filter: String::new(), selected: 0, others, _sub: sub });
        // Deferred: a mouse-down that opened us may still focus its target.
        focus.focus(window, cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            self.refocus_block(window, cx);
            cx.notify();
        }
    }

    fn picker_rows(&self) -> Vec<Row> {
        let text = self.picker.as_ref().map(|p| p.filter.trim().to_owned()).unwrap_or_default();
        let filter = text.to_lowercase();
        let mut rows = Vec::new();
        let mut exact = false;
        for (index, host) in self.hosts.iter().enumerate().filter(|(_, h)| !h.removed) {
            rows.push(Row::Header(index));
            for s in host.sessions.iter().filter(|s| s.name.to_lowercase().contains(&filter)) {
                exact |= index == self.active_host && s.name.to_lowercase() == filter;
                rows.push(Row::Item(Item::Session { host: index, id: s.id.clone() }));
            }
        }
        rows.push(Row::Divider);
        rows.push(Row::Item(if text.is_empty() || exact { Item::NewSession } else { Item::Create(text) }));
        rows
    }

    fn picker_items(&self) -> Vec<Item> {
        self.picker_rows().into_iter().filter_map(|r| if let Row::Item(i) = r { Some(i) } else { None }).collect()
    }

    fn picker_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.picker_items().len().max(1);
        let Some(picker) = self.picker.as_mut() else { return };
        match event {
            FieldEvent::Changed(text) => (picker.filter, picker.selected) = (text.clone(), 0),
            FieldEvent::Up => picker.selected = (picker.selected + count - 1) % count,
            FieldEvent::Down => picker.selected = (picker.selected + 1) % count,
            FieldEvent::Cancel => return self.close_picker(window, cx),
            FieldEvent::Submit => {
                let selected = picker.selected;
                if let Some(item) = self.picker_items().get(selected).cloned() {
                    self.choose(item, window, cx);
                }
                return;
            }
        }
        cx.notify();
    }

    fn choose(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        self.close_picker(window, cx);
        match item {
            Item::Session { host, id } => match self.shown(host, &id, window, cx) {
                Some(Shown::Here) => {}
                Some(Shown::Elsewhere(handle)) => crate::workspace::bring_forward(handle, None, cx),
                None => open_in_new_window(host, id, cx),
            },
            Item::Create(name) => self.create_in_new_window(Some(name), window, cx),
            Item::NewSession => self.create_in_new_window(None, window, cx),
        }
    }

    fn shown(&self, host: usize, id: &str, window: &Window, cx: &Context<Self>) -> Option<Shown> {
        self.shown_in(host, id, &self.other_windows(window, cx))
    }

    fn shown_in(&self, host: usize, id: &str, others: &[(usize, String, AnyWindowHandle)]) -> Option<Shown> {
        if host == self.active_host && self.view.as_ref().is_some_and(|v| v.session.id == id) {
            return Some(Shown::Here);
        }
        others.iter().find(|(h, i, _)| *h == host && i == id).map(|(_, _, w)| Shown::Elsewhere(*w))
    }

    /// Create a session on this window's host, then open it in a new window.
    fn create_in_new_window(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.active_host;
        let Some(control) = self.control() else {
            return self.flash(crate::status::Status::warning(format!("{} is not connected", self.hosts[host].info.name)), cx);
        };
        let cwd = self.new_block_folder(window, cx);
        let spawn = SpawnSpec { cwd, command: Vec::new(), size: None };
        let create = Method::SessionCreate { name, spawn, kind: BlockKind::Terminal, url: None };
        let task = self.rt.spawn(async move { control.call::<Created>(create).await });
        cx.spawn(async move |this, cx| match task.await {
            Ok(Ok(created)) => cx.update(|cx| open_in_new_window(host, created.session_id, cx)),
            Ok(Err(error)) => {
                tracing::warn!("create session: {error:#}");
                let _ = this.update(cx, |this, cx| this.flash(this.failed(&error, "Couldn't create the session"), cx));
            }
            Err(_) => {}
        })
        .detach();
    }

    pub(crate) fn render_picker(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let _glass = crate::surface::enter();
        let picker = self.picker.as_ref()?;
        let others = &picker.others;
        let mut list = div().flex().flex_col().py(px(4.0));
        let mut index = 0;
        for row in self.picker_rows() {
            list = match row {
                Row::Header(host) => list.child(self.picker_header(host)),
                Row::Divider => list.child(div().h(px(1.0)).mx(px(8.0)).my(px(4.0)).bg(rgb(theme::border()))),
                Row::Item(item) => {
                    let shown = if let Item::Session { host, id } = &item { self.shown_in(*host, id, others) } else { None };
                    index += 1;
                    list.child(self.picker_item(item, index - 1, index - 1 == picker.selected, shown, cx))
                }
            };
        }
        Some(self.picker_frame(picker, list, window, cx))
    }

    fn picker_header(&self, host: usize) -> AnyElement {
        let entry = &self.hosts[host];
        let state = state_label(&entry.state, entry.info.local).map(str::to_owned).or_else(|| older(entry));
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
            .children(state.map(|s| div().flex_none().text_size(px(10.5)).font_weight(FontWeight::NORMAL).child(s)))
            .child(div().flex_none().size(px(6.0)).rounded_full().bg(rgb(state_color(&entry.state))))
            .into_any_element()
    }

    fn picker_item(&self, item: Item, index: usize, selected: bool, shown: Option<Shown>, cx: &mut Context<Self>) -> AnyElement {
        let (glyph, label) = match &item {
            Item::Session { host, id } => {
                let name = self.hosts[*host].sessions.iter().find(|s| &s.id == id).map(|s| s.name.clone()).unwrap_or_default();
                (matches!(shown, Some(Shown::Here)).then_some("check"), name)
            }
            Item::Create(name) => (Some("plus"), format!("Create “{name}”")),
            Item::NewSession => (Some("new-session"), "New Session".into()),
        };
        let hint = match shown {
            Some(Shown::Here) => Some("This window"),
            Some(Shown::Elsewhere(_)) => Some("Open in a window"),
            None => None,
        };
        div()
            .id(("picker-item", index))
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
            .cursor_pointer()
            .child(div().w(px(14.0)).flex_none().flex().justify_center().children(glyph.map(|g| icon(g, 13.0, theme::muted()))))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(label))
            .children(hint.map(|h| div().flex_none().text_size(px(11.0)).text_color(rgb(theme::muted())).child(h)))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if let Some(p) = this.picker.as_mut()
                    && p.selected != index
                {
                    p.selected = index;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| this.choose(item.clone(), window, cx)))
            .into_any_element()
    }

    fn picker_frame(&self, picker: &WindowPicker, list: gpui::Div, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let max_h = (f32::from(window.viewport_size().height) - 140.0).max(160.0);
        let search = div().flex().flex_row().items_center().gap(px(8.0)).h(px(34.0)).px(px(12.0)).border_b_1().border_color(rgb(theme::border()));
        let popover = div()
            .id("window-picker")
            .occlude()
            .w(px(340.0))
            .max_h(px(max_h))
            .overflow_y_scroll()
            .glass(10.0)
            .font_family(theme::ui_font())
            .text_size(px(12.5))
            .child(search.child(icon("filter", 13.0, theme::muted())).child(picker.field.clone()))
            .child(list);
        let close = |this: &mut Self, _: &gpui::MouseDownEvent, window: &mut Window, cx: &mut Context<Self>| this.close_picker(window, cx);
        let backdrop = div()
            .id("window-picker-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(close))
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(close));
        let center = div().absolute().inset_0().flex().justify_center().items_start().pt(px(96.0)).child(popover);
        div().absolute().inset_0().child(backdrop).child(center).into_any_element()
    }
}
