//! Drawing the pane: title row, transcript (Trace-style lines, no
//! containers), composer, tray, approval card and popovers.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, App, BoxShadow, ClickEvent, Context, FontStyle, FontWeight, HighlightStyle, Hsla, InteractiveText, MouseButton, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle, Window, div, linear_color_stop, linear_gradient, point, prelude::*, px, relative, rgb,
};
use otto_proto::{ApprovalDecision, ApprovalMode, AuthMode, SessionStatus};

use super::{
    AgentPane, AgentPaneEvent, Conn, ENTER, FADE, FOLD, Overlay,
    editor::{Editor, EditorColors},
    markdown::{Block, Inline},
    transcript::{self, Detail, Group, Kind, Line, Row, Status, TodoStatus},
};
use crate::{
    assets::icon,
    motion,
    theme::{self, Continuous as _},
};

/// Width of the reading column.
const COLUMN: f32 = 760.0;
const LINE_H: f32 = 24.0;
/// Lines visible in a live group before older ones scroll away.
const LIVE_LINES: f32 = 6.0;
const TEXT: f32 = 13.5;

fn hsla(c: u32, alpha: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a *= alpha;
    h
}

fn faint() -> u32 {
    theme::mix(theme::background(), theme::foreground(), if theme::is_light() { 0.04 } else { 0.06 })
}

fn mono() -> SharedString {
    theme::summary().1.into()
}

/// Eased progress of an animation, or `None` once done (instant with reduce motion).
fn eased(started: Instant, duration: std::time::Duration, now: Instant, cx: &App) -> Option<f32> {
    if cx.reduce_motion() {
        return None;
    }
    motion::progress(started, duration, now)
}

/// A running title: a bright band sweeping across muted text.
fn shimmer(text: &str, now: Instant, epoch: Instant, cx: &App) -> StyledText {
    let base = theme::muted();
    let bright = theme::foreground();
    if cx.reduce_motion() || text.is_empty() {
        return StyledText::new(text.to_owned())
            .with_highlights([(0..text.len(), HighlightStyle { color: Some(rgb(bright).into()), ..Default::default() })]);
    }
    let period = 1.8_f32;
    let t = (now.duration_since(epoch).as_secs_f32() % period) / period;
    let center = -0.3 + t * 1.6;
    #[allow(clippy::cast_precision_loss)]
    let n = text.chars().count().max(1) as f32;
    let highlights: Vec<_> = text
        .char_indices()
        .enumerate()
        .map(|(i, (at, ch))| {
            #[allow(clippy::cast_precision_loss)]
            let x = i as f32 / n;
            let k = (1.0 - (x - center).abs() / 0.18).clamp(0.0, 1.0);
            (at..at + ch.len_utf8(), HighlightStyle { color: Some(rgb(theme::mix(base, bright, 0.35 + 0.65 * k)).into()), ..Default::default() })
        })
        .collect();
    StyledText::new(text.to_owned()).with_highlights(highlights)
}

fn session_dot(status: SessionStatus) -> Option<u32> {
    match status {
        SessionStatus::Busy => Some(theme::accent()),
        SessionStatus::WaitingApproval => Some(theme::warning()),
        SessionStatus::Error => Some(theme::danger()),
        SessionStatus::Idle | SessionStatus::Archived => None,
    }
}

fn ago(ms: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let s = (now - ms).max(0) / 1000;
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

fn provider_glyph(provider: &str) -> String {
    match provider {
        "anthropic" => "A\\".into(),
        "openai" => "OpenAI".into(),
        "google" => "G".into(),
        "xai" => "xAI".into(),
        other => other.into(),
    }
}

fn provider_name(provider: &str) -> String {
    match provider {
        "anthropic" => "Anthropic".into(),
        "openai" => "OpenAI".into(),
        "google" => "Google".into(),
        "xai" => "xAI".into(),
        other => {
            let mut c = other.chars();
            c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
        }
    }
}

fn mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Auto => "Auto",
        ApprovalMode::AskOnEffects | ApprovalMode::AskAll => "Ask",
        ApprovalMode::Plan => "Plan",
    }
}

/// A small text button.
fn chip(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(8.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .continuous(7.0)
        .text_size(px(12.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme::hover_fill()))
        .child(label.into())
}

impl AgentPane {
    /// Something moves this frame (shimmer, entries, folds, fades).
    fn animating(&self, now: Instant) -> bool {
        self.running
            || self.rows.iter().any(|r| matches!(r, Row::Group(g) if g.live))
            || self.entered.values().any(|at| now.duration_since(*at) < ENTER)
            || self.fold_at.values().any(|at| now < *at + FOLD)
            || self.marks.as_ref().is_some_and(|(_, m)| m.iter().any(|(_, at)| now.duration_since(*at) < FADE))
            || self.tray_shown.is_some_and(|at| now.duration_since(at) < ENTER)
            || self.morph.is_some_and(|at| now.duration_since(at) < ENTER)
    }

    pub(super) fn render_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let now = Instant::now();
        if self.animating(now) && !cx.reduce_motion() {
            window.request_animation_frame();
        }
        // Folding groups change height every frame; a finished fold is measured once more.
        let mut settled = Vec::new();
        for (ix, row) in self.rows.iter().enumerate() {
            if let Row::Group(g) = row
                && let Some(at) = self.fold_at.get(&g.key)
                && now >= *at
            {
                self.list.remeasure_items(ix..ix + 1);
                if now >= *at + FOLD || cx.reduce_motion() {
                    settled.push(g.key.clone());
                }
            }
        }
        for key in settled {
            self.fold_at.remove(&key);
        }
        if self.list.is_following_tail() {
            self.unseen = false;
        }
        let bg = theme::background();
        div()
            .id(gpui::ElementId::Name(format!("agent-{}", self.block_id).into()))
            .key_context("AgentPane")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(bg))
            .font_family(theme::ui_font())
            .text_size(px(TEXT))
            .text_color(rgb(theme::foreground()))
            .on_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.focus.is_focused(window) || !this.focus.contains_focused(window, cx) {
                        this.focus_composer(window, cx);
                    }
                }),
            )
            .child(self.render_title(cx))
            .child(self.render_body(window, cx))
            .child(self.render_bottom(window, cx))
            .children(self.render_overlay(window, cx))
            .into_any_element()
    }

    fn render_title(&self, cx: &mut Context<Self>) -> AnyElement {
        let title = self
            .mirror()
            .map(|m| m.info.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| if self.session_id.is_some() { "Untitled".into() } else { "New session".into() });
        div()
            .flex_none()
            .h(px(34.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .child(
                div()
                    .id("agent-title")
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .min_w_0()
                    .px(px(6.0))
                    .h(px(24.0))
                    .continuous(7.0)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_fill()))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Sessions, window, cx)))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    )
                    .child(div().flex_none().text_size(px(11.0)).text_color(rgb(theme::muted())).child("⌄")),
            )
            .into_any_element()
    }

    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let body = div().flex_1().min_h_0().w_full().relative();
        let centered =
            |child: AnyElement| div().size_full().flex().flex_col().items_center().justify_center().px(px(24.0)).child(child).into_any_element();
        match &self.conn {
            Conn::Connecting => {
                return body
                    .child(centered(div().text_color(rgb(theme::muted())).child("Connecting to otto…").into_any_element()))
                    .into_any_element();
            }
            Conn::Failed(error) => {
                let retry = chip("agent-retry", "Try again").text_color(rgb(theme::accent())).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.conn = Conn::Connecting;
                    this.load(cx);
                    cx.notify();
                }));
                return body
                    .child(centered(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().text_color(rgb(theme::muted())).child(error.clone()))
                            .child(retry)
                            .into_any_element(),
                    ))
                    .into_any_element();
            }
            Conn::Ready => {}
        }
        if self.catalog.as_ref().is_some_and(|c| !c.any_signed_in()) {
            return body.child(centered(self.render_signin(window, cx))).into_any_element();
        }
        if let Some(error) = &self.error {
            return body.child(centered(div().text_color(rgb(theme::muted())).child(error.clone()).into_any_element())).into_any_element();
        }
        if self.rows.is_empty() {
            let hint = if self.session_id.is_some() && self.state.is_none() {
                div().text_color(rgb(theme::muted())).child("Loading…")
            } else {
                let place = self.project.as_ref().and_then(|p| std::path::Path::new(&p.root).file_name().map(|n| n.to_string_lossy().into_owned()));
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .child(div().text_size(px(16.0)).font_weight(FontWeight::MEDIUM).child("What should we work on?"))
                    .children(place.map(|p| div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(p)))
            };
            return body.child(centered(hint.into_any_element())).into_any_element();
        }
        let list = gpui::list(self.list.clone(), cx.processor(|this, ix: usize, window, cx| this.render_row(ix, window, cx))).size_full();
        let pill = (self.unseen && !self.list.is_following_tail()).then(|| {
            div().absolute().bottom(px(8.0)).left_0().right_0().flex().justify_center().child(
                div()
                    .id("agent-new-activity")
                    .px(px(10.0))
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .capsule()
                    .bg(rgb(theme::popover()))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .shadow_sm()
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .child("↓ New activity")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.list.scroll_to_end();
                        this.list.set_follow_mode(gpui::FollowMode::Tail);
                        this.unseen = false;
                        cx.notify();
                    })),
            )
        });
        body.child(list).children(pill).into_any_element()
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else { return div().into_any_element() };
        let now = Instant::now();
        let content = match &row {
            Row::User { text, .. } => div()
                .pt(px(if ix == 0 { 12.0 } else { 22.0 }))
                .pb(px(8.0))
                .flex()
                .justify_end()
                .child(div().max_w(relative(0.8)).text_color(rgb(theme::muted())).child(text.clone()))
                .into_any_element(),
            Row::Group(g) => self.render_group(g, now, window, cx),
            Row::Text { key, text, streaming } => self.render_text(ix, key, text, *streaming, now, cx),
        };
        let enter = self.entered.get(row.key()).and_then(|at| eased(*at, ENTER, now, cx));
        let content = match enter {
            Some(t) => div().relative().top(px((1.0 - t) * 6.0)).opacity(t).child(content).into_any_element(),
            None => content,
        };
        div().w_full().flex().justify_center().px(px(20.0)).child(div().w_full().max_w(px(COLUMN)).child(content)).into_any_element()
    }

    fn group_started(&self, g: &Group) -> Option<u64> {
        g.lines.iter().filter_map(|l| self.times.get(&l.key).and_then(|s| s.start)).min()
    }

    fn render_group(&mut self, g: &Group, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let key = g.key.clone();
        let fold = if g.live {
            0.0
        } else {
            match self.fold_at.get(&key) {
                Some(at) if now < *at => 0.0,
                Some(at) => eased(*at, FOLD, now, cx).unwrap_or(1.0),
                None => 1.0,
            }
        };
        let open = self.toggled.contains(&key);
        let muted = theme::muted();
        let header_text = if g.live {
            match self.group_started(g) {
                Some(start) => format!("Working for {}", transcript::dur(self.now().saturating_sub(start))),
                None => "Working".to_owned(),
            }
        } else {
            g.summary.clone()
        };
        let toggle_key = key.clone();
        let header =
            div()
                .id(gpui::ElementId::Name(format!("hdr-{key}").into()))
                .h(px(LINE_H))
                .flex()
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if !this.toggled.remove(&toggle_key) {
                        this.toggled.insert(toggle_key.clone());
                    }
                    if let Some(ix) = this.rows.iter().position(|r| r.key() == toggle_key) {
                        this.list.remeasure_items(ix..ix + 1);
                    }
                    cx.notify();
                }))
                .child(div().w(px(16.0)).flex_none().flex().justify_center().map(|el| {
                    if g.live {
                        el.child(crate::browser_pane::spinner(&key, 13.0, muted, cx))
                    } else {
                        el.text_size(px(12.0)).text_color(rgb(if g.failed { theme::danger() } else { muted })).child(if open && fold >= 1.0 {
                            "⌄"
                        } else {
                            "›"
                        })
                    }
                }))
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().map(|el| {
                    if g.live { el.child(shimmer(&header_text, now, self.epoch, cx)) } else { el.text_color(rgb(muted)).child(header_text) }
                }));
        let mut column = div().py(px(4.0)).flex().flex_col().child(header);
        if fold >= 1.0 {
            if open {
                let lines: Vec<AnyElement> = g.lines.iter().map(|l| self.render_line(l, false, 1.0, now, window, cx)).collect();
                column = column.child(div().pl(px(18.0)).flex().flex_col().children(lines));
            }
            return column.into_any_element();
        }
        // Live, or folding: the newest lines under a top fade.
        let frozen = self.frozen.as_ref().filter(|(k, _)| *k == key).map(|(_, n)| *n);
        let shown = &g.lines[..frozen.unwrap_or(g.lines.len()).min(g.lines.len())];
        let n = shown.len();
        let lines: Vec<AnyElement> = shown
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let dim = if n - i <= 2 { 1.0 } else { 0.55 };
                self.render_line(l, g.live && i + 1 == n, dim, now, window, cx)
            })
            .collect();
        let max = LINE_H * LIVE_LINES;
        let hover_key = key.clone();
        let count = g.lines.len();
        let window_el = div()
            .id(gpui::ElementId::Name(format!("live-{key}").into()))
            .relative()
            .pl(px(18.0))
            .max_h(px(max * (1.0 - fold)))
            .opacity(1.0 - fold)
            .overflow_hidden()
            .flex()
            .flex_col()
            .justify_end()
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.frozen = hovered.then(|| (hover_key.clone(), count));
                cx.notify();
            }))
            .children(lines.into_iter().map(|l| div().flex_none().child(l)))
            .when(f32::from(window.line_height()) > 0.0 && n as f32 > LIVE_LINES - 0.5, |el| {
                let bg = theme::background();
                el.child(div().absolute().top_0().left_0().right_0().h(px(28.0)).bg(linear_gradient(
                    180.0,
                    linear_color_stop(hsla(bg, 1.0), 0.0),
                    linear_color_stop(hsla(bg, 0.0), 1.0),
                )))
            });
        column.child(window_el).into_any_element()
    }

    fn render_line(&mut self, line: &Line, newest: bool, dim: f32, now: Instant, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let key = line.key.clone();
        let open = self.details.contains(&key);
        let glyph: AnyElement = match line.status {
            Status::Running => crate::browser_pane::spinner(&key, 12.0, muted, cx),
            Status::Error => div().text_color(rgb(theme::danger())).child("✕").into_any_element(),
            Status::Pending => div().text_color(rgb(muted)).opacity(0.6).child("○").into_any_element(),
            Status::Done => div().text_color(rgb(muted)).child(if line.kind == Kind::Edit { "✎" } else { "✓" }).into_any_element(),
        };
        let title = div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().map(|el| match line.status {
            Status::Running if newest => el.child(shimmer(&line.title, now, self.epoch, cx)),
            Status::Error => el.text_color(rgb(theme::danger())).child(line.title.clone()),
            Status::Pending => el.text_color(rgb(muted)).child(line.title.clone()),
            _ => el.child(line.title.clone()),
        });
        let detail_key = key.clone();
        let has_detail = line.detail.is_some();
        let reveal = line.reveal.clone();
        let row = div()
            .id(gpui::ElementId::Name(format!("line-{key}").into()))
            .h(px(LINE_H))
            .flex()
            .items_center()
            .gap(px(8.0))
            .when(has_detail, |el| el.cursor_pointer())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if !has_detail {
                    return;
                }
                if !this.details.remove(&detail_key) {
                    this.details.insert(detail_key.clone());
                }
                this.list.remeasure();
                cx.notify();
            }))
            .child(div().w(px(16.0)).flex_none().flex().justify_center().text_size(px(12.0)).child(glyph))
            .child(title)
            .child(div().flex_1())
            .children(line.desc.clone().map(|d| div().flex_none().text_size(px(12.0)).text_color(rgb(muted)).child(d)))
            .when(has_detail, |el| {
                el.child(div().flex_none().w(px(12.0)).text_size(px(12.0)).text_color(rgb(muted)).child(if open { "⌄" } else { "›" }))
            })
            .children(reveal.map(|block| {
                div()
                    .id(gpui::ElementId::Name(format!("reveal-{key}").into()))
                    .flex_none()
                    .px(px(4.0))
                    .continuous(5.0)
                    .text_size(px(12.0))
                    .text_color(rgb(muted))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_fill()).text_color(rgb(theme::foreground())))
                    .child("↗")
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        cx.emit(AgentPaneEvent::Reveal(block.clone()));
                    }))
            }));
        let stream = line.stream.as_ref().filter(|s| !s.trim().is_empty()).map(|s| {
            let tail: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = tail[tail.len().saturating_sub(4)..].join("\n");
            div().pl(px(24.0)).pb(px(4.0)).text_size(px(12.0)).line_height(px(18.0)).text_color(rgb(muted)).opacity(0.8).child(tail)
        });
        let detail = open.then(|| line.detail.as_ref().map(|d| render_detail(&key, d))).flatten();
        let enter = self.entered.get(&key).and_then(|at| eased(*at, ENTER, now, cx));
        let (shift, alpha) = enter.map_or((0.0, 1.0), |t| ((1.0 - t) * 6.0, t));
        div().relative().top(px(shift)).opacity(alpha * dim).child(row).children(stream).children(detail).into_any_element()
    }

    fn markdown(&mut self, key: &str, text: &str) -> std::rc::Rc<Vec<Block>> {
        if let Some((cached, blocks)) = self.parsed.get(key)
            && cached == text
        {
            return blocks.clone();
        }
        let blocks = std::rc::Rc::new(super::markdown::parse(text));
        self.parsed.insert(key.to_owned(), (text.to_owned(), blocks.clone()));
        blocks
    }

    fn render_text(&mut self, ix: usize, key: &str, text: &str, streaming: bool, now: Instant, cx: &mut Context<Self>) -> AnyElement {
        let blocks = self.markdown(key, text);
        // Freshly arrived text: (byte length before it, opacity), mapped onto the last block.
        let fades: Vec<(usize, f32)> = match &self.marks {
            Some((k, marks)) if k == key && !cx.reduce_motion() => {
                let mut out = Vec::new();
                let mut prev = None;
                for (len, at) in marks {
                    if let (Some(prev), Some(t)) = (prev, motion::progress(*at, FADE, now)) {
                        out.push((text.len().saturating_sub(prev), t));
                    }
                    prev = Some(*len);
                }
                out
            }
            _ => Vec::new(),
        };
        let mut els: Vec<AnyElement> = Vec::new();
        let last = blocks.len().saturating_sub(1);
        for (i, block) in blocks.iter().enumerate() {
            let tail = (i == last).then_some((&fades[..], streaming));
            els.push(render_block(&format!("{key}-{i}"), block, tail));
        }
        if streaming && matches!(blocks.last(), None | Some(Block::Code { .. } | Block::Rule)) {
            els.push(div().text_color(rgb(theme::muted())).child("▍").into_any_element());
        }
        let answer = !streaming && matches!(self.rows.get(ix + 1), None | Some(Row::User { .. }));
        let actions = answer.then(|| {
            let copy = text.to_owned();
            let question = self.rows[..ix].iter().rev().find_map(|r| if let Row::User { text, .. } = r { Some(text.clone()) } else { None });
            let group = SharedString::from(format!("answer-{key}"));
            div()
                .flex()
                .gap(px(2.0))
                .h(px(24.0))
                .opacity(0.0)
                .group_hover(group, |s| s.opacity(1.0))
                .text_color(rgb(theme::muted()))
                .child(
                    chip(gpui::ElementId::Name(format!("copy-{key}").into()), "Copy")
                        .on_click(move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))),
                )
                .children(question.map(|q| {
                    chip(gpui::ElementId::Name(format!("retry-{key}").into()), "Retry")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.retry(q.clone(), cx)))
                }))
        });
        div().group(SharedString::from(format!("answer-{key}"))).py(px(6.0)).flex().flex_col().children(els).children(actions).into_any_element()
    }

    // ---- bottom: notice, card, tray, composer, footer ----

    fn render_bottom(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let notice = self.notice.clone().map(|n| div().pb(px(6.0)).px(px(6.0)).text_size(px(12.0)).text_color(rgb(theme::danger())).child(n));
        div()
            .flex_none()
            .w_full()
            .flex()
            .justify_center()
            .px(px(14.0))
            .pb(px(8.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(COLUMN + 16.0))
                    .flex()
                    .flex_col()
                    .children(notice)
                    .children(self.render_card(cx))
                    .children(self.render_tray(cx))
                    .child(self.render_composer(window, cx))
                    .child(self.render_footer(cx)),
            )
            .into_any_element()
    }

    fn render_card(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let m = self.mirror()?;
        let card = || {
            div()
                .mb(px(8.0))
                .p(px(12.0))
                .continuous(12.0)
                .bg(rgb(theme::popover()))
                .border_1()
                .border_color(rgb(theme::border()))
                .flex()
                .flex_col()
                .gap(px(8.0))
        };
        if let Some(q) = m.pending_questions.first() {
            let options: Vec<AnyElement> = q
                .options
                .iter()
                .enumerate()
                .map(|(i, option)| {
                    let id = q.question_id.clone();
                    let answer = option.clone();
                    chip(("agent-option", i), option.clone())
                        .bg(theme::control_fill())
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.answer_question(id.clone(), answer.clone(), cx)))
                        .into_any_element()
                })
                .collect();
            return Some(
                card()
                    .child(div().font_weight(FontWeight::MEDIUM).child(q.question.clone()))
                    .when(!options.is_empty(), |el| el.child(div().flex().flex_wrap().gap(px(6.0)).children(options)))
                    .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child("Or type an answer below ⏎"))
                    .into_any_element(),
            );
        }
        let a = m.pending_approvals.first()?;
        let more = m.pending_approvals.len() - 1;
        let summary = transcript::clip(&a.summary, 160);
        let button = |id: &'static str, label: &'static str, hint: &'static str, primary: bool| {
            div()
                .id(id)
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(10.0))
                .h(px(26.0))
                .continuous(8.0)
                .text_size(px(12.5))
                .cursor_pointer()
                .map(|el| if primary { el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())) } else { el.bg(theme::control_fill()) })
                .hover(|s| s.opacity(0.9))
                .child(label)
                .when(!hint.is_empty(), |el| el.child(div().text_size(px(11.0)).opacity(0.7).child(hint)))
        };
        Some(
            card()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon("lock", 13.0, theme::muted()))
                        .child(div().font_weight(FontWeight::MEDIUM).child(format!("Allow {}?", a.tool)))
                        .when(more > 0, |el| el.child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(format!("+{more} more")))),
                )
                .when(!summary.is_empty(), |el| el.child(div().text_size(px(12.5)).text_color(rgb(theme::muted())).child(summary)))
                .child(
                    div()
                        .flex()
                        .gap(px(6.0))
                        .child(
                            button("agent-allow", "Allow", "⏎", true)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.answer_approval(ApprovalDecision::Allow, cx))),
                        )
                        .child(
                            button("agent-always", "Always", "", false)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.answer_approval(ApprovalDecision::AlwaysAllow, cx))),
                        )
                        .child(
                            button("agent-deny", "Deny", "esc", false)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.answer_approval(ApprovalDecision::Deny, cx))),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_tray(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let queue: Vec<String> = self
            .mirror()
            .map(|m| {
                m.queue
                    .iter()
                    .map(|q| {
                        q.input
                            .iter()
                            .filter_map(|p| if let otto_proto::InputPart::Text { text } = p { Some(text.as_str()) } else { None })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect()
            })
            .unwrap_or_default();
        if self.todos.is_empty() && queue.is_empty() {
            return None;
        }
        let now = Instant::now();
        let muted = theme::muted();
        let (done, total) = transcript::progress(&self.todos);
        let current = transcript::current(&self.todos).map(|t| t.step.clone());
        let open = self.tray_open;
        let mut tray = div()
            .mx(px(14.0))
            .mb(px(-12.0))
            .pb(px(12.0))
            .px(px(12.0))
            .pt(px(4.0))
            .continuous(12.0)
            .bg(theme::control_fill())
            .text_size(px(12.0))
            .flex()
            .flex_col();
        if !self.todos.is_empty() {
            tray = tray.child(
                div()
                    .id("agent-tray")
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.tray_open = !this.tray_open;
                        cx.notify();
                    }))
                    .child(div().flex_none().text_color(rgb(muted)).child("☰ Todos"))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().when(!open, |el| el.children(current)))
                    .child(div().flex_none().text_color(rgb(muted)).child(format!("{done}/{total} done")))
                    .child(div().flex_none().text_color(rgb(muted)).child(if open { "⌄" } else { "⌃" })),
            );
            if open {
                tray = tray.children(self.todos.iter().map(|t| {
                    let (glyph, color) = match t.status {
                        TodoStatus::Completed => ("✓", muted),
                        TodoStatus::InProgress => ("◐", theme::accent()),
                        TodoStatus::Cancelled => ("⊘", muted),
                        TodoStatus::Pending => ("○", muted),
                    };
                    div()
                        .h(px(22.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().w(px(14.0)).flex_none().text_color(rgb(color)).child(glyph))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .when(matches!(t.status, TodoStatus::Completed | TodoStatus::Cancelled), |el| el.text_color(rgb(muted)))
                                .when(t.status == TodoStatus::Cancelled, |el| el.line_through())
                                .child(t.step.clone()),
                        )
                }));
            }
        }
        if !queue.is_empty() {
            tray = tray.child(div().flex().flex_wrap().gap(px(6.0)).py(px(4.0)).children(queue.into_iter().map(|q| {
                div()
                    .max_w(px(260.0))
                    .px(px(8.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .capsule()
                    .bg(rgb(theme::background()))
                    .text_color(rgb(muted))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(format!("Queued · {}", transcript::clip(&q, 60)))
            })));
        }
        let shift = self.tray_shown.and_then(|at| eased(at, ENTER, now, cx));
        Some(match shift {
            Some(t) => div().relative().top(px((1.0 - t) * 12.0)).opacity(t).child(tray).into_any_element(),
            None => tray.into_any_element(),
        })
    }

    fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.editor.read(cx).focus.is_focused(window);
        let empty = self.editor.read(cx).text().trim().is_empty();
        let question = self.mirror().is_some_and(|m| !m.pending_questions.is_empty());
        let placeholder = if question {
            "Answer…"
        } else if self.running {
            "Queue a follow-up"
        } else if self.session_id.is_some() {
            "Reply"
        } else {
            "Ask anything"
        };
        self.editor.update(cx, |e, cx| e.set_placeholder(placeholder, cx));
        let colors = EditorColors {
            text: rgb(theme::foreground()).into(),
            placeholder: rgb(theme::muted()).into(),
            caret: rgb(theme::accent()).into(),
            selection: hsla(theme::selection(), 0.6),
        };
        let editor = Editor::element(&self.editor, 8, colors, cx);
        let stop = self.running && empty;
        let enabled = stop || !empty;
        let now = Instant::now();
        let morph = self.morph.and_then(|at| eased(at, ENTER, now, cx)).unwrap_or(1.0);
        let send = div()
            .id("agent-send")
            .flex_none()
            .size(px(30.0))
            .flex()
            .items_center()
            .justify_center()
            .capsule()
            .map(|el| {
                if enabled {
                    el.bg(rgb(theme::foreground())).text_color(rgb(theme::background())).cursor_pointer()
                } else {
                    el.bg(theme::control_fill()).text_color(rgb(theme::muted()))
                }
            })
            .child(
                div()
                    .text_size(px((if stop { 11.0 } else { 15.0 }) * (0.6 + 0.4 * morph)))
                    .opacity(morph)
                    .font_weight(FontWeight::BOLD)
                    .child(if stop { "■" } else { "↑" }),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if stop {
                    this.stop(cx);
                } else {
                    this.submit(window, cx);
                }
            }));
        let menu = div()
            .id("agent-menu")
            .flex_none()
            .size(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .capsule()
            .text_color(rgb(theme::muted()))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .child("⋮")
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Menu, window, cx)));
        let accent = theme::accent();
        div()
            .flex()
            .items_end()
            .gap(px(6.0))
            .p(px(8.0))
            .continuous(16.0)
            .bg(rgb(theme::popover()))
            .border_1()
            .border_color(rgb(if focused { theme::mix(theme::border(), accent, 0.45) } else { theme::border() }))
            .when(focused, |el| {
                el.shadow(vec![BoxShadow {
                    color: hsla(accent, 0.18),
                    offset: point(px(0.0), px(0.0)),
                    blur_radius: px(0.0),
                    spread_radius: px(3.0),
                    inset: false,
                }])
            })
            .child(menu)
            .child(div().flex_1().min_w_0().py(px(4.0)).line_height(px(20.0)).child(editor))
            .child(send)
            .into_any_element()
    }

    fn render_footer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let info = self.mirror().map(|m| m.info.clone());
        let catalog = self.catalog.as_ref();
        let agent = info.as_ref().map(|i| i.agent.clone()).or_else(|| self.choice.agent.clone()).unwrap_or_else(|| "build".into());
        let (provider, model) = match (&info, &self.choice.model) {
            (Some(i), _) => (i.provider.clone(), i.model.clone()),
            (None, Some((p, m))) => (p.clone(), m.clone()),
            (None, None) => (String::new(), "Default model".into()),
        };
        let sub = catalog.is_some_and(|c| c.subscription(&provider));
        let window_size =
            catalog.and_then(|c| c.models.iter().find(|m| m.id == model && m.provider.as_str() == provider)).map(|m| m.caps.context_window);
        let mut usage = Vec::new();
        if let (Some(used), Some(size)) = (self.context, window_size.filter(|s| *s > 0)) {
            #[allow(clippy::cast_precision_loss)]
            usage.push(format!("{:.0}%", used as f64 * 100.0 / size as f64));
        }
        if let Some(cost) = info.as_ref().map(|i| i.usage.cost_usd).filter(|c| *c > 0.0) {
            usage.push(format!("${cost:.2}"));
        }
        let mode = self.mirror().map_or(self.choice.mode, |m| m.info.approval_mode);
        let model_label =
            if provider.is_empty() { model } else { format!("{} / {}{}", provider_glyph(&provider), model, if sub { " (sub)" } else { "" }) };
        div()
            .h(px(28.0))
            .px(px(4.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .text_size(px(11.5))
            .text_color(rgb(muted))
            .child(
                chip("agent-agent", format!("{} ⌃", agent.to_uppercase()))
                    .font_weight(FontWeight::MEDIUM)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Agents, window, cx))),
            )
            .child(
                chip("agent-model", model_label)
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Models, window, cx))),
            )
            .when(mode != ApprovalMode::Auto, |el| el.child(div().flex_none().px(px(6.0)).child(mode_label(mode))))
            .child(div().flex_1())
            .child(div().flex_none().px(px(6.0)).child(usage.join(" · ")))
            .into_any_element()
    }

    // ---- popovers ----

    fn render_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let overlay = self.overlay?;
        let card = div()
            .id("agent-popover")
            .occlude()
            .absolute()
            .left(px(12.0))
            .right(px(12.0))
            .max_w(px(if overlay == Overlay::Sessions { 440.0 } else { 340.0 }))
            .continuous(10.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_lg()
            .text_size(px(12.5))
            .flex()
            .flex_col()
            .overflow_hidden()
            .map(
                |el| if overlay == Overlay::Sessions { el.top(px(34.0)) } else { el.bottom(px(if overlay == Overlay::Menu { 84.0 } else { 36.0 })) },
            );
        let body = match overlay {
            Overlay::Sessions => self.render_sessions(window, cx),
            Overlay::Models => self.render_models(cx),
            Overlay::Agents => self.render_agents(cx),
            Overlay::Menu => self.render_menu(cx),
        };
        let backdrop = div()
            .id("agent-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_overlay(window, cx)));
        let card = card.when(overlay == Overlay::Sessions, |el| {
            el.on_action(cx.listener(|this, _: &crate::commands::act::NewWindow, window, cx| this.new_session(window, cx)))
        });
        Some(div().absolute().top_0().left_0().size_full().child(backdrop).child(card.child(body)).into_any_element())
    }

    fn filter_row(&self) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(34.0))
            .px(px(12.0))
            .border_b_1()
            .border_color(rgb(theme::border()))
            .child(icon("filter", 13.0, theme::muted()))
            .child(self.filter.clone())
            .into_any_element()
    }

    fn menu_row(&self, id: gpui::ElementId, index: usize, selected: bool, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(28.0))
            .mx(px(4.0))
            .px(px(8.0))
            .continuous(crate::radius::MENU_ROW)
            .when(selected, |el| el.bg(theme::row_fill()))
            .when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill())))
            .cursor_pointer()
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.selected != index {
                    this.selected = index;
                    cx.notify();
                }
            }))
    }

    fn render_sessions(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let current = self.session_id.clone();
        let rows: Vec<(String, String, String, SessionStatus, i64)> =
            self.session_choices(cx).into_iter().map(|s| (s.id.clone(), super::session_label(s), s.model.clone(), s.status, s.updated_at)).collect();
        let count = rows.len();
        let mut list = div().id("agent-sessions").flex().flex_col().py(px(4.0)).max_h(px(320.0)).overflow_y_scroll();
        for (i, (id, title, model, status, updated)) in rows.into_iter().enumerate() {
            let checked = current.as_deref() == Some(id.as_str());
            let state = match status {
                SessionStatus::Busy => "running",
                SessionStatus::WaitingApproval => "needs you",
                SessionStatus::Error => "error",
                _ => "",
            };
            let dot = session_dot(status);
            list = list.child(
                self.menu_row(gpui::ElementId::Name(format!("session-{id}").into()), i, i == self.selected, cx)
                    .child(div().w(px(10.0)).flex_none().flex().justify_center().children(dot.map(|c| {
                        if status == SessionStatus::WaitingApproval {
                            div().text_size(px(9.0)).text_color(rgb(c)).child("◆").into_any_element()
                        } else {
                            div().size(px(6.0)).rounded_full().bg(rgb(c)).into_any_element()
                        }
                    })))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .when(checked, |el| el.font_weight(FontWeight::MEDIUM))
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(110.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(rgb(muted))
                            .child(model),
                    )
                    .child(div().flex_none().w(px(58.0)).text_size(px(11.0)).text_color(rgb(muted)).child(state))
                    .child(div().flex_none().w(px(28.0)).text_size(px(11.0)).text_color(rgb(muted)).text_right().child(ago(updated)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.pick_session(id.clone(), window, cx))),
            );
        }
        list = list.child(
            self.menu_row("agent-new-session".into(), count, self.selected == count, cx)
                .child(div().w(px(10.0)).flex_none().text_color(rgb(muted)).child("＋"))
                .child(div().flex_1().child("New session"))
                .child(div().flex_none().text_size(px(11.0)).text_color(rgb(muted)).child("⌘N"))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.new_session(window, cx))),
        );
        div().flex().flex_col().child(self.filter_row()).child(list).into_any_element()
    }

    fn render_models(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let current = self.choice.model.clone();
        let catalog = self.catalog.clone().unwrap_or_default();
        let rows: Vec<(String, String, String)> =
            self.model_choices(cx).into_iter().map(|m| (m.provider.as_str().to_owned(), m.id.clone(), m.name.clone())).collect();
        let mut list = div().id("agent-models").flex().flex_col().py(px(4.0)).max_h(px(300.0)).overflow_y_scroll();
        if rows.is_empty() {
            list = list.child(div().px(px(12.0)).py(px(8.0)).text_color(rgb(muted)).child("No models"));
        }
        for (i, (provider, id, name)) in rows.into_iter().enumerate() {
            let checked = current.as_ref().is_some_and(|(p, m)| *p == provider && *m == id);
            let sub = catalog.subscription(&provider);
            list = list.child(
                self.menu_row(gpui::ElementId::Name(format!("model-{provider}-{id}").into()), i, i == self.selected, cx)
                    .child(div().w(px(12.0)).flex_none().when(checked, |el| el.child(icon("check", 11.0, theme::foreground()))))
                    .child(div().flex_none().w(px(44.0)).text_size(px(11.0)).text_color(rgb(muted)).child(provider_glyph(&provider)))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(id))
                    .when(!name.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_none()
                                .max_w(px(110.0))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(11.0))
                                .text_color(rgb(muted))
                                .child(name),
                        )
                    })
                    .when(sub, |el| el.child(div().flex_none().text_size(px(10.5)).text_color(rgb(muted)).child("sub")))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.activate_choice(i, window, cx))),
            );
        }
        div()
            .flex()
            .flex_col()
            .child(self.filter_row())
            .child(list)
            .when(self.session_id.is_some(), |el| {
                el.child(
                    div()
                        .px(px(12.0))
                        .py(px(6.0))
                        .border_t_1()
                        .border_color(rgb(theme::border()))
                        .text_size(px(11.0))
                        .text_color(rgb(muted))
                        .child("Applies to new sessions"),
                )
            })
            .into_any_element()
    }

    fn render_agents(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let current = self.mirror().map(|m| m.info.agent.clone()).or_else(|| self.choice.agent.clone()).unwrap_or_else(|| "build".into());
        let mut agents: Vec<(String, String)> =
            self.catalog.as_ref().map(|c| c.agents.iter().map(|a| (a.name.clone(), a.description.clone())).collect()).unwrap_or_default();
        if agents.is_empty() {
            agents = vec![("build".into(), String::new()), ("plan".into(), String::new())];
        }
        let mut list = div().id("agent-agents").flex().flex_col().py(px(4.0)).max_h(px(300.0)).overflow_y_scroll();
        for (i, (name, description)) in agents.into_iter().enumerate() {
            let checked = name == current;
            let pick = name.clone();
            list = list.child(
                self.menu_row(gpui::ElementId::Name(format!("agent-{name}").into()), i, false, cx)
                    .child(div().w(px(12.0)).flex_none().when(checked, |el| el.child(icon("check", 11.0, theme::foreground()))))
                    .child(div().flex_none().font_weight(FontWeight::MEDIUM).child(name.to_uppercase()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(rgb(muted))
                            .child(transcript::clip(&description, 60)),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.choice.agent = Some(pick.clone());
                        this.close_overlay(window, cx);
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .child(list)
            .when(self.session_id.is_some(), |el| {
                el.child(
                    div()
                        .px(px(12.0))
                        .py(px(6.0))
                        .border_t_1()
                        .border_color(rgb(theme::border()))
                        .text_size(px(11.0))
                        .text_color(rgb(muted))
                        .child("Applies to new sessions"),
                )
            })
            .into_any_element()
    }

    fn render_menu(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let mode = self.mirror().map_or(self.choice.mode, |m| m.info.approval_mode);
        let mut list = div().id("agent-menu-list").flex().flex_col().py(px(4.0));
        list = list.child(div().px(px(12.0)).pt(px(4.0)).pb(px(2.0)).text_size(px(11.0)).text_color(rgb(muted)).child("Approvals"));
        for (i, (m, label, hint)) in [
            (ApprovalMode::Auto, "Auto", "Run tools without asking"),
            (ApprovalMode::AskOnEffects, "Ask", "Ask before changes"),
            (ApprovalMode::Plan, "Plan", "Read-only"),
        ]
        .into_iter()
        .enumerate()
        {
            let checked = mode_label(mode) == label;
            list = list.child(
                self.menu_row(("agent-mode", i).into(), i, false, cx)
                    .child(div().w(px(12.0)).flex_none().when(checked, |el| el.child(icon("check", 11.0, theme::foreground()))))
                    .child(div().flex_none().child(label))
                    .child(div().flex_1().text_size(px(11.0)).text_color(rgb(muted)).child(hint))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_mode(m, cx);
                        this.close_overlay(window, cx);
                    })),
            );
        }
        list = list.child(div().h(px(1.0)).mx(px(8.0)).my(px(4.0)).bg(rgb(theme::border())));
        list = list.child(
            self.menu_row("agent-menu-new".into(), 9, false, cx)
                .child(div().w(px(12.0)).flex_none().text_color(rgb(muted)).child("＋"))
                .child(div().flex_1().child("New session"))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.new_session(window, cx))),
        );
        list.into_any_element()
    }

    // ---- sign in ----

    fn render_signin(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let auth = self.catalog.as_ref().map(|c| c.auth.clone()).unwrap_or_default();
        let mut providers: Vec<String> = auth.iter().map(|a| a.provider.clone()).collect();
        providers.dedup();
        let mut column = div()
            .w_full()
            .max_w(px(380.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(div().text_size(px(16.0)).font_weight(FontWeight::MEDIUM).child("Sign in to start"))
            .child(div().text_size(px(12.5)).text_color(rgb(muted)).child("otto needs a model provider."));
        let field = |entity: &gpui::Entity<Editor>, cx: &mut Context<Self>| {
            let colors = EditorColors {
                text: rgb(theme::foreground()).into(),
                placeholder: rgb(muted).into(),
                caret: rgb(theme::accent()).into(),
                selection: hsla(theme::selection(), 0.6),
            };
            div()
                .flex_1()
                .min_w_0()
                .px(px(8.0))
                .py(px(4.0))
                .continuous(8.0)
                .bg(theme::well_fill())
                .border_1()
                .border_color(rgb(theme::border()))
                .child(Editor::element(entity, 1, colors, cx))
        };
        if let Some(pending) = &self.signin.pending {
            let provider = provider_name(&pending.provider);
            let url = pending.url.clone();
            column = column.child(div().font_weight(FontWeight::MEDIUM).child(format!("Signing in to {provider}")));
            if let Some(code) = &pending.user_code {
                column = column
                    .child(div().text_size(px(12.5)).text_color(rgb(muted)).child("Enter this code in your browser:"))
                    .child(div().text_size(px(18.0)).font_family(mono()).child(code.clone()))
                    .child(div().text_size(px(12.0)).text_color(rgb(muted)).child("Waiting…"));
            } else {
                column = column.child(div().text_size(px(12.5)).text_color(rgb(muted)).child("Finish in your browser, then paste the code:")).child(
                    div().flex().gap(px(6.0)).child(field(&self.code_field, cx)).child(
                        chip("agent-code-ok", "Continue")
                            .bg(theme::control_fill())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.submit_code(cx))),
                    ),
                );
            }
            column = column.child(
                div()
                    .flex()
                    .gap(px(4.0))
                    .child(chip("agent-login-open", "Open again").text_color(rgb(theme::accent())).on_click(move |_, _, cx| cx.open_url(&url)))
                    .child(chip("agent-login-cancel", "Cancel").text_color(rgb(muted)).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.signin = super::SignIn::default();
                        cx.notify();
                    }))),
            );
        } else {
            for (i, provider) in providers.iter().enumerate() {
                let modes: Vec<AuthMode> = auth.iter().filter(|a| a.provider == *provider).map(|a| a.mode).collect();
                let key_open = self.signin.key_for.as_deref() == Some(provider.as_str());
                let p1 = provider.clone();
                let p2 = provider.clone();
                column = column.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(div().flex_1().child(provider_name(provider)))
                        .when(modes.contains(&AuthMode::ApiKey), |el| {
                            el.child(chip(("agent-key", i), "API key").text_color(rgb(theme::accent())).on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| {
                                    this.signin.key_for = Some(p1.clone());
                                    this.signin.error = None;
                                    let focus = this.key_field.read(cx).focus.clone();
                                    window.focus(&focus, cx);
                                    cx.notify();
                                },
                            )))
                        })
                        .when(modes.contains(&AuthMode::Subscription), |el| {
                            el.child(chip(("agent-sub", i), "Sign in").text_color(rgb(theme::accent())).on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    this.login(p2.clone(), AuthMode::Subscription, None, cx);
                                },
                            )))
                        }),
                );
                if key_open {
                    column = column.child(
                        div().flex().gap(px(6.0)).child(field(&self.key_field, cx)).child(
                            chip("agent-key-save", "Save")
                                .bg(theme::control_fill())
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.submit_key(cx))),
                        ),
                    );
                }
            }
        }
        if self.signin.busy {
            column = column.child(div().text_size(px(12.0)).text_color(rgb(muted)).child("Working…"));
        }
        if let Some(error) = &self.signin.error {
            column = column.child(div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(error.clone()));
        }
        column.into_any_element()
    }
}

fn render_detail(key: &str, detail: &Detail) -> AnyElement {
    let muted = theme::muted();
    let base = div()
        .id(gpui::ElementId::Name(format!("detail-{key}").into()))
        .ml(px(24.0))
        .mt(px(2.0))
        .mb(px(6.0))
        .px(px(10.0))
        .py(px(8.0))
        .continuous(8.0)
        .bg(rgb(faint()))
        .text_size(px(12.0))
        .line_height(px(17.0))
        .text_color(rgb(muted))
        .max_h(px(260.0))
        .overflow_y_scroll();
    match detail {
        Detail::Text(text) => base.child(text.clone()).into_any_element(),
        Detail::Mono(text) => base
            .font_family(mono())
            .children(text.lines().map(|l| div().whitespace_nowrap().child(if l.is_empty() { " ".to_owned() } else { l.to_owned() })))
            .into_any_element(),
        Detail::Diff(text) => base
            .font_family(mono())
            .children(text.lines().map(|l| {
                let color = if l.starts_with('+') && !l.starts_with("+++") {
                    theme::success()
                } else if l.starts_with('-') && !l.starts_with("---") {
                    theme::danger()
                } else {
                    muted
                };
                div().whitespace_nowrap().text_color(rgb(color)).child(if l.is_empty() { " ".to_owned() } else { l.to_owned() })
            }))
            .into_any_element(),
    }
}

/// Styled runs of an inline: emphasis, code, links, and the fade of fresh text.
fn inline_text(id: &str, inline: &Inline, fades: &[(usize, f32)], caret: bool) -> AnyElement {
    let mut text = inline.text.clone();
    let caret_at = caret.then(|| {
        let at = text.len();
        text.push('▍');
        at
    });
    let len = text.len();
    let mut cuts: Vec<usize> = vec![0, len];
    for (r, _) in &inline.spans {
        cuts.extend([r.start, r.end]);
    }
    for (r, _) in &inline.links {
        cuts.extend([r.start, r.end]);
    }
    // Fades count back from the end of the text (before the caret).
    let body = caret_at.unwrap_or(len);
    let fade_cuts: Vec<(usize, f32)> = fades
        .iter()
        .map(|(from_end, alpha)| {
            let mut at = body.saturating_sub(*from_end);
            while !text.is_char_boundary(at) {
                at += 1;
            }
            (at, *alpha)
        })
        .collect();
    cuts.extend(fade_cuts.iter().map(|(at, _)| *at));
    cuts.extend(caret_at);
    cuts.sort_unstable();
    cuts.dedup();
    let fg = theme::foreground();
    let mut highlights = Vec::new();
    let mut code_ranges = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let style = inline.spans.iter().find(|(r, _)| r.start <= a && b <= r.end).map(|(_, s)| *s).unwrap_or_default();
        let link = inline.links.iter().any(|(r, _)| r.start <= a && b <= r.end);
        let fade = fade_cuts.iter().rev().find(|(at, _)| *at <= a).map(|(_, alpha)| *alpha);
        let is_caret = caret_at == Some(a);
        let mut h = HighlightStyle::default();
        if style.bold {
            h.font_weight = Some(FontWeight::SEMIBOLD);
        }
        if style.italic {
            h.font_style = Some(FontStyle::Italic);
        }
        if style.strike {
            h.strikethrough = Some(StrikethroughStyle { thickness: px(1.0), color: None });
        }
        if style.code {
            h.background_color = Some(rgb(faint()).into());
            code_ranges.push((a..b, mono()));
        }
        let mut color = if link { theme::accent() } else { fg };
        if link {
            h.underline = Some(UnderlineStyle { thickness: px(1.0), color: None, wavy: false });
        }
        if is_caret {
            color = theme::muted();
        }
        if link || is_caret || fade.is_some() {
            h.color = Some(hsla(color, if is_caret { 1.0 } else { fade.unwrap_or(1.0) }));
        }
        if h != HighlightStyle::default() {
            highlights.push((a..b, h));
        }
    }
    let styled = StyledText::new(text).with_highlights(highlights).with_font_family_overrides(code_ranges);
    if inline.links.is_empty() {
        return styled.into_any_element();
    }
    let ranges: Vec<_> = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let urls: Vec<String> = inline.links.iter().map(|(_, u)| u.clone()).collect();
    InteractiveText::new(gpui::ElementId::Name(id.to_owned().into()), styled)
        .on_click(ranges, move |ix, _, cx| cx.open_url(&urls[ix]))
        .into_any_element()
}

fn render_block(id: &str, block: &Block, tail: Option<(&[(usize, f32)], bool)>) -> AnyElement {
    let (fades, caret) = tail.unwrap_or((&[], false));
    match block {
        Block::Para(inline) => div().pb(px(8.0)).line_height(px(21.0)).child(inline_text(id, inline, fades, caret)).into_any_element(),
        Block::Heading(level, inline) => div()
            .pt(px(8.0))
            .pb(px(6.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(match level {
                1 => 18.0,
                2 => 16.0,
                _ => 14.5,
            }))
            .child(inline_text(id, inline, fades, caret))
            .into_any_element(),
        Block::Code { text, .. } => div()
            .id(gpui::ElementId::Name(id.to_owned().into()))
            .mb(px(10.0))
            .px(px(12.0))
            .py(px(10.0))
            .continuous(8.0)
            .bg(rgb(faint()))
            .font_family(mono())
            .text_size(px(12.5))
            .line_height(px(18.0))
            .overflow_x_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .children(text.lines().map(|l| div().whitespace_nowrap().child(if l.is_empty() { " ".to_owned() } else { l.to_owned() }))),
            )
            .into_any_element(),
        Block::List { start, items } => {
            let last = items.len().saturating_sub(1);
            div()
                .pb(px(6.0))
                .flex()
                .flex_col()
                .children(items.iter().enumerate().map(|(i, blocks)| {
                    let marker = start.map_or_else(|| "•".to_owned(), |s| format!("{}.", s + i as u64));
                    let inner_last = blocks.len().saturating_sub(1);
                    div()
                        .flex()
                        .gap(px(6.0))
                        .child(div().flex_none().min_w(px(16.0)).text_color(rgb(theme::muted())).line_height(px(21.0)).child(marker))
                        .child(div().flex_1().min_w_0().flex().flex_col().children(blocks.iter().enumerate().map(|(j, b)| {
                            let t = (i == last && j == inner_last).then_some((fades, caret));
                            render_block(&format!("{id}-{i}-{j}"), b, t)
                        })))
                }))
                .into_any_element()
        }
        Block::Quote(blocks) => div()
            .mb(px(8.0))
            .pl(px(12.0))
            .border_l_2()
            .border_color(rgb(theme::border()))
            .text_color(rgb(theme::muted()))
            .children(
                blocks.iter().enumerate().map(|(i, b)| render_block(&format!("{id}-q{i}"), b, (i + 1 == blocks.len()).then_some((fades, caret)))),
            )
            .into_any_element(),
        Block::Rule => div().my(px(10.0)).h(px(1.0)).bg(rgb(theme::border())).into_any_element(),
    }
}
