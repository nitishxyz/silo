//! A card's settings (Settings in its menu or the selection actions): a slim panel beside the card, its fields
//! from [`super::board::Board::settings`] with otto's models (`media.models`), the card's preview
//! (`studio.preview_card`: the request it would send, and which models fit what is wired) and the model's own
//! options (`media.model_schema`: top-level fields only: enums as menus, numbers and strings as fields, booleans
//! as switches). Every change goes through [`super::board::Board::change_setting`].

use std::collections::HashMap;

use gpui::{AnyElement, AppContext as _, Context, Entity, FontWeight, Subscription, Window, div, prelude::*, px, rgb};
use otto_proto::{CardPreview, MediaKind, MediaModelInfo, MediaModelSchema};
use serde_json::Value;
use silo_proto::studio::{GenKind, NodeBody};

use super::{CanvasView, Pick, overlays::PickerFor};
use crate::{
    assets::icon,
    context_menu::{Item, Menu},
    studio::{
        board::{self, ModelInfo, Preview, SettingChange, SettingsInput, settings::ChoiceField},
        store::Load,
        tiles::ink,
    },
    surface::Glassy as _,
    text_field::{FieldEvent, TextField},
    theme::{self, Continuous as _},
};

const W: f32 = 290.0;

/// A choice field's value as its change.
type ToChange = fn(Option<String>) -> SettingChange;

/// Option keys the panel's own rows cover, or that aren't the card's to set.
const COVERED: &[&str] = &["prompt", "aspect_ratio", "duration", "resolution", "voice", "style", "negative_prompt_files", "model", "mode"];

pub(crate) struct Panel {
    pub node: String,
    title: Entity<TextField>,
    /// Option text fields by key.
    fields: HashMap<String, Entity<TextField>>,
    preview: Option<CardPreview>,
    /// The canvas rev the preview was asked at.
    preview_rev: Option<u64>,
    previewing: bool,
    _subs: Vec<Subscription>,
}

/// otto's media kind for what a card makes.
pub(crate) fn media_kind(makes: GenKind) -> MediaKind {
    match makes {
        GenKind::Video => MediaKind::Video,
        GenKind::Speech | GenKind::Sound | GenKind::Music => MediaKind::Audio,
        _ => MediaKind::Image,
    }
}

/// An enum's values as strings.
fn enum_strings(schema: &Value, key: &str) -> Vec<String> {
    schema["properties"][key]["enum"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).collect())
        .unwrap_or_default()
}

impl CanvasView {
    fn makes_of(&self, node: &str) -> Option<GenKind> {
        match self.board.canvas().node(node).map(|n| &n.body) {
            Some(NodeBody::Gen { makes, .. }) => Some(*makes),
            _ => None,
        }
    }

    pub(super) fn open_settings(&mut self, node: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(makes) = self.makes_of(&node) else { return };
        let current = match self.board.canvas().node(&node).map(|n| &n.body) {
            Some(NodeBody::Gen { title, .. }) => title.clone().unwrap_or_default(),
            _ => String::new(),
        };
        let title = cx.new(|cx| {
            let mut f = TextField::new("Title", cx);
            f.set_text(current, cx);
            f
        });
        let id = node.clone();
        let sub = cx.subscribe_in(&title, window, move |this, f, e: &FieldEvent, window, cx| {
            if matches!(e, FieldEvent::Submit) {
                let text = f.read(cx).text.clone();
                let fx = this.board.change_setting(&id, &SettingChange::Title(text));
                this.apply(fx, window, cx);
                window.focus(&this.focus, cx);
            }
        });
        if self.board.selection() != [node.clone()] {
            let fx = self.board.select(std::slice::from_ref(&node));
            self.apply(fx, window, cx);
        }
        self.settings = Some(Panel { node, title, fields: HashMap::new(), preview: None, preview_rev: None, previewing: false, _subs: vec![sub] });
        self.load_models(media_kind(makes), cx);
        self.request_preview(cx);
        cx.notify();
    }

    /// The canvas changed: the preview may have.
    pub(super) fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let rev = self.board.canvas().rev;
        if self.settings.as_ref().is_some_and(|p| p.preview_rev != Some(rev)) {
            self.request_preview(cx);
        }
    }

    /// `studio.preview_card` for the open panel's card, with every model of its kind asked about.
    fn request_preview(&mut self, cx: &mut Context<Self>) {
        let rev = self.board.canvas().rev;
        let Some(p) = self.settings.as_mut().filter(|p| !p.previewing) else { return };
        let Some(makes) = self.board.canvas().node(&p.node).and_then(|n| match &n.body {
            NodeBody::Gen { makes, .. } => Some(*makes),
            _ => None,
        }) else {
            return;
        };
        p.previewing = true;
        p.preview_rev = Some(rev);
        let models: Vec<String> = match self.store.read(cx).models(media_kind(makes)) {
            Some(Load::Ready(m)) => m.iter().map(|m| m.handle.clone()).collect(),
            _ => Vec::new(),
        };
        let (root, canvas, node) = (self.store.read(cx).root.clone(), self.name.clone(), p.node.clone());
        let store = self.store.clone();
        store.read(cx).caller().otto(
            cx,
            move |c| async move { c.studio_preview_card(&root, &canvas, &node, models).await },
            |this: &mut Self, r, cx| {
                let again = this.settings.as_ref().is_some_and(|p| p.preview_rev != Some(this.board.canvas().rev));
                if let Some(p) = this.settings.as_mut() {
                    p.previewing = false;
                    match r {
                        Ok(preview) => p.preview = Some(preview),
                        Err(e) => {
                            p.preview = Some(CardPreview {
                                error: Some(otto_proto::CardProblem {
                                    code: "otto".into(),
                                    message: crate::agent::describe(&e),
                                    kind: None,
                                    provider: None,
                                }),
                                ..CardPreview::default()
                            })
                        }
                    }
                }
                this.load_schema(cx);
                if again {
                    this.request_preview(cx);
                }
            },
        );
    }

    /// otto's models of `kind`, listed once by the store (its `StoreEvent::Models` asks for the preview again).
    fn load_models(&mut self, kind: MediaKind, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| s.want_models(kind, cx));
    }

    /// The models came: the fits need them.
    pub(super) fn models_listed(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.settings.as_mut() {
            p.preview_rev = None;
        }
        self.request_preview(cx);
    }

    /// The model in effect for the panel's card: its own, else what Automatic picked.
    fn effective_model(&self) -> Option<String> {
        let p = self.settings.as_ref()?;
        match self.board.canvas().node(&p.node).map(|n| &n.body) {
            Some(NodeBody::Gen { model: Some(m), .. }) => Some(m.clone()),
            _ => p.preview.as_ref()?.compiled.as_ref().map(|c| c.model.clone()).filter(|m| !m.is_empty()),
        }
    }

    /// `media.model_schema` of the model in effect, read once.
    fn load_schema(&mut self, cx: &mut Context<Self>) {
        let Some(model) = self.effective_model() else { return };
        if self.runs.schemas.contains_key(&model) {
            return;
        }
        self.runs.schemas.insert(model.clone(), Load::Loading);
        let store = self.store.clone();
        let m = model.clone();
        store.read(cx).caller().otto(
            cx,
            move |c| async move { c.media_model_schema(&m, None).await },
            move |this: &mut Self, r, _| {
                this.runs.schemas.insert(model, r.map_or_else(|e| Load::Failed(crate::agent::describe(&e)), Load::Ready));
            },
        );
    }

    fn schema(&self, model: &str) -> Option<&MediaModelSchema> {
        self.runs.schemas.get(model)?.ready()
    }

    /// The models of a card's kind as the panel takes them.
    fn model_infos(&self, makes: GenKind, cx: &gpui::App) -> Vec<ModelInfo> {
        let Some(Load::Ready(models)) = self.store.read(cx).models(media_kind(makes)) else { return Vec::new() };
        model_infos(makes, models, |m| self.schema(m))
    }

    fn settings_preview(&self) -> Option<Preview> {
        self.settings.as_ref()?.preview.as_ref().map(settings_preview)
    }

    /// The card's settings as the board gives them, with the models' fits from the preview.
    fn card_settings(&self, node: &str, cx: &gpui::App) -> Option<board::CardSettings> {
        let makes = self.makes_of(node)?;
        let models = self.model_infos(makes, cx);
        let preview = self.settings_preview();
        let automatic = self.settings.as_ref().and_then(|p| p.preview.as_ref()?.compiled.as_ref().map(|c| c.model.clone()));
        // Voices: the model's own `voice` choices, when its schema lists them.
        let voices: Vec<board::VoiceInfo> = self
            .effective_model()
            .and_then(|m| self.schema(&m))
            .and_then(|s| s.modes.first())
            .map(|m| enum_strings(&m.schema, "voice").into_iter().map(|v| board::VoiceInfo { label: v.clone(), id: v }).collect())
            .unwrap_or_default();
        let input = SettingsInput {
            project: None,
            sections: &[],
            models: &models,
            automatic: automatic.as_deref(),
            voices: &voices,
            preview: preview.as_ref(),
        };
        let mut s = self.board.settings(node, &input)?;
        if let Some(fits) = self.settings.as_ref().and_then(|p| p.preview.as_ref()).map(|p| &p.fits) {
            dim_unfit(&mut s.model.choices, fits);
        }
        Some(s)
    }

    /// The model choices of card `node` (the model picker's rows).
    pub(super) fn model_choices(&self, node: &str, cx: &gpui::App) -> Vec<board::settings::Choice> {
        self.card_settings(node, cx).map(|s| s.model.choices).unwrap_or_default()
    }

    /// A choice field's menu under the pointer.
    fn choice_menu(&mut self, node: &str, field: &ChoiceField, change: ToChange, at: gpui::Point<gpui::Pixels>) {
        let items = field
            .choices
            .iter()
            .map(|c| {
                Item::action(choice_label(c), "", Pick::Setting(node.to_owned(), change(c.value.clone()))).checked(c.selected).enabled(c.enabled)
            })
            .collect();
        self.menu = Some(Menu::new(at, items));
    }
}

/// otto's models of a card's kind as the board's [`ModelInfo`]s: what each offers read from its schema
/// (`schema_of(handle)`, when loaded), unconnected ones needing their provider.
pub(super) fn model_infos<'a>(makes: GenKind, models: &[MediaModelInfo], schema_of: impl Fn(&str) -> Option<&'a MediaModelSchema>) -> Vec<ModelInfo> {
    models
        .iter()
        .map(|m| {
            let schema = schema_of(&m.handle).and_then(|s| s.modes.first()).map(|m| &m.schema);
            let durations =
                schema.map(|s| enum_strings(s, "duration").iter().filter_map(|d| d.trim_end_matches('s').parse().ok()).collect()).unwrap_or_default();
            ModelInfo {
                id: m.handle.clone(),
                label: if m.provider_name.is_empty() { m.name.clone() } else { format!("{} \u{b7} {}", m.name, m.provider_name) },
                makes,
                ports: makes.ports().to_vec(),
                available: m.connected,
                requires: (!m.connected).then(|| m.provider_name.clone()),
                aspects: schema.map(|s| enum_strings(s, "aspect_ratio")).unwrap_or_default(),
                durations,
                resolutions: schema.map(|s| enum_strings(s, "resolution")).unwrap_or_default(),
            }
        })
        .collect()
}

/// otto's preview as the panel shows it (§6.2): the prompt, the model, the files by port, the size, what stops it.
pub(super) fn settings_preview(p: &CardPreview) -> Preview {
    let c = p.compiled.as_ref();
    let input = c.map(|c| &c.input);
    let size: Vec<String> = ["aspect_ratio", "resolution", "duration"]
        .iter()
        .filter_map(|k| input.and_then(|i| i.get(*k)).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)))
        .collect();
    Preview {
        prompt: c.map(|c| c.prompt.clone()).unwrap_or_default(),
        model: c.map(|c| if c.model_name.is_empty() { c.model.clone() } else { c.model_name.clone() }).filter(|m| !m.is_empty()),
        files: c
            .map(|c| c.files.iter().map(|f| format!("{} {} \u{b7} {}", super::nodes::port_label(port_role(&f.port)), f.index, f.label)).collect())
            .unwrap_or_default(),
        size: (!size.is_empty()).then(|| size.join(" \u{b7} ")),
        error: p.error.as_ref().map(|e| e.message.clone()),
    }
}

/// Models that can't take what is wired (otto's fits) are dimmed, with its reason.
pub(super) fn dim_unfit(choices: &mut [board::settings::Choice], fits: &[otto_proto::ModelFit]) {
    for c in choices {
        if let Some(f) = c.value.as_ref().and_then(|v| fits.iter().find(|f| &f.model == v))
            && !f.fits
            && c.enabled
        {
            c.enabled = false;
            c.reason = Some(f.reason.clone().unwrap_or_else(|| "Can't take what is wired".into()));
        }
    }
}

/// A choice's menu row: a dimmed one says why.
pub(super) fn choice_label(c: &board::settings::Choice) -> String {
    match (&c.reason, c.enabled) {
        (Some(r), false) => format!("{} \u{2014} {r}", c.label),
        _ => c.label.clone(),
    }
}

fn port_role(name: &str) -> silo_proto::studio::EdgeRole {
    serde_json::from_value(Value::String(name.to_owned())).unwrap_or(silo_proto::studio::EdgeRole::Unknown)
}

fn label(text: &'static str) -> gpui::Div {
    div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(text)
}

/// A row: its label over the control.
fn row(name: &'static str, control: impl IntoElement) -> gpui::Div {
    div().flex().flex_col().gap(px(4.0)).child(label(name)).child(control)
}

/// A button showing a choice; a click opens its menu.
fn chooser(id: impl Into<gpui::ElementId>, text: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(9.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .continuous(crate::radius::CONTROL + 1.0)
        .bg(theme::control_fill())
        .cursor_pointer()
        .hover(|s| s.bg(theme::row_fill()))
        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(text))
        .child(icon("chevron-down", 11.0, theme::muted()))
}

fn switch(id: impl Into<gpui::ElementId>, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(30.0))
        .h(px(18.0))
        .p(px(2.0))
        .rounded_full()
        .cursor_pointer()
        .bg(rgb(if on { theme::accent() } else { ink(0.2) }))
        .flex()
        .when(on, |el| el.justify_end())
        .child(div().size(px(14.0)).rounded_full().bg(rgb(0x00ff_ffff)))
}

#[allow(clippy::too_many_lines)]
pub(super) fn render(view: &mut CanvasView, window: &mut Window, cx: &mut Context<CanvasView>) -> Option<AnyElement> {
    let node = view.settings.as_ref()?.node.clone();
    let Some(s) = view.card_settings(&node, cx) else {
        view.settings = None;
        return None;
    };
    let laid = view.board.laid(&node)?;
    let camera = view.board.camera();
    let r = camera.rect_to_view(laid.outer());
    let bounds = view.bounds.get();
    let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let right = r.right() as f32 + 12.0;
    let left = if right + W + 8.0 <= bw { right } else { (r.x as f32 - W - 12.0).max(8.0).min(bw - W - 8.0).max(8.0) };
    let top = (r.y as f32).clamp(8.0, (bh - 200.0).max(8.0));
    let p = view.settings.as_ref()?;
    let n = node.clone();

    // Title, model.
    let mut col = div().flex().flex_col().gap(px(12.0));
    col = col.child(row("Title", div().child(p.title.clone())));
    let model_text = s.model.selected().map_or_else(|| "Automatic".to_owned(), |c| c.label.clone());
    col = col.child(row(
        "Model",
        chooser("studio-set-model", model_text)
            .on_click(cx.listener(move |this, _, window, cx| this.open_picker(PickerFor::Model(n.clone()), window, cx))),
    ));
    // Format, length, resolution, voice: menus.
    let fields: [(&'static str, Option<&ChoiceField>, ToChange); 4] = [
        ("Format", s.format.as_ref(), SettingChange::Aspect),
        ("Length", s.length.as_ref(), |v| SettingChange::Duration(v.and_then(|v| v.parse().ok()))),
        ("Resolution", s.resolution.as_ref(), SettingChange::Resolution),
        ("Voice", s.voice.as_ref(), SettingChange::Voice),
    ];
    for (name, field, change) in fields {
        let Some(field) = field.cloned() else { continue };
        let text = field.selected().map_or_else(|| field.choices.first().map(|c| c.label.clone()).unwrap_or_default(), |c| c.label.clone());
        let n = node.clone();
        col = col.child(row(
            name,
            chooser(gpui::ElementId::Name(format!("studio-set-{name}").into()), text).on_click(cx.listener(
                move |this, e: &gpui::ClickEvent, _, cx| {
                    this.choice_menu(&n, &field, change, e.position());
                    cx.notify();
                },
            )),
        ));
    }
    if let Some(on) = s.style {
        let n = node.clone();
        col = col.child(div().flex().items_center().gap(px(8.0)).child(div().flex_1().child("Use the project's style")).child(
            switch("studio-set-style", on).on_click(cx.listener(move |this, _, window, cx| {
                let fx = this.board.change_setting(&n, &SettingChange::Style(!on));
                this.apply(fx, window, cx);
            })),
        ));
    }

    // The model's own options.
    let options = view.board.canvas().node(&node).and_then(|n| match &n.body {
        NodeBody::Gen { options, mode, .. } => Some((options.clone(), mode.clone())),
        _ => None,
    });
    let schema = view.effective_model().and_then(|m| view.schema(&m)).and_then(|sc| {
        let mode = options.as_ref().and_then(|(_, m)| m.clone());
        sc.modes.iter().find(|x| Some(&x.mode) == mode.as_ref()).or_else(|| sc.modes.first()).cloned()
    });
    if let (Some(mode), Some((values, _))) = (schema, options) {
        let media: Vec<String> = mode.fields.iter().map(|f| f.name.split(['[', '.']).next().unwrap_or(&f.name).to_owned()).collect();
        let props = mode.schema["properties"].as_object().cloned().unwrap_or_default();
        let mut extra = div().flex().flex_col().gap(px(10.0));
        let mut any = false;
        for (key, spec) in props {
            if COVERED.contains(&key.as_str()) || media.contains(&key) {
                continue;
            }
            let now = values.get(&key).cloned().unwrap_or(Value::Null);
            let kind = spec["type"].as_str().unwrap_or("");
            let title = spec["title"].as_str().map_or_else(|| key.replace('_', " "), str::to_owned);
            let control: AnyElement = if let Some(choices) = spec["enum"].as_array().cloned() {
                let text = if now.is_null() {
                    spec.get("default").map_or_else(
                        || "Default".to_owned(),
                        |d| format!("Default \u{b7} {}", d.as_str().map_or_else(|| d.to_string(), str::to_owned)),
                    )
                } else {
                    now.as_str().map_or_else(|| now.to_string(), str::to_owned)
                };
                let (n, k) = (node.clone(), key.clone());
                chooser(gpui::ElementId::Name(format!("studio-opt-{key}").into()), text)
                    .on_click(cx.listener(move |this, e: &gpui::ClickEvent, _, cx| {
                        let mut items = vec![
                            Item::action("Default", "", Pick::Setting(n.clone(), SettingChange::Option(k.clone(), Value::Null)))
                                .checked(now.is_null()),
                        ];
                        items.extend(choices.iter().map(|c| {
                            let l = c.as_str().map_or_else(|| c.to_string(), str::to_owned);
                            Item::action(l, "", Pick::Setting(n.clone(), SettingChange::Option(k.clone(), c.clone()))).checked(*c == now)
                        }));
                        this.menu = Some(Menu::new(e.position(), items));
                        cx.notify();
                    }))
                    .into_any_element()
            } else if kind == "boolean" {
                let on = now.as_bool().or_else(|| spec["default"].as_bool()).unwrap_or(false);
                let (n, k) = (node.clone(), key.clone());
                switch(gpui::ElementId::Name(format!("studio-opt-{key}").into()), on)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let fx = this.board.change_setting(&n, &SettingChange::Option(k.clone(), Value::Bool(!on)));
                        this.apply(fx, window, cx);
                    }))
                    .into_any_element()
            } else if matches!(kind, "number" | "integer" | "string") {
                let field = view.option_field(&node, &key, kind, &now, window, cx);
                field.into_any_element()
            } else {
                continue;
            };
            any = true;
            extra = extra.child(
                div().flex().flex_col().gap(px(4.0)).child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(title)).child(control),
            );
        }
        if any {
            col = col.child(div().h(px(1.0)).bg(crate::surface::hairline())).child(label("Model options")).child(extra);
        }
    }

    // Inputs, numbered as the model gets them.
    if !s.inputs.is_empty() {
        let rows = s.inputs.iter().map(|i| {
            let (from, edge) = (i.from.clone(), i.edge.clone());
            div()
                .id(gpui::ElementId::Name(format!("studio-input-{}", i.edge).into()))
                .flex()
                .items_center()
                .gap(px(6.0))
                .h(px(24.0))
                .px(px(4.0))
                .continuous(crate::radius::MENU_ROW)
                .cursor_pointer()
                .hover(|s| s.bg(theme::row_fill()))
                .child(div().w(px(16.0)).text_color(rgb(theme::muted())).child(format!("{}", i.n)))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(i.label.clone()))
                .children(i.problem.clone().map(|p| {
                    div()
                        .flex_none()
                        .max_w(px(110.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(11.0))
                        .text_color(rgb(theme::danger()))
                        .child(p)
                }))
                .when(i.pending, |el| el.child(div().text_size(px(11.0)).text_color(rgb(theme::muted())).child("waiting")))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let fx = this.board.reveal(&from);
                    this.apply(fx, window, cx);
                }))
                .child(
                    div()
                        .id(gpui::ElementId::Name(format!("studio-unwire-{}", i.edge).into()))
                        .size(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .continuous(4.0)
                        .hover(|s| s.bg(theme::hover_fill()))
                        .child(icon("close", 9.0, theme::muted()))
                        .tooltip(crate::tooltip::Tooltip::text("Unwire"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            let fx = this.board.unwire(&edge);
                            this.apply(fx, window, cx);
                        })),
                )
        });
        col = col.child(div().h(px(1.0)).bg(crate::surface::hairline())).child(label("Inputs")).child(div().flex().flex_col().children(rows));
    }

    // The request it would send.
    let request: AnyElement = match &s.preview {
        Some(pv) => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .text_size(px(11.5))
            .children(pv.error.clone().map(|e| div().text_color(rgb(theme::danger())).child(e)))
            .children(pv.model.clone().map(|m| div().font_weight(FontWeight::MEDIUM).child(m)))
            .children(pv.size.clone().map(|z| div().text_color(rgb(theme::muted())).child(z)))
            .children(pv.files.iter().map(|f| div().text_color(rgb(theme::muted())).child(f.clone())))
            .when(!pv.prompt.is_empty(), |el| {
                el.child(
                    div().mt(px(2.0)).p(px(6.0)).continuous(crate::radius::CONTROL).bg(theme::well_fill()).line_clamp(8).child(pv.prompt.clone()),
                )
            })
            .into_any_element(),
        None => div().text_size(px(11.5)).text_color(rgb(theme::muted())).child("Reading\u{2026}").into_any_element(),
    };
    col = col.child(div().h(px(1.0)).bg(crate::surface::hairline())).child(label("Request")).child(request);

    let header = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(icon(crate::studio::tiles::kind_glyph(s.makes), 13.0, theme::muted()))
        .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(format!("{} settings", s.makes.label())))
        .child(
            div()
                .id("studio-settings-close")
                .size(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .continuous(5.0)
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
                .child(icon("close", 10.0, theme::muted()))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings = None;
                    cx.notify();
                })),
        );
    Some(
        div()
            .id("studio-settings")
            .occlude()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(W))
            .max_h(px((bh - top - 8.0).max(160.0)))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(12.0))
            .glass(crate::radius::CARD)
            .text_size(px(12.5))
            .text_color(rgb(theme::foreground()))
            .child(header)
            .child(col)
            .into_any_element(),
    )
}

impl CanvasView {
    /// The text field of option `key`, made on first show; ↩ sets it (empty: unset).
    fn option_field(&mut self, node: &str, key: &str, kind: &str, now: &Value, window: &mut Window, cx: &mut Context<Self>) -> Entity<TextField> {
        if let Some(f) = self.settings.as_ref().and_then(|p| p.fields.get(key)) {
            return f.clone();
        }
        let text = if now.is_null() { String::new() } else { now.as_str().map_or_else(|| now.to_string(), str::to_owned) };
        let field = cx.new(|cx| {
            let mut f = TextField::new("Default", cx);
            f.set_text(text, cx);
            f
        });
        let (n, k, kind) = (node.to_owned(), key.to_owned(), kind.to_owned());
        let sub = cx.subscribe_in(&field, window, move |this, f, e: &FieldEvent, window, cx| {
            if !matches!(e, FieldEvent::Submit) {
                return;
            }
            let t = f.read(cx).text.trim().to_owned();
            let v = match kind.as_str() {
                _ if t.is_empty() => Value::Null,
                "integer" => t.parse::<i64>().map_or(Value::String(t), Value::from),
                "number" => t.parse::<f64>().map_or(Value::String(t), Value::from),
                _ => Value::String(t),
            };
            let fx = this.board.change_setting(&n, &SettingChange::Option(k.clone(), v));
            this.apply(fx, window, cx);
            window.focus(&this.focus, cx);
        });
        if let Some(p) = self.settings.as_mut() {
            p.fields.insert(key.to_owned(), field.clone());
            p._subs.push(sub);
        }
        field
    }
}
