//! Settings › Voice › Vocabulary and Replacements: words to listen for,
//! and "heard → write" rules, each added from a field.

use gpui::{AnyElement, Context, Entity, IntoElement, Subscription, Window, div, prelude::*, px, rgb};
use silo_voice::{VoicePrefs, prefs::Replacement};

use super::{SettingsEvent, SettingsPanel};
use crate::{
    assets::icon,
    text_field::{FieldEvent, TextField},
    theme::{self, Continuous as _},
    voice::settings::VoiceEvent,
};

/// The page's text fields: a word to add, a replacement's two sides.
pub(super) struct VoiceFields {
    word: Entity<TextField>,
    heard: Entity<TextField>,
    write: Entity<TextField>,
}

/// `words` (comma-separated) added to `vocabulary`, each once in any case.
fn add_words(vocabulary: &mut Vec<String>, words: &str) -> bool {
    let before = vocabulary.len();
    for word in words.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        if !vocabulary.iter().any(|v| v.eq_ignore_ascii_case(word)) {
            vocabulary.push(word.to_owned());
        }
    }
    vocabulary.len() > before
}

/// The rule `heard → write`, replacing one for the same words.
fn add_rule(rules: &mut Vec<Replacement>, heard: &str, write: &str) -> bool {
    let (heard, write) = (heard.split_whitespace().collect::<Vec<_>>().join(" "), write.trim());
    if heard.is_empty() || write.is_empty() || heard == write {
        return false;
    }
    rules.retain(|r| !r.heard.eq_ignore_ascii_case(&heard));
    rules.push(Replacement { heard, write: write.to_owned() });
    true
}

impl VoiceFields {
    pub(super) fn new(subscriptions: &mut Vec<Subscription>, window: &mut Window, cx: &mut Context<SettingsPanel>) -> Self {
        let field = |placeholder: &'static str, cx: &mut Context<SettingsPanel>| cx.new(|cx| TextField::new(placeholder, cx).on_glass());
        let (word, heard, write) = (field("Add a word or name", cx), field("Heard", cx), field("Written", cx));
        subscriptions.push(cx.subscribe(&word, |this, field, event, cx| match event {
            FieldEvent::Submit => {
                let mut prefs = this.snapshot.voice.prefs.clone();
                if add_words(&mut prefs.vocabulary, &field.read(cx).text) {
                    cx.emit(SettingsEvent::Voice(VoiceEvent::Set(prefs)));
                }
                field.update(cx, |f, cx| f.replace_text("", cx));
            }
            FieldEvent::Cancel => field.update(cx, |f, cx| f.replace_text("", cx)),
            _ => {}
        }));
        for side in [heard.clone(), write.clone()] {
            subscriptions.push(cx.subscribe_in(&side, window, |this, _, event, window, cx| match event {
                FieldEvent::Submit => this.add_replacement(window, cx),
                FieldEvent::Cancel => {
                    for f in [&this.voice_fields.heard, &this.voice_fields.write] {
                        f.update(cx, |f, cx| f.replace_text("", cx));
                    }
                }
                _ => {}
            }));
        }
        Self { word, heard, write }
    }
}

impl SettingsPanel {
    /// Add the typed rule, or move on to the side still empty.
    fn add_replacement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let VoiceFields { heard, write, .. } = &self.voice_fields;
        let (h, w) = (heard.read(cx).text.clone(), write.read(cx).text.clone());
        if h.trim().is_empty() || w.trim().is_empty() {
            let empty = if h.trim().is_empty() { heard } else { write };
            let focus = empty.read(cx).focus.clone();
            focus.focus(window, cx);
            return;
        }
        let mut prefs = self.snapshot.voice.prefs.clone();
        if add_rule(&mut prefs.replacements, &h, &w) {
            cx.emit(SettingsEvent::Voice(VoiceEvent::Set(prefs)));
        }
        for f in [heard.clone(), write.clone()] {
            f.update(cx, |f, cx| f.replace_text("", cx));
        }
        let focus = heard.read(cx).focus.clone();
        focus.focus(window, cx);
    }

    /// A removable chip: `label` and an × that emits `prefs`.
    fn voice_chip(id: (&'static str, usize), label: String, prefs: VoicePrefs, cx: &mut Context<Self>) -> AnyElement {
        div()
            .h(px(24.0))
            .pl(px(9.0))
            .pr(px(3.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .continuous(7.0)
            .border_1()
            .border_color(rgb(theme::border()))
            .bg(theme::control_fill())
            .text_size(px(12.5))
            .child(div().max_w(px(220.0)).truncate().child(label))
            .child(
                div()
                    .id(id)
                    .size(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .continuous(5.0)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_fill()))
                    .child(icon("close", 10.0, theme::muted()))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(SettingsEvent::Voice(VoiceEvent::Set(prefs.clone()))))),
            )
            .into_any_element()
    }

    fn voice_chips(chips: Vec<AnyElement>) -> AnyElement {
        div().px(px(14.0)).py(px(10.0)).flex().flex_wrap().gap(px(6.0)).children(chips).into_any_element()
    }

    /// The Vocabulary and Replacements groups.
    pub(super) fn voice_words(&self, narrow: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let prefs = &self.snapshot.voice.prefs;
        let fields = &self.voice_fields;
        let mut words = vec![Self::row(
            "Words to listen for",
            Some("Names and terms spelled your way, like nextest".into()),
            Some(Self::field_box(&fields.word, 180.0)),
            narrow,
        )];
        let chips: Vec<_> = (prefs.vocabulary.iter().enumerate())
            .map(|(i, word)| {
                let mut next = prefs.clone();
                next.vocabulary.remove(i);
                Self::voice_chip(("settings-voice-word", i), word.clone(), next, cx)
            })
            .collect();
        if !chips.is_empty() {
            words.push(Self::voice_chips(chips));
        }
        let pair = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(Self::field_box(&fields.heard, 110.0))
            .child("→")
            .child(Self::field_box(&fields.write, 110.0));
        let mut rules = vec![Self::row("Replace", Some("Whole words, in any case".into()), Some(pair.into_any_element()), narrow)];
        let chips: Vec<_> = (prefs.replacements.iter().enumerate())
            .map(|(i, rule)| {
                let mut next = prefs.clone();
                next.replacements.remove(i);
                Self::voice_chip(("settings-voice-rule", i), format!("{} → {}", rule.heard, rule.write), next, cx)
            })
            .collect();
        if !chips.is_empty() {
            rules.push(Self::voice_chips(chips));
        }
        vec![self.group(Some("Vocabulary"), words), self.group(Some("Replacements"), rules)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_rules_are_added_once() {
        let mut words = vec!["Silo".to_owned()];
        assert!(add_words(&mut words, " nextest, silo ,, otto "));
        assert_eq!(words, ["Silo", "nextest", "otto"]);
        assert!(!add_words(&mut words, "NEXTEST"));
        let mut rules = Vec::new();
        assert!(add_rule(&mut rules, " auto ", "otto"));
        assert!(add_rule(&mut rules, "Auto", "Otto"), "the same words again replace the rule");
        assert!(!add_rule(&mut rules, "x", " ") && !add_rule(&mut rules, "same", "same"));
        assert_eq!(rules, [Replacement { heard: "Auto".into(), write: "Otto".into() }]);
        assert!(add_rule(&mut rules, "get   hub", "GitHub"));
        assert_eq!(rules[1].heard, "get hub");
    }
}
