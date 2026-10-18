//! "Add Remote Host…" sheet: name + address/ticket, saved to hosts.json.

use gpui::{AnyElement, Context, Entity, FontWeight, IntoElement, Subscription, Window, div, prelude::*, px, rgb};

use crate::theme::Continuous as _;
use crate::{
    hosts::{self, HostInfo, RemoteHost},
    text_field::{FieldEvent, TextField},
    theme,
    workspace::Workspace,
};

pub struct AddHostSheet {
    name: Entity<TextField>,
    address: Entity<TextField>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl AddHostSheet {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let name = cx.new(|cx| TextField::new("e.g. studio-mini", cx));
        let address = cx.new(|cx| TextField::new("iroh ticket, node id or host", cx));
        let subs = vec![
            cx.subscribe_in(&name, window, |this, _, event: &FieldEvent, window, cx| this.sheet_event(0, event, window, cx)),
            cx.subscribe_in(&address, window, |this, _, event: &FieldEvent, window, cx| this.sheet_event(1, event, window, cx)),
        ];
        let focus = name.read(cx).focus.clone();
        focus.focus(window, cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        Self { name, address, error: None, _subs: subs }
    }
}

impl Workspace {
    fn sheet_event(&mut self, field: usize, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = &self.add_host else { return };
        match event {
            FieldEvent::Cancel => self.close_add_host(window, cx),
            FieldEvent::Down | FieldEvent::Up => {
                let next = if field == 0 { &sheet.address } else { &sheet.name };
                let focus = next.read(cx).focus.clone();
                focus.focus(window, cx);
            }
            FieldEvent::Submit if field == 0 => {
                let focus = sheet.address.read(cx).focus.clone();
                focus.focus(window, cx);
            }
            FieldEvent::Submit => self.save_add_host(window, cx),
            FieldEvent::Changed(_) => {}
        }
        cx.notify();
    }

    fn close_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_host = None;
        self.refocus_terminal(window, cx);
        cx.notify();
    }

    fn save_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = &self.add_host else { return };
        let name = sheet.name.read(cx).text.trim().to_owned();
        let address = sheet.address.read(cx).text.trim().to_owned();
        match self.save_host(name, address, window, cx) {
            Ok(()) => self.close_add_host(window, cx),
            Err(error) => {
                if let Some(sheet) = &mut self.add_host {
                    sheet.error = Some(error);
                }
                cx.notify();
            }
        }
    }

    /// Validate, persist to hosts.json and connect a new remote host.
    pub(crate) fn save_host(&mut self, name: String, address: String, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        if name.is_empty() || address.is_empty() {
            return Err("Enter a name and an address.".into());
        }
        let mut saved = hosts::load(&self.home);
        if saved.iter().any(|h| h.name == name) {
            return Err(format!("A host named “{name}” already exists."));
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let host = RemoteHost { id: format!("h{:x}", nanos & 0xffff_ffff_ffff), name, address };
        saved.push(host.clone());
        hosts::save(&self.home, &saved).map_err(|e| format!("Could not save: {e:#}"))?;
        self.add_host_entry(HostInfo::remote(&host), window, cx);
        Ok(())
    }

    /// Forget a remote host (hosts.json and the switcher).
    pub(crate) fn remove_host(&mut self, index: usize) -> Result<String, String> {
        let entry = self.hosts.get_mut(index).ok_or("no such host")?;
        if entry.info.local {
            return Err("This Mac cannot be removed.".into());
        }
        let name = entry.info.name.clone();
        let saved: Vec<RemoteHost> = hosts::load(&self.home).into_iter().filter(|h| h.name != name).collect();
        hosts::save(&self.home, &saved).map_err(|e| format!("Could not save: {e:#}"))?;
        entry.removed = true;
        entry.conn = None;
        entry.control = None;
        Ok(name)
    }
    pub(crate) fn render_add_host(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.add_host.as_ref()?;
        let field = |label: &'static str, input: &Entity<TextField>| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(label))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(34.0))
                        .px(px(10.0))
                        .continuous(8.0)
                        .bg(rgb(theme::background()))
                        .border_1()
                        .border_color(rgb(theme::border()))
                        .child(input.clone()),
                )
        };
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px(px(14.0))
                .py(px(6.0))
                .continuous(8.0)
                .text_size(px(13.0))
                .cursor_pointer()
                .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())))
                .when(!primary, |el| el.bg(rgb(theme::tab_active())).hover(|s| s.bg(rgb(theme::row_active()))))
                .child(label)
        };
        let card = div()
            .id("add-host")
            .occlude()
            .w(px(420.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .p(px(20.0))
            .continuous(14.0)
            .bg(rgb(theme::popover()))
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .font_family(theme::ui_font())
            .text_size(px(13.0))
            .child(div().text_size(px(15.0)).font_weight(FontWeight::BOLD).child("Add Remote Host"))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::muted()))
                    .child("Hosts are saved to hosts.json. Connecting to remote servers (iroh) is not available yet."),
            )
            .child(field("Name", &sheet.name))
            .child(field("Address or ticket", &sheet.address))
            .children(sheet.error.clone().map(|e| div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(e)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(button("add-host-cancel", "Cancel", false).on_click(cx.listener(|this, _, window, cx| this.close_add_host(window, cx))))
                    .child(button("add-host-save", "Add Host", true).on_click(cx.listener(|this, _, window, cx| this.save_add_host(window, cx)))),
            );
        Some(
            div()
                .id("add-host-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(90.0))
                .bg(gpui::rgba(0x0000_0088))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_add_host(window, cx)))
                .child(card)
                .into_any_element(),
        )
    }
}
