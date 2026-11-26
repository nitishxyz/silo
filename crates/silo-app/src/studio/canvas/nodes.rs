//! Nodes as the board lays them out (only those in view): frames (title bar, body), cards (§4.2: slim header,
//! the picked take edge to edge, the prompt in place, Run / Stop / Retry, the running face), assets, notes and
//! entities through `super::super::tiles`, a card's port modules on its left and the output dot on the right of
//! anything with an output, selection outlines and resize corners. Faces are drawn at the zoomed size; text
//! scales with the zoom. Nothing here handles the pointer (the board does) except the prompt being edited.

use gpui::{AnyElement, Context, FontWeight, Window, div, prelude::*, px, rgb};
use silo_proto::studio::{EdgeRole, Node, NodeBody, Section, StudioMedia};

use super::{CanvasView, InlineKind, at, mentions::Tip};
use crate::{
    agent::{editor::Editor, media},
    assets::icon,
    studio::{
        board::{self, Hit, Laid, Part, Shape, layout},
        mentions as studio_mentions, model, tiles,
        tiles::ink,
    },
    theme::{self, Continuous as _},
};

/// Below this zoom, cards and frames skip their small text and ports.
const DETAIL: f64 = 0.3;

/// Sizes at the current zoom.
#[derive(Clone, Copy)]
struct Z(f32);

impl Z {
    fn s(self, v: f32) -> gpui::Pixels {
        px(v * self.0)
    }
}

pub(super) fn render(view: &CanvasView, window: &mut Window, cx: &mut Context<CanvasView>) -> Vec<AnyElement> {
    let sections: Vec<Section> = view.store.read(cx).sections.clone().unwrap_or_default();
    let laid: Vec<Laid> = view.board.visible().collect();
    let mut out = Vec::with_capacity(laid.len() * 2);
    for l in &laid {
        node(view, l, &sections, &mut out, window, cx);
    }
    out
}

fn node(view: &CanvasView, l: &Laid, sections: &[Section], out: &mut Vec<AnyElement>, window: &mut Window, cx: &mut Context<CanvasView>) {
    let b = &view.board;
    let camera = b.camera();
    let z = Z(camera.zoom as f32);
    let n = &b.canvas().nodes[l.index];
    let r = camera.rect_to_view(l.rect);
    if r.w < 2.0 || r.h < 2.0 {
        return;
    }
    let selected = b.is_selected(&n.id);
    let body: AnyElement = match l.shape {
        Shape::Frame(role) => frame(view, n, l, r, role, z, selected, cx),
        Shape::Card(_) => card(view, n, l, r, sections, z, selected, window, cx),
        Shape::Note => note(view, n, r, z, selected, cx),
        Shape::Asset(media) => asset(view, n, l, r, media, selected, window, cx),
        Shape::Entity => {
            let NodeBody::Entity { id } = &n.body else { return };
            outlined(r, div().child(tiles::entity(&view.store, sections, id, r.w as f32, r.h as f32, window, cx)), selected).into_any_element()
        }
        Shape::Unknown => outlined(r, div().child(media::frame(r.w as f32, r.h as f32, None).child(icon("layers", 16.0, theme::muted()))), selected)
            .into_any_element(),
    };
    out.push(body);
    if camera.zoom >= DETAIL {
        ports(view, l, z, out);
    }
    if l.output {
        let c = camera.to_view(l.output_center());
        let hovered = matches!(b.hovered(), Hit::Output { id } if *id == n.id);
        let d = if hovered { 14.0 } else { 10.0 } * z.0.clamp(0.6, 1.4);
        out.push(
            div()
                .absolute()
                .left(px(c.x as f32 - d / 2.0))
                .top(px(c.y as f32 - d / 2.0))
                .size(px(d))
                .rounded_full()
                .border_2()
                .border_color(rgb(if hovered { theme::accent() } else { ink(0.45) }))
                .bg(rgb(theme::background()))
                .into_any_element(),
        );
    }
    if selected && b.selection().len() == 1 {
        let h = board::scene::HANDLE_VIEW as f32;
        for c in board::Corner::ALL {
            let p = camera.to_view(l.rect.corner(c));
            out.push(
                div()
                    .absolute()
                    .left(px(p.x as f32 - h / 2.0))
                    .top(px(p.y as f32 - h / 2.0))
                    .size(px(h))
                    .rounded(px(2.0))
                    .border_1()
                    .border_color(rgb(theme::accent()))
                    .bg(rgb(theme::background()))
                    .into_any_element(),
            );
        }
    }
}

/// An inline editor of node `id`: its presses are its own (the board's would end the edit).
fn editing(id: &str, editor: impl IntoElement, cx: &mut Context<CanvasView>) -> AnyElement {
    let id = id.to_owned();
    div()
        .id(gpui::ElementId::Name(format!("studio-edit-{id}").into()))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                if this.board.selection() != [id.clone()] {
                    let fx = this.board.select(std::slice::from_ref(&id));
                    this.apply(fx, window, cx);
                }
                cx.stop_propagation();
            }),
        )
        .child(editor)
        .into_any_element()
}

/// A node's body at view rect `r` (`inner` fills it, clipping its own content), outlined when selected.
fn outlined(r: board::Rect, inner: gpui::Div, selected: bool) -> gpui::Div {
    at(r).child(inner.absolute().top_0().left_0().size_full()).when(selected, |el| {
        el.child(
            div()
                .absolute()
                .top(px(-3.0))
                .left(px(-3.0))
                .right(px(-3.0))
                .bottom(px(-3.0))
                .border_2()
                .continuous(9.0)
                .border_color(rgb(theme::accent())),
        )
    })
}

/// A text line clipped to its box.
fn line(text: impl Into<gpui::SharedString>, size: gpui::Pixels, color: u32) -> gpui::Div {
    div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(size).text_color(rgb(color)).child(text.into())
}

#[allow(clippy::too_many_arguments)]
fn frame(
    view: &CanvasView,
    n: &Node,
    l: &Laid,
    r: board::Rect,
    role: silo_proto::studio::FrameRole,
    z: Z,
    selected: bool,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let NodeBody::Frame { title, order, .. } = &n.body else { return div().into_any_element() };
    let th = l.title.map_or(layout::FRAME_TITLE, |t| t.h) * f64::from(z.0);
    let label: AnyElement = match view.inline.as_ref().filter(|i| i.node == n.id && i.kind == InlineKind::Title) {
        Some(i) => div().flex_1().text_size(z.s(13.0)).child(editing(&n.id, Editor::element(&i.editor, 1, colors(), cx), cx)).into_any_element(),
        None if f64::from(z.0) >= DETAIL / 2.0 => {
            line(model::frame_label(title, role, *order), z.s(13.0), theme::muted()).font_weight(FontWeight::SEMIBOLD).into_any_element()
        }
        None => div().into_any_element(),
    };
    let depth_fill = 0.025 + 0.015 * f32::from(l.depth.min(3));
    outlined(
        r,
        div()
            .continuous(12.0 * z.0.clamp(0.3, 1.0))
            .border_1()
            .border_color(rgb(ink(0.12)))
            .bg(rgb(ink(depth_fill)))
            .child(div().absolute().top_0().left_0().right_0().h(px(th as f32)).flex().items_center().px(z.s(12.0)).child(label)),
        selected,
    )
    .into_any_element()
}

fn note(view: &CanvasView, n: &Node, r: board::Rect, z: Z, selected: bool, cx: &mut Context<CanvasView>) -> AnyElement {
    let NodeBody::Note { text } = &n.body else { return div().into_any_element() };
    let inner: AnyElement = match view.inline.as_ref().filter(|i| i.node == n.id && i.kind == InlineKind::Note) {
        Some(i) => editing(&n.id, Editor::element(&i.editor, 40, colors(), cx), cx),
        None if text.trim().is_empty() => div().text_color(rgb(theme::muted())).child("Empty note").into_any_element(),
        // Rendered markdown, clipped to the note.
        None => {
            let blocks = view.md.borrow_mut().blocks(&n.id, text);
            super::markdown::render(&blocks, z.0)
        }
    };
    let fill = theme::mix(theme::background(), theme::warning(), 0.1);
    outlined(
        r,
        div()
            .continuous(10.0 * z.0.clamp(0.3, 1.0))
            .overflow_hidden()
            .border_1()
            .border_color(rgb(ink(0.12)))
            .bg(rgb(fill))
            .p(z.s(10.0))
            .text_size(z.s(12.5))
            .line_height(z.s(18.0))
            .text_color(rgb(theme::foreground()))
            .child(inner),
        selected,
    )
    .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn asset(
    view: &CanvasView,
    n: &Node,
    l: &Laid,
    r: board::Rect,
    m: StudioMedia,
    selected: bool,
    window: &mut Window,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let NodeBody::Asset { path, seconds, .. } = &n.body else { return div().into_any_element() };
    if m == StudioMedia::Text {
        return document(view, l, r, path, selected, cx);
    }
    let f = l.face.map_or(r, |f| view.board.camera().rect_to_view(f));
    let (w, h) = (f.w as f32, f.h as f32);
    let face = match m {
        StudioMedia::Video => playing(view, path, tiles::video(&view.store, path, *seconds, w, h, window, cx)),
        StudioMedia::Audio => tiles::audio(&view.store, path, w, window, cx),
        _ => tiles::image(&view.store, path, w, h, window, cx),
    };
    outlined(r, div().child(face), selected).into_any_element()
}

/// A document card: its file name in a title bar over the text rendered, clipped and fading at the bottom.
/// The text is the store's (read again when the file changes).
fn document(view: &CanvasView, l: &Laid, r: board::Rect, path: &str, selected: bool, cx: &mut Context<CanvasView>) -> AnyElement {
    let z = Z(view.board.camera().zoom as f32);
    let bar = l.face.map_or(layout::DOC_HEADER, |f| f.y - l.rect.y) as f32 * z.0;
    let name = path.rsplit('/').next().unwrap_or(path).to_owned();
    let muted = theme::muted();
    let fill = theme::mix(theme::background(), theme::foreground(), 0.035);
    let header = div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(bar))
        .flex()
        .items_center()
        .gap(z.s(6.0))
        .px(z.s(10.0))
        .border_b_1()
        .border_color(rgb(ink(0.1)))
        .when(f64::from(z.0) >= DETAIL, |el| {
            el.child(icon("file-text", 12.0 * z.0, muted)).child(line(name, z.s(12.5), theme::foreground()).flex_1().font_weight(FontWeight::MEDIUM))
        });
    let text = match view.store.update(cx, |s, cx| s.asset(path, crate::studio::store::AssetKind::Text, cx)) {
        crate::studio::store::Asset::Text(t) => Some(t),
        _ => None,
    };
    let preview: AnyElement = match text {
        _ if f64::from(z.0) < DETAIL => div().into_any_element(),
        Some(t) if t.trim().is_empty() => div().text_size(z.s(12.0)).text_color(rgb(muted)).child("Empty document").into_any_element(),
        Some(t) => {
            let blocks = view.md.borrow_mut().blocks(path, &t);
            super::markdown::render(&blocks, z.0)
        }
        None => div().text_size(z.s(12.0)).text_color(rgb(muted)).child("Loading\u{2026}").into_any_element(),
    };
    let mut clear: gpui::Hsla = rgb(fill).into();
    clear.a = 0.0;
    let fade = div().absolute().left_0().right_0().bottom_0().h(z.s(48.0)).bg(gpui::linear_gradient(
        180.0,
        gpui::linear_color_stop(clear, 0.0),
        gpui::linear_color_stop(rgb(fill), 1.0),
    ));
    let body = div()
        .absolute()
        .top(px(bar))
        .left_0()
        .right_0()
        .bottom_0()
        .overflow_hidden()
        .child(div().px(z.s(12.0)).pt(z.s(10.0)).text_color(rgb(theme::foreground())).child(preview))
        .child(fade);
    let el = div()
        .continuous(12.0 * z.0.clamp(0.3, 1.0))
        .border_1()
        .border_color(rgb(ink(0.13)))
        .bg(rgb(fill))
        .overflow_hidden()
        .child(body)
        .child(header);
    outlined(r, el, selected).into_any_element()
}

/// A video face with its hover playback over it, when it plays.
fn playing(view: &CanvasView, path: &str, face: AnyElement) -> AnyElement {
    match view.media.frame(path) {
        Some(frame) => div()
            .relative()
            .size_full()
            .child(face)
            .child(
                div().absolute().top_0().left_0().size_full().overflow_hidden().continuous(media::RADIUS).child(crate::agent::video_surface(frame)),
            )
            .into_any_element(),
        None => face,
    }
}

pub(super) fn colors() -> crate::agent::editor::EditorColors {
    crate::agent::editor::EditorColors {
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn card(
    view: &CanvasView,
    n: &Node,
    l: &Laid,
    r: board::Rect,
    sections: &[Section],
    z: Z,
    selected: bool,
    window: &mut Window,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let NodeBody::Gen { makes, model: chosen, status, progress, takes, pick, prompt, .. } = &n.body else { return div().into_any_element() };
    let Some(parts) = l.card else { return div().into_any_element() };
    let b = &view.board;
    let canvas = b.canvas();
    let zf = f64::from(z.0);
    let rel = |p: board::Rect| board::Rect::new((p.x - l.rect.x) * zf, (p.y - l.rect.y) * zf, p.w * zf, p.h * zf);
    let detail = zf >= DETAIL;
    let muted = theme::muted();
    let take = n.body.take();
    let running = model::running(*status);
    let hover_part = match b.hovered() {
        Hit::Node { id, part } if *id == n.id => Some(*part),
        _ => None,
    };

    // Header: kind, title, the model or Out of date, the take stepper.
    let out_of_date = canvas.out_of_date.contains(&n.id);
    let hr = rel(parts.header);
    let mut header = at(hr).flex().items_center().gap(z.s(6.0)).px(z.s(10.0));
    if detail {
        let subtitle = model::card_subtitle(chosen.as_deref(), take.and_then(|t| t.model.as_deref()), out_of_date);
        let stepper_w = parts.stepper.map_or(0.0, |(a, bb)| (a.w + bb.w) * zf) as f32;
        header = header
            .child(icon(tiles::kind_glyph(*makes), 12.0 * z.0, muted))
            // The title gives way first; the model label keeps its room.
            .child(line(canvas.label_of(&n.id, sections), z.s(12.5), theme::foreground()).flex_1().font_weight(FontWeight::MEDIUM))
            .child(
                line(subtitle, z.s(11.0), if out_of_date { theme::warning() } else { muted })
                    .flex_shrink_0()
                    .max_w(px(hr.w as f32 * 0.6))
                    .when(out_of_date, |el| el.font_weight(FontWeight::MEDIUM)),
            )
            .child(div().flex_none().w(px(stepper_w)));
    }
    let stepper = parts.stepper.filter(|_| detail).and_then(|(back, fwd)| {
        let text = model::take_stepper(takes, *pick)?;
        let can = |by: isize| model::stepped_pick(takes.len(), *pick, by).is_some();
        let half = |p: board::Rect, glyph: &'static str, on: bool, hot: bool, left: bool| {
            at(rel(p))
                .flex()
                .items_center()
                .px(z.s(4.0))
                .when(left, gpui::Styled::justify_start)
                .when(!left, gpui::Styled::justify_end)
                .text_size(z.s(13.0))
                .text_color(rgb(if on { theme::foreground() } else { ink(0.3) }))
                .when(hot && on, |el| el.bg(theme::row_fill()).continuous(4.0))
                .child(glyph)
        };
        Some([
            half(back, "\u{2039}", can(-1), hover_part == Some(Part::TakeBack), true).into_any_element(),
            half(fwd, "\u{203a}", can(1), hover_part == Some(Part::TakeForward), false).into_any_element(),
            at(rel(back.union(fwd)))
                .flex()
                .items_center()
                .justify_center()
                .text_size(z.s(11.0))
                .text_color(rgb(muted))
                .child(text)
                .into_any_element(),
        ])
    });

    // Face: the picked take, the running shimmer, the error, or an empty draft.
    let fr = rel(parts.face);
    let (fw, fh) = (fr.w as f32, fr.h as f32);
    let face: AnyElement = if running {
        let s = view.store.read(cx);
        let since = s.running_since(&view.name, &n.id);
        let p = tiles::pulse(s, window, cx);
        #[allow(clippy::cast_possible_truncation)]
        let elapsed = since.map(|t| media::clock(t.elapsed().as_millis() as u64));
        let mut f = media::frame(fw, fh, p).child(icon(tiles::kind_glyph(*makes), 18.0 * z.0.clamp(0.5, 1.2), muted));
        if detail {
            f = f
                .children(elapsed.map(|t| div().text_size(z.s(12.0)).text_color(rgb(muted)).child(t)))
                .child(line(model::progress_text(*status, progress.as_deref()), z.s(11.5), muted).max_w(px(fw - 16.0)));
        }
        f.into_any_element()
    } else if let Some(t) = take {
        match t.media {
            StudioMedia::Video => playing(view, &t.path, tiles::video(&view.store, &t.path, t.seconds, fw, fh, window, cx)),
            StudioMedia::Audio => tiles::audio(&view.store, &t.path, fw, window, cx),
            _ => tiles::image(&view.store, &t.path, fw, fh, window, cx),
        }
    } else {
        media::frame(fw, fh, None).child(icon(tiles::kind_glyph(*makes), 18.0 * z.0.clamp(0.5, 1.2), ink(0.3))).into_any_element()
    };
    // What went wrong, wrapped on the face (the full text in a tooltip); Retry or Connect provider below.
    let refused = view.runs.errors.get(&n.id).map(String::as_str);
    let failure = super::run::failure_text(&n.body, refused).filter(|_| detail).map(|text| {
        let lines = if Shape::Card(*makes).is_audio() { 2 } else { 4 };
        div()
            .id(gpui::ElementId::Name(format!("studio-failure-{}", n.id).into()))
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_start()
            .gap(z.s(6.0))
            .px(z.s(10.0))
            .py(z.s(6.0))
            .bg(rgb(theme::mix(theme::background(), theme::danger(), 0.1)))
            .child(div().flex_none().pt(z.s(2.0)).child(icon("alert", 11.0 * z.0, theme::danger())))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(z.s(11.5))
                    .line_height(z.s(15.0))
                    .text_color(rgb(theme::danger()))
                    .line_clamp(lines)
                    .text_ellipsis()
                    .child(text.clone()),
            )
            .tooltip(crate::tooltip::Tooltip::text(text))
    });
    let face = at(fr).child(face).children(failure);

    // The prompt, in place: a text field while edited, else its text; its height is measured for the board.
    let pr = rel(parts.prompt);
    let pad = (layout::PROMPT_PAD * zf) as f32;
    // Collapsed to a few lines with an ellipsis; in full (the editor scrolls beyond) when the board opens it.
    let edited = view.inline.as_ref().filter(|i| i.node == n.id && i.kind == InlineKind::Prompt);
    let lines = if parts.open { layout::PROMPT_LINES_OPEN } else { layout::PROMPT_LINES };
    let text: AnyElement = match edited {
        Some(i) => editing(&n.id, Editor::element(&i.editor, layout::PROMPT_LINES_OPEN, colors(), cx), cx),
        None if !detail => div().into_any_element(),
        None if prompt.trim().is_empty() => {
            div().text_color(rgb(muted)).child(format!("Describe the {}\u{2026}", makes.label().to_lowercase())).into_any_element()
        }
        None => resting_prompt(view, &n.id, prompt, sections, lines, cx),
    };
    let measured = view.measured.clone();
    let (id, now_h, zoom, open) = (n.id.clone(), parts.prompt.h, zf, parts.open);
    let probe = gpui::canvas(
        move |bounds, window, _| {
            let h = f64::from(f32::from(bounds.size.height));
            let target = layout::shown_prompt(h / zoom + 2.0 * layout::PROMPT_PAD, open);
            if (target - now_h).abs() > 0.5 && zoom >= DETAIL {
                measured.borrow_mut().insert(id.clone(), h);
                window.refresh();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();
    let prompt_el = at(pr).overflow_hidden().when(edited.is_some(), |el| el.bg(theme::well_fill())).child(
        div()
            .absolute()
            .top(px(pad))
            .left(px(pad))
            .w(px((pr.w as f32 - 2.0 * pad).max(1.0)))
            .text_size(z.s(12.5))
            .line_height(z.s(layout::PROMPT_LINE as f32))
            .text_color(rgb(theme::foreground()))
            .child(text)
            .child(probe),
    );

    // Footer: Run / Stop / Retry / Connect provider.
    let ftr = rel(parts.footer);
    let rr = rel(parts.run);
    let (_, label, glyph) = super::run::footer_button(&n.body, refused.is_some());
    let hot = hover_part == Some(Part::Run);
    let run_button = at(board::Rect::new(rr.x - ftr.x, rr.y - ftr.y, rr.w, rr.h))
        .flex()
        .items_center()
        .justify_center()
        .gap(z.s(4.0))
        .continuous(6.0 * z.0.clamp(0.4, 1.0))
        .bg(rgb(if running {
            ink(if hot { 0.2 } else { 0.12 })
        } else if hot {
            theme::mix(theme::accent(), theme::foreground(), 0.15)
        } else {
            theme::accent()
        }))
        .text_color(rgb(if running { theme::foreground() } else { theme::on_accent() }))
        .text_size(z.s(11.5))
        .font_weight(FontWeight::MEDIUM)
        .child(icon(glyph, 10.0 * z.0, if running { theme::foreground() } else { theme::on_accent() }))
        .when(detail, |el| el.child(label));
    let footer = at(ftr).child(run_button);

    let el = div()
        .continuous(12.0 * z.0.clamp(0.3, 1.0))
        .border_1()
        .border_color(rgb(if out_of_date { theme::mix(ink(0.12), theme::warning(), 0.5) } else { ink(0.13) }))
        .bg(rgb(theme::mix(theme::background(), theme::foreground(), 0.035)))
        .overflow_hidden()
        .child(header)
        .children(stepper.into_iter().flatten())
        .child(face)
        .child(prompt_el)
        .child(footer);
    outlined(r, el, selected).into_any_element()
}

/// A card's prompt at rest, `lines` at most: its mentions as chips reading what they name (quiet when unwired, a
/// tooltip saying so; while edits are in flight none is), the rest as text.
fn resting_prompt(view: &CanvasView, id: &str, prompt: &str, sections: &[Section], lines: usize, cx: &gpui::App) -> AnyElement {
    let b = &view.board;
    let list = studio_mentions::chips(prompt, b.canvas(), Some(id), sections, b.settled().then_some(prompt));
    let quiet: Vec<(std::ops::Range<usize>, String)> =
        list.iter().filter_map(|c| c.face.as_ref().filter(|f| f.quiet).map(|f| (c.range.clone(), studio_mentions::unwired_tip(&f.label)))).collect();
    if quiet.is_empty() {
        view.chip_tips.borrow_mut().remove(id);
    }
    if list.is_empty() {
        return div().line_clamp(lines).text_ellipsis().child(prompt.to_owned()).into_any_element();
    }
    let (text, layout) = crate::agent::mention::inline(prompt, &list, None, cx);
    // The quiet chips' places, measured as drawn (the next frame puts their tooltips there).
    let probe = (!quiet.is_empty()).then(|| {
        let (tips, id) = (view.chip_tips.clone(), id.to_owned());
        gpui::canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                let (origin, lh) = (bounds.origin, layout.line_height());
                let found: Vec<Tip> = quiet
                    .iter()
                    .filter_map(|(range, text)| {
                        let (a, z) = (layout.position_for_index(range.start)?, layout.position_for_index(range.end)?);
                        let w = if (z.y - a.y).abs() < px(1.0) { z.x - a.x } else { lh };
                        let f = |p: gpui::Pixels| f64::from(f32::from(p));
                        Some(Tip { rect: board::Rect::new(f(a.x - origin.x), f(a.y - origin.y), f(w), f(lh)), text: text.clone() })
                    })
                    .collect();
                let mut tips = tips.borrow_mut();
                if tips.get(&id) != Some(&found) {
                    tips.insert(id.clone(), found);
                    window.refresh();
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    });
    let regions: Vec<AnyElement> = view
        .chip_tips
        .borrow()
        .get(id)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, t)| {
            at(t.rect)
                .id(gpui::ElementId::Name(format!("studio-chip-{id}-{i}").into()))
                .tooltip(crate::tooltip::Tooltip::text(t.text.clone()))
                .into_any_element()
        })
        .collect();
    div().relative().line_clamp(lines).text_ellipsis().child(text).children(probe).children(regions).into_any_element()
}

/// A card's port modules on its left: an icon each, hover names it, wired ones filled, lit while a wire that
/// fits is dragged.
fn ports(view: &CanvasView, l: &Laid, z: Z, out: &mut Vec<AnyElement>) {
    let b = &view.board;
    let camera = b.camera();
    for (k, role) in l.ports.iter().enumerate() {
        let s = b.port_state(l, k);
        let r = camera.rect_to_view(l.port_rect(k));
        let (bg, fg, border) = if s.target {
            (theme::accent(), theme::on_accent(), theme::accent())
        } else if s.refused {
            (theme::background(), theme::danger(), theme::danger())
        } else if s.wired {
            (ink(0.8), theme::background(), ink(0.8))
        } else if s.fits || s.hovered {
            (theme::background(), theme::accent(), theme::accent())
        } else {
            (theme::background(), ink(0.55), ink(0.22))
        };
        let glyph = match role {
            EdgeRole::Video => "video",
            EdgeRole::Audio => "audio-lines",
            EdgeRole::Text => "file-text",
            EdgeRole::FirstFrame | EdgeRole::MiddleFrame | EdgeRole::LastFrame => "frame",
            _ => "image",
        };
        let el = at(r)
            .flex()
            .items_center()
            .justify_center()
            .continuous(7.0 * z.0.clamp(0.4, 1.0))
            .border_1()
            .border_color(rgb(border))
            .bg(rgb(bg))
            .child(icon(glyph, (13.0 * z.0).max(6.0), fg))
            .when(s.hovered || s.refused, |el| {
                let name = port_label(*role);
                el.child(
                    div()
                        .absolute()
                        .right(px(r.w as f32 + 6.0))
                        .px(px(6.0))
                        .py(px(2.0))
                        .continuous(crate::radius::TOOLTIP)
                        .bg(rgb(theme::popover()))
                        .border_1()
                        .border_color(rgb(ink(0.12)))
                        .whitespace_nowrap()
                        .text_size(px(11.5))
                        .text_color(rgb(theme::foreground()))
                        .child(name),
                )
            });
        out.push(el.into_any_element());
    }
}

/// `First frame`, `Reference`.
pub(super) fn port_label(role: EdgeRole) -> String {
    let name = role.name();
    let mut c = name.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}
