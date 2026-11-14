//! A question with fields: an MCP server's elicitation (`docs/OTTO.md` §5).
//! One input per field (text, number, a switch, a choice), required marks,
//! defaults prefilled, checked against the form before Submit; Decline and
//! Cancel answer without values.

use gpui::{AnyElement, AppContext as _, Context, Div, Entity, FontWeight, SharedString, Subscription, div, prelude::*, px, rgb};
use otto_proto::{FieldKind, FormField, PendingQuestion, QuestionAction, QuestionForm};
use serde_json::{Map, Value};

use super::{AgentPane, settings_agents::AgentsSettings as Ui};
use crate::{
    controls::Toggle,
    text_field::{FieldEvent, TextField},
    theme::{self, Continuous as _},
};

/// One field's input as edited.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Input {
    /// Text, number and integer fields: the typed text.
    Text(String),
    Bool(bool),
    /// The chosen option's index.
    Choice(Option<usize>),
}

/// The card's values (the view-model, without entities).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct FormDraft {
    pub form: QuestionForm,
    pub inputs: Vec<Input>,
}

impl FormDraft {
    /// Inputs prefilled with the fields' defaults; a switch without one is off.
    pub(super) fn new(form: &QuestionForm) -> Self {
        let inputs = form
            .fields
            .iter()
            .map(|f| match f.kind {
                FieldKind::Boolean => Input::Bool(f.default.as_ref().and_then(Value::as_bool).unwrap_or(false)),
                FieldKind::Choice => {
                    let default = f.default.as_ref().and_then(Value::as_str);
                    Input::Choice(default.and_then(|d| f.choices.iter().position(|c| c.value == d)))
                }
                _ => Input::Text(f.default_text()),
            })
            .collect();
        Self { form: form.clone(), inputs }
    }

    pub(super) fn set_text(&mut self, i: usize, text: String) {
        if let Some(Input::Text(t)) = self.inputs.get_mut(i) {
            *t = text;
        }
    }

    pub(super) fn toggle(&mut self, i: usize, on: bool) {
        if let Some(Input::Bool(b)) = self.inputs.get_mut(i) {
            *b = on;
        }
    }

    /// Pick option `choice`; picking an optional field's chosen option again clears it.
    pub(super) fn choose(&mut self, i: usize, choice: usize) {
        let required = self.form.fields.get(i).is_some_and(|f| f.required);
        if let Some(Input::Choice(c)) = self.inputs.get_mut(i) {
            *c = if *c == Some(choice) && !required { None } else { Some(choice) };
        }
    }

    /// The values to send, checked against the form; the problems otherwise.
    pub(super) fn content(&self) -> Result<Map<String, Value>, String> {
        let mut content = Map::new();
        let mut problems = Vec::new();
        for (field, input) in self.form.fields.iter().zip(&self.inputs) {
            let value = match input {
                Input::Text(text) => match field.parse(text) {
                    Ok(value) => value,
                    Err(problem) => {
                        problems.push(problem);
                        continue;
                    }
                },
                Input::Bool(on) => Some(Value::Bool(*on)),
                Input::Choice(choice) => choice.and_then(|c| field.choices.get(c)).map(|c| Value::String(c.value.clone())),
            };
            if let Some(value) = value {
                content.insert(field.name.clone(), value);
            }
        }
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        self.form.validate(&content)
    }

    /// Every required field has a value (Submit is enabled).
    pub(super) fn complete(&self) -> bool {
        self.form.fields.iter().zip(&self.inputs).all(|(f, input)| {
            !f.required
                || match input {
                    Input::Text(t) => !t.trim().is_empty(),
                    Input::Bool(_) => true,
                    Input::Choice(c) => c.is_some(),
                }
        })
    }
}

/// The hint under a text input: what it takes.
pub(super) fn placeholder(field: &FormField) -> &'static str {
    match (field.kind, field.format.as_deref()) {
        (FieldKind::Integer, _) => "Whole number",
        (FieldKind::Number, _) => "Number",
        (_, Some("email")) => "name@example.com",
        (_, Some("uri")) => "https://",
        (_, Some("date")) => "YYYY-MM-DD",
        (_, Some("date-time")) => "YYYY-MM-DDThh:mm",
        _ => "",
    }
}

/// The card's live state for the question it shows.
pub(super) struct FormState {
    pub question_id: String,
    pub draft: FormDraft,
    /// Text inputs, by field (`None` for switches and choices).
    fields: Vec<Option<Entity<TextField>>>,
    pub error: Option<String>,
    pub sending: bool,
    _subs: Vec<Subscription>,
}

impl AgentPane {
    /// The state for `q`, made fresh when the card shows a new question.
    fn form_state(&mut self, q: &PendingQuestion, form: &QuestionForm, cx: &mut Context<Self>) {
        if self.form.as_ref().is_some_and(|f| f.question_id == q.question_id) {
            return;
        }
        let draft = FormDraft::new(form);
        let mut subs = Vec::new();
        let fields = form
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let Input::Text(text) = &draft.inputs[i] else { return None };
                let entity = cx.new(|cx| {
                    let mut f = TextField::new(placeholder(field), cx);
                    f.replace_text(text.clone(), cx);
                    f
                });
                subs.push(cx.subscribe(&entity, move |this: &mut Self, _, event: &FieldEvent, cx| match event {
                    FieldEvent::Changed(text) => {
                        if let Some(state) = &mut this.form {
                            state.draft.set_text(i, text.clone());
                            state.error = None;
                        }
                        cx.notify();
                    }
                    FieldEvent::Submit => this.submit_form(cx),
                    FieldEvent::Cancel | FieldEvent::Up | FieldEvent::Down => {}
                }));
                Some(entity)
            })
            .collect();
        self.form = Some(FormState { question_id: q.question_id.clone(), draft, fields, error: None, sending: false, _subs: subs });
    }

    /// Submit the shown form: its values if they fit, else the problem under the fields.
    pub(super) fn submit_form(&mut self, cx: &mut Context<Self>) {
        let live = self.live_form();
        let Some(state) = self.form.as_mut().filter(|f| live.as_ref() == Some(&f.question_id)) else { return };
        if state.sending {
            return;
        }
        match state.draft.content() {
            Ok(content) => {
                let summary = state.draft.form.summary(&content);
                let id = state.question_id.clone();
                self.answer_form(id, QuestionAction::Accept, summary, Some(content), cx);
            }
            Err(problem) => state.error = Some(problem),
        }
        cx.notify();
    }

    /// The shown card's question id when it has fields.
    pub(super) fn live_form(&self) -> Option<String> {
        match self.live_card() {
            Some((_, _, super::Card::Question(q))) if q.form.is_some() => Some(q.question_id),
            _ => None,
        }
    }

    /// Answer form question `question` (the shown session's or a waiting child's).
    pub(super) fn answer_form(
        &mut self,
        question: String,
        action: QuestionAction,
        answer: String,
        content: Option<Map<String, Value>>,
        cx: &mut Context<Self>,
    ) {
        let child = self.subs.waits.iter().find(|w| w.questions.iter().any(|q| q.question_id == question)).map(|w| w.id.clone());
        let Some(id) = child.or_else(|| self.session_id.clone()) else { return };
        if let Some(state) = self.form.as_mut().filter(|f| f.question_id == question) {
            (state.sending, state.error) = (true, None);
        }
        let asked = question.clone();
        self.run(
            cx,
            move |c| async move { c.question_reply(&id, &question, action, answer, content).await },
            move |this, result, cx| {
                let state = this.form.as_mut().filter(|f| f.question_id == asked);
                match (result, state) {
                    // Values otto refused (checked against the form there too): the card stays.
                    (Err(error), Some(state)) => (state.sending, state.error) = (false, Some(error)),
                    (Err(error), None) => this.notice = Some(error),
                    (Ok(_), _) => {
                        this.subs.waits.iter_mut().for_each(|w| w.questions.retain(|q| q.question_id != asked));
                    }
                }
                cx.notify();
            },
        );
    }

    /// The form under the card's question: one row per field, the problem, then the buttons.
    pub(super) fn render_form(
        &mut self,
        card: Div,
        q: &PendingQuestion,
        form: &QuestionForm,
        active: bool,
        keys: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        self.form_state(q, form, cx);
        let Some(state) = &self.form else { return card };
        let enabled = active && !state.sending;
        let muted = theme::muted();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (i, (field, input)) in form.fields.iter().zip(&state.draft.inputs).enumerate() {
            let label = div()
                .flex()
                .items_baseline()
                .gap(px(4.0))
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .child(field.label().to_owned())
                .when(field.required, |el| el.child(div().text_color(rgb(theme::danger())).child("*")));
            let control: AnyElement = match input {
                Input::Text(_) => match &state.fields[i] {
                    Some(entity) => Ui::field_box(entity, 320.0),
                    None => div().into_any_element(),
                },
                Input::Bool(on) => {
                    let entity = cx.entity().downgrade();
                    Toggle::new(SharedString::from(format!("form-{}-{i}", q.question_id)), SharedString::default(), *on)
                        .on_toggle(move |on, _, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                if let Some(state) = &mut this.form {
                                    state.draft.toggle(i, on);
                                    state.error = None;
                                }
                                cx.notify();
                            });
                        })
                        .into_any_element()
                }
                Input::Choice(chosen) => div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.0))
                    .children(field.choices.iter().enumerate().map(|(c, choice)| {
                        let selected = *chosen == Some(c);
                        div()
                            .id(SharedString::from(format!("form-{}-{i}-{c}", q.question_id)))
                            .px(px(10.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .capsule()
                            .text_size(px(12.0))
                            .map(|el| {
                                if selected {
                                    el.bg(rgb(theme::foreground())).text_color(rgb(theme::background()))
                                } else {
                                    el.bg(theme::control_fill())
                                }
                            })
                            .child(choice.label().to_owned())
                            .when(enabled, |el| {
                                el.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(state) = &mut this.form {
                                        state.draft.choose(i, c);
                                        state.error = None;
                                    }
                                    cx.notify();
                                }))
                            })
                    }))
                    .into_any_element(),
            };
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(label)
                    .when_some(field.description.clone().filter(|d| !d.trim().is_empty()), |el, d| {
                        el.child(div().text_size(px(11.5)).text_color(rgb(muted)).child(d))
                    })
                    .child(control)
                    .into_any_element(),
            );
        }
        let can_submit = enabled && state.draft.complete();
        let error = state.error.clone();
        let id = q.question_id.clone();
        let button = |key: &'static str, label: &'static str, hint: &'static str, primary: bool, on: bool| {
            div()
                .id(SharedString::from(format!("form-{key}-{id}")))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(12.0))
                .h(px(28.0))
                .capsule()
                .text_size(px(12.5))
                .map(|el| if primary { el.bg(rgb(theme::foreground())).text_color(rgb(theme::background())) } else { el.bg(theme::control_fill()) })
                .when(!on, |el| el.opacity(0.5))
                .when(on, |el| el.cursor_pointer().hover(|s| s.opacity(0.88)).active(|s| s.scale(0.96)))
                .child(label)
                .when(keys && !hint.is_empty(), |el| {
                    el.child(div().text_size(px(11.0)).opacity(0.6).font_family(super::settings_agents::mono()).child(hint))
                })
        };
        let answer = |action: QuestionAction| {
            let id = id.clone();
            cx.listener(move |this: &mut Self, _: &gpui::ClickEvent, _, cx| {
                let answer = if action == QuestionAction::Decline { "Declined" } else { "Cancelled" };
                this.answer_form(id.clone(), action, answer.into(), None, cx);
            })
        };
        card.when(!rows.is_empty(), |el| el.child(div().flex().flex_col().gap(px(10.0)).children(rows))).children(error.map(Ui::error_line)).child(
            div()
                .flex()
                .gap(px(6.0))
                .child(
                    button("submit", "Submit", "⏎", true, can_submit)
                        .when(can_submit, |el| el.on_click(cx.listener(|this, _, _, cx| this.submit_form(cx)))),
                )
                .child(button("decline", "Decline", "", false, enabled).when(enabled, |el| el.on_click(answer(QuestionAction::Decline))))
                .child(button("cancel", "Cancel", "esc", false, enabled).when(enabled, |el| el.on_click(answer(QuestionAction::Cancel)))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form() -> QuestionForm {
        QuestionForm::from_schema(&json!({"type":"object","properties":{
            "name":{"type":"string","title":"Name","default":"Ann"},
            "age":{"type":"integer","minimum":0},
            "admin":{"type":"boolean"},
            "notify":{"type":"boolean","default":true},
            "size":{"type":"string","enum":["s","m"],"enumNames":["Small","Medium"],"default":"m"},
            "team":{"type":"string","enum":["a","b"]}
        },"required":["name","team"]}))
        .unwrap()
    }

    #[test]
    fn defaults_prefill_the_inputs() {
        let draft = FormDraft::new(&form());
        assert_eq!(
            draft.inputs,
            [
                Input::Text("Ann".into()),
                Input::Text(String::new()),
                Input::Bool(false),
                Input::Bool(true),
                Input::Choice(Some(1)),
                Input::Choice(None),
            ]
        );
        assert!(!draft.complete(), "team is required and unset");
    }

    #[test]
    fn submit_checks_the_values() {
        let mut draft = FormDraft::new(&form());
        assert!(draft.content().unwrap_err().contains("team is required"));
        draft.choose(5, 0);
        assert!(draft.complete());
        draft.set_text(1, "two".into());
        assert!(draft.content().unwrap_err().contains("whole number"));
        draft.set_text(1, "-1".into());
        assert!(draft.content().unwrap_err().contains("at least 0"));
        draft.set_text(1, "7".into());
        draft.toggle(2, true);
        assert_eq!(Value::Object(draft.content().unwrap()), json!({"name":"Ann","age":7,"admin":true,"notify":true,"size":"m","team":"a"}));
        draft.set_text(0, "  ".into());
        assert!(!draft.complete());
        assert!(draft.content().unwrap_err().contains("Name is required"));
    }

    #[test]
    fn an_optional_choice_clears_but_a_required_one_stays() {
        let mut draft = FormDraft::new(&form());
        draft.choose(4, 1);
        assert_eq!(draft.inputs[4], Input::Choice(None));
        draft.choose(5, 1);
        draft.choose(5, 1);
        assert_eq!(draft.inputs[5], Input::Choice(Some(1)));
    }

    #[test]
    fn no_fields_submits_empty_content() {
        let draft = FormDraft::new(&QuestionForm::default());
        assert!(draft.complete());
        assert!(draft.content().unwrap().is_empty());
    }

    #[test]
    fn placeholders_name_the_format() {
        let form = QuestionForm::from_schema(&json!({"properties":{"e":{"type":"string","format":"email"},"n":{"type":"number"}}})).unwrap();
        assert_eq!(placeholder(&form.fields[0]), "name@example.com");
        assert_eq!(placeholder(&form.fields[1]), "Number");
    }
}
