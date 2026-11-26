//! The Production view (docs/STUDIO.md §4.6): where the studio stands and what is next, all from
//! `studio.production`. Next up (the steps as buttons, the first primary), Assets (by section, a row each with
//! its sheet, state marks and how many references beyond the sheet; a prop that goes with one character also
//! under that character's row, indented), Episodes (each canvas's stages and counts, ▶ Play on its newest cut) and Models
//! (the project's Image, Video and Speech picks). The marks ([`marks`]) are shared with the World grid's cards
//! and the entity page's head; the Models menu ([`model_menu`]) with the entity page's model chips.

use std::collections::BTreeMap;

use gpui::{AnyElement, ClickEvent, Context, FontWeight, Window, div, prelude::*, px, rgb};
use otto_proto::{MediaKind, MediaModelInfo};
use silo_proto::studio::{Approval, AssetState, EntityInfo, EpisodeState, Production, StepKind, StudioSetModelsParams, StudioSetParams, StudioView};

use super::{
    Dialog, MenuAction, StudioPane, StudioPaneEvent, centered, model,
    store::{Load, Store},
    tiles,
};
use crate::{
    assets::icon,
    context_menu::{Item, Menu},
    studio_block::Then,
    text_field::TextField,
    theme::{self, Continuous as _},
};

// ---------------------------------------------------------------- marks

/// How a mark reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkState {
    Done,
    /// Hollow: not there yet.
    Missing,
    /// Done once, needs a look again (`Changed`, a stale cut).
    Warn,
}

/// A small state mark (`Sheet`, `Approved`, `Shots 5`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub label: String,
    pub state: MarkState,
}

impl Mark {
    fn new(label: impl Into<String>, state: MarkState) -> Self {
        Self { label: label.into(), state }
    }

    fn done(label: impl Into<String>, done: bool) -> Self {
        Self::new(label, if done { MarkState::Done } else { MarkState::Missing })
    }
}

/// What an entity's marks are made from: its production row, else (not read yet) its info.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Readiness {
    pub sheet: bool,
    pub approval: Approval,
    /// `None`: no voice to cast (not a character, or one that doesn't speak).
    pub voice: Option<bool>,
    /// `None`: not a character or prop.
    pub video_sheet: Option<bool>,
}

impl Readiness {
    #[must_use]
    pub fn of_asset(a: &AssetState) -> Self {
        Self { sheet: a.sheet.is_some(), approval: a.approval, voice: a.voice, video_sheet: a.video_sheet }
    }

    /// From the world's listing alone: a character always has a voice to cast (only the production knows it
    /// doesn't speak), and a video sheet is a video among its refs.
    #[must_use]
    pub fn of_info(info: &EntityInfo) -> Self {
        let kind = model::EntityKind::of_entity(&info.id);
        let video = info.refs.iter().any(|r| model::RefMedia::of(r) == model::RefMedia::Video);
        Self {
            sheet: info.sheet.is_some(),
            approval: info.approval,
            voice: (kind == model::EntityKind::Character).then_some(info.voice),
            video_sheet: matches!(kind, model::EntityKind::Character | model::EntityKind::Prop).then_some(video),
        }
    }
}

/// Entity `id`'s readiness: its production row, with the approval and sheet of `info` when given (the newer of
/// the two after an approval), else `info` alone.
#[must_use]
pub fn readiness(production: Option<&Production>, info: Option<&EntityInfo>, id: &str) -> Option<Readiness> {
    let row = production.and_then(|p| p.assets.iter().find(|a| a.id == id));
    match (row, info) {
        (Some(a), Some(i)) => Some(Readiness { sheet: i.sheet.is_some(), approval: i.approval, ..Readiness::of_asset(a) }),
        (Some(a), None) => Some(Readiness::of_asset(a)),
        (None, Some(i)) => Some(Readiness::of_info(i)),
        (None, None) => None,
    }
}

/// An entity's marks, the same on its Production row, its World card and its page: `Sheet`, `Approved` (or
/// `Changed`), then `Voice` and `Video sheet` where they apply. Missing ones are hollow.
#[must_use]
pub fn marks(r: &Readiness) -> Vec<Mark> {
    let mut out = vec![Mark::done("Sheet", r.sheet)];
    out.push(match r.approval {
        Approval::Approved => Mark::new("Approved", MarkState::Done),
        Approval::Changed => Mark::new("Changed", MarkState::Warn),
        Approval::None | Approval::Unknown => Mark::new("Approved", MarkState::Missing),
    });
    if let Some(v) = r.voice {
        out.push(Mark::done("Voice", v));
    }
    if let Some(v) = r.video_sheet {
        out.push(Mark::done("Video sheet", v));
    }
    out
}

// ---------------------------------------------------------------- episodes

/// An episode row's title: `Ep 1 · Pilot`, else the canvas's title (its name without one).
#[must_use]
pub fn episode_title(e: &EpisodeState) -> String {
    let title = if e.title.trim().is_empty() { e.canvas.as_str() } else { e.title.trim() };
    match e.episode {
        Some(n) => format!("Ep {n} \u{b7} {title}"),
        None => title.to_owned(),
    }
}

/// A canvas's stages: `Script` · `Shots n` · `Keyframes a/n` · `Video b/n` · `Sound` · `Cut vN`. A canvas that
/// isn't an episode and has no script, shots or cut (a moodboard) has none.
#[must_use]
pub fn stages(e: &EpisodeState) -> Vec<Mark> {
    if e.episode.is_none() && !e.script && e.shots == 0 && e.cut.is_none() {
        return Vec::new();
    }
    let of = |label: &str, have: u32| {
        if e.shots == 0 { Mark::done(label, false) } else { Mark::done(format!("{label} {have}/{}", e.shots), have >= e.shots) }
    };
    let shots = if e.shots == 0 { Mark::done("Shots", false) } else { Mark::done(format!("Shots {}", e.shots), true) };
    let cut = match &e.cut {
        Some(c) if c.stale => Mark::new(format!("Cut v{} \u{b7} stale", c.version), MarkState::Warn),
        Some(c) => Mark::new(format!("Cut v{}", c.version), MarkState::Done),
        None => Mark::done("Cut", false),
    };
    vec![Mark::done("Script", e.script), shots, of("Keyframes", e.keyframes), of("Video", e.videos), Mark::done("Sound", e.sound > 0), cut]
}

/// What needs a look on a canvas, when not zero: `2 out of date`, `1 failed`, `3 running`.
#[must_use]
pub fn issues(e: &EpisodeState) -> Vec<String> {
    [(e.out_of_date, "out of date"), (e.failed, "failed"), (e.running, "running")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect()
}

// ---------------------------------------------------------------- steps

/// What a next step's button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepAction {
    /// Its message to the linked agent.
    Send(String),
    /// The user's to decide: the entity's page, or the canvas.
    OpenEntity(String),
    OpenCanvas(String),
    /// Nothing to open (a newer server's step without either).
    None,
}

#[must_use]
pub fn step_action(step: &silo_proto::studio::Step) -> StepAction {
    match (&step.ask, &step.entity, &step.canvas) {
        (Some(ask), ..) if !ask.trim().is_empty() => StepAction::Send(ask.clone()),
        (_, Some(id), _) => StepAction::OpenEntity(id.clone()),
        (_, None, Some(c)) => StepAction::OpenCanvas(c.clone()),
        _ => StepAction::None,
    }
}

/// A step button's tooltip: the message it sends, or what it opens.
#[must_use]
pub fn step_tip(step: &silo_proto::studio::Step) -> String {
    match step_action(step) {
        StepAction::Send(ask) => format!("Asks the agent: {ask}"),
        StepAction::OpenEntity(id) => format!("Opens {id} for you to review"),
        StepAction::OpenCanvas(c) => format!("Opens the canvas {c}"),
        StepAction::None => step.text.clone(),
    }
}

// ---------------------------------------------------------------- models

/// The project's model picks shown: `[models]` key, chip label, otto's media kind.
pub const MODEL_KINDS: [(&str, &str, MediaKind); 3] =
    [("image", "Image", MediaKind::Image), ("video", "Video", MediaKind::Video), ("speech", "Speech", MediaKind::Audio)];

/// A model's name as the chip shows it: its name in otto's list, else the handle without its provider.
#[must_use]
pub fn model_name(handle: &str, models: Option<&[MediaModelInfo]>) -> String {
    models.and_then(|ms| ms.iter().find(|m| m.handle == handle)).map_or_else(|| model::short_model(handle), |m| m.name.clone())
}

/// A row of the Models menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    pub label: String,
    /// What picking it sets (`Some(None)`: Automatic); `None`: a note, not pickable.
    pub pick: Option<Option<String>>,
    pub checked: bool,
}

/// The Models menu for a kind whose pick is `current`: Automatic, the current pick when otto doesn't list it as
/// connected (so it stays visible), then otto's connected models of the kind (the card settings' list); a note
/// while they load, when they can't be listed, or when none is connected.
#[must_use]
pub fn model_menu(listed: Option<&Load<Vec<MediaModelInfo>>>, current: Option<&str>, kind_label: &str) -> Vec<ModelRow> {
    let mut rows = vec![ModelRow { label: "Automatic".into(), pick: Some(None), checked: current.is_none() }];
    let models: &[MediaModelInfo] = match listed {
        Some(Load::Ready(m)) => m,
        _ => &[],
    };
    let connected: Vec<&MediaModelInfo> = models.iter().filter(|m| m.connected).collect();
    if let Some(cur) = current.filter(|c| !connected.iter().any(|m| m.handle == *c)) {
        let name = model_name(cur, Some(models));
        rows.push(ModelRow { label: format!("{name} (not connected)"), pick: Some(Some(cur.to_owned())), checked: true });
    }
    for m in &connected {
        let label = if m.provider_name.is_empty() { m.name.clone() } else { format!("{} \u{b7} {}", m.name, m.provider_name) };
        rows.push(ModelRow { label, pick: Some(Some(m.handle.clone())), checked: current == Some(m.handle.as_str()) });
    }
    let note = match listed {
        None | Some(Load::Loading) => Some("Loading models\u{2026}".to_owned()),
        Some(Load::Failed(why)) => Some(format!("Models unavailable: {why}")),
        Some(Load::Ready(_)) if connected.is_empty() => Some(format!("No {} provider connected", kind_label.to_lowercase())),
        Some(Load::Ready(_)) => None,
    };
    rows.extend(note.map(|label| ModelRow { label, pick: None, checked: false }));
    rows
}

/// A chip's text for a pick: its model's name, or `Automatic`.
#[must_use]
pub fn model_chip(current: Option<&str>, listed: Option<&Load<Vec<MediaModelInfo>>>) -> String {
    let models = listed.and_then(Load::ready).map(Vec::as_slice);
    current.map_or_else(|| "Automatic".to_owned(), |h| model_name(h, models))
}

/// `studio.set_models` for one pick (`None`: Automatic, removing the key).
#[must_use]
pub fn set_models_params(block_id: &str, kind: &str, model: Option<String>) -> StudioSetModelsParams {
    StudioSetModelsParams { block_id: block_id.to_owned(), models: BTreeMap::from([(kind.to_owned(), model)]) }
}

/// Whether the body shows the Production view: asked for, or a canvas view with no canvas open.
#[must_use]
pub fn shows_production(view: StudioView, active: Option<&str>) -> bool {
    match view {
        StudioView::Production => true,
        StudioView::Canvas | StudioView::Unknown => active.is_none(),
        StudioView::World | StudioView::Setup => false,
    }
}

/// Asset rows: at least this wide, this far apart.
const ROW_MIN: f32 = 300.0;
const ROW_GAP: f32 = 8.0;
const THUMB: f32 = 44.0;
/// A prop's row under its owner's is indented this much.
const NEST: f32 = 22.0;
/// The page's column at most this wide (centred), its padding.
const PAGE_MAX: f32 = 1040.0;
const PAD: f32 = 20.0;
/// Episode rows put their marks beside the title from this inner width.
const EPISODE_WIDE: f32 = 640.0;

// ---------------------------------------------------------------- drawing

/// A mark: done ones filled, missing ones hollow, the ones to look at again in the warning colour.
fn mark(m: &Mark, small: bool) -> gpui::Div {
    let (glyph, ink) = match m.state {
        MarkState::Done => ("circle-check", theme::success()),
        MarkState::Missing => ("circle", theme::muted()),
        MarkState::Warn => ("alert", theme::warning()),
    };
    let size = if small { 10.5 } else { 11.5 };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(3.0))
        .h(px(if small { 18.0 } else { 20.0 }))
        .px(px(6.0))
        .continuous(6.0)
        .text_size(px(size))
        .whitespace_nowrap()
        .map(|el| match m.state {
            MarkState::Done => el.bg(theme::control_fill()).text_color(rgb(theme::foreground())),
            MarkState::Missing => el.border_1().border_color(theme::control_fill()).text_color(rgb(theme::muted())),
            MarkState::Warn => el.bg(theme::control_fill()).text_color(rgb(theme::warning())),
        })
        .child(icon(glyph, size - 1.0, ink))
        .child(m.label.clone())
}

/// A wrapping row of marks.
pub(super) fn marks_row(marks: &[Mark], small: bool) -> gpui::Div {
    div().flex().flex_wrap().gap(px(4.0)).children(marks.iter().map(|m| mark(m, small)))
}

/// A section's heading.
fn heading(text: &str) -> gpui::Div {
    div().text_size(px(12.0)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(theme::muted())).child(text.to_owned())
}

/// A step's button: the first primary (accent), the rest quiet.
fn step_button(id: impl Into<gpui::ElementId>, label: String, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .max_w_full()
        .h(px(28.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .continuous(8.0)
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())).hover(|s| s.opacity(0.88)))
        .when(!primary, |el| el.bg(theme::control_fill()).hover(|s| s.bg(theme::row_fill())))
        .child(label)
}

/// A quiet box saying what goes in an empty section, with its first steps.
fn empty_box(text: &str, actions: Vec<AnyElement>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .p(px(14.0))
        .continuous(crate::agent::media::RADIUS)
        .border_1()
        .border_dashed()
        .border_color(theme::control_fill())
        .child(div().text_size(px(12.5)).text_color(rgb(theme::muted())).child(text.to_owned()))
        .when(!actions.is_empty(), |el| el.child(div().flex().flex_wrap().gap(px(6.0)).children(actions)))
}

/// A model chip: the kind, its pick, a chevron; opens the Models menu.
pub(super) fn model_chip_element(id: impl Into<gpui::ElementId>, kind_label: &str, text: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(24.0))
        .px(px(9.0))
        .continuous(7.0)
        .bg(theme::control_fill())
        .cursor_pointer()
        .hover(|s| s.bg(theme::row_fill()))
        .text_size(px(12.0))
        .whitespace_nowrap()
        .child(div().text_color(rgb(theme::muted())).child(kind_label.to_owned()))
        .child(text)
        .child(icon("chevron-down", 9.0, theme::muted()))
}

impl StudioPane {
    /// Show the Production view.
    pub(crate) fn show_production(&mut self, cx: &mut Context<Self>) {
        self.set(StudioSetParams { view: Some(StudioView::Production), ..StudioSetParams::default() }, cx);
    }

    /// The Models menu for `kind` (`image`, `video`, `speech`) under the pointer.
    pub(super) fn models_menu(&mut self, kind: &str, e: &ClickEvent, cx: &mut Context<Self>) {
        let Some((_, label, media)) = MODEL_KINDS.iter().find(|(k, ..)| *k == kind).copied() else { return };
        self.store.update(cx, |s, cx| s.want_models(media, cx));
        let s = self.store.read(cx);
        let current = s.project().and_then(|p| p.models.get(kind)).cloned();
        let rows = model_menu(s.models(media), current.as_deref(), label);
        let items = rows
            .into_iter()
            .map(|r| match r.pick {
                Some(pick) => Item::action(r.label, "", MenuAction::SetModel(kind.to_owned(), pick)).checked(r.checked),
                None => Item::action(r.label, "", MenuAction::SetModel(kind.to_owned(), current.clone())).enabled(false),
            })
            .collect();
        self.open_menu(Menu::new(e.position(), items), cx);
    }

    /// A chip for the project's `kind` pick (the Models menu on click).
    pub(super) fn model_chip_for(&mut self, kind: &'static str, id: impl Into<gpui::ElementId>, cx: &mut Context<Self>) -> AnyElement {
        let Some((_, label, media)) = MODEL_KINDS.iter().find(|(k, ..)| *k == kind).copied() else { return div().into_any_element() };
        self.store.update(cx, |s, cx| s.want_models(media, cx));
        let s = self.store.read(cx);
        let current = s.project().and_then(|p| p.models.get(kind)).cloned();
        let text = model_chip(current.as_deref(), s.models(media));
        model_chip_element(id, label, text)
            .tooltip(crate::tooltip::Tooltip::text(format!("The {} model generations use", label.to_lowercase())))
            .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| this.models_menu(kind, e, cx)))
            .into_any_element()
    }

    /// Do what a step's button does.
    fn run_step(&mut self, action: StepAction, cx: &mut Context<Self>) {
        match action {
            StepAction::Send(text) => cx.emit(StudioPaneEvent::Agent(Then::Send(text))),
            StepAction::OpenEntity(id) => self.show_entity(id, cx),
            StepAction::OpenCanvas(name) => self.show_canvas(name, cx),
            StepAction::None => {}
        }
    }

    pub(super) fn render_production(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (production, project) = {
            let s = self.store.read(cx);
            (s.production.clone(), s.project().cloned())
        };
        let production = match production {
            Some(Load::Ready(p)) => p,
            Some(Load::Failed(e)) => return centered(e),
            _ => return centered("Loading\u{2026}".into()),
        };
        let bw = self.bounds().map_or(900.0, |b| f32::from(b.size.width));
        let inner = (bw.min(PAGE_MAX + 2.0 * PAD) - 2.0 * PAD).max(200.0);
        let sections = self.store.read(cx).sections.clone().unwrap_or_default();

        // The title and a line of what is in it.
        let name = project.as_ref().map_or_else(|| crate::files::model::name(&self.state.root).to_owned(), |p| p.name.clone());
        let episodes = production.episodes.iter().filter(|e| e.episode.is_some()).count();
        let summary = format!(
            "{} asset{} \u{b7} {} episode{}",
            production.assets.len(),
            if production.assets.len() == 1 { "" } else { "s" },
            episodes,
            if episodes == 1 { "" } else { "s" }
        );
        let title = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(div().text_size(px(18.0)).font_weight(FontWeight::SEMIBOLD).child(name))
            .child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(summary));

        let next = self.next_up(&production, cx);
        let assets = self.assets_section(&production, &sections, inner, window, cx);
        let episodes = self.episodes_section(&production, project.as_ref(), inner, cx);
        let models = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(heading("Models"))
            .child(
                div().flex().flex_wrap().gap(px(6.0)).children(
                    MODEL_KINDS
                        .iter()
                        .map(|(kind, ..)| self.model_chip_for(kind, gpui::ElementId::Name(format!("studio-model-{kind}").into()), cx))
                        .collect::<Vec<_>>(),
                ),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(theme::muted()))
                    .child("Automatic: the agent and the cards choose. A pick is written to studio.toml."),
            );
        div()
            .id("studio-production")
            .size_full()
            .overflow_y_scroll()
            .child(div().w_full().flex().justify_center().child(
                div().w(px(inner)).py(px(PAD)).flex().flex_col().gap(px(22.0)).child(title).child(next).child(assets).child(episodes).child(models),
            ))
            .into_any_element()
    }

    /// Next up: one to three steps as buttons, the first primary.
    fn next_up(&mut self, production: &Production, cx: &mut Context<Self>) -> gpui::Div {
        let body = if production.next.is_empty() {
            div()
                .text_size(px(12.5))
                .text_color(rgb(theme::muted()))
                .child("Nothing waiting: every asset and episode is as far as this view tracks.")
                .into_any_element()
        } else {
            let buttons = production.next.iter().enumerate().map(|(i, step)| {
                let action = step_action(step);
                let enabled = action != StepAction::None;
                step_button(("studio-step", i), step.text.clone(), i == 0)
                    .when(!enabled, |el| el.opacity(0.5))
                    .tooltip(crate::tooltip::Tooltip::text(step_tip(step)))
                    .on_click(cx.listener(move |this, _, _, cx| this.run_step(action.clone(), cx)))
            });
            div().flex().flex_wrap().gap(px(6.0)).children(buttons.collect::<Vec<_>>()).into_any_element()
        };
        div().flex().flex_col().gap(px(8.0)).child(heading("Next up")).child(body)
    }

    /// Assets: the world's entities by section, a row each (sheet, name, marks, where used).
    fn assets_section(
        &mut self,
        production: &Production,
        sections: &[silo_proto::studio::Section],
        inner: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut out = div().flex().flex_col().gap(px(10.0)).child(heading("Assets"));
        if production.assets.is_empty() {
            let actions = vec![
                step_button("studio-assets-agent", "Create a character with the agent\u{2026}".into(), false)
                    .on_click(cx.listener(|this, _, window, cx| this.pick(Some(MenuAction::CreateWithAgent("characters".into())), window, cx)))
                    .into_any_element(),
                step_button("studio-assets-new", "New character\u{2026}".into(), false)
                    .on_click(cx.listener(|this, _, window, cx| this.pick(Some(MenuAction::NewEntity("characters".into())), window, cx)))
                    .into_any_element(),
                step_button("studio-assets-world", "Open the World".into(), false)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set(StudioSetParams { view: Some(StudioView::World), world_entity: Some(None), ..StudioSetParams::default() }, cx);
                    }))
                    .into_any_element(),
            ];
            return out.child(empty_box(
                "No characters, places or props yet. They are who and what appears on screen; each gets a sheet the generations keep to, \
                 approved by you.",
                actions,
            ));
        }
        let columns = model::grid_columns(inner, ROW_MIN, ROW_GAP);
        #[allow(clippy::cast_precision_loss)]
        let row_w = ((inner - ROW_GAP * (columns as f32 - 1.0)) / columns as f32).max(160.0);
        // In the world's section order, then whatever the world listing doesn't have yet.
        let mut order: Vec<(String, String)> = sections.iter().map(|s| (s.id.clone(), s.title.clone())).collect();
        for a in &production.assets {
            if !order.iter().any(|(id, _)| *id == a.section) {
                order.push((a.section.clone(), crate::files::model::name(&a.section).to_owned()));
            }
        }
        // A prop that goes with one character also sits under that character's row, indented.
        let owned = model::props_by_owner(sections);
        for (section, title) in order {
            let rows: Vec<&AssetState> = production.assets.iter().filter(|a| a.section == section).collect();
            if rows.is_empty() {
                continue;
            }
            let mut grid = div().flex().flex_wrap().items_start().gap(px(ROW_GAP));
            for a in rows.iter().copied() {
                let props: Vec<&AssetState> =
                    owned.get(&a.id).into_iter().flatten().filter_map(|p| production.assets.iter().find(|x| x.id == *p)).collect();
                if props.is_empty() {
                    grid = grid.child(self.asset_row(a, row_w, false, window, cx));
                    continue;
                }
                let mut group = div().w(px(row_w)).flex().flex_col().gap(px(2.0)).child(self.asset_row(a, row_w, false, window, cx));
                for p in props {
                    let nested = self.asset_row(p, row_w - NEST, true, window, cx);
                    group = group.child(
                        div()
                            .flex()
                            .child(div().flex_none().w(px(NEST - 10.0)).ml(px(10.0)).border_l_1().border_color(theme::control_fill()))
                            .child(nested),
                    );
                }
                grid = grid.child(group);
            }
            out = out.child(
                div().flex().flex_col().gap(px(6.0)).child(div().text_size(px(12.5)).child(format!("{title} \u{b7} {}", rows.len()))).child(grid),
            );
        }
        out
    }

    /// `nested`: a prop's row under its owner's (its id apart from its row in its own section).
    fn asset_row(&mut self, a: &AssetState, w: f32, nested: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let thumb = match &a.sheet {
            Some(sheet) => tiles::image(&self.store, sheet, THUMB, THUMB, window, cx),
            None => crate::agent::media::frame(THUMB, THUMB, None).child(icon("user", 16.0, theme::muted())).into_any_element(),
        };
        let marks = entity_marks(self.store.read(cx), &a.id);
        let looks = self.store.read(cx).entity_info(&a.id).and_then(model::looks_count);
        let used = match a.used {
            0 => "Not used yet".to_owned(),
            1 => "1 canvas".to_owned(),
            n => format!("{n} canvases"),
        };
        let used = match looks {
            Some(l) => format!("{l} \u{b7} {used}"),
            None => used,
        };
        let id = a.id.clone();
        div()
            .id(gpui::ElementId::Name(format!("studio-asset-{}{}", if nested { "nested-" } else { "" }, a.id).into()))
            .w(px(w))
            .flex()
            .items_center()
            .gap(px(10.0))
            .p(px(6.0))
            .continuous(9.0)
            .cursor_pointer()
            .hover(|s| s.bg(theme::control_fill()))
            .child(thumb)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(a.name.clone()),
                            )
                            .child(div().flex_none().text_size(px(11.0)).text_color(rgb(theme::muted())).child(used)),
                    )
                    .child(marks_row(&marks, true)),
            )
            .tooltip(crate::tooltip::Tooltip::text(format!("Open {}", a.name)))
            .on_click(cx.listener(move |this, _, _, cx| this.show_entity(id.clone(), cx)))
            .into_any_element()
    }

    /// Episodes: each canvas (episodes first), its stages and counts; ▶ Play with a cut.
    fn episodes_section(
        &mut self,
        production: &Production,
        project: Option<&silo_proto::studio::Project>,
        inner: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut out = div().flex().flex_col().gap(px(8.0)).child(heading("Episodes"));
        if !production.episodes.iter().any(|e| e.episode.is_some()) {
            let next = project.map_or(1, model::next_episode);
            let mut actions = Vec::new();
            if let Some(step) = production.next.iter().find(|s| s.kind == StepKind::Episode) {
                let action = step_action(step);
                actions.push(
                    step_button("studio-episodes-step", step.text.clone(), false)
                        .tooltip(crate::tooltip::Tooltip::text(step_tip(step)))
                        .on_click(cx.listener(move |this, _, _, cx| this.run_step(action.clone(), cx)))
                        .into_any_element(),
                );
            }
            actions.push(
                step_button("studio-episodes-new", format!("New episode {next}\u{2026}"), false)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let field = cx.new(|cx| TextField::new("Title", cx));
                        this.open_dialog(Dialog::NewCanvas { field, episode: Some(next), format: None }, window, cx);
                    }))
                    .into_any_element(),
            );
            out = out.child(empty_box(
                "No episodes yet. An episode is a canvas with a number; its script, shots, keyframes, video, sound and cut show here as \
                 they are made.",
                actions,
            ));
        }
        let wide = inner >= EPISODE_WIDE;
        for e in &production.episodes {
            out = out.child(self.episode_row(e, wide, cx));
        }
        out
    }

    fn episode_row(&mut self, e: &EpisodeState, wide: bool, cx: &mut Context<Self>) -> AnyElement {
        let stages = stages(e);
        let issues = issues(e);
        let marks = if stages.is_empty() {
            div().text_size(px(11.5)).text_color(rgb(theme::muted())).child("Not an episode").into_any_element()
        } else {
            marks_row(&stages, true).into_any_element()
        };
        let issues = (!issues.is_empty()).then(|| div().text_size(px(11.5)).text_color(rgb(theme::warning())).child(issues.join(" \u{b7} ")));
        let play = e.cut.as_ref().map(|cut| {
            let canvas = e.canvas.clone();
            let tip = if cut.stale { "Play the newest cut (stale: takes changed since it was made)" } else { "Play the newest cut" };
            div()
                .id(gpui::ElementId::Name(format!("studio-play-{}", e.canvas).into()))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.0))
                .h(px(24.0))
                .px(px(9.0))
                .continuous(7.0)
                .bg(theme::control_fill())
                .hover(|s| s.bg(theme::row_fill()))
                .cursor_pointer()
                .text_size(px(12.0))
                .child(icon("play", 10.0, theme::foreground()))
                .child("Play")
                .tooltip(crate::tooltip::Tooltip::text(tip))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.view_cut(&canvas, window, cx);
                }))
        });
        let title = div()
            .min_w_0()
            .text_size(px(13.0))
            .font_weight(FontWeight::MEDIUM)
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .child(episode_title(e));
        let canvas = e.canvas.clone();
        let row = div()
            .id(gpui::ElementId::Name(format!("studio-episode-{}", e.canvas).into()))
            .w_full()
            .p(px(8.0))
            .continuous(9.0)
            .cursor_pointer()
            .hover(|s| s.bg(theme::control_fill()))
            .tooltip(crate::tooltip::Tooltip::text(format!("Open the canvas {}", e.canvas)))
            .on_click(cx.listener(move |this, _, _, cx| this.show_canvas(canvas.clone(), cx)));
        if wide {
            row.flex()
                .items_center()
                .gap(px(12.0))
                .child(title.w(px(200.0)).flex_none())
                .child(div().flex_1().min_w_0().flex().flex_col().gap(px(3.0)).child(marks).children(issues))
                .children(play)
                .into_any_element()
        } else {
            row.flex()
                .flex_col()
                .gap(px(5.0))
                .child(div().flex().items_center().gap(px(8.0)).child(title.flex_1()).children(play))
                .child(marks)
                .children(issues)
                .into_any_element()
        }
    }
}

/// The marks of entity `id` as the store knows them (the World grid's cards, the entity page's head).
pub(super) fn entity_marks(store: &Store, id: &str) -> Vec<Mark> {
    readiness(store.production(), store.entity_info(id), id).map(|r| marks(&r)).unwrap_or_default()
}

/// The approval control's words for `approval`: the button's label, the note before it, and what a click sets.
#[must_use]
pub fn approve_button(approval: Approval) -> (&'static str, Option<&'static str>, bool) {
    match approval {
        Approval::Approved => ("Approved \u{2713}", None, false),
        Approval::Changed => ("Approve again", Some("Changed since approved"), true),
        Approval::None | Approval::Unknown => ("Approve", None, true),
    }
}
