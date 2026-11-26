//! The World view (docs/STUDIO.md §4.3): the section switcher (Overview, Style, each section with its count,
//! `+ Section`), Overview and Style rendered with an inline Edit, a section's entity grid (`New …`, Create with
//! agent), and an entity's detail (its sheet, the default look; its named references drawn by their file with their
//! names and what each defines, then Add reference; fields, Goes with (its links), description, Used in, the agent's
//! actions for its kind of entity; the sheet and image and video refs open in the media viewer, audio plays in
//! place; right-click renames, describes, reorders or removes a reference, or opens it in Finder or its default app
//! to fix by hand). Cards and the page's head show the entity's state marks (`production::marks`), a prop's card
//! whose it is; the page has Approve and the model chips of its Generate buttons.

use gpui::{AnyElement, AppContext as _, ClickEvent, Context, MouseButton, ObjectFit, Window, div, img, prelude::*, px, rgb};
use silo_proto::studio::{EntityInfo, RefInfo, Section, SectionKind, StudioSetParams, mention::Target};

use super::{
    Dialog, EditTarget, MenuAction, StudioPane, StudioPaneEvent, centered, model, production,
    store::{Asset, AssetKind, Load},
    tiles,
};
use crate::{
    agent::editor::{Editor, EditorColors},
    context_menu::{Item, Menu},
    studio_block::Then,
    text_field::TextField,
    theme::{self, Continuous as _},
};

/// Entity cards: at least this wide, this far apart.
const CARD_MIN: f32 = 150.0;
const GAP: f32 = 12.0;

/// The entity page's padding, and the gap between its columns.
const PAGE_PAD: f32 = 16.0;
const COLUMN_GAP: f32 = 20.0;
/// Side by side from this inner width; the sheet's column is half of it, within these.
const WIDE_MIN: f32 = 560.0;
const LEFT_MIN: f32 = 240.0;
const LEFT_MAX: f32 = 640.0;
/// A sheet's shape before it loads (and without one).
const SHEET_PLACEHOLDER: f32 = 4.0 / 3.0;
/// Ref thumbnails (an audio ref's player is this wide instead).
const REF: f32 = 90.0;
/// A reference tile's side: room under it for its name and two lines of what it defines.
const REF_TILE: f32 = 124.0;
const AUDIO_REF: f32 = 220.0;

/// How an entity's page is laid out in a body `w`×`h`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EntityLayout {
    /// Sheet and refs in a column on the left, the rest on the right; else stacked.
    pub wide: bool,
    /// The sheet's column (stacked: the page's inner width).
    pub left: f32,
    /// The box the sheet is contained in.
    pub sheet_max: (f32, f32),
}

#[must_use]
pub(crate) fn entity_layout(w: f32, h: f32) -> EntityLayout {
    let inner = (w - 2.0 * PAGE_PAD).max(0.0);
    if inner >= WIDE_MIN {
        let left = ((inner - COLUMN_GAP) / 2.0).clamp(LEFT_MIN, LEFT_MAX);
        // Room under it for a row of refs.
        let sheet_max = (left, (h - 2.0 * PAGE_PAD - 36.0 - REF - 20.0).clamp(200.0, 640.0));
        EntityLayout { wide: true, left, sheet_max }
    } else {
        EntityLayout { wide: false, left: inner, sheet_max: (inner.max(120.0), (h * 0.5).clamp(200.0, 420.0)) }
    }
}

/// The sheet's box: its pixels' aspect (`dims`; the placeholder's before they are known), as large as fits `max`.
#[must_use]
pub(crate) fn contain(dims: Option<(u32, u32)>, max: (f32, f32)) -> (f32, f32) {
    #[allow(clippy::cast_precision_loss)]
    let ratio = dims.filter(|(w, h)| *w > 0 && *h > 0).map_or(SHEET_PLACEHOLDER, |(w, h)| w as f32 / h as f32);
    let (mw, mh) = (max.0.max(1.0), max.1.max(1.0));
    if mw / mh > ratio { (mh * ratio, mh) } else { (mw, mw / ratio) }
}

/// A root-relative path as the entity's frontmatter writes it (relative to its folder `world/<id>/`).
#[must_use]
pub(crate) fn entity_relative(id: &str, path: &str) -> String {
    path.strip_prefix(&format!("world/{id}/")).unwrap_or(path).to_owned()
}

/// The menu rows for `actions` on file `path` of entity `entity`; `look`: the reference it is, at its place
/// among `count` (the sheet has none, and no row needing one).
fn file_menu(actions: &[model::FileAction], entity: &str, path: &str, look: Option<(&RefInfo, usize, usize)>) -> Vec<Item<MenuAction>> {
    use model::FileAction as A;
    actions
        .iter()
        .filter_map(|a| {
            let (entity, path) = (entity.to_owned(), path.to_owned());
            let id = look.map(|(l, ..)| l.id.clone());
            let action = match a {
                A::SetSheet => MenuAction::SetSheet(entity, path),
                A::RevealInFinder => MenuAction::RevealInFinder(path),
                A::OpenInApp => MenuAction::OpenInApp(path),
                A::OpenInFiles => MenuAction::OpenInFiles(path),
                A::Rename => MenuAction::RenameRef(entity, id?),
                A::Describe => MenuAction::DescribeRef(entity, id?),
                A::Remove => MenuAction::RemoveRef(entity, id?),
                A::MoveEarlier | A::MoveLater => {
                    let (l, at, count) = look?;
                    MenuAction::MoveRef(entity, l.id.clone(), model::moved_to(*a, at, count)?)
                }
                A::CopyMention => MenuAction::CopyText(Target::Ref { entity, id: id? }.token()),
            };
            Some(Item::action(a.label(), "", action))
        })
        .collect()
}

/// The "Goes with" add menu of entity `id`: the other entities by section (headed, apart), each linking it.
fn link_items(sections: &[Section], id: &str, linked: &[String]) -> Vec<Item<MenuAction>> {
    let mut items = Vec::new();
    for (_, title, entities) in model::link_menu(sections, id, linked) {
        if !items.is_empty() {
            items.push(Item::Separator);
        }
        items.push(Item::action(title.to_owned(), "", MenuAction::CopyText(String::new())).enabled(false));
        for e in entities {
            items.push(Item::action(model::entity_name(e), "", MenuAction::Link(id.to_owned(), e.id.clone(), true)));
        }
    }
    if items.is_empty() {
        items.push(Item::action("No other entities yet", "", MenuAction::CopyText(String::new())).enabled(false));
    }
    items
}

/// The model kinds an entity's Generate buttons use, in their order, once each (`image`, then `video`).
#[must_use]
pub(crate) fn entity_model_kinds(kind: model::EntityKind) -> Vec<&'static str> {
    let mut out = Vec::new();
    for k in kind.asks().iter().filter_map(|a| a.model_kind()) {
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

/// A small text button in the app's pill look.
pub(super) fn pill(id: impl Into<gpui::ElementId>, label: impl Into<gpui::SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(10.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .continuous(7.0)
        .text_size(px(12.0))
        .bg(theme::control_fill())
        .cursor_pointer()
        .hover(|s| s.bg(theme::row_fill()))
        .child(label.into())
}

impl StudioPane {
    pub(super) fn render_world(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (sections, error) = {
            let s = self.store.read(cx);
            (s.sections.clone(), s.world_error.clone())
        };
        let Some(mut sections) = sections else { return centered(error.unwrap_or_else(|| "Loading…".into())) };
        for id in &self.new_sections {
            if !sections.iter().any(|s| s.id == *id) {
                let title = crate::files::model::name(id).to_owned();
                sections.push(Section { id: id.clone(), title, kind: SectionKind::Entities, entities: Vec::new(), refs: Vec::new() });
            }
        }
        let shown = model::shown_section(&sections, self.state.world_section.as_deref()).cloned();
        let switcher = self.switcher(&sections, shown.as_ref().map(|s| s.id.as_str()), cx);
        let body = match (&shown, self.state.world_entity.clone()) {
            (_, Some(id)) => self.render_entity(&id, window, cx),
            (Some(s), None) if s.kind == SectionKind::Overview || s.kind == SectionKind::Style => {
                let path = if s.kind == SectionKind::Overview { "world/overview.md" } else { "world/style.md" };
                let doc = self.render_doc(path, cx);
                if s.kind == SectionKind::Style && !s.refs.is_empty() {
                    let strip = self.style_refs(&s.refs, window, cx);
                    div().size_full().flex().flex_col().child(strip).child(div().flex_1().min_h_0().child(doc)).into_any_element()
                } else {
                    doc
                }
            }
            (Some(s), None) => self.render_grid(s, window, cx),
            (None, None) => centered("No world yet".into()),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(switcher)
            .child(div().h(px(1.0)).bg(crate::surface::hairline()))
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn switcher(&self, sections: &[Section], shown: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div().id("studio-switcher").flex_none().flex().items_center().gap(px(2.0)).px(px(12.0)).py(px(6.0)).overflow_x_scroll();
        for s in model::switcher(sections) {
            let on = shown == Some(s.id.as_str()) && self.state.world_entity.is_none();
            let id = s.id.clone();
            row = row.child(
                div()
                    .id(gpui::ElementId::Name(format!("studio-section-{}", s.id).into()))
                    .flex_none()
                    .px(px(9.0))
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .continuous(7.0)
                    .text_size(px(12.5))
                    .cursor_pointer()
                    .when(on, |el| el.bg(theme::row_fill()))
                    .when(!on, |el| el.text_color(rgb(theme::muted())).hover(|st| st.bg(theme::control_fill())))
                    .child(model::section_label(s))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set(
                            StudioSetParams { world_section: Some(Some(id.clone())), world_entity: Some(None), ..StudioSetParams::default() },
                            cx,
                        );
                    })),
            );
        }
        row.child(pill("studio-section-new", "+ Section").ml(px(6.0)).on_click(cx.listener(|this, _, window, cx| {
            let field = cx.new(|cx| TextField::new("Section name (Factions)", cx));
            this.open_dialog(Dialog::NewSection { field }, window, cx);
        })))
        .into_any_element()
    }

    /// The editor's colours.
    fn colors() -> EditorColors {
        EditorColors {
            text: rgb(theme::foreground()).into(),
            placeholder: rgb(theme::muted()).into(),
            caret: rgb(theme::foreground()).into(),
            selection: {
                let mut h: gpui::Hsla = rgb(theme::selection()).into();
                h.a = theme::selection_alpha();
                h
            },
        }
    }

    /// The inline editor for `target`, with Save and Cancel, if it is the one being edited.
    pub(super) fn editor_for(&self, target: &EditTarget, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.editing.as_ref().filter(|e| e.target == *target)?;
        let saving = e.saving;
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .p(px(16.0))
                .child(div().px(px(10.0)).py(px(8.0)).continuous(7.0).bg(theme::well_fill()).text_size(px(13.0)).child(Editor::element(
                    &e.editor,
                    30,
                    Self::colors(),
                    cx,
                )))
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(
                            pill("studio-edit-save", if saving { "Saving…" } else { "Save" })
                                .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
                        )
                        .child(pill("studio-edit-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| {
                            this.editing = None;
                            cx.notify();
                        })))
                        .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child("⌘↩ saves, esc cancels")),
                )
                .into_any_element(),
        )
    }

    /// The Style section's reference images (every image under `world/style/`), a strip over its text.
    fn style_refs(&mut self, refs: &[String], window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tiles = refs.iter().map(|r| {
            let path = r.clone();
            div()
                .id(gpui::ElementId::Name(format!("studio-style-ref-{r}").into()))
                .flex_none()
                .cursor_pointer()
                .child(tiles::image(&self.store, r, 120.0, 90.0, window, cx))
                .tooltip(crate::tooltip::Tooltip::text(crate::files::model::name(r).to_owned()))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(StudioPaneEvent::OpenFile(path.clone()))))
        });
        let tiles: Vec<_> = tiles.collect();
        div().id("studio-style-refs").flex_none().flex().gap(px(8.0)).px(px(16.0)).pt(px(14.0)).overflow_x_scroll().children(tiles).into_any_element()
    }

    /// Overview or Style: the file rendered, Edit over it.
    fn render_doc(&mut self, path: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let target = EditTarget::File(path.to_owned());
        if let Some(editor) = self.editor_for(&target, cx) {
            return div().id("studio-doc-edit").size_full().overflow_y_scroll().child(editor).into_any_element();
        }
        let asset = self.store.update(cx, |s, cx| s.asset(path, AssetKind::Text, cx));
        let text: Option<std::sync::Arc<str>> = match asset {
            Asset::Text(t) => Some(t),
            Asset::Failed => Some("".into()),
            _ => None,
        };
        let Some(text) = text else { return centered("Loading…".into()) };
        let content = if text.trim().is_empty() { centered("Nothing written yet".into()) } else { self.markdown(path, &text, 16.0) };
        let edit = pill("studio-doc-edit-button", "Edit").on_click(cx.listener(move |this, _, window, cx| {
            this.start_edit(EditTarget::File(path.to_owned()), &text, window, cx);
        }));
        div().relative().size_full().child(content).child(div().absolute().top(px(10.0)).right(px(14.0)).child(edit)).into_any_element()
    }

    fn render_grid(&mut self, section: &Section, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let width = self.bounds().map_or(600.0, |b| f32::from(b.size.width)) - 32.0;
        let columns = model::grid_columns(width, CARD_MIN, GAP);
        #[allow(clippy::cast_precision_loss)]
        let card_w = ((width - GAP * (columns as f32 - 1.0)) / columns as f32).max(CARD_MIN.min(width));
        let mut grid = div().flex().flex_wrap().gap(px(GAP)).p(px(16.0));
        for e in &section.entities {
            grid = grid.child(self.entity_card(e, card_w, window, cx));
        }
        let one = model::singular(&section.title);
        let (new_id, agent_id) = (section.id.clone(), section.id.clone());
        grid = grid.child(
            div()
                .id("studio-entity-new")
                .w(px(card_w))
                .h(px(card_w * 1.25 + 44.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .continuous(crate::agent::media::RADIUS)
                .border_1()
                .border_dashed()
                .border_color(theme::control_fill())
                .text_color(rgb(theme::muted()))
                .child(
                    pill("studio-entity-new-button", format!("New {one}"))
                        .on_click(cx.listener(move |this, _, window, cx| this.pick(Some(MenuAction::NewEntity(new_id.clone())), window, cx))),
                )
                .child(pill("studio-entity-agent", "Create with agent").on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                    let items = vec![
                        Item::action(
                            format!("New {}…", model::singular(&this.section_title(&agent_id, cx))),
                            "",
                            MenuAction::NewEntity(agent_id.clone()),
                        ),
                        Item::action("Create with agent…", "", MenuAction::CreateWithAgent(agent_id.clone())),
                    ];
                    this.open_menu(Menu::new(e.position(), items), cx);
                }))),
        );
        div().id("studio-grid").size_full().overflow_y_scroll().child(grid).into_any_element()
    }

    fn entity_card(&mut self, e: &EntityInfo, w: f32, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let face = match &e.sheet {
            Some(sheet) => tiles::image(&self.store, sheet, w, w * 1.25, window, cx),
            None => crate::agent::media::frame(w, w * 1.25, None).child(crate::assets::icon("user", 22.0, theme::muted())).into_any_element(),
        };
        let id = e.id.clone();
        // A prop that goes with one character says whose it is; otherwise nothing.
        let owner = self.store.read(cx).sections.as_deref().and_then(|s| model::owner(s, e)).map(|o| model::possessive(&model::entity_name(o)));
        div()
            .id(gpui::ElementId::Name(format!("studio-entity-{}", e.id).into()))
            .w(px(w))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .cursor_pointer()
            .child(face)
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(5.0))
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(e.name.clone()),
                    )
                    .children(owner.map(|o| {
                        div()
                            .flex_none()
                            .max_w(px(w * 0.5))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.5))
                            .text_color(rgb(theme::muted()))
                            .child(o)
                    })),
            )
            .children(e.summary.clone().map(|s| div().text_size(px(11.5)).text_color(rgb(theme::muted())).line_clamp(2).child(s)))
            .child(production::marks_row(&production::entity_marks(self.store.read(cx), &e.id), true))
            .on_click(cx.listener(move |this, _, _, cx| this.show_entity(id.clone(), cx)))
            .into_any_element()
    }

    /// An entity's page: on a wide body the sheet and refs in a column on the left, the rest on the right (each
    /// scrolling on its own); on a narrow one stacked, the page scrolling as one. The description is at its full
    /// height: its column or the page is the one scroller.
    fn render_entity(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let entity = self.store.update(cx, |s, cx| s.entity(id, cx));
        let entity = match entity {
            Load::Ready(e) => e,
            Load::Loading => return centered("Loading…".into()),
            Load::Failed(e) => return centered(e),
        };
        let muted = theme::muted();
        let (bw, bh) = self.bounds().map_or((900.0, 700.0), |b| (f32::from(b.size.width), f32::from(b.size.height)));
        let layout = entity_layout(bw, bh);
        let back = div().flex_none().flex().child(pill("studio-entity-back", "\u{2039} Back").on_click(cx.listener(|this, _, _, cx| {
            this.set(StudioSetParams { world_entity: Some(None), ..StudioSetParams::default() }, cx);
        })));
        let sheet = self.entity_sheet(id, &entity.info, layout.sheet_max, window, cx);
        let looks = model::entity_looks(&entity.info);
        let mut refs = div().w_full().flex().flex_wrap().items_start().gap(px(10.0));
        for (at, look) in looks.iter().enumerate() {
            refs = refs.child(self.ref_tile(id, look, at, looks.len(), window, cx));
        }
        refs = refs.child(self.add_ref_tile(id, &entity.info.name, cx));
        let heading = div()
            .flex()
            .items_baseline()
            .gap(px(6.0))
            .text_size(px(12.0))
            .child(div().text_color(rgb(muted)).child("References"))
            .when(!looks.is_empty(), |el| el.child(div().text_size(px(11.0)).text_color(rgb(muted)).child(looks.len().to_string())));
        let media = div()
            .flex_none()
            .flex()
            .flex_col()
            .items_start()
            .gap(px(8.0))
            .child(sheet)
            .child(div().w_full().flex().flex_col().gap(px(6.0)).pt(px(4.0)).child(heading).child(refs));
        let fields = entity.fields.iter().filter(|(k, _)| !model::HIDDEN_FIELDS.contains(&k.as_str())).map(|(k, v)| {
            div()
                .flex()
                .gap(px(10.0))
                .text_size(px(12.5))
                .child(div().w(px(90.0)).flex_none().text_color(rgb(muted)).child(k.clone()))
                .child(div().flex_1().min_w_0().child(model::field_text(v)))
        });
        let name = entity.info.name.clone();
        let kind = model::EntityKind::of_entity(id);
        let one = model::singular(&self.section_title(id.split_once('/').map_or(id, |(s, _)| s), cx));
        let has_sheet = entity.info.sheet.is_some();
        let picks = self.store.read(cx).project().map(|p| p.models.clone()).unwrap_or_default();
        let asks = kind.asks().iter().map(|&ask| {
            let (name, eid, one) = (name.clone(), id.to_owned(), one.clone());
            let model = ask.model_kind().and_then(|k| picks.get(k).cloned());
            pill(gpui::ElementId::Name(format!("studio-ask-{}", ask.key()).into()), ask.label(has_sheet)).on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(StudioPaneEvent::Agent(Then::Send(ask.message(kind, &one, &name, &eid, model.as_deref()))));
            }))
        });
        let asks: Vec<_> = asks.collect();
        // Beside them, the model each kind of job uses (one chip per kind: Image, Video), the Models menu on click.
        let chips: Vec<AnyElement> = entity_model_kinds(kind)
            .into_iter()
            .map(|k| self.model_chip_for(k, gpui::ElementId::Name(format!("studio-entity-model-{k}").into()), cx))
            .collect();
        let approve = has_sheet.then(|| self.approve_control(id, entity.info.approval, cx));
        let active = self.state.active.clone();
        let add = {
            let eid = id.to_owned();
            pill("studio-entity-add", "Add to canvas").when(active.is_none(), |el| el.opacity(0.5)).on_click(cx.listener(
                move |this, _, window, cx| {
                    this.add_to_canvas(&eid, window, cx);
                },
            ))
        };
        let head = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(div().text_size(px(18.0)).font_weight(gpui::FontWeight::SEMIBOLD).child(name.clone()))
                    .child(production::marks_row(&production::entity_marks(self.store.read(cx), id), false)),
            )
            .children(entity.info.summary.clone().map(|s| div().text_color(rgb(muted)).child(s)))
            .child(div().flex().flex_col().gap(px(4.0)).children(fields))
            .child(self.goes_with(id, &entity.info.links, window, cx))
            .child(div().flex().flex_wrap().items_center().gap(px(6.0)).children(asks).children(chips))
            .child(div().flex().flex_wrap().items_center().gap(px(6.0)).children(approve).child(add));
        let target = EditTarget::EntityBody(id.to_owned());
        let description = match self.editor_for(&target, cx) {
            Some(editor) => div().flex_none().child(editor),
            None => {
                let body = entity.body.clone();
                let rendered = if body.trim().is_empty() {
                    div().text_color(rgb(muted)).child("No description yet").into_any_element()
                } else {
                    // At full height: a list scrolling inside the page would scroll along with it.
                    self.markdown_flat(&format!("entity:{id}"), &body, cx)
                };
                let eid = id.to_owned();
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(div().flex_none().flex().child(pill("studio-entity-edit", "Edit description").on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.start_edit(EditTarget::EntityBody(eid.clone()), &body, window, cx);
                        },
                    ))))
                    .child(rendered)
            }
        };
        let used = (!entity.used_in.is_empty()).then(|| {
            let rows = entity.used_in.iter().map(|u| {
                let canvas = u.canvas.clone();
                div()
                    .id(gpui::ElementId::Name(format!("studio-used-{}-{}", u.canvas, u.node).into()))
                    .text_size(px(12.5))
                    .text_color(rgb(theme::accent()))
                    .cursor_pointer()
                    .child(format!("{} \u{b7} {}", u.canvas, u.node))
                    .on_click(cx.listener(move |this, _, _, cx| this.show_canvas(canvas.clone(), cx)))
            });
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(div().text_size(px(12.0)).text_color(rgb(muted)).child("Used in"))
                .child(div().flex().flex_col().gap(px(2.0)).children(rows))
        });
        if layout.wide {
            let left = div().id("studio-entity-media").flex_none().w(px(layout.left)).h_full().overflow_y_scroll().child(media);
            let right = div()
                .id("studio-entity-info")
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(head)
                .child(description)
                .children(used);
            div()
                .size_full()
                .p(px(PAGE_PAD))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(back)
                .child(div().flex_1().min_h_0().flex().gap(px(COLUMN_GAP)).child(left).child(right))
                .into_any_element()
        } else {
            div()
                .id("studio-entity")
                .size_full()
                .overflow_y_scroll()
                .p(px(PAGE_PAD))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(back)
                .child(media)
                .child(head)
                .child(description)
                .children(used)
                .into_any_element()
        }
    }

    /// A named reference: its face by its file (an image; a video's poster with its length and play badge, viewed
    /// on click; audio as the canvas's player, played in place), its name under it and what it defines as a second
    /// muted line; the menu (rename, describe, sheet, order, by hand, mention, remove) on right-click.
    fn ref_tile(&mut self, id: &str, look: &RefInfo, at: usize, count: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let media = model::RefMedia::of_look(look);
        let path = look.path.clone();
        let (face, w) = match media {
            model::RefMedia::Image => (tiles::image(&self.store, &path, REF_TILE, REF_TILE, window, cx), REF_TILE),
            model::RefMedia::Video => (tiles::video(&self.store, &path, None, REF_TILE, REF_TILE, window, cx), REF_TILE),
            model::RefMedia::Audio => (tiles::audio(&self.store, &path, AUDIO_REF, window, cx), AUDIO_REF),
        };
        let (name, defines) = model::ref_caption(look);
        let actions = model::ref_menu(media, at, count, self.local);
        let (eid, menu_look) = (id.to_owned(), look.clone());
        let viewed = (id.to_owned(), path);
        let viewable = media != model::RefMedia::Audio;
        div()
            .id(gpui::ElementId::Name(format!("studio-ref-{}", look.id).into()))
            .flex_none()
            .w(px(w))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(face)
            .child(div().text_size(px(12.0)).font_weight(gpui::FontWeight::MEDIUM).overflow_hidden().whitespace_nowrap().text_ellipsis().child(name))
            .children(
                defines
                    .map(|d| div().text_size(px(11.0)).line_height(px(14.0)).text_color(rgb(theme::muted())).line_clamp(2).text_ellipsis().child(d)),
            )
            .tooltip(crate::tooltip::Tooltip::text(model::ref_tip(look)))
            .when(viewable, |el| {
                el.cursor_pointer().on_click(cx.listener(move |this, _, window, cx| this.view_entity_media(&viewed.0, &viewed.1, window, cx)))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                    let items = file_menu(&actions, &eid, &menu_look.path, Some((&menu_look, at, count)));
                    this.open_menu(Menu::new(e.position, items), cx);
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    /// The tile after the references: ask the linked agent for a look (the usual way), or pick a file on this Mac
    /// (copied into the entity's `refs/` folder; a remote host's studio can't take this Mac's files).
    fn add_ref_tile(&mut self, id: &str, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let kind = model::EntityKind::of_entity(id);
        let pick = self.store.read(cx).project().and_then(|p| p.models.get("image").cloned());
        let message = model::EntityAsk::Look.message(kind, "", name, id, pick.as_deref());
        let tip = format!("Asks the agent: {message}");
        let ask = pill("studio-ref-add-agent", model::EntityAsk::Look.label(false))
            .px(px(8.0))
            .tooltip(crate::tooltip::Tooltip::text(tip))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(StudioPaneEvent::Agent(Then::Send(message.clone())))));
        let file = self.local.then(|| {
            let eid = id.to_owned();
            pill("studio-ref-add-file", "File\u{2026}")
                .px(px(8.0))
                .tooltip(crate::tooltip::Tooltip::text("Pick a file on this Mac: it is copied into the entity's refs folder"))
                .on_click(cx.listener(move |this, _, _, cx| this.add_ref_files(eid.clone(), cx)))
        });
        div()
            .id("studio-ref-add")
            .flex_none()
            .w(px(REF_TILE))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .size(px(REF_TILE))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .continuous(crate::agent::media::RADIUS)
                    .border_1()
                    .border_dashed()
                    .border_color(theme::control_fill())
                    .child(ask)
                    .children(file),
            )
            .child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child("Add reference"))
            .into_any_element()
    }

    /// "Goes with": the entities linked with it (both ends) as chips with their sheet and name (a click opens one,
    /// right-click unlinks it), and `+ Add`, its menu the world's other entities by section.
    fn goes_with(&mut self, id: &str, links: &[String], window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let sections = self.store.read(cx).sections.clone().unwrap_or_default();
        let muted = theme::muted();
        let mut row = div().flex_1().min_w_0().flex().flex_wrap().items_center().gap(px(6.0));
        for other in links {
            let e = model::find_entity(&sections, other);
            let name = e.map_or_else(|| other.rsplit('/').next().unwrap_or(other).to_owned(), model::entity_name);
            let thumb = match e.and_then(|e| e.sheet.clone()) {
                Some(sheet) => tiles::image(&self.store, &sheet, 18.0, 18.0, window, cx),
                None => crate::agent::media::frame(18.0, 18.0, None).child(crate::assets::icon("user", 10.0, muted)).into_any_element(),
            };
            let (open, eid, unlink) = (other.clone(), id.to_owned(), other.clone());
            row = row.child(
                div()
                    .id(gpui::ElementId::Name(format!("studio-link-{other}").into()))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .h(px(24.0))
                    .pl(px(3.0))
                    .pr(px(9.0))
                    .continuous(7.0)
                    .text_size(px(12.0))
                    .bg(theme::control_fill())
                    .hover(|s| s.bg(theme::row_fill()))
                    .cursor_pointer()
                    .child(thumb)
                    .child(name.clone())
                    .tooltip(crate::tooltip::Tooltip::text(format!("Open {name}; right-click to unlink")))
                    .on_click(cx.listener(move |this, _, _, cx| this.show_entity(open.clone(), cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                            let items = vec![Item::action("Remove link", "", MenuAction::Link(eid.clone(), unlink.clone(), false))];
                            this.open_menu(Menu::new(e.position, items), cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        let eid = id.to_owned();
        row = row.child(pill("studio-link-add", if links.is_empty() { "+ Link\u{2026}" } else { "+" }).on_click(cx.listener(
            move |this, e: &ClickEvent, _, cx| {
                let (sections, linked) = {
                    let s = this.store.read(cx);
                    (s.sections.clone().unwrap_or_default(), s.entity_info(&eid).map(|i| i.links.clone()).unwrap_or_default())
                };
                this.open_menu(Menu::new(e.position(), link_items(&sections, &eid, &linked)), cx);
            },
        )));
        div()
            .flex()
            .items_start()
            .gap(px(10.0))
            .text_size(px(12.5))
            .child(div().w(px(90.0)).flex_none().pt(px(4.0)).text_color(rgb(muted)).child("Goes with"))
            .child(row)
            .into_any_element()
    }

    /// The sheet's approval: `Approve`; `Approved ✓` (a click withdraws it); `Changed since approved` with
    /// `Approve again`. Shown as the server answers (the store has no optimistic entity writes).
    fn approve_control(&mut self, id: &str, approval: silo_proto::studio::Approval, cx: &mut Context<Self>) -> AnyElement {
        let (label, note, set) = production::approve_button(approval);
        let busy = self.store.read(cx).approving.contains(id);
        let tip = if set { "Mark this sheet as final: the agent keeps to it" } else { "Approved: click to withdraw the approval" };
        let eid = id.to_owned();
        let button = pill("studio-entity-approve", label)
            .when(!set, |el| el.text_color(rgb(theme::success())))
            .when(busy, |el| el.opacity(0.5))
            .tooltip(crate::tooltip::Tooltip::text(tip))
            .on_click(cx.listener(move |this, _, _, cx| this.store.update(cx, |s, cx| s.approve(&eid, set, cx))));
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .children(note.map(|n| div().text_size(px(12.0)).text_color(rgb(theme::warning())).child(n)))
            .child(button)
            .into_any_element()
    }

    /// Entity `id`'s sheet, whole: contained at its own aspect in `max` (a placeholder's shape until it loads);
    /// a click opens it in the media viewer.
    fn entity_sheet(&mut self, id: &str, info: &EntityInfo, max: (f32, f32), window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let Some(path) = info.sheet.clone() else {
            let (w, h) = contain(None, max);
            return crate::agent::media::frame(w, h, None).child(crate::assets::icon("user", 28.0, muted)).into_any_element();
        };
        let face = match self.store.update(cx, |s, cx| s.asset(&path, AssetKind::Image, cx)) {
            Asset::Image(l) => {
                let (w, h) = contain(Some(l.dims), max);
                // The full image (the preview is tile-sized); an SVG's preview is its raster.
                let shown = if l.svg.is_some() { l.preview.clone() } else { l.image.clone() };
                crate::agent::media::frame(w, h, None).child(img(shown).absolute().size_full().object_fit(ObjectFit::Contain))
            }
            Asset::Loading => {
                let (w, h) = contain(None, max);
                let p = tiles::pulse(self.store.read(cx), window, cx);
                crate::agent::media::frame(w, h, p).child(crate::assets::icon("image", 18.0, muted))
            }
            _ => {
                let (w, h) = contain(None, max);
                crate::agent::media::frame(w, h, None)
                    .child(crate::assets::icon("image", 18.0, muted))
                    .child(div().text_size(px(11.5)).text_color(rgb(muted)).child("Image unavailable"))
            }
        };
        // It is the default look: what `@<entity>` wires; the named references are the others.
        let badge = div()
            .absolute()
            .top(px(6.0))
            .left(px(6.0))
            .h(px(18.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .continuous(5.0)
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.55))
            .text_color(gpui::white())
            .text_size(px(11.0))
            .child(model::SHEET_LABEL);
        let face = face.child(badge);
        let (eid, menu_eid) = (id.to_owned(), id.to_owned());
        let (actions, menu_path) = (model::sheet_menu(self.local), path.clone());
        let tip = format!("The default look: @{id} wires it into a card. Click to view, right-click for options");
        div()
            .id("studio-entity-sheet")
            .flex_none()
            .cursor_pointer()
            .child(face)
            .tooltip(crate::tooltip::Tooltip::text(tip))
            .on_click(cx.listener(move |this, _, window, cx| this.view_entity_media(&eid, &path, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                    this.open_menu(Menu::new(e.position, file_menu(&actions, &menu_eid, &menu_path, None)), cx);
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }
}
