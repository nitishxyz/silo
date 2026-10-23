//! Drawing the pane: title row, transcript (Trace-style lines, no
//! containers), composer, tray, approval card and popovers.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, App, BoxShadow, ClickEvent, Context, FontStyle, FontWeight, HighlightStyle, Hsla, InteractiveText, MouseButton, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle, Window, div, linear_color_stop, linear_gradient, point, prelude::*, px, relative, rgb,
};
use otto_proto::{ApprovalDecision, ApprovalMode, Delivery, Effort, SessionStatus};

use super::{
    AgentPane, AgentPaneEvent, Card, Conn, ENTER, FOLD, Overlay,
    editor::{Editor, EditorColors},
    markdown::{Block, Inline},
    pill_input, reveal, rows,
    transcript::{self, Detail, Group, Line, Row, Status, TodoStatus},
};
use crate::{
    assets::icon,
    motion,
    theme::{self, Continuous as _},
    tooltip::Tooltip,
};

/// Width of the reading column.
const COLUMN: f32 = 760.0;
const LINE_H: f32 = 24.0;
/// The fade above the composer column, where the transcript scrolls out under it.
const COMPOSER_FADE: f32 = 28.0;
/// Lines visible in a live group before older ones scroll away.
const LIVE_LINES: f32 = 6.0;
/// Streaming reasoning's window: four of its lines and the gap below.
const STREAM_H: f32 = 4.0 * 18.0 + 4.0;
const TEXT: f32 = 13.5;
/// Space above and below an open `›` detail: the box sits between its row and the next with equal room, so the
/// row after it keeps its rhythm instead of reading as attached to the box.
const DETAIL_GAP: f32 = 8.0;
/// Height of an open `›` detail (240 max plus its margins).
const DETAIL_H: f32 = 240.0 + 2.0 * DETAIL_GAP;
/// The composer's text line height.
const COMPOSER_LINE: f32 = 20.0;
/// Indent of a group's lines under its header.
const NEST: f32 = 12.0;
/// A bar's header and row heights (bars above the composer).
const BAR_HEADER: f32 = 34.0;
pub(super) const BAR_ROW: f32 = 28.0;
/// Width of a queue row's hover actions (`Send later` + `×`).
const QUEUE_ACTIONS: f32 = 112.0;

/// Streaming reasoning under a `Thinking` line: its last text (kept while it
/// collapses after the stream ends) and its height, eased to what it measured.
pub(super) struct Stream {
    text: String,
    live: bool,
    height: motion::Glide,
    measured: std::rc::Rc<std::cell::Cell<f32>>,
}

/// Tabular figures: ticking durations keep their width.
fn tabular() -> gpui::FontFeatures {
    gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]))
}

/// Within this many points of the end the transcript counts as at the bottom.
const AT_END: f32 = 3.0;

/// How far the list is scrolled up from its end, from its last layout (0 when it doesn't scroll).
pub(super) fn gap_to_end(list: &gpui::ListState) -> f32 {
    f32::from(list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y).max(0.0)
}

/// Tail-follow from the actual scroll position, not only the list's follow
/// flag (which the list turns back on only when it lays out, a frame late).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Tail {
    /// At the bottom (following, or within [`AT_END`] of the end).
    pub at_end: bool,
    /// Turn following back on (back at the bottom, by scrolling or the content settling).
    pub resume: bool,
    /// Something new arrived since the user left the bottom (kept).
    pub unseen: bool,
    /// Show `↓ New activity`.
    pub pill: bool,
}

impl Tail {
    pub(super) fn of(following: bool, gap: f32, unseen: bool) -> Self {
        let at_end = following || gap <= AT_END;
        let unseen = unseen && !at_end;
        Self { at_end, resume: at_end && !following, unseen, pill: unseen }
    }
}

/// What a pane this wide shows (a split can be as narrow as 60): secondary
/// items go first, the rest truncate, nothing overlaps.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct Fit {
    /// The subscription marker after the model (`(Max)`).
    pub plan: bool,
    /// The provider logo and `/` before the model id.
    pub provider: bool,
    /// The effort and image buttons.
    pub tools: bool,
    /// The model button.
    pub model: bool,
    /// The footer at all.
    pub footer: bool,
    /// The composer's `⋮` circle (the slash menu does the same).
    pub lead: bool,
    /// Bars' current item and sub-agent rows' agent names; the strip's parent title.
    pub detail: bool,
    /// Labels next to icons (`Main agent`, a bar's label).
    pub labels: bool,
    /// The send circle, the strip's `Sub-agent` and Stop's words (⏎ sends, a stop icon stays).
    pub send: bool,
    /// Side padding of the column.
    pub pad: f32,
}

impl Fit {
    /// `width`: the pane's, 0 before it was laid out (then everything shows).
    pub(super) fn of(width: f32) -> Self {
        let w = if width > 0.0 { width } else { f32::MAX };
        Self {
            plan: w >= 440.0,
            provider: w >= 340.0,
            tools: w >= 270.0,
            model: w >= 200.0,
            footer: w >= 130.0,
            lead: w >= 240.0,
            detail: w >= 320.0,
            labels: w >= 230.0,
            send: w >= 120.0,
            pad: if w >= 300.0 { 20.0 } else { 8.0 },
        }
    }
}

/// One bar above the composer (see `render_bars`).
pub(super) struct Bar {
    pub key: &'static str,
    pub icon: AnyElement,
    pub label: &'static str,
    /// Collapsed: the current item, after a divider.
    pub summary: Option<AnyElement>,
    pub note: String,
    pub open_label: String,
    pub open_note: String,
    pub rows: Vec<AnyElement>,
}

/// A todo with its status icon; finished ones struck through and muted.
fn todo_item(status: TodoStatus, step: String) -> gpui::Div {
    let muted = theme::muted();
    let finished = matches!(status, TodoStatus::Completed | TodoStatus::Cancelled);
    let glyph = match status {
        TodoStatus::Completed => icon("circle-check", 14.0, muted),
        TodoStatus::InProgress => icon("circle-dot", 14.0, theme::accent()),
        TodoStatus::Cancelled => icon("x-circle", 14.0, muted),
        TodoStatus::Pending => icon("circle", 14.0, muted),
    };
    div().flex_1().min_w_0().flex().items_center().gap(px(8.0)).child(glyph).child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_color(rgb(if finished { muted } else { theme::foreground() }))
            .when(finished, |el| el.line_through())
            .child(step),
    )
}

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

/// The foreground over the background at `amount`.
pub(super) fn ink(amount: f32) -> u32 {
    theme::mix(theme::background(), theme::foreground(), amount)
}

/// One step above the pane background (composer, tray, cards).
fn raised() -> u32 {
    theme::popover()
}

fn provider_logo(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("provider-anthropic"),
        "openai" => Some("provider-openai"),
        _ => None,
    }
}

/// A square icon button.
fn icon_button(id: impl Into<gpui::ElementId>, name: &'static str, size: f32, side: f32, color: u32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(side))
        .flex()
        .items_center()
        .justify_center()
        .continuous(7.0)
        .cursor_pointer()
        .hover(|s| s.bg(theme::hover_fill()))
        .child(icon(name, size, color))
}

/// A disclosure chevron: right when closed (0), down when open (1), turning in between.
fn chevron(open: f32, size: f32, color: u32) -> gpui::Svg {
    icon("chevron-right", size, color).with_transformation(gpui::Transformation::rotate(gpui::radians(open * std::f32::consts::FRAC_PI_2)))
}

/// A centered empty state: the agent mark, a title and an optional subtitle.
fn empty_state(title: impl Into<SharedString>, subtitle: Option<String>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .text_center()
        .child(icon("agent", 22.0, theme::muted()))
        .child(div().mt(px(10.0)).text_size(px(15.0)).font_weight(FontWeight::MEDIUM).child(title.into()))
        .children(subtitle.map(|s| div().mt(px(6.0)).text_size(px(12.5)).text_color(rgb(theme::muted())).child(s)))
}

/// A muted note under a popover's list.
fn footnote(text: &'static str) -> gpui::Div {
    div()
        .mt(px(4.0))
        .pt(px(6.0))
        .pb(px(2.0))
        .px(px(8.0))
        .border_t_1()
        .border_color(rgb(ink(0.1)))
        .text_size(px(11.5))
        .text_color(rgb(theme::muted()))
        .child(text)
}

/// The brain glyph while reasoning is on.
fn violet() -> u32 {
    if theme::is_light() { 0x007c_3aed } else { 0x00a7_8bfa }
}

/// A reasoning effort as the menu and tooltip name it; `None` is the default.
fn effort_label(effort: Option<Effort>) -> &'static str {
    match effort {
        None => "Default",
        Some(Effort::None) => "Off",
        Some(Effort::Minimal) => "Minimal",
        Some(Effort::Low) => "Low",
        Some(Effort::Medium) => "Medium",
        Some(Effort::High) => "High",
        Some(Effort::Xhigh) => "Extra high",
        Some(Effort::Max) => "Max",
    }
}

fn effort_about(effort: Option<Effort>) -> &'static str {
    match effort {
        None => "The agent's or the model's",
        Some(Effort::None | Effort::Minimal | Effort::Low) => "Quick answers",
        Some(Effort::Medium) => "Balanced",
        Some(Effort::High) => "Thinks things through",
        Some(Effort::Xhigh | Effort::Max) => "Most thorough, slowest",
    }
}

thread_local! {
    /// Set by [`eased`] while anything is mid-animation in this frame.
    static MOVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Eased (`motion::ease_ui`) progress of an animation, or `None` once done
/// (instant with Reduce Motion). Keeps frames coming while it runs.
pub(super) fn eased(started: Instant, duration: std::time::Duration, now: Instant, cx: &App) -> Option<f32> {
    let p = motion::enter(started, duration, now, cx.reduce_motion());
    if p.is_some() {
        MOVING.set(true);
    }
    p
}

/// A [`motion::Toggle`]'s value, keeping frames coming while it moves.
pub(super) fn toggled(toggle: &motion::Toggle, now: Instant, cx: &App) -> f32 {
    let reduce = cx.reduce_motion();
    if toggle.moving(now, reduce) {
        MOVING.set(true);
    }
    toggle.value(now, reduce)
}

/// Request the next frame if anything drawn so far was mid-animation.
pub(super) fn keep_moving(window: &mut Window) {
    if MOVING.take() {
        window.request_animation_frame();
    }
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

pub(super) fn provider_name(provider: &str) -> String {
    match provider {
        "anthropic" => "Anthropic".into(),
        "openai" => "OpenAI".into(),
        "google" => "Google".into(),
        "xai" => "xAI".into(),
        "openrouter" => "OpenRouter".into(),
        "deepseek" => "DeepSeek".into(),
        "moonshotai" => "Kimi".into(),
        "zai" => "Z.ai".into(),
        "minimax" => "MiniMax".into(),
        "lmstudio" => "LM Studio".into(),
        "custom" => "Custom".into(),
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
    /// What this pane's width leaves room for.
    pub(super) fn fit(&self) -> Fit {
        Fit::of(self.width.get())
    }

    /// Where the transcript is relative to its end (as last laid out).
    pub(super) fn tail(&self) -> Tail {
        Tail::of(self.list.is_following_tail(), gap_to_end(&self.list), self.unseen)
    }

    /// Start an expand/collapse motion `key` (inside transcript row `row`) toward `open`.
    fn flip(&mut self, key: String, row: String, open: bool, cx: &App) {
        let now = Instant::now();
        let reduce = cx.reduce_motion();
        let entry = self.anims.entry(key).or_insert_with(|| (row, motion::Toggle::new(!open, motion::UI)));
        // Following is left alone: at the bottom the list keeps following the tail as the row grows or shrinks;
        // scrolled up, it stays where the user put it (the remeasure keeps its anchor).
        entry.1.set(open, now, reduce);
    }

    /// 0 (closed) … 1 (open) of motion `key`; `open` once it settled.
    fn openness(&self, key: &str, open: bool, now: Instant, cx: &App) -> f32 {
        self.anims.get(key).map_or(if open { 1.0 } else { 0.0 }, |(_, t)| toggled(t, now, cx))
    }

    /// Something moves this frame (shimmer, entries, folds, fades).
    fn animating(&self, now: Instant, cx: &App) -> bool {
        let reduce = cx.reduce_motion();
        self.running
            || self.awaiting()
            || self.editor.read(cx).growing(now, reduce)
            || self.rows.iter().any(|r| matches!(r, Row::Group(g) if g.live))
            || self.entered.values().any(|at| motion::enter(*at, ENTER, now, reduce).is_some())
            || self.fold_at.values().any(|at| now < *at || motion::enter(*at, FOLD, now, reduce).is_some())
    }

    pub(super) fn render_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let now = Instant::now();
        self.settle_focus(window, cx);
        let header = Some(self.header_title());
        if self.header_seen != header {
            self.header_seen = header;
            cx.emit(AgentPaneEvent::Header);
        }
        if self.animating(now, cx) && !cx.reduce_motion() {
            window.request_animation_frame();
        }
        // Folding groups change height every frame; a finished fold is measured once more. Live groups too:
        // their window opens as lines arrive and streaming reasoning glides; and streaming text, whose height glides.
        let mut settled = Vec::new();
        for (ix, row) in self.rows.iter().enumerate() {
            if matches!(row, Row::Group(g) if g.live) || matches!(row, Row::Text { key, .. } if self.reveals.contains_key(key)) {
                self.list.remeasure_items(ix..ix + 1);
            }
            if let Row::Group(g) = row
                && let Some(at) = self.fold_at.get(&g.key)
                && now >= *at
            {
                self.list.remeasure_items(ix..ix + 1);
                if motion::enter(*at, FOLD, now, cx.reduce_motion()).is_none() {
                    settled.push(g.key.clone());
                }
            }
        }
        for key in settled {
            self.fold_at.remove(&key);
        }
        // Rows opening or closing change height every frame; a finished one once more.
        let reduce = cx.reduce_motion();
        let mut done = Vec::new();
        for (key, (row, toggle)) in &self.anims {
            if let Some(ix) = self.rows.iter().position(|r| r.key() == row) {
                self.list.remeasure_items(ix..ix + 1);
            }
            if !toggle.moving(now, reduce) {
                done.push(key.clone());
            }
        }
        for key in done {
            self.anims.remove(&key);
        }
        // The activity row eases its height; its gap follows the last row.
        self.activity.set(self.activity_shown(), now, reduce);
        if self.running || self.awaiting() || self.activity.moving(now, reduce) {
            let end = self.rows.len();
            self.list.remeasure_items(end..end + 1);
        }
        // Back at the bottom (scrolled there, or the content settled): follow again, nothing unseen.
        // Not while a box inside takes a gesture: the transcript stays where it is under the pointer.
        let tail = self.tail();
        if self.scroll.holding(now) {
            self.list.pause_following_tail();
        } else if tail.resume {
            self.list.set_follow_mode(gpui::FollowMode::Tail);
        }
        self.unseen = tail.unseen;
        // The composer arrives (first connection, back from Providers) with a rise.
        let visible = self.composer_visible();
        self.bottom_shown.set(visible, now, cx.reduce_motion());
        let rise = toggled(&self.bottom_shown, now, cx);
        // The composer column floats over the transcript; the list's end is padded clear of it (its last item),
        // following on through a change at the bottom (the input grows, a bar comes or goes).
        if !visible {
            self.composer_h.set(0.0);
        }
        let pad = self.composer_h.get();
        if (pad - self.composer_pad).abs() > 0.5 {
            self.composer_pad = pad;
            let end = self.rows.len();
            self.list.remeasure_items(end..end + 1);
            if tail.at_end {
                self.list.set_follow_mode(gpui::FollowMode::Tail);
            }
        }
        self.scroll.set_inset(pad);
        let bg = theme::background();
        let width = self.width.clone();
        // The pane's width, for what fits (a change lays out again).
        let measure = gpui::canvas(
            move |b, window, _| {
                let w = f32::from(b.size.width);
                if (width.get() - w).abs() > 0.5 {
                    width.set(w);
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let pane = div()
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
            .child(measure)
            .on_key_down(cx.listener(|this, event, window, cx| this.key_down(event, window, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.focus.is_focused(window) || !this.focus.contains_focused(window, cx) {
                        this.focus_composer(window, cx);
                    }
                }),
            )
            .children(visible.then(|| self.render_subagent_strip(cx)).flatten())
            .child(self.render_body(window, cx))
            .when(visible, |el| {
                let bottom = self.render_bottom(window, cx);
                let height = self.composer_h.clone();
                let measure = gpui::canvas(
                    move |b, window, _| {
                        let h = f32::from(b.size.height);
                        if (height.get() - h).abs() > 0.5 {
                            height.set(h);
                            window.refresh();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full();
                // What scrolls under it fades out above the column (as a live group's top does); only the
                // column takes the pointer.
                let fade = div().h(px(COMPOSER_FADE)).bg(linear_gradient(
                    180.0,
                    linear_color_stop(hsla(bg, 0.0), 0.0),
                    linear_color_stop(hsla(bg, 1.0), 1.0),
                ));
                let column = div()
                    .id("agent-composer-column")
                    .occlude()
                    .relative()
                    .bg(rgb(bg))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if !this.focus.contains_focused(window, cx) {
                                this.focus_composer(window, cx);
                            }
                        }),
                    )
                    .child(bottom)
                    .child(measure);
                el.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .child(div().relative().top(px((1.0 - rise) * 8.0)).opacity(rise).flex().flex_col().child(fade).child(column)),
                )
            })
            .children(self.render_backdrop(cx))
            .children(self.popover(Overlay::Sessions, window, cx))
            .children(self.popover(Overlay::Projects, window, cx))
            .children(self.render_signin_dialog(window, cx))
            .into_any_element();
        keep_moving(window);
        pane
    }

    /// The area above the composer; switching what it shows (connecting,
    /// sign-in, empty, transcript…) fades the new content in with a 6 pt rise.
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let now = Instant::now();
        let (mode, content) = self.body_content(window, cx);
        if self.body_mode.0 != mode {
            self.body_mode = (mode, Some(now));
        }
        let t = self.body_mode.1.and_then(|at| eased(at, motion::UI, now, cx)).unwrap_or(1.0);
        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .child(div().size_full().relative().top(px((1.0 - t) * 6.0)).opacity(t).child(content))
            .child(self.tail_check())
            .child(self.scroll.router())
            .children(self.render_new_activity(now, cx))
            .into_any_element()
    }

    /// After the list laid out this frame: a pill that's showing at the bottom, or following still off there,
    /// draws once more (the list only learns it is back at the end when it lays out, after this render).
    fn tail_check(&self) -> AnyElement {
        let (list, pill) = (self.list.clone(), self.unseen);
        gpui::canvas(
            move |_, window, _| {
                let tail = Tail::of(list.is_following_tail(), gap_to_end(&list), pill);
                if tail.resume || (pill && !tail.pill) {
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_0()
        .into_any_element()
    }

    fn body_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> (u8, AnyElement) {
        // Centred above the composer column (it floats over the body).
        let pad = self.composer_h.get();
        let centered = |child: AnyElement| {
            div().size_full().flex().flex_col().items_center().justify_center().px(px(24.0)).pb(px(pad)).child(child).into_any_element()
        };
        match &self.conn {
            Conn::Connecting => return (1, centered(empty_state("Connecting to otto…", None).into_any_element())),
            Conn::Failed(error) => {
                let retry = chip("agent-retry", "Try again").mt(px(12.0)).text_color(rgb(theme::accent())).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.conn = Conn::Connecting;
                        this.load(cx);
                        cx.notify();
                    },
                ));
                return (2, centered(empty_state("Can't reach otto", Some(error.clone())).child(retry).into_any_element()));
            }
            Conn::Ready => {}
        }
        if self.showing_signin() {
            return (if self.managing_providers() { 3 } else { 4 }, centered(self.render_signin(window, cx)));
        }
        if let Some(error) = &self.error {
            // Sending from here starts a new session; the old one can't be reached.
            let reason =
                if error.contains("not found") { "It may have been deleted. Ask anything to start a new one.".to_owned() } else { error.clone() };
            let new = chip("agent-error-new", "New session")
                .mt(px(12.0))
                .bg(theme::control_fill())
                .active(|s| s.scale(0.96))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.start_new_session(window, cx)));
            return (5, centered(empty_state("Couldn't open this session", Some(reason)).child(new).into_any_element()));
        }
        if self.rows.is_empty() {
            if self.session_id.is_some() && self.state.is_none() {
                return (6, centered(empty_state("Loading…", None).into_any_element()));
            }
            let place = self.project.as_ref().and_then(|p| std::path::Path::new(&p.root).file_name().map(|n| n.to_string_lossy().into_owned()));
            return (7, centered(empty_state("What should we work on?", place).into_any_element()));
        }
        (8, gpui::list(self.list.clone(), cx.processor(|this, ix: usize, window, cx| this.render_row(ix, window, cx))).size_full().into_any_element())
    }

    /// `↓ New activity` while scrolled up and something arrived below; it rises in and sinks away.
    fn render_new_activity(&mut self, now: Instant, cx: &mut Context<Self>) -> Option<AnyElement> {
        let reduce = cx.reduce_motion();
        let show = self.tail().pill && !self.rows.is_empty();
        self.unseen_anim.set(show, now, reduce);
        if self.unseen_anim.hidden(now, reduce) {
            return None;
        }
        let t = toggled(&self.unseen_anim, now, cx);
        Some(
            div()
                .absolute()
                .bottom(px(self.composer_h.get() + 8.0 + (1.0 - t) * -8.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .opacity(t)
                .child(
                    div()
                        .id("agent-new-activity")
                        .occlude()
                        .pl(px(8.0))
                        .pr(px(10.0))
                        .h(px(26.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .capsule()
                        .bg(theme::overlay_fill())
                        .border_1()
                        .border_color(rgb(ink(0.12)))
                        .shadow_sm()
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::hover_fill()))
                        .active(|s| s.scale(0.96))
                        .child(icon("chevron-down", 12.0, theme::muted()))
                        .child("New activity")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.list.scroll_to_end();
                            this.list.set_follow_mode(gpui::FollowMode::Tail);
                            this.unseen = false;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// `· 1 queued` after the activity label.
    fn queued_note(&self) -> Option<gpui::Div> {
        let queued = self.queue_rows().len();
        (queued > 0).then(|| div().flex_none().pl(px(8.0)).text_color(rgb(theme::muted())).child(format!("· {queued} queued")))
    }

    /// The quiet `Stop  esc` at the right end of the activity indicator; its
    /// text ends on the column's edge, the hover fill reaches past it. The
    /// narrowest panes get a stop icon.
    fn stop_button(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let words = self.fit().send;
        let button = div()
            .id("agent-stop")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(22.0))
            .px(px(6.0))
            .mr(px(-6.0))
            .continuous(6.0)
            .text_size(px(12.0))
            .text_color(rgb(theme::muted()))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()).text_color(rgb(theme::foreground())))
            .active(|s| s.scale(0.96))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop(cx)));
        if words {
            button.child("Stop").child(div().font_family(mono()).text_size(px(11.0)).opacity(0.6).child("esc"))
        } else {
            button.child(icon("stop", 11.0, theme::muted())).tooltip(Tooltip::text("Stop  esc"))
        }
    }

    /// Whether the activity row shows: a turn runs and no live group ends the transcript.
    fn activity_shown(&self) -> bool {
        (self.running || self.awaiting()) && !transcript::indicator_is_group(&self.rows)
    }

    /// The list's last item: `Thinking…` / `Working for 12s` with Stop while a
    /// turn runs (fades and slides in, eases its height), else bottom room.
    fn render_activity(&mut self, now: Instant, cx: &mut Context<Self>) -> AnyElement {
        let t = toggled(&self.activity, now, cx);
        // Room at the end, clear of the composer column floating over it and its fade.
        let pad = self.composer_h.get();
        let room = div().h(px(if pad > 0.0 { pad + COMPOSER_FADE } else { 12.0 }));
        if t <= 0.0 {
            return room.into_any_element();
        }
        let gap = match self.rows.last() {
            Some(Row::User { .. }) => 20.0,
            // The gaps a live group would have in its place, so the handoff doesn't shift.
            Some(Row::Group(_) | Row::Text { .. }) | None => 4.0,
        };
        let label = match (self.rows.last(), self.running_since) {
            (Some(Row::Text { .. } | Row::Group(_)), Some(since)) => format!("Working for {}", transcript::dur(self.now().saturating_sub(since))),
            _ => "Thinking…".to_owned(),
        };
        let line = div()
            .h(px(LINE_H))
            .flex()
            .items_center()
            .text_size(px(13.0))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_features(tabular())
                    .child(shimmer(&label, now, self.epoch, cx)),
            )
            .children(self.queued_note())
            .child(div().flex_1())
            .child(self.stop_button(cx));
        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .px(px(self.fit().pad))
            .child(
                div()
                    .w_full()
                    .max_w(px(COLUMN))
                    .h(px((gap + LINE_H) * t))
                    .overflow_hidden()
                    .child(div().pt(px(gap)).relative().top(px((1.0 - t) * 6.0)).opacity(t).child(line)),
            )
            .child(room)
            .into_any_element()
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if ix == self.rows.len() {
            let row = self.render_activity(Instant::now(), cx);
            keep_moving(window);
            return row;
        }
        let Some(row) = self.rows.get(ix).cloned() else { return div().into_any_element() };
        let now = Instant::now();
        // Gap above the row; a paragraph already ends with 8 of its own.
        let top = match (ix.checked_sub(1).and_then(|i| self.rows.get(i)), &row) {
            (None, _) => 16.0,
            (Some(Row::User { .. }), _) => 20.0,
            (Some(Row::Text { .. }), Row::User { .. }) => 20.0,
            (Some(Row::Group(_)), Row::User { .. }) => 28.0,
            (Some(Row::Text { .. }), _) => 4.0,
            (Some(Row::Group(_)), Row::Text { .. }) => 12.0,
            (Some(Row::Group(_)), Row::Group(_)) => 4.0,
        };
        let content = match &row {
            Row::User { text, files, .. } => self.render_user(text, files, cx),
            Row::Group(g) => self.render_group(g, now, window, cx),
            Row::Text { key, text, streaming } => self.render_text(ix, key, text, *streaming, now, cx),
        };
        let enter = self.entered.get(row.key()).and_then(|at| eased(*at, ENTER, now, cx));
        let content = match enter {
            Some(t) => div().relative().top(px((1.0 - t) * 6.0)).opacity(t).child(content).into_any_element(),
            None => content,
        };
        let row =
            div().w_full().flex().justify_center().px(px(20.0)).pt(px(top)).child(div().w_full().max_w(px(COLUMN)).child(content)).into_any_element();
        keep_moving(window);
        row
    }

    fn group_started(&self, g: &Group) -> Option<u64> {
        g.lines.iter().filter_map(|l| self.times.get(&l.key).and_then(|s| s.start)).min()
    }

    fn render_group(&mut self, g: &Group, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let key = g.key.clone();
        // A command waiting for the user stays in sight.
        let fold = if g.live || g.lines.iter().any(|l| l.status == Status::Input) {
            0.0
        } else {
            match self.fold_at.get(&key) {
                Some(at) if now < *at => 0.0,
                Some(at) => eased(*at, FOLD, now, cx).unwrap_or(1.0),
                None => 1.0,
            }
        };
        let open = self.toggled.contains(&key);
        let openness = self.openness(&format!("grp:{key}"), open, now, cx);
        let muted = theme::muted();
        // Live: the turn's clock, as the activity row it replaces showed; folded: the group's own wall time.
        let header_text = if g.live {
            match self.running_since.or_else(|| self.group_started(g)) {
                Some(start) => format!("Working for {}", transcript::dur(self.now().saturating_sub(start))),
                None => "Working".to_owned(),
            }
        } else {
            g.summary.clone()
        };
        let toggle_key = key.clone();
        // The live group at the end is the activity indicator: Stop on its right.
        let indicator = g.live && matches!(self.rows.last(), Some(Row::Group(last)) if last.key == key);
        // Trace-style: the text on the column's edge, the chevron after it.
        let trigger =
            div()
                .id(gpui::ElementId::Name(format!("hdr-{key}").into()))
                .min_w_0()
                .h(px(LINE_H))
                .flex()
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let open = !this.toggled.remove(&toggle_key);
                    if open {
                        this.toggled.insert(toggle_key.clone());
                    }
                    this.flip(format!("grp:{toggle_key}"), toggle_key.clone(), open, cx);
                    cx.notify();
                }))
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().font_features(tabular()).map(|el| {
                    if g.live { el.child(shimmer(&header_text, now, self.epoch, cx)) } else { el.text_color(rgb(muted)).child(header_text) }
                }))
                .child(div().flex_none().flex().map(|el| {
                    if g.live {
                        el.child(crate::browser_pane::spinner(&key, 12.0, muted, cx))
                    } else {
                        el.child(chevron(if fold >= 1.0 { openness } else { 0.0 }, 12.0, if g.failed { theme::danger() } else { muted }))
                    }
                }));
        let header = div()
            .h(px(LINE_H))
            .flex()
            .items_center()
            .child(trigger)
            .when(indicator, |el| el.children(self.queued_note()).child(div().flex_1()).child(self.stop_button(cx)));
        let mut column = div().text_size(px(13.0)).flex().flex_col().child(header);
        if fold >= 1.0 {
            if openness > 0.0 {
                let lines: Vec<AnyElement> = g.lines.iter().map(|l| self.render_line(&key, l, false, 1.0, now, window, cx)).collect();
                // Grows to its lines (and any open details) while it opens.
                #[allow(clippy::cast_precision_loss)]
                let full = g.lines.len() as f32 * LINE_H + g.lines.iter().filter(|l| self.details.contains(&l.key)).count() as f32 * DETAIL_H;
                column = column.child(
                    div()
                        .pl(px(NEST))
                        .flex()
                        .flex_col()
                        .when(openness < 1.0, |el| el.max_h(px(full * openness)).overflow_hidden().opacity(openness))
                        .children(lines),
                );
            }
            return column.into_any_element();
        }
        // Live, or folding: a window on the newest lines under a top fade. It scrolls (an inner box,
        // `scroll.rs`): pinned to its newest line until the user scrolls up in it, then it stays put and a
        // bottom fade says more is below, until they scroll back down.
        let shown = &g.lines[..];
        let n = shown.len();
        // How much of each line has arrived (0 → 1 while it enters, from its own `entered`).
        let arrived: Vec<f32> = shown
            .iter()
            .map(|l| if g.live { self.entered.get(&l.key).and_then(|at| eased(*at, ENTER, now, cx)).unwrap_or(1.0) } else { 1.0 })
            .collect();
        // An entering line opens to its height as it arrives: the window grows by that much, so the header and
        // older lines stay put while it opens (once full, they slide up together). Arrivals in a row add up.
        let lines: Vec<AnyElement> = shown
            .iter()
            .enumerate()
            .map(|(i, l)| {
                // The two newest lines are bright; one pushed past them dims as the newcomer arrives.
                let below: f32 = arrived[i + 1..].iter().sum();
                let dim = 1.0 - 0.45 * (below - 1.0).clamp(0.0, 1.0);
                let line = self.render_line(&key, l, g.live && i + 1 == n, dim, now, window, cx);
                div().flex_none().when(arrived[i] < 1.0, |el| el.max_h(px(LINE_H * arrived[i])).overflow_hidden()).child(line).into_any_element()
            })
            .collect();
        // The top fade comes in with the line that fills the window.
        let fade = (arrived.iter().sum::<f32>() - (LIVE_LINES - 1.0)).clamp(0.0, 1.0);
        // An open detail gets its own room in the window (its box scrolls on its own).
        #[allow(clippy::cast_precision_loss)]
        let max = LINE_H * LIVE_LINES + shown.iter().filter(|l| self.details.contains(&l.key)).count() as f32 * DETAIL_H;
        let id = format!("live-{key}");
        let handle = self.scroll.handle(&id);
        let pin = self.live_pins.entry(key.clone()).or_default();
        if let Some(y) = pin.follow(f32::from(handle.offset().y), f32::from(handle.max_offset().y)) {
            handle.set_offset(gpui::point(handle.offset().x, px(y)));
        }
        let below = !pin.pinned;
        // Thinking folding away is gone in the fold's first third (its line repeats the header) while the
        // space closes over the whole fold.
        let alpha = if !g.live && transcript::thought(g) { (1.0 - 3.0 * fold).max(0.0) } else { 1.0 - fold };
        let scroller =
            self.scroll.scroller(&id, div().id(gpui::ElementId::Name(id.clone().into())).max_h(px(max * (1.0 - fold))).flex().flex_col(), false);
        let bg = theme::background();
        let edge = |top: bool| {
            let (a, b) = if top { (1.0, 0.0) } else { (0.0, 1.0) };
            div().absolute().left_0().right_0().h(px(28.0)).map(|el| if top { el.top_0() } else { el.bottom_0() }).bg(linear_gradient(
                180.0,
                linear_color_stop(hsla(bg, a), 0.0),
                linear_color_stop(hsla(bg, b), 1.0),
            ))
        };
        let window_el = div()
            .relative()
            .pl(px(NEST))
            .opacity(alpha)
            .child(scroller.child(div().flex_none().flex().flex_col().children(lines)))
            .when(f32::from(window.line_height()) > 0.0 && fade > 0.0, |el| el.child(edge(true).opacity(fade)))
            .when(below, |el| el.child(edge(false)));
        column.child(window_el).into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_line(
        &mut self,
        group: &str,
        line: &Line,
        newest: bool,
        dim: f32,
        now: Instant,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = theme::muted();
        let key = line.key.clone();
        let open = self.details.contains(&key);
        let openness = self.openness(&format!("det:{key}"), open, now, cx);
        // A status change crossfades the icons: the new one swaps in (scale 0.8 → 1), the old one fades out.
        let (since, before) = match self.line_status.get(&key) {
            Some((status, at, before)) if *status == line.status => (*at, *before),
            Some((status, ..)) => (now, Some(*status)),
            None => (now.checked_sub(std::time::Duration::from_secs(60)).unwrap_or(now), None),
        };
        self.line_status.insert(key.clone(), (line.status, since, before));
        let swap = eased(since, motion::UI_FAST, now, cx);
        let glyph_of = |status: Status| -> AnyElement {
            match status {
                Status::Running => crate::browser_pane::spinner(&key, 13.0, muted, cx),
                Status::Error => icon("x-circle", 14.0, theme::danger()).into_any_element(),
                Status::Pending => div().opacity(0.5).child(icon(line.kind.icon(), 14.0, muted)).into_any_element(),
                Status::Waiting(question) => icon(if question { "question" } else { "lock" }, 14.0, theme::foreground()).into_any_element(),
                Status::Input => icon("terminal", 14.0, theme::warning()).into_any_element(),
                Status::Stopped => icon("stop", 12.0, muted).into_any_element(),
                Status::Done => icon(line.kind.icon(), 14.0, muted).into_any_element(),
            }
        };
        let old = swap.zip(before).map(|(t, status)| {
            div().absolute().top_0().left_0().right_0().bottom_0().flex().items_center().justify_center().opacity(1.0 - t).child(glyph_of(status))
        });
        let t = swap.unwrap_or(1.0);
        let glyph = div().flex().opacity(t).scale(0.8 + 0.2 * t).child(glyph_of(line.status));
        let title = div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().map(|el| match line.status {
            Status::Running if newest => el.child(shimmer(&line.title, now, self.epoch, cx)),
            Status::Error => el.text_color(rgb(theme::danger())).child(line.title.clone()),
            Status::Input => el.text_color(rgb(theme::warning())).child(line.title.clone()),
            Status::Pending => el.text_color(rgb(muted)).child(line.title.clone()),
            _ => el.child(line.title.clone()),
        });
        let detail_key = key.clone();
        let group = group.to_owned();
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
                let open = !this.details.remove(&detail_key);
                if open {
                    this.details.insert(detail_key.clone());
                }
                this.flip(format!("det:{detail_key}"), group.clone(), open, cx);
                cx.notify();
            }))
            .child(div().relative().w(px(16.0)).flex_none().flex().justify_center().children(old).child(glyph))
            .child(title)
            .child(div().flex_1())
            .children(line.desc.clone().map(|d| if line.clock { self.times.elapsed(&key, self.now()).map_or(d, transcript::short) } else { d }).map(
                |d| {
                    div()
                        .flex_none()
                        .max_w(relative(0.5))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(12.0))
                        .font_features(tabular())
                        .text_color(rgb(muted))
                        .child(d)
                },
            ))
            .when(has_detail, |el| el.child(chevron(openness, 12.0, muted)))
            .children(reveal.map(|block| {
                icon_button(gpui::ElementId::Name(format!("reveal-{key}").into()), "arrow-up-right", 12.0, 20.0, muted)
                    .tooltip(Tooltip::text("Open"))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        cx.emit(AgentPaneEvent::Reveal(block.clone()));
                    }))
            }))
            .children(line.stop.clone().map(|block| {
                icon_button(gpui::ElementId::Name(format!("stop-{key}").into()), "stop", 12.0, 20.0, muted).tooltip(Tooltip::text("Stop")).on_click(
                    cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        cx.emit(AgentPaneEvent::Stop(block.clone()));
                    }),
                )
            }))
            .children(line.session.clone().map(|session| {
                icon_button(gpui::ElementId::Name(format!("open-{key}").into()), "arrow-up-right", 12.0, 20.0, muted)
                    .tooltip(Tooltip::text("Open sub-agent"))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.open_session_block(session.clone(), cx);
                    }))
            }));
        // Streaming reasoning shows its newest lines at the bottom of a window up to four lines tall: what's
        // older moves up out of a top fade, never dropping a whole paragraph at once. Its height eases to what it
        // measured as it grows, and folds away once it ends.
        let tail = line.stream.as_ref().filter(|s| !s.trim().is_empty()).map(|s| {
            let tail: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
            tail[tail.len().saturating_sub(12)..].join("\n")
        });
        if let Some(text) = tail {
            let s = self.streams.entry(key.clone()).or_insert_with(|| Stream {
                text: String::new(),
                live: true,
                height: motion::Glide::new(motion::UI),
                measured: std::rc::Rc::default(),
            });
            (s.text, s.live) = (text, true);
        } else if let Some(s) = self.streams.get_mut(&key) {
            s.live = false;
        }
        let reduce = cx.reduce_motion();
        let stream = self.streams.get_mut(&key).map(|s| {
            let natural = s.measured.get().min(STREAM_H);
            s.height.target(if s.live { natural } else { 0.0 }, now, reduce);
            let h = s.height.value(now, reduce);
            let moving = s.height.moving(now, reduce);
            if moving {
                MOVING.set(true);
            }
            let fade = if s.live { 1.0 } else { (h / natural.max(1.0)).clamp(0.0, 1.0) };
            let measured = s.measured.clone();
            let body = div()
                .flex_none()
                .relative()
                .pl(px(24.0))
                .pb(px(4.0))
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(rgb(muted))
                .child(s.text.clone())
                .child(gpui::canvas(move |b, _, _| measured.set(f32::from(b.size.height)), |_, _, _, _| {}).absolute().top_0().left_0().size_full());
            let bg = theme::background();
            let over = s.measured.get() > STREAM_H;
            let el = div().relative().h(px(h)).overflow_hidden().flex().flex_col().justify_end().opacity(0.8 * fade).child(body).when(over, |el| {
                el.child(div().absolute().top_0().left_0().right_0().h(px(18.0)).bg(linear_gradient(
                    180.0,
                    linear_color_stop(hsla(bg, 1.0), 0.0),
                    linear_color_stop(hsla(bg, 0.0), 1.0),
                )))
            });
            (el, !s.live && !moving)
        });
        let stream = match stream {
            Some((_, true)) => {
                self.streams.remove(&key);
                None
            }
            other => other.map(|(el, _)| el),
        };
        let detail = (openness > 0.0)
            .then(|| line.detail.as_ref().map(|d| render_detail(&key, d, &self.scroll)))
            .flatten()
            // The gap is the wrapper's padding: a margin on the box collapsed out of the (block) line, so the next row
            // overlapped the box once it was fully open.
            .map(|d| {
                div().py(px(DETAIL_GAP)).when(openness < 1.0, |el| el.max_h(px(DETAIL_H * openness)).overflow_hidden().opacity(openness)).child(d)
            });
        let enter = self.entered.get(&key).and_then(|at| eased(*at, ENTER, now, cx));
        let (shift, alpha) = enter.map_or((0.0, 1.0), |t| ((1.0 - t) * 6.0, t));
        div().relative().top(px(shift)).opacity(alpha * dim).child(row).children(stream).children(detail).into_any_element()
    }

    fn render_text(&mut self, ix: usize, key: &str, text: &str, streaming: bool, now: Instant, cx: &mut Context<Self>) -> AnyElement {
        // Parsed once per frame at most; completed blocks are kept as they are.
        let mut doc = self.parsed.remove(key).unwrap_or_default();
        doc.update(text);
        let blocks = doc.blocks();
        let reduce = cx.reduce_motion();
        // Streaming: lines show once complete and sweep in (`reveal`); blocks fully shown draw as usual.
        let mut reveal = self.reveals.remove(key);
        let shown = reveal.as_mut().map(|r| r.frame(key, blocks, text, streaming, self.width.get(), now, reduce));
        let fx = reveal.as_ref().zip(shown.as_ref()).map(|(r, s)| Fx { lines: &s.lines, wraps: &r.wraps });
        let settled = shown.as_ref().map_or(blocks.len(), |s| s.settled);
        let els: Vec<AnyElement> = blocks
            .iter()
            .enumerate()
            .map(|(i, block)| render_block(&format!("{key}-{i}"), block, fx.as_ref().filter(|_| i >= settled), theme::foreground(), &self.scroll))
            .collect();
        self.parsed.insert(key.to_owned(), doc);
        let answer = !streaming && matches!(self.rows.get(ix + 1), None | Some(Row::User { .. }));
        let actions = answer.then(|| {
            let copy = text.to_owned();
            let question = self.rows[..ix].iter().rev().find_map(|r| if let Row::User { text, .. } = r { Some(text.clone()) } else { None });
            let group = SharedString::from(format!("answer-{key}"));
            div()
                .flex()
                .gap(px(2.0))
                .h(px(24.0))
                .ml(px(-5.0))
                .opacity(0.0)
                .group_hover(group, |s| s.opacity(1.0))
                .child(
                    icon_button(gpui::ElementId::Name(format!("copy-{key}").into()), "copy", 13.0, 24.0, theme::muted())
                        .tooltip(Tooltip::text("Copy"))
                        .on_click(move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))),
                )
                .children(question.map(|q| {
                    icon_button(gpui::ElementId::Name(format!("retry-{key}").into()), "rotate-ccw", 13.0, 24.0, theme::muted())
                        .tooltip(Tooltip::text("Retry"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.retry(q.clone(), cx)))
                }))
        });
        let body = div().group(SharedString::from(format!("answer-{key}"))).flex().flex_col().children(els).children(actions);
        let (Some(mut r), Some(s)) = (reveal, shown) else { return body.into_any_element() };
        // The height glides to what was measured, less the line being written (the list follows it at the
        // bottom, so the transcript scrolls smoothly).
        let natural = r.measured.get();
        if r.quiet(now) {
            r.height = motion::Glide::new(motion::UI);
        }
        let target = (natural - s.held).max(0.0);
        r.height.target(target, now, reduce);
        let h = r.height.value(now, reduce);
        r.drawn = h;
        let moving = r.height.moving(now, reduce);
        r.idle = if s.done && !moving { r.idle + 1 } else { 0 };
        if moving || s.moving || r.idle > 0 {
            MOVING.set(true);
        }
        let measured = r.measured.clone();
        // Done (twice, so the last measure is in): drawn as usual from the next frame, at the same height.
        if r.idle < 2 {
            self.reveals.insert(key.to_owned(), r);
        }
        // The list gives it `h`; it draws up to `SLACK` past that (a line fades in once its slot is nearly open,
        // so nothing is uncovered by a moving edge), and the transparent line being written stays clipped.
        div()
            .h(px(h))
            .relative()
            .child(div().absolute().top_0().left_0().right_0().h(px(h + reveal::SLACK)).overflow_hidden().child(
                div().relative().child(body).child(
                    gpui::canvas(move |b, _, _| measured.set(f32::from(b.size.height)), |_, _, _, _| {}).absolute().top_0().left_0().size_full(),
                ),
            ))
            .into_any_element()
    }

    // ---- bottom: notice, card, tray, composer, footer ----

    fn render_bottom(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let notice = self
            .notice
            .clone()
            .map(|n| div().pb(px(8.0)).px(px(pill_input::HALO + pill_input::TEXT_X)).text_size(px(12.0)).text_color(rgb(theme::danger())).child(n));
        let popovers: Vec<AnyElement> = [Overlay::Agents, Overlay::Models, Overlay::Effort, Overlay::Menu, Overlay::Slash]
            .into_iter()
            .filter_map(|o| self.popover(o, window, cx))
            .collect();
        let fit = self.fit();
        div()
            .flex_none()
            .w_full()
            .flex()
            .justify_center()
            .px(px(fit.pad))
            .pt(px(4.0))
            .pb(px(12.0))
            .child(
                div()
                    .relative()
                    .w_full()
                    .max_w(px(COLUMN))
                    .flex()
                    .flex_col()
                    .children(notice)
                    .children(self.render_card(cx))
                    .children(self.render_bars(cx))
                    .child(self.render_composer(window, cx))
                    .children(fit.footer.then(|| self.render_footer(fit, cx)))
                    .children(popovers),
            )
            .into_any_element()
    }

    /// The approval or question card above the composer: rises in (height,
    /// opacity, 8 pt slide) and sinks away once answered.
    fn render_card(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let now = Instant::now();
        let reduce = cx.reduce_motion();
        let live = self.mirror().and_then(|m| {
            m.pending_questions
                .first()
                .cloned()
                .map(Card::Question)
                .or_else(|| m.pending_approvals.first().cloned().map(|a| Card::Approval(a, m.pending_approvals.len() - 1)))
        });
        match &live {
            Some(c) => {
                if self.card.as_ref().map(Card::id) != Some(c.id()) || !self.card_anim.on() {
                    self.card_anim = motion::Toggle::new(false, motion::UI);
                    self.card_anim.set(true, now, reduce);
                }
                self.card = live.clone();
            }
            None => self.card_anim.set(false, now, reduce),
        }
        if self.card_anim.hidden(now, reduce) {
            self.card = None;
            return None;
        }
        let t = toggled(&self.card_anim, now, cx);
        let active = live.is_some();
        let card =
            div().mb(px(10.0)).p(px(14.0)).continuous(14.0).bg(rgb(raised())).border_1().border_color(rgb(ink(0.15))).flex().flex_col().gap(px(10.0));
        let body = match self.card.clone()? {
            Card::Question(q) => {
                let options: Vec<AnyElement> = q
                    .options
                    .iter()
                    .enumerate()
                    .map(|(i, option)| {
                        let id = q.question_id.clone();
                        let answer = option.clone();
                        chip(("agent-option", i), option.clone())
                            .bg(theme::control_fill())
                            .when(active, |el| {
                                el.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.answer_question(id.clone(), answer.clone(), cx)))
                            })
                            .into_any_element()
                    })
                    .collect();
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon("question", 16.0, theme::muted()))
                        .child(div().font_weight(FontWeight::MEDIUM).child(q.question.clone())),
                )
                .when(!options.is_empty(), |el| el.child(div().flex().flex_wrap().gap(px(6.0)).children(options)))
                .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child("Or type an answer below"))
            }
            Card::Approval(a, more) => {
                // `shell: cd <project> && …`: the command reads as on its line.
                let summary = match a.summary.split_once(": ") {
                    Some((tool, rest)) if !tool.contains(' ') => format!("{tool}: {}", self.shown(rest)),
                    _ => self.shown(&a.summary),
                };
                let summary = transcript::clip(&summary, 160);
                let button = |id: &'static str, label: &'static str, hint: &'static str, primary: bool| {
                    div()
                        .id(id)
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(12.0))
                        .h(px(28.0))
                        .capsule()
                        .text_size(px(12.5))
                        .map(|el| {
                            if primary { el.bg(rgb(theme::foreground())).text_color(rgb(theme::background())) } else { el.bg(theme::control_fill()) }
                        })
                        .when(active, |el| el.cursor_pointer().hover(|s| s.opacity(0.88)).active(|s| s.scale(0.96)))
                        .child(label)
                        .when(!hint.is_empty(), |el| el.child(div().text_size(px(11.0)).opacity(0.6).font_family(mono()).child(hint)))
                };
                let answer =
                    |decision: ApprovalDecision| cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| this.answer_approval(decision, cx));
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon("lock", 14.0, theme::muted()))
                        .child(div().font_weight(FontWeight::MEDIUM).child(format!("Allow {}?", a.tool)))
                        .when(more > 0, |el| el.child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(format!("+{more} more")))),
                )
                .when(!summary.is_empty(), |el| {
                    el.child(div().text_size(px(12.0)).font_family(mono()).text_color(rgb(theme::muted())).child(summary))
                })
                .child(
                    div()
                        .flex()
                        .gap(px(6.0))
                        .child(button("agent-allow", "Allow", "⏎", true).when(active, |el| el.on_click(answer(ApprovalDecision::Allow))))
                        .child(button("agent-always", "Always", "", false).when(active, |el| el.on_click(answer(ApprovalDecision::AlwaysAllow))))
                        .child(button("agent-deny", "Deny", "esc", false).when(active, |el| el.on_click(answer(ApprovalDecision::Deny)))),
                )
            }
        };
        Some(div().flex_none().max_h(px(320.0 * t)).overflow_hidden().opacity(t).relative().top(px((1.0 - t) * 8.0)).child(body).into_any_element())
    }

    /// The bars stacked above the composer (subagents, todos, queue). Only
    /// the bottom one tucks into the composer; those above float with a gap.
    fn render_bars(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let now = Instant::now();
        let muted = theme::muted();
        let fg = theme::foreground();
        let mut bars: Vec<Bar> = Vec::new();
        let text = |s: String| div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(fg)).child(s);

        bars.extend(self.subagent_bar(cx));

        if !self.todos.is_empty() {
            let (done, total) = transcript::progress(&self.todos);
            let current = transcript::current(&self.todos).map(|t| (t.status, t.step.clone()));
            bars.push(Bar {
                key: "todos",
                icon: icon("list-todo", 14.0, muted).into_any_element(),
                label: "Todos",
                summary: current.map(|(s, step)| todo_item(s, step).into_any_element()),
                note: format!("{done}/{total} done"),
                open_label: "Todos".into(),
                open_note: format!("{done}/{total} done"),
                rows: self
                    .todos
                    .iter()
                    .map(|t| div().h(px(BAR_ROW)).px(px(14.0)).flex().items_center().child(todo_item(t.status, t.step.clone())).into_any_element())
                    .collect(),
            });
        }

        // Pending steers (they land at the next step), then inputs waiting for the turn to end.
        let queue = self.queue_rows();
        if !queue.is_empty() {
            let n = queue.len();
            let first = queue[0].text.clone();
            bars.push(Bar {
                key: "queue",
                icon: icon("clock", 14.0, theme::warning()).into_any_element(),
                label: "Queued",
                summary: Some(text(if first.is_empty() { "Attachment".into() } else { first }).into_any_element()),
                note: format!("{n} queued"),
                open_label: "Queued messages".into(),
                open_note: format!("{n} waiting"),
                rows: queue.into_iter().enumerate().map(|(i, row)| self.queue_row(i, row, cx)).collect(),
            });
        }

        let shown: Vec<&'static str> = bars.iter().map(|b| b.key).collect();
        self.bar_shown.retain(|k, _| shown.contains(k));
        let last = bars.len().saturating_sub(1);
        bars.into_iter().enumerate().map(|(i, bar)| self.render_bar(bar, i == last, now, cx)).collect()
    }

    /// A pending input: position, text and when it lands (`Next step` / `After
    /// turn`); on hover that note gives way to switching it and `×` removing it.
    fn queue_row(&self, i: usize, row: super::QueueRow, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let group = SharedString::from(format!("queued-{i}"));
        let super::QueueRow { id, text, steer } = row;
        // Wide enough for the hover actions that take its place.
        let note = div().flex_none().min_w(px(QUEUE_ACTIONS)).flex().justify_end().text_size(px(11.5)).text_color(rgb(muted)).child(if steer {
            "Next step"
        } else {
            "After turn"
        });
        div()
            .id(gpui::ElementId::Name(group.clone()))
            .group(group.clone())
            .relative()
            .h(px(BAR_ROW))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .hover(|s| s.bg(theme::hover_fill()))
            .child(
                div()
                    .flex_none()
                    .size(px(18.0))
                    .rounded_full()
                    .bg(rgb(ink(0.08)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.5))
                    .text_color(rgb(muted))
                    .child(format!("{}", i + 1)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(theme::foreground()))
                    .child(if text.is_empty() { "Attachment".to_owned() } else { text }),
            )
            .map(|el| match id {
                // Not confirmed by otto yet: nothing to act on.
                None => el.child(note),
                Some(id) => {
                    let (label, delivery) = if steer { ("Send later", Delivery::Queue) } else { ("Steer now", Delivery::Steer) };
                    let switch_id = id.clone();
                    let actions = div()
                        .absolute()
                        .right(px(8.0))
                        .top_0()
                        .h_full()
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .opacity(0.0)
                        .group_hover(group.clone(), |s| s.opacity(1.0))
                        .child(
                            div()
                                .id(gpui::ElementId::Name(format!("queue-switch-{i}").into()))
                                .flex_none()
                                .h(px(22.0))
                                .px(px(6.0))
                                .flex()
                                .items_center()
                                .continuous(6.0)
                                .text_size(px(11.5))
                                .text_color(rgb(muted))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::hover_fill()).text_color(rgb(theme::foreground())))
                                .active(|s| s.scale(0.96))
                                .child(label)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.edit_queued(switch_id.clone(), Some(delivery), cx))),
                        )
                        .child(
                            icon_button(gpui::ElementId::Name(format!("queue-remove-{i}").into()), "close", 12.0, 22.0, muted)
                                .tooltip(Tooltip::text("Remove"))
                                .active(|s| s.scale(0.92))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.edit_queued(id.clone(), None, cx))),
                        );
                    el.child(note.group_hover(group, |s| s.opacity(0.0))).child(actions)
                }
            })
            .into_any_element()
    }

    /// One bar: a 34 header that crossfades between the collapsed summary and
    /// the expanded title, then its rows (height + opacity, chevron turns).
    fn render_bar(&mut self, bar: Bar, tucked: bool, now: Instant, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let fg = theme::foreground();
        let primed = self.primed;
        let appeared = *self.bar_shown.entry(bar.key).or_insert_with(|| if primed { now } else { now.checked_sub(motion::UI).unwrap_or(now) });
        let o = toggled(self.bars.entry(bar.key).or_insert_with(|| motion::Toggle::new(false, motion::UI)), now, cx);
        let key = bar.key;
        let toggle = move |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
            let reduce = cx.reduce_motion();
            let t = this.bars.entry(key).or_insert_with(|| motion::Toggle::new(false, motion::UI));
            let open = !t.on();
            t.set(open, Instant::now(), reduce);
            cx.notify();
        };
        let chevron =
            |o: f32| icon("chevron-up", 12.0, muted).with_transformation(gpui::Transformation::rotate(gpui::radians(o * std::f32::consts::PI)));
        let header = |id: String, h: f32, alpha: f32| {
            div()
                .id(gpui::ElementId::Name(id.into()))
                .flex_none()
                .h(px(BAR_HEADER * h))
                .overflow_hidden()
                .opacity(alpha)
                .px(px(14.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
        };
        // Narrow: the label and current item go; the note and chevron stay.
        let fit = self.fit();
        let summary = bar.summary.filter(|_| fit.detail);
        let has_summary = summary.is_some();
        let collapsed = header(format!("bar-{key}"), 1.0 - o, 1.0 - o)
            .child(
                div().flex_none().flex().items_center().gap(px(8.0)).text_color(rgb(fg)).child(bar.icon).when(fit.labels, |el| el.child(bar.label)),
            )
            .when_some(summary, |el, s| {
                el.child(div().flex_none().w(px(1.0)).h(px(14.0)).bg(rgb(ink(0.15)))).child(div().flex_1().min_w_0().flex().child(s))
            })
            .when(!has_summary, |el| el.child(div().flex_1()))
            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(muted)).child(bar.note))
            .child(chevron(o))
            .on_click(cx.listener(toggle));
        #[allow(clippy::cast_precision_loss)]
        let full = bar.rows.len() as f32 * BAR_ROW + 8.0;
        let mut el = div()
            .mx(px(if fit.detail { 28.0 } else { 12.0 }))
            .bg(rgb(raised()))
            .border_1()
            .border_color(rgb(ink(0.15)))
            .overflow_hidden()
            .font_family(mono())
            .text_size(px(12.5))
            .flex()
            .flex_col()
            .child(collapsed);
        if o > 0.0 {
            let icon_name = match key {
                "queue" => "clock",
                "subagents" => "bot",
                _ => "list-todo",
            };
            let tint = match key {
                "queue" => theme::warning(),
                "subagents" => theme::accent(),
                _ => muted,
            };
            el = el
                .child(
                    header(format!("bar-open-{key}"), o, o)
                        .border_b_1()
                        .border_color(rgb(ink(0.1)))
                        .child(icon(icon_name, 14.0, tint))
                        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(fg)).child(bar.open_label))
                        .child(div().flex_1())
                        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(muted)).child(bar.open_note))
                        .child(chevron(o))
                        .on_click(cx.listener(toggle)),
                )
                .child(div().flex_none().max_h(px(full * o)).overflow_hidden().opacity(o).py(px(4.0)).flex().flex_col().children(bar.rows));
        }
        // Tucked under the composer's top half, or floating above the bar below.
        let tuck = if tucked { pill_input::HEIGHT / 2.0 + pill_input::HALO } else { 0.0 };
        let el = if tucked { el.rounded_tl(px(12.0)).rounded_tr(px(12.0)).mb(px(-tuck)).pb(px(tuck)) } else { el.continuous(12.0).mb(px(6.0)) };
        // Rises from behind what's below: a bottom-anchored clip grows to the
        // bar's height, pushing the transcript up as it goes. (No negative
        // margins on this wrapper: taffy then mis-sizes the composer column.)
        let t = eased(appeared, motion::UI, now, cx).unwrap_or(1.0);
        let wrap = div().flex_none().opacity(t);
        if t >= 1.0 {
            return wrap.child(el).into_any_element();
        }
        let outer = BAR_HEADER + 2.0 + o * (BAR_HEADER + full) + if tucked { 0.0 } else { 6.0 };
        wrap.h(px(outer * t)).overflow_hidden().flex().flex_col().justify_end().child(el.flex_none()).into_any_element()
    }

    fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.editor.read(cx).focus.is_focused(window);
        let empty = self.editor.read(cx).text().trim().is_empty() && self.attachments.is_empty();
        let question = self.mirror().is_some_and(|m| !m.pending_questions.is_empty());
        let placeholder = if question {
            "Answer…"
        } else if self.running {
            "Message the agent"
        } else if self.session_id.is_some() && self.error.is_none() {
            "Reply"
        } else {
            "Ask anything"
        };
        self.editor.update(cx, |e, cx| e.set_placeholder(placeholder, cx));
        let colors = EditorColors {
            text: rgb(theme::foreground()).into(),
            placeholder: rgb(theme::muted()).into(),
            caret: rgb(theme::foreground()).into(),
            selection: hsla(theme::selection(), 0.6),
        };
        let editor = Editor::element(&self.editor, 8, colors, cx);
        // Always send: while a turn runs it steers (stopping is the activity row's, and esc).
        let enabled = !empty;
        let now = Instant::now();
        let reduce = cx.reduce_motion();
        self.send_ready.set(enabled, now, reduce);
        let t = toggled(&self.send_ready, now, cx);
        let send = pill_input::circle("agent-send", t, enabled, icon("arrow-up", 16.0, pill_input::circle_colors(t).1).into_any_element())
            .absolute()
            .right(px(pill_input::INSET - pill_input::BORDER))
            .bottom(px(pill_input::INSET - pill_input::BORDER))
            .tooltip(Tooltip::text(if self.running { "Steer  ⏎" } else { "Send  ⏎" }))
            .when(enabled, |el| el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.submit(window, cx))));
        // The send circle, mirrored.
        let menu = pill_input::lead_button("agent-menu", "more-vertical")
            .absolute()
            .left(px(pill_input::INSET - pill_input::BORDER))
            .bottom(px(pill_input::INSET - pill_input::BORDER))
            .tooltip(Tooltip::text("Options"))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Menu, window, cx)));
        let pad_y = (pill_input::HEIGHT - 2.0 * pill_input::BORDER - COMPOSER_LINE) / 2.0;
        // Narrow: no `⋮` (the slash menu has it all), the text starts near the
        // edge; narrowest, no send circle (⏎ sends).
        let Fit { lead, send: circle, .. } = self.fit();
        let pad_l = if lead {
            pill_input::TEXT_X - pill_input::BORDER
        } else if circle {
            16.0
        } else {
            10.0
        };
        let pad_r = if circle { pill_input::INSET + pill_input::CIRCLE + 10.0 - pill_input::BORDER } else { 10.0 };
        // The chips belong to the whole pill: their row spans it from the buttons' inset, above the input row.
        // Its height glides to what it measured (the pill grows and shrinks smoothly, the tail stays put).
        let edge = pill_input::INSET - pill_input::BORDER;
        let measured = self.chip_row.1.clone();
        let want = if self.attachments.is_empty() { 0.0 } else { measured.get() };
        self.chip_row.0.target(want, now, reduce);
        let row_h = self.chip_row.0.value(now, reduce);
        if self.chip_row.0.moving(now, reduce) {
            MOVING.set(true);
        }
        let chips = (!self.attachments.is_empty() || row_h > 0.0).then(|| {
            let inner = div().relative().pb(px(8.0)).flex().flex_wrap().gap(px(6.0)).children(self.attachments.iter().enumerate().map(|(i, a)| {
                let enter = eased(a.at, motion::UI, now, cx).unwrap_or(1.0);
                div()
                    .relative()
                    .left(px((1.0 - enter) * -6.0))
                    .opacity(enter)
                    .min_w_0()
                    .max_w_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(24.0))
                    .pl(px(8.0))
                    .pr(px(4.0))
                    .capsule()
                    .bg(rgb(ink(0.08)))
                    .text_size(px(12.0))
                    .text_color(rgb(theme::muted()))
                    .child(super::attach::glyph(a.thumb.clone(), &a.mime))
                    .child(div().min_w_0().max_w(px(160.0)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(a.name.clone()))
                    .child(
                        div()
                            .id(("agent-attachment", i))
                            .flex_none()
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::hover_fill()))
                            .child(icon("close", 10.0, theme::muted()))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if i < this.attachments.len() {
                                    this.attachments.remove(i);
                                }
                                cx.notify();
                            })),
                    )
            }));
            // A new height (rewrapped, a chip added) draws again to glide to it.
            let report = gpui::canvas(
                move |bounds, window, _| {
                    let h = f32::from(bounds.size.height);
                    if (measured.replace(h) - h).abs() >= 0.5 {
                        window.refresh();
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full();
            div()
                .flex_none()
                .ml(px(edge.min(pad_l) - pad_l))
                .mr(px(edge.min(pad_r) - pad_r))
                .h(px(row_h))
                .overflow_hidden()
                .child(inner.child(report))
        });
        let pill = pill_input::pill(focused, false)
            .pl(px(pad_l))
            .pr(px(pad_r))
            .py(px(pad_y))
            .font_family(mono())
            .text_size(px(14.0))
            .line_height(px(COMPOSER_LINE))
            .flex()
            .flex_col()
            .children(chips)
            .child(editor)
            .when(lead, |el| el.child(menu))
            .when(circle, |el| el.child(send));
        pill_input::halo(false, pill).into_any_element()
    }

    /// The model the next prompt goes to: the session's, else the pick, else the daemon's default.
    fn model(&self) -> Option<(String, String)> {
        if let Some(update) = self.update.model.clone().filter(|_| self.mirror().is_some()) {
            return Some(update);
        }
        match (self.mirror(), &self.choice.model) {
            (Some(m), _) if !m.info.model.is_empty() => Some((m.info.provider.clone(), m.info.model.clone())),
            (_, Some(pick)) => Some(pick.clone()),
            _ => self.catalog.as_ref()?.default_model(self.choice.agent.as_deref().unwrap_or("build")),
        }
    }

    /// Context use and cost of the shown session (`12% · $0.31`).
    fn usage(&self) -> Option<String> {
        let (provider, model) = self.model()?;
        let size = self.catalog.as_ref()?.models.iter().find(|m| m.id == model && m.provider.as_str() == provider).map(|m| m.caps.context_window);
        let mut parts = Vec::new();
        if let (Some(used), Some(size)) = (self.context, size.filter(|s| *s > 0)) {
            #[allow(clippy::cast_precision_loss)]
            parts.push(format!("{:.0}% context", used as f64 * 100.0 / size as f64));
        }
        if let Some(cost) = self.mirror().map(|m| m.info.usage.cost_usd).filter(|c| *c > 0.0) {
            parts.push(format!("${cost:.2}"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    fn render_footer(&mut self, fit: Fit, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let agent = self.agent_name();
        let model = self.model();
        let catalog = self.catalog.as_ref();
        let plan = model.as_ref().filter(|_| fit.plan).and_then(|(p, _)| catalog.and_then(|c| c.subscription(p)));
        let reasons = model
            .as_ref()
            .is_some_and(|(p, id)| catalog.is_some_and(|c| c.models.iter().any(|m| m.id == *id && m.provider.as_str() == p && m.caps.reasoning)))
            && fit.tools;
        // Reasoning is on when the session asks for some effort (the provider's default reads as off).
        let effort = self.effort();
        let thinking = effort.is_some_and(|e| e != Effort::None);
        let tooltip = self.usage();
        let agent_button = div()
            .id("agent-agent")
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(4.0))
            .h(px(22.0))
            .px(px(6.0))
            .continuous(6.0)
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(agent.to_uppercase()))
            .child(div().flex_none().flex().child(icon("chevron-up", 12.0, muted)))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Agents, window, cx)));
        let model_button = div()
            .id("agent-model")
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(22.0))
            .px(px(6.0))
            .continuous(6.0)
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .when_some(tooltip, |el, usage| el.tooltip(Tooltip::text(usage)))
            .map(|el| match &model {
                Some((provider, id)) => el
                    .when(fit.provider, |el| {
                        el.map(|el| match provider_logo(provider) {
                            Some(logo) => el.child(div().flex_none().flex().child(icon(logo, 13.0, ink(0.8)))),
                            None => el.child(div().flex_none().text_color(rgb(ink(0.8))).child(provider.clone())),
                        })
                        .child(div().flex_none().text_color(rgb(ink(0.35))).child("/"))
                    })
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(ink(0.72))).child(id.clone()))
                    .when_some(plan, |el, plan| el.child(div().flex_none().text_color(rgb(ink(0.38))).child(format!("({plan})")))),
                None => el.child("No model"),
            })
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Models, window, cx)));
        // The effort menu; lit at any level but the default. Only for models that reason.
        let brain = reasons.then(|| {
            // Read when drawn, so it follows a change made while the pointer stays on it.
            let pane = cx.entity().downgrade();
            let tip = Tooltip::live(move |cx| {
                let effort = pane.upgrade().and_then(|p| p.read(cx).effort());
                format!("Effort: {}", effort_label(effort)).into()
            });
            div()
                .id("agent-brain")
                .size(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .continuous(6.0)
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
                .active(|s| s.scale(0.92))
                .when(self.overlay == Some(Overlay::Effort), |el| el.bg(theme::hover_fill()))
                .tooltip(tip)
                .child(icon("brain", 14.0, if thinking { violet() } else { muted }))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Effort, window, cx)))
        });
        let image = div()
            .id("agent-image")
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .continuous(6.0)
            .tooltip(Tooltip::text("Attach files"))
            .child(icon("plus", 14.0, muted))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .active(|s| s.scale(0.92))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.attach_files(cx)));
        // At the circles' outer edges: the agent label starts under the ⋮
        // circle's left edge, the image glyph ends under the send circle's
        // right edge (button paddings 6 and 4).
        // Left and right share what the model leaves; each clips its own (no overlap).
        let side = || div().flex_1().flex_basis(px(0.0)).min_w_0().overflow_hidden().flex().items_center();
        div()
            .mt(px(8.0))
            .h(px(22.0))
            .flex()
            .items_center()
            .font_family(mono())
            .text_size(px(12.0))
            .text_color(rgb(muted))
            .child(side().pl(px(pill_input::HALO + pill_input::INSET - 6.0)).child(agent_button))
            .when(fit.model, |el| {
                el.child(div().flex_none().max_w(relative(0.5)).min_w_0().overflow_hidden().flex().justify_center().child(model_button))
            })
            .child(
                side()
                    .justify_end()
                    .gap(px(2.0))
                    .pr(px(pill_input::HALO + pill_input::INSET - 4.0))
                    .children(brain)
                    .when(fit.tools, |el| el.child(image)),
            )
            .into_any_element()
    }

    // ---- popovers ----

    /// Clicking outside an open popover closes it (the slash menu stays while the `/` does).
    fn render_backdrop(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.overlay.is_none() || self.overlay == Some(Overlay::Slash) {
            return None;
        }
        Some(
            div()
                .id("agent-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_overlay(window, cx)))
                .into_any_element(),
        )
    }

    /// `which`'s card while open or fading out: the switcher hangs under the
    /// pane header, the others sit above their trigger. Opens with height +
    /// opacity and a 6 pt slide from its anchor.
    fn popover(&mut self, which: Overlay, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let now = Instant::now();
        if self.overlay_shown != Some(which) || self.overlay_anim.hidden(now, cx.reduce_motion()) {
            return None;
        }
        let t = toggled(&self.overlay_anim, now, cx);
        let body = match which {
            Overlay::Sessions => self.render_sessions(window, cx),
            Overlay::Projects => self.render_projects(window, cx),
            Overlay::Models => self.render_models(window, cx),
            Overlay::Agents => self.render_agents(cx),
            Overlay::Effort => self.render_efforts(cx),
            Overlay::Menu => self.render_menu(cx),
            Overlay::Slash => self.render_slash(cx),
        };
        let width = match which {
            Overlay::Sessions | Overlay::Projects => 440.0,
            Overlay::Models => 340.0,
            Overlay::Effort => 260.0,
            Overlay::Agents | Overlay::Menu | Overlay::Slash => 320.0,
        };
        let below = matches!(which, Overlay::Sessions | Overlay::Projects);
        let card = div()
            .id("agent-popover")
            .occlude()
            .relative()
            .top(px((1.0 - t) * if below { -6.0 } else { 6.0 }))
            .opacity(t)
            .w(px(width))
            .max_w(relative(1.0))
            .max_h(px(40.0 + 520.0 * t))
            .overflow_hidden()
            .p(px(6.0))
            .continuous(12.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(ink(0.12)))
            .shadow(vec![BoxShadow {
                color: hsla(0, if theme::is_light() { 0.12 } else { 0.4 }),
                offset: point(px(0.0), px(8.0)),
                blur_radius: px(28.0),
                spread_radius: px(0.0),
                inset: false,
            }])
            .text_size(px(13.0))
            .flex()
            .flex_col()
            .child(body);
        let anchor = div()
            .absolute()
            .left_0()
            .right_0()
            .flex()
            .map(|el| match which {
                Overlay::Models => el.justify_center(),
                // Above the brain, at the composer's right.
                Overlay::Effort => el.justify_end().pr(px(pill_input::HALO)),
                Overlay::Sessions | Overlay::Projects => el,
                _ => el.pl(px(pill_input::HALO)),
            })
            .map(|el| if below { el.top(px(4.0)).px(px(6.0)) } else { el.bottom(relative(1.0)).pb(px(8.0)) })
            .child(card);
        Some(gpui::deferred(anchor).with_priority(1).into_any_element())
    }

    /// A row that takes the keyboard selection on hover.
    fn pick_row(&self, row: gpui::Stateful<gpui::Div>, index: usize, selected: bool, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        rows::clickable(row, selected).on_mouse_move(cx.listener(move |this, _, _, cx| {
            if this.selected != index {
                this.selected = index;
                cx.notify();
            }
        }))
    }

    fn search_field(&self, window: &Window, cx: &App) -> AnyElement {
        let focused = self.filter.read(cx).focus.is_focused(window);
        rows::search(focused, self.filter.clone()).into_any_element()
    }

    fn render_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.session_id.clone();
        // The shown session is named as in the pane header (its first prompt until otto titles it).
        let shown = self.header_title();
        let choices: Vec<(String, String, String, SessionStatus, i64)> = self
            .session_choices(cx)
            .into_iter()
            .map(|s| {
                let title = match &shown {
                    Some(t) if current.as_deref() == Some(s.id.as_str()) => t.clone(),
                    _ => super::session_label(s),
                };
                (s.id.clone(), title, s.model.clone(), s.status, s.updated_at)
            })
            .collect();
        let count = choices.len();
        // The block's project on top: opens the recent projects.
        let project = self.project.as_ref().map_or_else(|| "Project".to_owned(), |p| super::project_name(&p.root));
        let project_row = rows::row(
            "agent-project",
            self.selected == 0,
            Some(rows::lead("folder", false)),
            project,
            Some(div().child("Project").into_any_element()),
            Some(icon("chevron-right", 12.0, theme::muted()).into_any_element()),
        );
        let project_row = self
            .pick_row(project_row, 0, self.selected == 0, cx)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.open_overlay(Overlay::Projects, window, cx)));
        let mut list = div().id("agent-sessions").flex().flex_col().max_h(px(rows::ROW_H * 7.0)).overflow_y_scroll();
        for (i, (id, title, model, status, updated)) in choices.into_iter().enumerate() {
            let i = i + 1;
            let checked = current.as_deref() == Some(id.as_str());
            let state = match status {
                SessionStatus::Busy => Some("running"),
                SessionStatus::WaitingApproval => Some("needs you"),
                SessionStatus::Error => Some("error"),
                _ => None,
            };
            let subtitle = [Some(model.as_str()).filter(|m| !m.is_empty()), state, Some(ago(updated).as_str())]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            let lead = session_dot(status).map(|c| div().size(px(7.0)).rounded_full().bg(rgb(c)).into_any_element());
            let row = rows::row(
                gpui::ElementId::Name(format!("session-{id}").into()),
                i == self.selected,
                lead,
                div().when(checked, |el| el.font_weight(FontWeight::MEDIUM)).child(title),
                Some(div().child(subtitle).into_any_element()),
                checked.then(rows::check),
            );
            list = list.child(
                self.pick_row(row, i, i == self.selected, cx)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.pick_session(id.clone(), window, cx))),
            );
        }
        if count == 0 {
            let empty = if self.filter.read(cx).text.trim().is_empty() { "No sessions yet" } else { "No matching sessions" };
            list = list.child(rows::row("agent-sessions-empty", false, None, div().text_color(rgb(theme::muted())).child(empty), None, None));
        }
        let new = rows::row("agent-new-session", self.selected == count + 1, Some(rows::lead("new-session", false)), "New session", None, None);
        let new = self
            .pick_row(new, count + 1, self.selected == count + 1, cx)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.new_session(window, cx)));
        div()
            .flex()
            .flex_col()
            .child(project_row)
            .child(rows::divider())
            .child(self.search_field(window, cx))
            .child(list)
            .child(rows::divider())
            .child(new)
            .into_any_element()
    }

    /// Recent otto projects by folder name, then Open folder…; picking one moves the block there.
    fn render_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.project.as_ref().map(|p| p.id.clone());
        let names: std::collections::HashMap<String, (String, Option<String>)> =
            self.projects.iter().map(|p| p.id.clone()).zip(super::project_names(self.projects.iter().map(|p| p.root.as_str()))).collect();
        let choices: Vec<(String, String, Option<String>, i64)> = self
            .project_choices(cx)
            .into_iter()
            .map(|p| {
                let (name, parent) = names.get(&p.id).cloned().unwrap_or_else(|| (super::project_name(&p.root), None));
                (p.id.clone(), name, parent, p.updated_at)
            })
            .collect();
        let count = choices.len();
        let mut list = div().id("agent-projects").flex().flex_col().max_h(px(rows::ROW_H * 7.0)).overflow_y_scroll();
        for (i, (id, name, parent, updated)) in choices.into_iter().enumerate() {
            let checked = current.as_deref() == Some(id.as_str());
            let subtitle = [parent.as_deref(), Some(ago(updated).as_str())].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            let row = rows::row(
                gpui::ElementId::Name(format!("project-{id}").into()),
                i == self.selected,
                Some(rows::lead("folder", checked)),
                div().when(checked, |el| el.font_weight(FontWeight::MEDIUM)).child(name),
                Some(div().child(subtitle).into_any_element()),
                checked.then(rows::check),
            );
            list = list.child(
                self.pick_row(row, i, i == self.selected, cx)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.activate_choice(i, window, cx))),
            );
        }
        if count == 0 {
            let empty = if self.filter.read(cx).text.trim().is_empty() { "No recent projects" } else { "No matching projects" };
            list = list.child(rows::row("agent-projects-empty", false, None, div().text_color(rgb(theme::muted())).child(empty), None, None));
        }
        let open = rows::row("agent-open-folder", self.selected == count, Some(rows::lead("plus", false)), "Open folder…", None, None);
        let open = self
            .pick_row(open, count, self.selected == count, cx)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.activate_choice(count, window, cx)));
        div().flex().flex_col().child(self.search_field(window, cx)).child(list).child(rows::divider()).child(open).into_any_element()
    }

    fn render_models(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let current = self.model();
        let catalog = self.catalog.clone().unwrap_or_default();
        let choices: Vec<(String, String, String)> =
            self.model_choices(cx).into_iter().map(|m| (m.provider.as_str().to_owned(), m.id.clone(), m.name.clone())).collect();
        let mut list = div().id("agent-models").flex().flex_col().max_h(px(rows::ROW_H * 7.0)).overflow_y_scroll();
        if choices.is_empty() {
            list = list.child(rows::row("agent-models-empty", false, None, div().text_color(rgb(muted)).child("No matching models"), None, None));
        }
        let mut group: Option<String> = None;
        for (i, (provider, id, name)) in choices.into_iter().enumerate() {
            if group.as_deref() != Some(provider.as_str()) {
                let label = match catalog.subscription(&provider) {
                    Some(plan) if plan != "sub" => format!("{} · {plan}", provider_name(&provider)),
                    Some(_) => format!("{} · subscription", provider_name(&provider)),
                    None => provider_name(&provider),
                };
                list = list.child(rows::section(label, provider_logo(&provider).map(|logo| icon(logo, 13.0, muted).into_any_element())));
                group = Some(provider.clone());
            }
            let checked = current.as_ref().is_some_and(|(p, m)| *p == provider && *m == id);
            let subtitle = (!name.is_empty() && name != id).then(|| div().child(name).into_any_element());
            let row = rows::row(
                gpui::ElementId::Name(format!("model-{provider}-{id}").into()),
                i == self.selected,
                None,
                div().font_family(mono()).text_size(px(12.5)).child(id),
                subtitle,
                checked.then(rows::check),
            );
            list = list.child(
                self.pick_row(row, i, i == self.selected, cx)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.activate_choice(i, window, cx))),
            );
        }
        let usage = self.usage();
        div()
            .flex()
            .flex_col()
            .when_some(usage, |el, u| {
                el.child(
                    div()
                        .h(px(26.0))
                        .px(px(rows::PAD))
                        .flex()
                        .items_center()
                        .justify_end()
                        .font_family(mono())
                        .text_size(px(11.5))
                        .text_color(rgb(muted))
                        .child(u),
                )
            })
            .child(self.search_field(window, cx))
            .child(list)
            .when(self.running && self.updatable().is_some(), |el| el.child(footnote("Applies from the next step")))
            .into_any_element()
    }

    fn render_agents(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.agent_name();
        let mut list = div().id("agent-agents").flex().flex_col().max_h(px(rows::ROW_H * 7.0)).overflow_y_scroll();
        for (i, (name, description)) in self.agent_choices().into_iter().enumerate() {
            let checked = name == current;
            let subtitle = (!description.is_empty()).then(|| div().child(transcript::clip(&description, 80)).into_any_element());
            let row = rows::row(
                gpui::ElementId::Name(format!("agent-{name}").into()),
                i == self.selected,
                Some(rows::lead(if name == "plan" { "list-todo" } else { "bot" }, checked)),
                div().font_family(mono()).text_size(px(12.5)).child(name.to_uppercase()),
                subtitle,
                checked.then(rows::check),
            );
            list = list.child(
                self.pick_row(row, i, i == self.selected, cx)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.activate_choice(i, window, cx))),
            );
        }
        div()
            .flex()
            .flex_col()
            .child(list)
            .when(self.running && self.updatable().is_some(), |el| el.child(footnote("Applies from the next step")))
            .into_any_element()
    }

    fn render_efforts(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.effort();
        let mut list = div().id("agent-efforts").flex().flex_col();
        for (i, effort) in super::EFFORTS.into_iter().enumerate() {
            let checked = effort == current;
            let row = rows::row(
                ("agent-effort", i),
                i == self.selected,
                Some(rows::lead("brain", checked)),
                effort_label(effort),
                Some(div().child(effort_about(effort)).into_any_element()),
                checked.then(rows::check),
            );
            list = list.child(self.pick_row(row, i, i == self.selected, cx).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.activate_choice(i, window, cx);
            })));
        }
        list.when(self.running && self.updatable().is_some(), |el| el.child(footnote("Applies from the next step"))).into_any_element()
    }

    fn render_menu(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.mirror().map_or(self.choice.mode, |m| m.info.approval_mode);
        let mut list = div().id("agent-menu-list").flex().flex_col().child(rows::section("Approvals", None));
        for (i, (label, glyph, hint)) in
            [("Auto", "lock-off", "Run tools without asking"), ("Ask", "lock", "Ask before changes"), ("Plan", "list-todo", "Read-only")]
                .into_iter()
                .enumerate()
        {
            let checked = mode_label(mode) == label;
            let row = rows::row(
                ("agent-mode", i),
                i == self.selected,
                Some(rows::lead(glyph, checked)),
                label,
                Some(div().child(hint).into_any_element()),
                checked.then(rows::check),
            );
            list = list.child(self.pick_row(row, i, i == self.selected, cx).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.activate_choice(i, window, cx);
            })));
        }
        list = list.child(rows::divider());
        for (i, (id, glyph, label, note)) in [
            ("agent-menu-new", "new-session", "New session", "/new"),
            ("agent-menu-sessions", "history", "Sessions", "⌘J"),
            ("agent-menu-providers", "plug", "Providers", "/providers"),
        ]
        .into_iter()
        .enumerate()
        {
            let i = i + 3;
            let row = rows::row(id, i == self.selected, Some(rows::lead(glyph, false)), label, None, Some(rows::note(note)));
            list = list.child(self.pick_row(row, i, i == self.selected, cx).on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.activate_choice(i, window, cx);
            })));
        }
        list.into_any_element()
    }

    fn render_slash(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.slash_choices(cx);
        let mode = mode_label(self.mirror().map_or(self.choice.mode, |m| m.info.approval_mode));
        if choices.is_empty() {
            return rows::row("agent-slash-empty", false, None, div().text_color(rgb(theme::muted())).child("No matching command"), None, None)
                .into_any_element();
        }
        let mut list = div().id("agent-slash").flex().flex_col().max_h(px(rows::ROW_H * 6.0)).overflow_y_scroll();
        for (i, c) in choices.into_iter().enumerate() {
            let act = c.act;
            let current = matches!(act, super::SlashAct::Mode(m) if mode_label(m) == mode);
            let row = rows::row(
                ("agent-slash", i),
                i == self.selected,
                Some(rows::lead(c.icon, i == self.selected || current)),
                div().font_family(mono()).text_size(px(12.5)).child(format!("/{}", c.name)),
                Some(div().child(c.about).into_any_element()),
                current.then(rows::check),
            );
            list = list.child(
                self.pick_row(row, i, i == self.selected, cx)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.run_slash(act, window, cx))),
            );
        }
        list.into_any_element()
    }
}

/// Lines that don't wrap, as one column as wide as the longest (so a box scrolls sideways to it); `scroller`
/// lays them out in a row that keeps their own height.
fn nowrap_lines<'a>(lines: impl Iterator<Item = (&'a str, Option<u32>)>) -> gpui::Div {
    div().flex_none().min_w_full().flex().flex_col().children(lines.map(|(l, color)| {
        div().whitespace_nowrap().when_some(color, |el, c| el.text_color(rgb(c))).child(if l.is_empty() { " ".to_owned() } else { l.to_owned() })
    }))
}

fn render_detail(key: &str, detail: &Detail, scroll: &std::rc::Rc<super::scroll::Inner>) -> AnyElement {
    let muted = theme::muted();
    let id = format!("detail-{key}");
    let base = div()
        .id(gpui::ElementId::Name(id.clone().into()))
        .ml(px(24.0))
        .px(px(10.0))
        .py(px(8.0))
        .continuous(8.0)
        .bg(rgb(ink(0.04)))
        .text_size(px(12.0))
        .line_height(px(17.0))
        .text_color(rgb(muted))
        .max_h(px(240.0));
    match detail {
        Detail::Text(text) => scroll.scroller(&id, base, false).child(text.clone()).into_any_element(),
        Detail::Mono(text) => scroll
            .scroller(&id, base.flex().items_start().font_family(mono()), true)
            .child(nowrap_lines(text.lines().map(|l| (l, None))))
            .into_any_element(),
        Detail::Diff(text) => scroll
            .scroller(&id, base.flex().items_start().font_family(mono()), true)
            .child(nowrap_lines(text.lines().map(|l| {
                let color = if l.starts_with('+') && !l.starts_with("+++") {
                    theme::success()
                } else if l.starts_with('-') && !l.starts_with("---") {
                    theme::danger()
                } else {
                    muted
                };
                (l, Some(color))
            })))
            .into_any_element(),
    }
}

/// A streaming message's line reveal while drawing its blocks not yet fully shown: each leaf's line
/// starts and sweep progress (`reveal::Shown::lines`; a leaf not in it shows), and where each leaf's
/// wrapped lines are reported after layout.
struct Fx<'a> {
    lines: &'a std::collections::HashMap<String, Vec<(usize, f32)>>,
    wraps: &'a std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, reveal::Wrap>>>,
}

/// Value at `at` (a byte or line index) of a leaf's `lines` (opacity, or a code line's progress): 1 when it isn't revealing.
fn line_alpha(lines: Option<&[(usize, f32)]>, at: usize) -> f32 {
    lines.map_or(1.0, |l| l.iter().rev().find(|(s, _)| *s <= at).map_or(0.0, |(_, a)| *a))
}

/// Reports a revealed code box's character advance as a share of the width its lines show in (the element's,
/// less the left padding: a long line runs under the right one).
fn code_cell(fx: &Fx, id: &str) -> impl IntoElement {
    let (wraps, id) = (fx.wraps.clone(), id.to_owned());
    gpui::canvas(
        move |bounds, window, _| {
            let ts = window.text_system();
            let advance = ts.em_advance(ts.resolve_font(&gpui::font(mono())), px(reveal::CODE_FONT)).map_or(0.0, f32::from);
            let inner = f32::from(bounds.size.width) - reveal::CODE_PAD_X;
            wraps.borrow_mut().entry(id).or_default().cell = if inner > 0.0 { advance / inner } else { 0.0 };
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Where each wrapped line of a laid out text starts, its width as a share of the text's, and its sweep segments.
fn wrap_of(layout: &gpui::TextLayout) -> reveal::Wrap {
    let lines = layout.line_layouts();
    let full = lines.iter().filter_map(|l| l.wrap_width).map(f32::from).fold(0.0, f32::max);
    let full = if full > 0.0 { full } else { lines.iter().map(|l| f32::from(l.width())).fold(1.0, f32::max) };
    let mut wrap = reveal::Wrap { line_h: f32::from(layout.line_height()), ..reveal::Wrap::default() };
    let mut at = 0;
    for line in &lines {
        let un = &line.unwrapped_layout;
        // Each visual line's first glyph (byte, x) in the unwrapped line, and its glyphs.
        let mut firsts = vec![(0, 0.0)];
        firsts.extend(line.wrap_boundaries.iter().map(|b| {
            let g = &un.runs[b.run_ix].glyphs[b.glyph_ix];
            (g.index, f32::from(g.position.x))
        }));
        let mut glyphs: Vec<Vec<(usize, f32)>> = vec![Vec::new(); firsts.len()];
        let mut k = 0;
        for g in un.runs.iter().flat_map(|r| &r.glyphs) {
            while k + 1 < firsts.len() && g.index >= firsts[k + 1].0 {
                k += 1;
            }
            glyphs[k].push((at + g.index, f32::from(g.position.x) - firsts[k].1));
        }
        for (k, (start, x0)) in firsts.iter().enumerate() {
            let width = (firsts.get(k + 1).map_or(f32::from(un.width), |f| f.1) - x0).max(0.0);
            wrap.starts.push(at + start);
            wrap.widths.push(width / full);
            wrap.cuts.push(reveal::cuts(glyphs[k].iter().copied(), width, reveal::SEGMENTS));
        }
        at += line.len() + 1;
    }
    wrap
}

/// Styled runs of an inline: emphasis, code, links, and the opacity of each wrapped line while it's revealed.
fn inline_text(id: &str, inline: &Inline, lines: Option<&[(usize, f32)]>, fg: u32) -> (AnyElement, gpui::TextLayout) {
    let text = inline.text.clone();
    let len = text.len();
    let mut cuts: Vec<usize> = vec![0, len];
    for (r, _) in &inline.spans {
        cuts.extend([r.start, r.end]);
    }
    for (r, _) in &inline.links {
        cuts.extend([r.start, r.end]);
    }
    // Line starts come from the last layout: kept inside the text, on a char boundary.
    let lines: Option<Vec<(usize, f32)>> = lines.map(|l| {
        l.iter()
            .map(|(at, alpha)| {
                let mut at = (*at).min(len);
                while !text.is_char_boundary(at) {
                    at += 1;
                }
                (at, *alpha)
            })
            .collect()
    });
    cuts.extend(lines.iter().flatten().map(|(at, _)| *at));
    cuts.sort_unstable();
    cuts.dedup();
    let mut highlights = Vec::new();
    let mut code_ranges = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let style = inline.spans.iter().find(|(r, _)| r.start <= a && b <= r.end).map(|(_, s)| *s).unwrap_or_default();
        let link = inline.links.iter().any(|(r, _)| r.start <= a && b <= r.end);
        let alpha = line_alpha(lines.as_deref(), a);
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
            h.background_color = Some(hsla(faint(), alpha));
            code_ranges.push((a..b, mono()));
        }
        let color = if link { theme::accent() } else { fg };
        if link {
            h.underline = Some(UnderlineStyle { thickness: px(1.0), color: None, wavy: false });
        }
        if link {
            h.color = Some(hsla(color, 1.0));
        }
        // Not a transparent colour: gpui blends a highlight's colour over the text's, so only `fade_out` fades it.
        if alpha < 1.0 {
            h.fade_out = Some(1.0 - alpha);
        }
        if h != HighlightStyle::default() {
            highlights.push((a..b, h));
        }
    }
    let styled = StyledText::new(text).with_highlights(highlights).with_font_family_overrides(code_ranges);
    let layout = styled.layout().clone();
    if inline.links.is_empty() {
        return (styled.into_any_element(), layout);
    }
    let ranges: Vec<_> = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let urls: Vec<String> = inline.links.iter().map(|(_, u)| u.clone()).collect();
    let el = InteractiveText::new(gpui::ElementId::Name(id.to_owned().into()), styled)
        .on_click(ranges, move |ix, _, cx| cx.open_url(&urls[ix]))
        .into_any_element();
    (el, layout)
}

/// `fx`: the block is being revealed (`Fx`); `fg`: its text colour.
fn render_block(id: &str, block: &Block, fx: Option<&Fx>, fg: u32, scroll: &std::rc::Rc<super::scroll::Inner>) -> AnyElement {
    let lines = fx.and_then(|f| f.lines.get(id)).map(Vec::as_slice);
    // A revealed text reports its wrapped lines once laid out; a line sweeping in is cut into segments.
    let text = |inline: &Inline| {
        let fades = fx.zip(lines).map(|(f, l)| reveal::fades(l, f.wraps.borrow().get(id)));
        let (el, layout) = inline_text(id, inline, fades.as_deref(), fg);
        let report = fx.map(|f| {
            let (wraps, id) = (f.wraps.clone(), id.to_owned());
            gpui::canvas(
                move |_, _, _| {
                    wraps.borrow_mut().insert(id, wrap_of(&layout));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_0()
        });
        (el, report)
    };
    match block {
        Block::Para(inline) => {
            let (el, report) = text(inline);
            div().relative().pb(px(8.0)).line_height(px(21.0)).child(el).children(report).into_any_element()
        }
        Block::Heading(level, inline) => {
            let (el, report) = text(inline);
            div()
                .relative()
                .pt(px(8.0))
                .pb(px(6.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(match level {
                    1 => 18.0,
                    2 => 16.0,
                    _ => 14.5,
                }))
                .child(el)
                .children(report)
                .into_any_element()
        }
        // A revealed box draws only its complete lines: nothing before the first; long lines scroll sideways.
        // It comes in as its first line starts; a line sweeping in has a mask of the box's colour over its own
        // width, within the visible part (the box doesn't scroll it). The box reports a character's advance
        // as a share of its visible width (from before its first line), so its lines sweep at a constant speed.
        Block::Code { .. } if lines.is_some_and(<[_]>::is_empty) => div().relative().children(fx.map(|f| code_cell(f, id))).into_any_element(),
        Block::Code { text, .. } => {
            let el = div()
                .id(gpui::ElementId::Name(id.to_owned().into()))
                .mb(px(reveal::CODE_MARGIN))
                .px(px(reveal::CODE_PAD_X))
                .py(px(reveal::CODE_PAD_Y))
                .continuous(8.0)
                .bg(rgb(faint()))
                .font_family(mono())
                .text_size(px(reveal::CODE_FONT))
                .line_height(px(reveal::CODE_LINE))
                .flex()
                .items_start();
            let code = scroll.scroller(id, el, true).child(div().flex_none().min_w_full().flex().flex_col().children(
                text.lines().take(lines.map_or(usize::MAX, <[_]>::len)).enumerate().map(|(v, l)| {
                    div()
                        .whitespace_nowrap()
                        .when(lines.is_some() && reveal::code_line(v, line_alpha(lines, v)) <= 0.0, |el| el.opacity(0.0))
                        .child(if l.is_empty() { " ".to_owned() } else { reveal::expand_tabs(l).into_owned() })
                }),
            ));
            let (Some(lines), Some(f)) = (lines, fx) else { return code.into_any_element() };
            let cell = f.wraps.borrow().get(id).map_or(0.0, |w| w.cell);
            let masks = lines.iter().map(|(v, p)| (*v, reveal::code_line(*v, *p))).filter(|(_, p)| *p > 0.0 && *p < 1.0).map(|(v, p)| {
                let share = reveal::code_share(text.lines().nth(v).unwrap_or(""), cell);
                let [(a, ma), (b, mb)] = reveal::mask(p);
                #[allow(clippy::cast_precision_loss)]
                let top = reveal::CODE_PAD_Y + v as f32 * reveal::CODE_LINE;
                div().absolute().left(px(reveal::CODE_PAD_X)).right_0().top(px(top)).h(px(reveal::CODE_LINE)).bg(linear_gradient(
                    90.0,
                    linear_color_stop(hsla(faint(), ma), a * share),
                    linear_color_stop(hsla(faint(), mb), b * share),
                ))
            });
            let shown = reveal::code_box(line_alpha(Some(lines), 0));
            div()
                .relative()
                .flex()
                .flex_col()
                .when(shown < 1.0, |el| el.opacity(shown))
                .child(code)
                .children(masks)
                .child(code_cell(f, id))
                .into_any_element()
        }
        Block::List { start, items } => div()
            .pb(px(6.0))
            .flex()
            .flex_col()
            .children(items.iter().enumerate().map(|(i, blocks)| {
                let marker = start.map_or_else(|| "•".to_owned(), |s| format!("{}.", s + i as u64));
                // A revealed item's marker comes in as its first line starts sweeping.
                let shown = match fx {
                    Some(_) if blocks.is_empty() => 0.0,
                    Some(f) => f.lines.get(&format!("{id}-{i}-0")).map_or(1.0, |l| l.first().map_or(0.0, |(_, p)| reveal::sweep(0.0, *p))),
                    None => 1.0,
                };
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(16.0))
                            .text_color(rgb(theme::muted()))
                            .line_height(px(21.0))
                            .when(shown < 1.0, |el| el.opacity(shown))
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .children(blocks.iter().enumerate().map(|(j, b)| render_block(&format!("{id}-{i}-{j}"), b, fx, fg, scroll))),
                    )
            }))
            .into_any_element(),
        Block::Quote(blocks) => div()
            .mb(px(8.0))
            .pl(px(12.0))
            .border_l_2()
            .border_color(rgb(theme::border()))
            .text_color(rgb(theme::muted()))
            .children(blocks.iter().enumerate().map(|(i, b)| render_block(&format!("{id}-q{i}"), b, fx, theme::muted(), scroll)))
            .into_any_element(),
        Block::Rule => {
            div().my(px(10.0)).h(px(1.0)).bg(rgb(theme::border())).when(lines.is_some(), |el| el.opacity(line_alpha(lines, 0))).into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Fit, Tail};

    #[test]
    fn the_pill_and_following_come_from_the_scroll_position() {
        // Following: at the end, nothing to resume, nothing unseen.
        assert_eq!(Tail::of(true, 0.0, true), Tail { at_end: true, resume: false, unseen: false, pill: false });
        // Scrolled up with something new below: the pill, following stays off.
        assert_eq!(Tail::of(false, 240.0, true), Tail { at_end: false, resume: false, unseen: true, pill: true });
        // Scrolled up, nothing new yet: no pill.
        assert!(!Tail::of(false, 240.0, false).pill);
        // Back at the bottom (scrolled, or a group folding/remeasuring settled) before the list noticed:
        // no pill, follow again, and what was unseen is seen.
        assert_eq!(Tail::of(false, 0.0, true), Tail { at_end: true, resume: true, unseen: false, pill: false });
        assert_eq!(Tail::of(false, 2.5, true), Tail { at_end: true, resume: true, unseen: false, pill: false }, "a few px still counts");
        assert!(Tail::of(false, 4.0, true).pill, "past the threshold it's scrolled up");
    }

    #[test]
    fn narrow_panes_drop_secondary_items_in_order() {
        let wide = Fit::of(800.0);
        assert!(wide.plan && wide.provider && wide.tools && wide.model && wide.footer && wide.lead && wide.detail && wide.labels);
        assert_eq!(Fit::of(0.0), wide, "not laid out yet: everything");
        // Widest first: the plan, the provider, then the bar details, effort/image, labels, the model, the footer.
        let order = |f: Fit| [f.plan, f.provider, f.detail, f.tools, f.lead, f.labels, f.model, f.footer, f.send];
        let mut last = order(wide);
        for w in (60..800).rev().step_by(10) {
            #[allow(clippy::cast_precision_loss)]
            let now = order(Fit::of(w as f32));
            assert!(now.iter().zip(last).all(|(n, l)| !*n || l), "nothing comes back as the pane narrows ({w})");
            assert!(now.windows(2).all(|p| !p[0] || p[1]), "a later item never goes before an earlier one ({w})");
            last = now;
        }
        let split = Fit::of(190.0);
        assert!(!split.model && split.footer && !split.lead && split.pad < 20.0);
        let smallest = Fit::of(60.0);
        assert!(!smallest.footer && !smallest.send, "the smallest split keeps only the composer, without its circle");
    }
}
