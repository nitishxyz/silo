//! The Studio block's header (docs/STUDIO.md §4.1): the studio's name (a button: the Production view, §4.6), the canvas tabs (closing one keeps the
//! canvas), `+` (New canvas…, Rename, Open canvas), then the format chip (its menu: this canvas's format, the
//! project default, Edit formats…) and the World toggle.

use gpui::{AnyElement, ClickEvent, Context, div, prelude::*, px, rgb};
use silo_proto::{
    model::BlockInfo,
    studio::{StudioSetParams, StudioView},
};

use super::{MenuAction, StudioPane, model};
use crate::{
    context_menu::{Item, Menu},
    header_tabs::{self, Ids, Tab},
    pane_header::{BUTTON, BlockHeader, HeaderParts, HeaderStyle, button, keep_press},
    theme::Continuous as _,
};

const IDS: Ids = Ids { strip: "studio-tabs", tab: "studio-tab", close: "studio-tab-close" };

impl BlockHeader for StudioPane {
    fn header(&mut self, style: HeaderStyle, _: Option<&BlockInfo>, cx: &mut Context<Self>) -> HeaderParts {
        let project = self.store.read(cx).project().cloned();
        let folder = crate::files::model::name(&self.state.root).to_owned();
        let name = project.as_ref().map_or(folder, |p| p.name.clone());
        let label = if name.is_empty() { "Studio".to_owned() } else { format!("Studio \u{b7} {name}") };
        // The name at its own width, then the tabs at theirs: only the strip gives way (and scrolls) when the
        // header is too narrow. Two growing children would share the width evenly and clip the tabs.
        let mut middle = div().flex().flex_row().items_center().gap(px(6.0)).min_w_0().overflow_hidden().whitespace_nowrap();
        let production = super::production::shows_production(self.state.view, self.state.active.as_deref());
        let palette = style.palette;
        middle = middle.child(
            keep_press(
                div()
                    .id("studio-name")
                    .flex_none()
                    .flex()
                    .items_center()
                    .h(px(BUTTON))
                    .px(px(6.0))
                    .continuous(style.control)
                    .cursor_pointer()
                    .text_color(rgb(style.ink()))
                    .when(production, |el| el.bg(palette.selected()))
                    .when(!production, |el| el.hover(|s| s.bg(palette.hover())))
                    .child(name),
            )
            .tooltip(crate::tooltip::Tooltip::text("Production: where the studio stands and what is next"))
            .on_click(cx.listener(|this, _, _, cx| this.show_production(cx))),
        );
        let mut actions = Vec::new();
        if let Some(project) = &project {
            let tabs = model::tab_order(project, &self.state.open);
            let names: Vec<String> = tabs.iter().map(|c| c.name.clone()).collect();
            let canvas_shown = self.state.view == StudioView::Canvas;
            let strip_tabs = tabs
                .iter()
                .map(|c| Tab {
                    name: model::tab_label(c),
                    dir: None,
                    tooltip: c.name.clone(),
                    shown: canvas_shown && self.state.active.as_deref() == Some(c.name.as_str()),
                    dirty: false,
                    dot: false,
                })
                .collect::<Vec<_>>();
            if !strip_tabs.is_empty() {
                let entity = cx.entity().downgrade();
                let (open_names, close_names) = (names.clone(), names);
                let close_entity = entity.clone();
                let strip = header_tabs::strip(
                    IDS,
                    strip_tabs,
                    style,
                    move |i, _, cx| {
                        if let Some(name) = open_names.get(i).cloned() {
                            let _ = entity.update(cx, |this, cx| this.show_canvas(name, cx));
                        }
                    },
                    move |i, _, cx| {
                        if let Some(name) = close_names.get(i).cloned() {
                            let _ = close_entity.update(cx, |this, cx| this.close_tab(&name, cx));
                        }
                    },
                );
                // As wide as its tabs, shrinking before anything else.
                middle = middle.child(div().flex().flex_row().items_center().flex_shrink(1.0).min_w_0().child(strip));
            }
            middle = middle.child(
                keep_press(button("studio-plus", "plus", "New or open a canvas", style.palette.icon, true, style))
                    .on_click(cx.listener(|this, e: &ClickEvent, _, cx| this.plus_menu(e, cx))),
            );
            let active = self.state.active.as_deref().and_then(|a| project.canvases.iter().find(|c| c.name == a));
            if let Some(chip) = model::format_chip(project, active) {
                actions.push(self.format_chip(chip, style, cx));
            }
        }
        let world = self.state.view == StudioView::World;
        actions.push(
            keep_press(button("studio-world", "earth", if world { "Back to the canvas" } else { "World" }, style.palette.icon, true, style))
                .when(world, |el| el.bg(style.palette.selected()))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_world(cx)))
                .into_any_element(),
        );
        let mut parts = HeaderParts::new(middle.into_any_element(), label);
        parts.actions = actions;
        parts
    }
}

impl StudioPane {
    /// `[9:16 ▾]`: the active canvas's effective format; opens its menu.
    fn format_chip(&self, text: String, style: HeaderStyle, cx: &mut Context<Self>) -> AnyElement {
        let palette = style.palette;
        keep_press(
            div()
                .id("studio-format")
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.0))
                .h(px(BUTTON))
                .px(px(8.0))
                .mr(px(2.0))
                .continuous(style.control)
                .bg(palette.hover())
                .cursor_pointer()
                .hover(|s| s.bg(palette.selected()))
                .text_size(px(12.0))
                .text_color(rgb(style.ink()))
                .child(text)
                .child(crate::assets::icon("chevron-down", 10.0, palette.icon)),
        )
        .tooltip(crate::tooltip::Tooltip::text("Format"))
        .on_click(cx.listener(|this, e: &ClickEvent, _, cx| this.format_menu(e, cx)))
        .into_any_element()
    }

    fn format_menu(&mut self, e: &ClickEvent, cx: &mut Context<Self>) {
        let Some(project) = self.store.read(cx).project().cloned() else { return };
        let active = self.state.active.as_deref().and_then(|a| project.canvases.iter().find(|c| c.name == a)).cloned();
        let mut items = Vec::new();
        if let Some(canvas) = &active {
            let default = project.formats.get(&project.format).map_or_else(|| project.format.clone(), |f| f.aspect.clone());
            items.push(Item::action(format!("Project default ({default})"), "", MenuAction::CanvasFormat(None)).checked(canvas.format.is_none()));
            for (name, f) in &project.formats {
                let on = canvas.format.as_deref() == Some(name.as_str());
                items.push(Item::action(model::preset_label(name, f), "", MenuAction::CanvasFormat(Some(name.clone()))).checked(on));
            }
            items.push(Item::Separator);
        }
        for (name, f) in &project.formats {
            let label = format!("Project: {}", model::preset_label(name, f));
            items.push(Item::action(label, "", MenuAction::ProjectFormat(name.clone())).checked(project.format == *name));
        }
        items.push(Item::Separator);
        items.push(Item::action("Edit formats…", "", MenuAction::EditFormats));
        self.open_menu(Menu::new(e.position(), items), cx);
    }

    fn plus_menu(&mut self, e: &ClickEvent, cx: &mut Context<Self>) {
        let Some(project) = self.store.read(cx).project().cloned() else { return };
        let mut items = vec![Item::action("New canvas…", "", MenuAction::NewCanvas)];
        if let Some(active) = self.state.active.clone().filter(|_| self.state.view == StudioView::Canvas) {
            items.push(Item::action("Rename canvas…", "", MenuAction::RenameCanvas(active)));
        }
        let closed = model::closed(&project, &self.state.open);
        if !closed.is_empty() {
            items.push(Item::Separator);
            for c in closed {
                items.push(Item::action(format!("Open {}", model::tab_label(c)), "", MenuAction::OpenCanvas(c.name.clone())));
            }
        }
        self.open_menu(Menu::new(e.position(), items), cx);
    }

    /// Close tab `name` (the canvas stays on disk); the shown one passes to its neighbour.
    pub(crate) fn close_tab(&mut self, name: &str, cx: &mut Context<Self>) {
        let order: Vec<String> = self
            .store
            .read(cx)
            .project()
            .map(|p| model::tab_order(p, &self.state.open).into_iter().map(|c| c.name.clone()).collect())
            .unwrap_or_else(|| self.state.open.clone());
        let open: Vec<String> = self.state.open.iter().filter(|n| *n != name).cloned().collect();
        let mut params = StudioSetParams { open: Some(open), ..StudioSetParams::default() };
        if self.state.active.as_deref() == Some(name) {
            let at = order.iter().position(|n| n == name).unwrap_or(0);
            let next = order.iter().filter(|n| *n != name).nth(at.saturating_sub(1).min(order.len().saturating_sub(2))).cloned();
            params.active = Some(next);
        }
        self.canvases.remove(name);
        self.set(params, cx);
    }
}
