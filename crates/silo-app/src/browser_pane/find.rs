//! Find in page (⌘F): a glass capsule over the page's top right, like the
//! terminal's, with the field, "3 of 12", previous, next and close. ↩ and
//! ⌘G go to the next match, ⇧↩ and ⇧⌘G to the previous; esc closes it and
//! clears the highlights. Chromium searches (`Browser::find`).

use gpui::{AppContext as _, div, prelude::*, px, rgb};

use super::BrowserPane;
use crate::surface::Glassy as _;
use crate::theme;

/// The bar's height and its round buttons' size (the terminal's).
const HEIGHT: f32 = 26.0;
const BUTTON: f32 = 20.0;

/// What to ask the engine for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FindCall {
    /// A new search, selecting the first match.
    Start(String),
    /// The next (`true`) or previous match of the current search.
    Step { forward: bool, text: String },
    /// End the search and clear its highlights.
    Stop,
}

/// The search as the bar shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FindState {
    query: String,
    /// The engine's last answer for `query`.
    result: Option<silo_web::FindResult>,
}

impl FindState {
    /// The field now says `text`.
    pub(crate) fn typed(&mut self, text: &str) -> Option<FindCall> {
        if text == self.query {
            return None;
        }
        text.clone_into(&mut self.query);
        self.result = None;
        Some(if text.is_empty() { FindCall::Stop } else { FindCall::Start(text.to_owned()) })
    }

    /// ↩ / ⇧↩, ⌘G / ⇧⌘G.
    pub(crate) fn step(&self, forward: bool) -> Option<FindCall> {
        (!self.query.is_empty()).then(|| FindCall::Step { forward, text: self.query.clone() })
    }

    pub(crate) fn found(&mut self, result: silo_web::FindResult) {
        if !self.query.is_empty() {
            self.result = Some(result);
        }
    }

    /// The page changed under the search: its count is unknown again.
    pub(crate) fn reset(&mut self) {
        self.result = None;
    }

    #[cfg(test)]
    fn query(&self) -> &str {
        &self.query
    }

    /// "3 of 12", "12" while none is selected, "No matches", or nothing
    /// before the first answer.
    pub(crate) fn label(&self) -> String {
        let Some(r) = self.result.filter(|_| !self.query.is_empty()) else { return String::new() };
        match (r.count, r.active) {
            (0, _) if r.done => "No matches".into(),
            (0, _) => String::new(),
            (count, 0) => count.to_string(),
            (count, active) => format!("{} of {count}", active.min(count)),
        }
    }
}

/// ⌘F opens, ⌘G and ⇧⌘G step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindChord {
    Open,
    Next,
    Previous,
}

pub(crate) fn find_chord(key: &str, m: gpui::Modifiers) -> Option<FindChord> {
    if !m.platform || m.control || m.alt {
        return None;
    }
    match key {
        "f" if !m.shift => Some(FindChord::Open),
        "g" if m.shift => Some(FindChord::Previous),
        "g" => Some(FindChord::Next),
        _ => None,
    }
}

/// The open bar: its field and the search.
pub(crate) struct FindBar {
    field: gpui::Entity<crate::text_field::TextField>,
    state: FindState,
    _events: gpui::Subscription,
}

impl BrowserPane {
    /// Whether the find bar's field has focus.
    pub(super) fn find_focused(&self, window: &gpui::Window, cx: &gpui::App) -> bool {
        self.find.as_ref().is_some_and(|f| f.field.read(cx).focus.is_focused(window))
    }

    pub(super) fn find_chord(&mut self, chord: FindChord, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        match chord {
            FindChord::Open => self.open_find(window, cx),
            FindChord::Next => self.find_step(true, cx),
            FindChord::Previous => self.find_step(false, cx),
        }
    }

    /// ⌘F: open the bar, or select its text again.
    pub(super) fn open_find(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if let Some(find) = &self.find {
            let field = find.field.clone();
            field.update(cx, |f, cx| f.select_all(cx));
            let focus = field.read(cx).focus.clone();
            window.focus(&focus, cx);
            return;
        }
        let field = cx.new(|cx| crate::text_field::TextField::new("Find in page", cx).on_glass());
        let events = cx.subscribe_in(&field, window, |this, field, event, window, cx| {
            use crate::text_field::FieldEvent;
            match event {
                FieldEvent::Changed(text) => {
                    let call = this.find.as_mut().and_then(|f| f.state.typed(text));
                    this.find_call(call, cx);
                }
                FieldEvent::Submit => {
                    let back = field.read(cx).submit_mods.shift;
                    this.find_step(!back, cx);
                }
                FieldEvent::Up => this.find_step(false, cx),
                FieldEvent::Down => this.find_step(true, cx),
                FieldEvent::Cancel => this.close_find(window, cx),
            }
        });
        let focus = field.read(cx).focus.clone();
        window.focus(&focus, cx);
        self.find = Some(FindBar { field, state: FindState::default(), _events: events });
        cx.notify();
    }

    /// Close the bar, clearing the highlights, and give the page its focus back.
    pub(super) fn close_find(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if self.find.take().is_none() {
            return;
        }
        if let Some(page) = &self.page {
            page.stop_finding();
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn find_step(&mut self, forward: bool, cx: &mut gpui::Context<Self>) {
        let call = self.find.as_ref().and_then(|f| f.state.step(forward));
        self.find_call(call, cx);
    }

    fn find_call(&mut self, call: Option<FindCall>, cx: &mut gpui::Context<Self>) {
        let (Some(call), Some(page)) = (call, &self.page) else { return };
        match call {
            FindCall::Start(text) => page.find(&text, true, false),
            FindCall::Step { forward, text } => page.find(&text, forward, true),
            FindCall::Stop => page.stop_finding(),
        }
        cx.notify();
    }

    /// `Event::Find`.
    pub(super) fn find_result(&mut self, result: silo_web::FindResult) {
        if let Some(find) = self.find.as_mut() {
            find.state.found(result);
        }
    }

    /// A new document: its matches are not the old one's.
    pub(super) fn find_navigated(&mut self) {
        if let Some(find) = self.find.as_mut() {
            find.state.reset();
        }
    }

    pub(super) fn find_bar(&self, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        let find = self.find.as_ref()?;
        let count = find.state.label();
        let _glass = crate::surface::enter();
        let muted = crate::surface::current().muted;
        let button = |id: &'static str, icon: &'static str, tip: &'static str| {
            div()
                .id(id)
                .flex_none()
                .size(px(BUTTON))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .hover(|s| s.bg(theme::row_fill()))
                .tooltip(crate::tooltip::Tooltip::text(tip))
                .child(crate::assets::icon(icon, 11.0, muted))
        };
        Some(
            div()
                .id("browser-find")
                .absolute()
                .top(px(6.0))
                .right(px(6.0))
                .w(px(252.0))
                .h(px(HEIGHT))
                .pl(px(10.0))
                .pr(px((HEIGHT - BUTTON) / 2.0))
                .flex()
                .items_center()
                .gap(px(2.0))
                .glass(HEIGHT / 2.0)
                .rounded_full()
                .occlude()
                .font_family(theme::ui_font())
                .text_size(px(12.0))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(div().flex_none().mr(px(4.0)).child(crate::assets::icon("search", 11.0, muted)))
                .child(div().flex_1().min_w_0().child(find.field.clone()))
                .child(div().flex_none().mx(px(4.0)).text_size(px(11.0)).text_color(rgb(muted)).child(count))
                .child(button("find-previous", "chevron-up", "Previous  ⇧↩ ⇧⌘G").on_click(cx.listener(|this, _, _, cx| this.find_step(false, cx))))
                .child(button("find-next", "chevron-down", "Next  ↩ ⌘G").on_click(cx.listener(|this, _, _, cx| this.find_step(true, cx))))
                .child(button("find-close", "close", "Close  esc").on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(count: u32, active: u32, done: bool) -> silo_web::FindResult {
        silo_web::FindResult { count, active, done }
    }

    #[test]
    fn typing_starts_and_clearing_stops() {
        let mut s = FindState::default();
        assert_eq!(s.step(true), None, "nothing to step through");
        assert_eq!(s.typed("ab"), Some(FindCall::Start("ab".into())));
        assert_eq!(s.typed("ab"), None, "unchanged");
        assert_eq!(s.step(false), Some(FindCall::Step { forward: false, text: "ab".into() }));
        assert_eq!(s.typed(""), Some(FindCall::Stop));
        assert_eq!(s.query(), "");
        assert_eq!(s.step(true), None);
    }

    #[test]
    fn counts_follow_the_engine() {
        let mut s = FindState::default();
        s.found(result(3, 1, true));
        assert_eq!(s.label(), "", "no search: answers are ignored");
        s.typed("x");
        assert_eq!(s.label(), "", "before the first answer");
        s.found(result(0, 0, false));
        assert_eq!(s.label(), "", "still counting");
        s.found(result(12, 0, false));
        assert_eq!(s.label(), "12");
        s.found(result(12, 3, true));
        assert_eq!(s.label(), "3 of 12");
        s.found(result(12, 4, true));
        assert_eq!(s.label(), "4 of 12", "next");
        s.typed("xy");
        assert_eq!(s.label(), "", "a new query forgets the old count");
        s.found(result(0, 0, true));
        assert_eq!(s.label(), "No matches");
        s.found(result(2, 1, true));
        s.reset();
        assert_eq!(s.label(), "", "navigated");
    }

    #[test]
    fn chords_open_and_step() {
        let cmd = gpui::Modifiers { platform: true, ..Default::default() };
        let shift = gpui::Modifiers { shift: true, ..cmd };
        assert_eq!(find_chord("f", cmd), Some(FindChord::Open));
        assert_eq!(find_chord("g", cmd), Some(FindChord::Next));
        assert_eq!(find_chord("g", shift), Some(FindChord::Previous));
        assert_eq!(find_chord("f", shift), None);
        assert_eq!(find_chord("f", gpui::Modifiers { control: true, ..cmd }), None, "full screen");
        assert_eq!(find_chord("f", gpui::Modifiers::default()), None);
    }
}
