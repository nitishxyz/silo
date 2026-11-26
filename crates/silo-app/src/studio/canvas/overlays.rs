//! What floats over the board: the toolbar (bottom centre), the selection actions over a single selected node,
//! the minimap (bottom right), the canvas prompt, the entity / model pickers, the Trash confirmation, the
//! notice and the empty state; and the board's menus as the app's context menu rows. Each reads the board's
//! data and gives the action back to it ([`super::board::Board::choose`]).

use gpui::{AnyElement, AppContext as _, Context, Entity, FontWeight, Subscription, Window, div, prelude::*, px, rgb};

use super::{CanvasEvent, CanvasView, Pick, at};
use crate::{
    assets::icon,
    context_menu::Item,
    studio::{
        board::{self, Action, Tool},
        tiles::ink,
    },
    surface::Glassy as _,
    text_field::{FieldEvent, TextField},
    theme::{self, Continuous as _},
    tooltip::Tooltip,
};

const MAP_W: f64 = 180.0;
const MAP_H: f64 = 120.0;
const MARGIN: f64 = 16.0;

/// Where the minimap goes in a board `size` big (none in a small one).
pub(super) fn minimap_frame(size: board::Size) -> Option<board::Rect> {
    (size.w >= 520.0 && size.h >= 320.0).then(|| board::Rect::new(size.w - MARGIN - MAP_W, size.h - MARGIN - MAP_H, MAP_W, MAP_H))
}

/// The board's menu as context menu rows: its title and groups' titles as quiet rows, disabled entries with
/// their reason.
pub(super) fn menu_items(m: &board::Menu) -> Vec<Item<Pick>> {
    let mut items = Vec::new();
    if let Some(t) = &m.title {
        items.push(Item::action(t.clone(), "", Pick::Nothing).enabled(false));
        items.push(Item::Separator);
    }
    for g in &m.groups {
        if let Some(t) = &g.title {
            items.push(Item::action(t.clone(), "", Pick::Nothing).enabled(false));
        }
        for e in &g.entries {
            let label = match (&e.reason, e.enabled) {
                (Some(r), false) => format!("{} \u{2014} {r}", e.label),
                _ => e.label.clone(),
            };
            items.push(Item::action(label, e.shortcut.unwrap_or(""), Pick::Board(e.action.clone())).enabled(e.enabled));
        }
        items.push(Item::Separator);
    }
    items
}

/// What a picker picks for.
#[derive(Clone, Debug)]
pub(crate) enum PickerFor {
    /// An entity placed at this canvas point.
    Place(board::Point),
    /// The entity whose sheet this file (root-relative) becomes.
    Sheet(String),
    /// Card `node`'s model.
    Model(String),
}

/// A searchable list over the board.
pub(crate) struct Picker {
    pub for_: PickerFor,
    pub field: Entity<TextField>,
    _sub: Subscription,
}

/// A picker's row.
struct Row {
    label: String,
    sub: String,
    enabled: bool,
    selected: bool,
    value: Option<String>,
}

impl CanvasView {
    pub(super) fn open_picker(&mut self, for_: PickerFor, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = if matches!(for_, PickerFor::Model(_)) { "Search models" } else { "Search the world" };
        let field = cx.new(|cx| TextField::new(placeholder, cx));
        let sub = cx.subscribe_in(&field, window, |this, _, e: &FieldEvent, window, cx| match e {
            FieldEvent::Submit => {
                let first = this.picker_rows(cx).into_iter().find(|r| r.enabled).map(|r| r.value);
                if let Some(v) = first {
                    this.picked(v, window, cx);
                }
            }
            FieldEvent::Cancel => {
                this.picker = None;
                window.focus(&this.focus, cx);
                cx.notify();
            }
            _ => cx.notify(),
        });
        field.read(cx).focus.clone().focus(window, cx);
        self.picker = Some(Picker { for_, field, _sub: sub });
        cx.notify();
    }

    fn picker_rows(&self, cx: &gpui::App) -> Vec<Row> {
        let Some(p) = &self.picker else { return Vec::new() };
        let q = p.field.read(cx).text.trim().to_lowercase();
        let hit = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);
        match &p.for_ {
            PickerFor::Place(_) | PickerFor::Sheet(_) => {
                let s = self.store.read(cx);
                s.sections
                    .iter()
                    .flatten()
                    .flat_map(|sec| sec.entities.iter().map(move |e| (sec, e)))
                    .filter(|(sec, e)| hit(&e.name) || hit(&sec.title))
                    .map(|(sec, e)| Row { label: e.name.clone(), sub: sec.title.clone(), enabled: true, selected: false, value: Some(e.id.clone()) })
                    .collect()
            }
            PickerFor::Model(node) => self
                .model_choices(node, cx)
                .into_iter()
                .filter(|c| hit(&c.label))
                .map(|c| Row { label: c.label, sub: c.reason.unwrap_or_default(), enabled: c.enabled, selected: c.selected, value: c.value })
                .collect(),
        }
    }

    fn picked(&mut self, value: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.picker.take() else { return };
        window.focus(&self.focus, cx);
        match p.for_ {
            PickerFor::Place(at) => {
                let Some(id) = value else { return };
                let aspect = self.sheet_aspect(&id, cx);
                let fx = self.board.add_entity(&id, Some(at), aspect);
                self.apply(fx, window, cx);
            }
            PickerFor::Sheet(path) => {
                let Some(id) = value else { return };
                let rel = crate::studio::world::entity_relative(&id, &path);
                let fields = std::collections::BTreeMap::from([("sheet".to_owned(), serde_json::Value::String(rel))]);
                let block_id = self.store.read(cx).block_id.clone();
                let method = silo_proto::control::Method::StudioEntityUpdate(silo_proto::studio::StudioEntityUpdateParams {
                    block_id,
                    id,
                    fields: Some(fields),
                    body: None,
                });
                self.store.update(cx, |s, cx| s.send(method, cx));
            }
            PickerFor::Model(node) => {
                let fx = self.board.change_setting(&node, &board::SettingChange::Model(value));
                self.apply(fx, window, cx);
            }
        }
        cx.notify();
    }

    /// Move the confirmed file to the Trash, then its node off the canvas.
    fn trash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((node, path)) = self.confirm.take() else { return };
        let block_id = self.store.read(cx).block_id.clone();
        let method = silo_proto::control::Method::FilesTrash(silo_proto::files::FilesTrashParams { block_id, paths: vec![path] });
        let caller = self.store.read(cx).caller();
        let _ = window;
        caller.call(method, cx, move |this: &mut Self, r: anyhow::Result<serde_json::Value>, cx| match r {
            Ok(_) => this.later(cx, move |this, window, cx| {
                let mut fx = this.board.select(std::slice::from_ref(&node));
                fx.extend(this.board.delete_selection());
                this.apply(fx, window, cx);
            }),
            Err(e) => this.say(crate::studio::store::describe(&e), cx),
        });
        cx.notify();
    }
}

/// Everything over the board.
pub(super) fn render(view: &mut CanvasView, window: &mut Window, cx: &mut Context<CanvasView>) -> Vec<AnyElement> {
    let mut out = Vec::new();
    if view.board.canvas().nodes.is_empty() && view.prompt.is_none() {
        out.push(empty(cx));
    }
    out.extend(minimap(view));
    out.extend(selection_bar(view, cx));
    out.push(toolbar(view, cx));
    out.extend(prompt(view, cx));
    out.extend(super::mentions::floating(view, cx));
    out.extend(mention_wire(view));
    out.extend(super::settings_panel::render(view, window, cx));
    out.extend(picker(view, cx));
    out.extend(confirm(view, cx));
    out.extend(super::run::render_setup(view, cx));
    if let Some((text, _)) = &view.notice {
        out.push(
            div()
                .absolute()
                .left(px(16.0))
                .bottom(px(16.0))
                .max_w(px(420.0))
                .px(px(12.0))
                .py(px(7.0))
                .glass(crate::radius::CONTROL + 4.0)
                .text_size(px(12.5))
                .text_color(rgb(theme::foreground()))
                .child(text.clone())
                .into_any_element(),
        );
        window.request_animation_frame();
    }
    out
}

fn empty(cx: &mut Context<CanvasView>) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.0))
        .text_color(rgb(theme::muted()))
        .child("Nothing on this canvas yet")
        .child(div().text_size(px(12.0)).child("Double-click to make something, press G, or drop files here"))
        .child(
            crate::confirm_card::button("studio-canvas-ask", "Ask the agent", false, false)
                .cursor_pointer()
                .on_click(cx.listener(|_, _, _, cx| cx.emit(CanvasEvent::Ask(Vec::new())))),
        )
        .into_any_element()
}

fn tool_icon(a: &Action) -> &'static str {
    match a {
        Action::Tool(Tool::Select) => "mouse-pointer",
        Action::Tool(Tool::Note) => "sticky-note",
        Action::Tool(Tool::Frame) => "frame",
        Action::Prompt => "sparkles",
        Action::Import => "upload",
        _ => "plus",
    }
}

fn toolbar(view: &CanvasView, cx: &mut Context<CanvasView>) -> AnyElement {
    let buttons = view.board.toolbar().into_iter().enumerate().map(|(i, e)| {
        let tip = match e.shortcut {
            Some(k) => format!("{}  {k}", e.label),
            None => e.label.clone(),
        };
        let action = e.action.clone();
        div()
            .id(("studio-tool", i))
            .size(px(30.0))
            .flex()
            .items_center()
            .justify_center()
            .continuous(crate::radius::CONTROL + 1.0)
            .cursor_pointer()
            .when(e.active, |el| el.bg(theme::row_fill()))
            .hover(|s| s.bg(theme::hover_fill()))
            .child(icon(tool_icon(&e.action), 15.0, if e.active { theme::foreground() } else { theme::muted() }))
            .tooltip(Tooltip::text(tip))
            .on_click(cx.listener(move |this, _, window, cx| {
                let fx = this.board.choose(&action);
                this.apply(fx, window, cx);
            }))
    });
    div()
        .absolute()
        .bottom(px(16.0))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(div().id("studio-toolbar").occlude().flex().gap(px(2.0)).p(px(4.0)).glass(crate::radius::CARD).children(buttons))
        .into_any_element()
}

fn selection_bar(view: &CanvasView, cx: &mut Context<CanvasView>) -> Option<AnyElement> {
    if view.inline.is_some() || view.settings.is_some() {
        return None;
    }
    let bar = view.board.selection_bar()?;
    #[allow(clippy::cast_precision_loss)]
    let width: f32 = bar.entries.iter().map(|e| e.label.chars().count() as f32 * 6.6 + 22.0).sum::<f32>() + 8.0;
    let left = (bar.anchor.x as f32 - width / 2.0).max(8.0);
    let top = (bar.anchor.y as f32 - 44.0).max(8.0);
    let buttons = bar.entries.into_iter().enumerate().map(|(i, e)| {
        let action = e.action.clone();
        let el = div()
            .id(("studio-sel", i))
            .h(px(26.0))
            .px(px(9.0))
            .flex()
            .items_center()
            .continuous(crate::radius::CONTROL)
            .whitespace_nowrap()
            .text_size(px(12.5))
            .text_color(rgb(if !e.enabled {
                theme::muted()
            } else if e.danger {
                theme::danger()
            } else {
                theme::foreground()
            }))
            .child(e.label.clone());
        let el = match &e.reason {
            Some(r) => el.tooltip(Tooltip::text(r.clone())),
            None => el.when_some(e.shortcut, |el, k| el.tooltip(Tooltip::text(format!("{}  {k}", e.label)))),
        };
        if e.enabled {
            el.cursor_pointer().hover(|s| s.bg(theme::row_fill())).on_click(cx.listener(move |this, _, window, cx| {
                let fx = this.board.choose(&action);
                this.apply(fx, window, cx);
            }))
        } else {
            el
        }
    });
    Some(
        div()
            .id("studio-selection-bar")
            .occlude()
            .absolute()
            .left(px(left))
            .top(px(top))
            .flex()
            .gap(px(1.0))
            .p(px(3.0))
            .glass(crate::radius::CARD - 3.0)
            .children(buttons)
            .into_any_element(),
    )
}

fn minimap(view: &CanvasView) -> Option<AnyElement> {
    let b = view.bounds.get();
    let frame = minimap_frame(board::Size::new(f64::from(f32::from(b.size.width)), f64::from(f32::from(b.size.height))))?;
    let map = view.board.minimap(frame)?;
    let canvas = view.board.canvas();
    // Painted, not laid out: a canvas may hold hundreds of nodes.
    let rects: Vec<(board::Rect, u32)> = view
        .board
        .scene()
        .laid
        .iter()
        .map(|l| {
            let fill = if l.shape.is_frame() {
                ink(0.1)
            } else if view.board.is_selected(&canvas.nodes[l.index].id) {
                theme::accent()
            } else {
                ink(0.35)
            };
            (map.rect(l.rect), fill)
        })
        .collect();
    let seen = map.rect(view.board.visible_rect());
    let accent: gpui::Hsla = rgb(theme::accent()).into();
    let paint = gpui::canvas(
        |_, _, _| (),
        move |bounds: gpui::Bounds<gpui::Pixels>, (), window, _| {
            // `map.rect` is in board view points; this element's origin is the board's.
            let origin = bounds.origin;
            let at = |r: board::Rect| {
                gpui::Bounds::new(origin + gpui::point(px(r.x as f32), px(r.y as f32)), gpui::size(px(r.w.max(1.0) as f32), px(r.h.max(1.0) as f32)))
            };
            window.with_content_mask(Some(gpui::ContentMask::new(at(frame))), |window| {
                for (r, c) in &rects {
                    window.paint_quad(gpui::fill(at(*r), rgb(*c)));
                }
                window.paint_quad(gpui::fill(at(seen), accent.opacity(0.08)).border_widths(px(1.0)).border_color(accent));
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();
    Some(div().absolute().top_0().left_0().size_full().child(at(frame).glass(crate::radius::CONTROL + 2.0)).child(paint).into_any_element())
}

fn prompt(view: &CanvasView, cx: &mut Context<CanvasView>) -> Option<AnyElement> {
    let p = view.prompt.as_ref()?;
    let b = view.bounds.get();
    let w = 380.0_f32;
    let left = (p.view.x as f32 - w / 2.0).clamp(8.0, (f32::from(b.size.width) - w - 8.0).max(8.0));
    let top = (p.view.y as f32 - 24.0).clamp(8.0, (f32::from(b.size.height) - 120.0).max(8.0));
    let kinds = board::wire::KINDS.iter().map(|&k| {
        let on = k == p.makes;
        div()
            .id(gpui::ElementId::Name(format!("studio-prompt-kind-{}", k.label()).into()))
            .h(px(24.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .continuous(crate::radius::CONTROL)
            .cursor_pointer()
            .text_size(px(12.0))
            .when(on, |el| el.bg(theme::row_fill()))
            .hover(|s| s.bg(theme::hover_fill()))
            .child(icon(crate::studio::tiles::kind_glyph(k), 12.0, if on { theme::foreground() } else { theme::muted() }))
            .child(board::wire::kind_label(k))
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(p) = this.prompt.as_mut() {
                    p.makes = k;
                    p.editor.read(cx).focus.clone().focus(window, cx);
                }
                cx.notify();
            }))
    });
    let kinds: Vec<_> = kinds.collect();
    let editor = crate::agent::editor::Editor::element(&p.editor, 6, super::nodes::colors(), cx);
    let mentions = super::mentions::in_prompt(view, &p.editor, cx);
    Some(
        div()
            .id("studio-canvas-prompt")
            .occlude()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(w))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .glass(crate::radius::CARD)
            .capture_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, window, cx| {
                if e.keystroke.key == "enter" && e.keystroke.modifiers.alt {
                    this.submit_prompt(false, window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| this.close_prompt(window, cx)))
            .child(div().flex().gap(px(2.0)).children(kinds))
            .child(div().px(px(8.0)).py(px(6.0)).continuous(crate::radius::CONTROL + 1.0).bg(theme::well_fill()).text_size(px(13.0)).child(editor))
            .children(mentions)
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(theme::muted()))
                    .child("\u{21a9} make and run \u{b7} \u{2325}\u{21a9} card only \u{b7} esc closes"),
            )
            .into_any_element(),
    )
}

/// A wire a mention in its card's prompt made, selected or hovered: a label at its middle says so.
fn mention_wire(view: &CanvasView) -> Option<AnyElement> {
    let b = &view.board;
    let hovered = match b.hovered() {
        board::Hit::Wire { id } => Some(id.as_str()),
        _ => None,
    };
    let w = b.wires().find(|w| (w.selected || hovered == Some(w.id)) && b.canvas().edges[w.edge].mention)?;
    let at = b.camera().to_view(w.curve.mid());
    Some(
        div()
            .absolute()
            // About centred under the wire's middle (the label is ~160 pt wide).
            .left(px(at.x as f32 - 80.0))
            .top(px(at.y as f32 + 8.0))
            .px(px(6.0))
            .py(px(2.0))
            .continuous(crate::radius::TOOLTIP)
            .bg(rgb(theme::popover()))
            .border_1()
            .border_color(rgb(ink(0.12)))
            .whitespace_nowrap()
            .text_size(px(11.5))
            .text_color(rgb(theme::foreground()))
            .child("From a mention in the prompt")
            .into_any_element(),
    )
}

fn picker(view: &CanvasView, cx: &mut Context<CanvasView>) -> Option<AnyElement> {
    let p = view.picker.as_ref()?;
    let title = match &p.for_ {
        PickerFor::Place(_) => "Add from World",
        PickerFor::Sheet(_) => "Set as the sheet of\u{2026}",
        PickerFor::Model(_) => "Model",
    };
    let rows = view.picker_rows(cx);
    let empty = rows.is_empty();
    let rows = rows.into_iter().enumerate().map(|(i, r)| {
        let value = r.value.clone();
        div()
            .id(("studio-picker-row", i))
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(28.0))
            .px(px(8.0))
            .continuous(crate::radius::MENU_ROW)
            .when(r.selected, |el| el.bg(theme::row_fill()))
            .when(!r.enabled, |el| el.opacity(0.5))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(r.label))
            .child(
                div()
                    .flex_none()
                    .max_w(px(170.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(11.5))
                    .text_color(rgb(theme::muted()))
                    .child(r.sub),
            )
            .when(r.enabled, |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(theme::hover_fill()))
                    .on_click(cx.listener(move |this, _, window, cx| this.picked(value.clone(), window, cx)))
            })
    });
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .pt(px(60.0))
            .child(
                div()
                    .id("studio-picker")
                    .occlude()
                    .w(px(340.0))
                    .max_h(px(420.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .p(px(8.0))
                    .glass_sheet(crate::radius::CARD)
                    .text_size(px(12.5))
                    .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                        this.picker = None;
                        window.focus(&this.focus, cx);
                        cx.notify();
                    }))
                    .child(div().px(px(4.0)).font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(p.field.clone())
                    .child(div().id("studio-picker-list").flex().flex_col().overflow_y_scroll().children(rows))
                    .when(empty, |el| el.child(div().p(px(8.0)).text_color(rgb(theme::muted())).child("Nothing matches"))),
            )
            .into_any_element(),
    )
}

fn confirm(view: &CanvasView, cx: &mut Context<CanvasView>) -> Option<AnyElement> {
    let (_, path) = view.confirm.as_ref()?;
    let name = path.rsplit('/').next().unwrap_or(path).to_owned();
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                crate::confirm_card::card("studio-trash-confirm")
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("Move \u{201c}{name}\u{201d} to the Trash?")))
                    .child(div().text_color(rgb(theme::muted())).child("Its node is removed from the canvas too."))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .child(crate::confirm_card::button("studio-trash-cancel", "Cancel", false, false).cursor_pointer().on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.confirm = None;
                                    cx.notify();
                                },
                            )))
                            .child(
                                crate::confirm_card::button("studio-trash-ok", "Move to Trash", false, true)
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| this.trash(window, cx))),
                            ),
                    ),
            )
            .into_any_element(),
    )
}
