//! Settings › Voice, and the microphone row of Settings › Permissions.

use gpui::{AnyElement, Context, IntoElement, SharedString, div, prelude::*, px, rgb};
use silo_voice::{
    EngineKind, Target, VoicePrefs,
    prefs::{FormatLists, language_name},
};

use super::{ButtonKind, SettingsEvent, SettingsPanel};
use crate::{
    assets::icon,
    context_menu::{Item, Menu},
    controls::{Segmented, Toggle},
    settings_table::SettingId,
    theme::{self, Continuous as _},
    voice::{
        fetch::{Fetch, percent},
        settings::{EngineRow, MicAccess, VoiceEvent},
    },
};

fn set(prefs: VoicePrefs) -> SettingsEvent {
    SettingsEvent::Voice(VoiceEvent::Set(prefs))
}

/// What an engine's row says about its model.
fn model_status(row: &EngineRow) -> String {
    let model = row.kind.model();
    match &row.fetch {
        _ if !row.available => "Not available on this Mac".into(),
        Some(Fetch::Running { done, total }) => match percent(*done, *total) {
            Some(p) => format!("Downloading… {p}%"),
            None => "Downloading…".into(),
        },
        Some(Fetch::Failed(error)) => error.clone(),
        None if row.installed => format!("Downloaded · {}", model.size()),
        None => format!("{} · downloads on first use", model.size()),
    }
}

impl SettingsPanel {
    /// A switch that edits the voice preferences.
    fn voice_toggle(&self, id: &'static str, label: &'static str, on: bool, change: fn(&mut VoicePrefs, bool), cx: &mut Context<Self>) -> AnyElement {
        let panel = cx.entity().downgrade();
        let prefs = self.snapshot.voice.prefs.clone();
        Toggle::new(id, label, on)
            .on_toggle(move |on, _, cx| {
                let mut prefs = prefs.clone();
                change(&mut prefs, on);
                let _ = panel.update(cx, |_, cx| cx.emit(set(prefs)));
            })
            .into_any_element()
    }

    /// A value with a chevron opening a menu of `items` under the pointer.
    fn voice_choice(id: &'static str, value: SharedString, items: Vec<Item<SettingsEvent>>, cx: &mut Context<Self>) -> AnyElement {
        let items = std::rc::Rc::new(std::cell::RefCell::new(Some(items)));
        div()
            .id(id)
            .h(px(26.0))
            .max_w(px(260.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .continuous(7.0)
            .border_1()
            .border_color(rgb(theme::border()))
            .bg(theme::control_fill())
            .hover(|s| s.bg(theme::row_fill()))
            .cursor_pointer()
            .text_size(px(12.5))
            .child(div().min_w_0().truncate().child(value))
            .child(icon("chevron", 10.0, theme::muted()))
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(items) = items.borrow_mut().take() {
                    this.voice_menu = Some(Menu::new(window.mouse_position(), items));
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    fn voice_engine_row(&self, row: &EngineRow, index: usize, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        let kind = row.kind;
        let action = match &row.fetch {
            _ if !row.available => None,
            Some(Fetch::Running { .. }) => None,
            None if row.installed => Some(Self::button(
                ("settings-voice-delete", index),
                "Delete",
                ButtonKind::Secondary,
                move |_, cx| cx.emit(SettingsEvent::Voice(VoiceEvent::Delete(kind))),
                cx,
            )),
            _ => Some(Self::emit_button(("settings-voice-download", index), "Download", SettingsEvent::Voice(VoiceEvent::Download(kind)), cx)),
        };
        let description = format!("{}. {}", kind.summary(), model_status(row));
        Self::row(kind.label(), Some(description.into()), action, narrow)
    }

    pub(super) fn voice_page(&self, narrow: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let v = &self.snapshot.voice;
        let prefs = &v.prefs;
        let mut out = Vec::new();
        let mut typing = vec![Self::row(
            SettingId::VoiceTyping.label(),
            Some("Hold Fn to dictate, or press it twice to keep listening. Esc cancels.".into()),
            Some(self.voice_toggle("settings-voice-on", "Voice typing", prefs.on, |p, on| p.on = on, cx)),
            false,
        )];
        if prefs.on && v.globe_disabled {
            typing.push(Self::row(
                "The 🌐 key is turned off",
                Some("Set Keyboard › Modifier Keys › 🌐 Globe key back to Globe".into()),
                Some(Self::emit_button("settings-voice-globe", "Open Keyboard Settings", SettingsEvent::Voice(VoiceEvent::OpenKeyboardSettings), cx)),
                narrow,
            ));
        }
        if prefs.on && v.fn_conflict {
            typing.push(Self::row(
                "macOS uses the 🌐 key",
                Some("Set Keyboard › “Press 🌐 key to” to Do Nothing".into()),
                Some(Self::emit_button(
                    "settings-voice-keyboard",
                    "Open Keyboard Settings",
                    SettingsEvent::Voice(VoiceEvent::OpenKeyboardSettings),
                    cx,
                )),
                narrow,
            ));
        }
        out.push(self.group(Some("Voice typing"), typing));

        let panel = cx.entity().downgrade();
        let engine_prefs = prefs.clone();
        let selected = EngineKind::ALL.iter().position(|e| *e == prefs.engine).unwrap_or_default();
        let engine = Segmented::new("settings-voice-engine", EngineKind::ALL.map(EngineKind::label), selected)
            .segment_width(96.0)
            .on_select(move |index, _, cx| {
                let mut prefs = engine_prefs.clone();
                prefs.engine = EngineKind::ALL.get(index).copied().unwrap_or_default();
                let _ = panel.update(cx, |_, cx| cx.emit(set(prefs)));
            })
            .into_any_element();
        let mut engines = vec![Self::row(SettingId::VoiceEngine.label(), None, Some(engine), narrow)];
        engines.extend(v.engines.iter().enumerate().map(|(i, row)| self.voice_engine_row(row, i, narrow, cx)));
        out.push(self.group(Some("Engine"), engines));

        let languages = std::iter::once(&("", "Detect"))
            .chain(prefs.engine.languages())
            .map(|(code, name)| {
                let mut next = prefs.clone();
                next.language = (*code).to_owned();
                Item::action(*name, "", set(next)).checked(prefs.engine.language(&prefs.language).unwrap_or("") == *code)
            })
            .collect();
        let language = language_name(prefs.engine.language(&prefs.language).unwrap_or(""));
        let microphones = std::iter::once(String::new())
            .chain(v.microphones.iter().cloned())
            .map(|name| {
                let label = if name.is_empty() { "System default".to_owned() } else { name.clone() };
                let checked = name == prefs.microphone;
                let mut next = prefs.clone();
                next.microphone = name;
                Item::action(label, "", set(next)).checked(checked)
            })
            .collect();
        let microphone = if prefs.microphone.is_empty() { "System default".to_owned() } else { prefs.microphone.clone() };
        out.push(self.group(
            Some("Input"),
            vec![
                Self::row("Language", None, Some(Self::voice_choice("settings-voice-language", language.into(), languages, cx)), narrow),
                Self::row("Microphone", None, Some(Self::voice_choice("settings-voice-mic", microphone.into(), microphones, cx)), narrow),
            ],
        ));

        out.extend(self.voice_words(narrow, cx));

        let lists = FormatLists::TARGETS
            .into_iter()
            .enumerate()
            .map(|(i, target)| {
                let panel = cx.entity().downgrade();
                let prefs = prefs.clone();
                let toggle = Toggle::new(("settings-voice-lists", i), target.label(), prefs.format_lists.get(target)).on_toggle(move |on, _, cx| {
                    let mut prefs = prefs.clone();
                    prefs.format_lists.set(target, on);
                    let _ = panel.update(cx, |_, cx| cx.emit(set(prefs)));
                });
                Self::row(target.label(), None, Some(toggle.into_any_element()), false)
            })
            .collect();
        out.push(self.group(Some("Format lists"), lists));
        out.push(self.group(
            Some("Polish"),
            vec![Self::row(
                "Polish with AI",
                Some("Sends the text, never audio, to your agent's model provider to fix punctuation, spelling and lists".into()),
                Some(self.voice_toggle("settings-voice-polish", "Polish with AI", prefs.polish, |p, on| p.polish = on, cx)),
                narrow,
            )],
        ));

        let enter = Target::ALL
            .into_iter()
            .enumerate()
            .map(|(i, target)| {
                let panel = cx.entity().downgrade();
                let prefs = prefs.clone();
                let toggle = Toggle::new(("settings-voice-enter", i), target.label(), prefs.enter_after.get(target)).on_toggle(move |on, _, cx| {
                    let mut prefs = prefs.clone();
                    prefs.enter_after.set(target, on);
                    let _ = panel.update(cx, |_, cx| cx.emit(set(prefs)));
                });
                Self::row(target.label(), None, Some(toggle.into_any_element()), false)
            })
            .collect();
        out.push(self.group(Some("Press Return after"), enter));
        out.push(self.group(
            Some("Sounds"),
            vec![Self::row(
                "Start and stop sounds",
                Some("When interface sounds are on".into()),
                Some(self.voice_toggle("settings-voice-sounds", "Start and stop sounds", prefs.sounds, |p, on| p.sounds = on, cx)),
                false,
            )],
        ));
        out.push(Self::note(
            "Say “new line”, “new paragraph”, “bullet”, “numbered list”, “period”, “comma”, “question mark” or “scratch that”. Terminals get one line.",
        ));
        out.push(Self::note("Speech is transcribed on this Mac; audio never leaves it."));
        out
    }

    /// Settings › Permissions: Silo's own microphone (voice typing).
    pub(super) fn microphone_group(&self, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        let mic = self.snapshot.voice.mic;
        let (label, color) = match mic {
            MicAccess::Granted => ("Granted", theme::success()),
            MicAccess::Denied => ("Not granted", theme::muted()),
            MicAccess::NotAsked => ("Not asked yet", theme::muted()),
        };
        let mut control = div().flex().items_center().gap(px(8.0)).child(div().text_size(px(12.0)).text_color(rgb(color)).child(label));
        if mic != MicAccess::Granted {
            control = control.child(Self::emit_button("settings-permission-mic", "Grant", SettingsEvent::Voice(VoiceEvent::GrantMicrophone), cx));
        }
        self.group(
            Some("Silo"),
            vec![Self::row("Microphone", Some("Voice typing listens on this Mac".into()), Some(control.into_any_element()), narrow)],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_engine_row_says_where_its_model_is() {
        let row = |installed, fetch| EngineRow { kind: EngineKind::Fast, available: true, installed, fetch };
        assert_eq!(model_status(&row(false, None)), "17 MB · downloads on first use");
        assert_eq!(model_status(&row(true, None)), "Downloaded · 17 MB");
        assert_eq!(model_status(&row(false, Some(Fetch::Running { done: 50, total: Some(200) }))), "Downloading… 25%");
        assert_eq!(model_status(&row(false, Some(Fetch::Failed("Offline.".into())))), "Offline.");
        assert_eq!(model_status(&EngineRow { available: false, ..row(false, None) }), "Not available on this Mac");
        assert_eq!(model_status(&EngineRow { kind: EngineKind::Balanced, ..row(false, None) }), "190 MB · downloads on first use");
    }
}
