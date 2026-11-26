//! Setup (docs/STUDIO.md §4.4), shown before `studio.toml` exists: the studio's name (the screen's one large
//! field, focused as soon as the pane is; ↩ starts with the agent), its default format as aspect tiles, Start
//! empty (`studio.init`) and Start with the agent (`studio.init`, then the `studio` agent split off the block,
//! linked, asked to begin the onboarding). Also the pane's one-line dialogs.

use gpui::{AnyElement, AppContext as _, Context, Entity, Window, div, prelude::*, px, rgb};
use silo_proto::{
    control::Method,
    studio::{Project, StudioInitParams},
};

use super::{Dialog, StudioPane, StudioPaneEvent, model, store};
use crate::{
    studio_block::Then,
    text_field::{FieldEvent, TextField},
    theme::{self, Continuous as _},
};

pub(crate) struct Setup {
    pub name: Entity<TextField>,
    pub format: String,
    pub busy: bool,
    _submit: gpui::Subscription,
}

impl Setup {
    pub fn new(cx: &mut Context<StudioPane>) -> Self {
        let name = cx.new(|cx| TextField::new("Name your studio", cx));
        // ↩ in the name is the primary button.
        let submit = cx.subscribe(&name, |this, _, e: &FieldEvent, cx| {
            if matches!(e, FieldEvent::Submit) {
                this.start(true, cx);
            }
        });
        Self { name, format: "landscape".into(), busy: false, _submit: submit }
    }
}

/// An aspect tile's face, at most this tall or wide.
const TILE: f32 = 56.0;
/// The setup column: the name field's width, and the tiles' row.
const COLUMN: f32 = 520.0;

/// A preset's name as a title (`vertical` → `Vertical`).
fn titled(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

impl StudioPane {
    fn start(&mut self, agent: bool, cx: &mut Context<Self>) {
        if self.setup.busy {
            return;
        }
        let typed = self.setup.name.read(cx).text.trim().to_owned();
        let name = if typed.is_empty() { crate::files::model::name(&self.state.root).to_owned() } else { typed };
        self.setup.busy = true;
        let method = Method::StudioInit(StudioInitParams { block_id: self.block_id.clone(), name, format: Some(self.setup.format.clone()) });
        self.caller(cx).call(method, cx, move |this: &mut Self, r: anyhow::Result<Project>, cx| {
            this.setup.busy = false;
            match r {
                Ok(p) => {
                    this.store.update(cx, |s, cx| {
                        s.took_project(p, cx);
                        s.load_world(cx);
                    });
                    this.set(
                        silo_proto::studio::StudioSetParams { view: Some(silo_proto::studio::StudioView::Production), ..Default::default() },
                        cx,
                    );
                    if agent {
                        cx.emit(StudioPaneEvent::Agent(Then::Send(model::ONBOARDING.to_owned())));
                    }
                }
                Err(e) => this.notice = Some(store::describe(&e)),
            }
        });
        cx.notify();
    }

    pub(super) fn render_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let name_focus = self.setup.name.read(cx).focus.clone();
        // The name is what the screen is for: the pane's focus goes to it (a click on the pane's body too).
        if self.dialog.is_none() && self.focus.is_focused(window) {
            name_focus.focus(window, cx);
        }
        let naming = name_focus.is_focused(window);
        let folder = crate::files::model::name(&self.state.root).to_owned();
        let tiles = model::INIT_PRESETS.iter().map(|(name, aspect, what)| {
            let (w, h) = model::fit(model::aspect_of(aspect), TILE);
            let on = self.setup.format == *name;
            let pick = (*name).to_owned();
            div()
                .id(gpui::ElementId::Name(format!("studio-setup-{name}").into()))
                .flex_1()
                .min_w(px(104.0))
                .h(px(148.0))
                .px(px(8.0))
                .py(px(12.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(3.0))
                .continuous(crate::radius::CARD)
                .cursor_pointer()
                .border_1()
                .border_color(if on { rgb(theme::accent()) } else { theme::control_fill() })
                .when(on, |el| el.bg(theme::row_fill()))
                .hover(|s| s.bg(theme::row_fill()))
                .child(
                    div().h(px(TILE)).mb(px(8.0)).flex().items_center().child(
                        div()
                            .w(px(w))
                            .h(px(h))
                            .continuous(4.0)
                            .border_1()
                            .border_color(rgb(if on { theme::accent() } else { muted }))
                            .bg(theme::well_fill()),
                    ),
                )
                .child(div().text_size(px(13.0)).font_weight(gpui::FontWeight::MEDIUM).whitespace_nowrap().child(titled(name)))
                .child(div().text_size(px(12.0)).text_color(rgb(muted)).whitespace_nowrap().child(*aspect))
                .child(div().w_full().text_center().text_size(px(11.0)).text_color(rgb(muted)).child(*what))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.setup.format.clone_from(&pick);
                    cx.notify();
                }))
        });
        let busy = self.setup.busy;
        let button = |id: &'static str, label: &'static str, primary: bool| {
            crate::confirm_card::button(id, label, primary, false)
                .h(px(34.0))
                .px(px(18.0))
                .when(!busy, |el| el.cursor_pointer())
                .when(busy, |el| el.opacity(0.6))
        };
        let field = div()
            .id("studio-setup-name")
            .w_full()
            .h(px(64.0))
            .px(px(20.0))
            .flex()
            .items_center()
            .continuous(14.0)
            .bg(theme::well_fill())
            .border_1()
            .border_color(if naming { rgb(theme::accent()) } else { theme::control_fill() })
            .text_size(px(26.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .cursor_text()
            .on_click({
                let focus = name_focus.clone();
                move |_, window, cx| focus.focus(window, cx)
            })
            .child(self.setup.name.clone());
        let label = |text: &'static str| div().w_full().text_size(px(12.0)).text_color(rgb(muted)).child(text);
        div()
            .id("studio-setup")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(COLUMN))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.0))
                    .child(label("New studio"))
                    .child(field)
                    .when(!folder.is_empty(), |el| {
                        el.child(
                            div()
                                .w_full()
                                .text_size(px(12.0))
                                .text_color(rgb(muted))
                                .child(format!("Leave it empty to use the folder's name, {folder}.")),
                        )
                    })
                    .child(div().h(px(10.0)))
                    .child(label("Default format"))
                    .child(div().w_full().flex().flex_wrap().gap(px(10.0)).children(tiles))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .pt(px(14.0))
                            .child(button("studio-start-empty", "Start empty", false).on_click(cx.listener(|this, _, _, cx| this.start(false, cx))))
                            .child(
                                button("studio-start-agent", "Start with the agent  \u{21a9}", true)
                                    .on_click(cx.listener(|this, _, _, cx| this.start(true, cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The open dialog: a card over the body with its question and field.
    pub(super) fn render_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (dialog, _) = self.dialog.as_ref()?;
        let (title, field) = match dialog {
            Dialog::NewCanvas { field, .. } => ("New canvas".to_owned(), field.clone()),
            Dialog::Rename { field, .. } => ("Rename canvas".to_owned(), field.clone()),
            Dialog::NewEntity { section, field, agent } => {
                let one = model::singular(&self.section_title(section, cx));
                (if *agent { format!("Create a {one} with the agent") } else { format!("New {one}") }, field.clone())
            }
            Dialog::NewSection { field } => ("New section".to_owned(), field.clone()),
            Dialog::Ref { entity, look, defines, field } => {
                let name = self.look_of(entity, look, cx).map_or_else(|| look.clone(), |l| model::ref_caption(&l).0);
                (if *defines { format!("What \u{201c}{name}\u{201d} defines") } else { format!("Rename \u{201c}{name}\u{201d}") }, field.clone())
            }
        };
        let extra = match dialog {
            Dialog::NewCanvas { episode, format, .. } => Some(self.canvas_options(*episode, format.clone(), cx)),
            _ => None,
        };
        let _glass = crate::surface::enter();
        let card = crate::confirm_card::card("studio-dialog")
            .child(crate::confirm_card::headline(title))
            .child(div().w(px(300.0)).child(field))
            .children(extra)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .pt(px(6.0))
                    .child(
                        crate::confirm_card::button("studio-dialog-cancel", "Cancel", false, false)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| this.close_dialog(window, cx))),
                    )
                    .child(
                        crate::confirm_card::button("studio-dialog-ok", "OK", true, false)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| this.submit_dialog(window, cx))),
                    ),
            );
        Some(crate::confirm_card::backdrop("studio-dialog-backdrop", card, cx.listener(|this, _, window, cx| this.close_dialog(window, cx))))
    }

    /// New canvas's episode toggle and format choice.
    fn canvas_options(&self, episode: Option<u32>, format: Option<String>, cx: &mut Context<Self>) -> AnyElement {
        let project = self.store.read(cx).project().cloned();
        let next = project.as_ref().map_or(1, model::next_episode);
        let chip = |id: String, label: String, on: bool| {
            div()
                .id(gpui::ElementId::Name(id.into()))
                .px(px(8.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .continuous(6.0)
                .text_size(px(12.0))
                .cursor_pointer()
                .when(on, |el| el.bg(theme::row_fill()))
                .when(!on, |el| el.text_color(rgb(theme::muted())).hover(|s| s.bg(theme::control_fill())))
                .child(label)
        };
        let set = |episode: Option<u32>, format: Option<String>| {
            cx.listener(move |this: &mut Self, _: &gpui::ClickEvent, _, cx| {
                if let Some((Dialog::NewCanvas { episode: e, format: f, .. }, _)) = this.dialog.as_mut() {
                    *e = episode;
                    f.clone_from(&format);
                    cx.notify();
                }
            })
        };
        let mut episodes = div()
            .flex()
            .gap(px(4.0))
            .child(chip("studio-ep-none".into(), "Not an episode".into(), episode.is_none()).on_click(set(None, format.clone())));
        episodes =
            episodes.child(chip("studio-ep-next".into(), format!("Episode {next}"), episode.is_some()).on_click(set(Some(next), format.clone())));
        let mut formats = div()
            .flex()
            .flex_wrap()
            .gap(px(4.0))
            .child(chip("studio-fmt-default".into(), "Project format".into(), format.is_none()).on_click(set(episode, None)));
        for (name, f) in project.iter().flat_map(|p| &p.formats) {
            let on = format.as_deref() == Some(name.as_str());
            formats = formats.child(chip(format!("studio-fmt-{name}"), model::preset_label(name, f), on).on_click(set(episode, Some(name.clone()))));
        }
        div().flex().flex_col().gap(px(6.0)).child(episodes).child(formats).into_any_element()
    }
}
