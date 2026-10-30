//! Settings › Keyboard: every command's shortcuts by group. Clicking one
//! records the next chord (Esc cancels, ⌫ removes it); a chord another
//! command has asks before replacing it.

use gpui::{AnyElement, Context, IntoElement, SharedString, Subscription, Window, div, prelude::*, px, rgb};

use super::{ButtonKind, SettingsEvent, SettingsPanel, keycap};
use crate::{
    assets::icon,
    commands::Command,
    keymap::{Chord, KeymapEvent},
    theme::{self, Continuous as _},
};

/// A recorded chord other commands have, waiting for Replace or Cancel.
pub(super) struct Conflict {
    command: Command,
    keys: Vec<Chord>,
    chord: Chord,
    others: Vec<Command>,
}

#[derive(Default)]
pub(super) struct Recorder {
    /// The command and which of its chords (`None`: a new one) the next key press sets.
    target: Option<(Command, Option<usize>)>,
    conflict: Option<Conflict>,
    /// Takes key presses ahead of the keymap while recording.
    intercept: Option<Subscription>,
}

/// `commands`' names, joined for "Also used by …".
fn names(commands: &[Command]) -> String {
    commands.iter().map(|c| c.title()).collect::<Vec<_>>().join(", ")
}

fn emit(event: KeymapEvent) -> SettingsEvent {
    SettingsEvent::Keymap(event)
}

impl SettingsPanel {
    fn keys_of(&self, command: Command) -> Vec<Chord> {
        self.snapshot.shortcuts.iter().find(|s| s.command == command).map(|s| s.keys.clone()).unwrap_or_default()
    }

    fn start_recording(&mut self, command: Command, slot: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let panel = cx.entity().downgrade();
        let id = window.window_handle().window_id();
        self.recorder.conflict = None;
        self.recorder.target = Some((command, slot));
        self.recorder.intercept = Some(cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != id {
                return;
            }
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            let _ = panel.update(cx, |this, cx| this.record(&keystroke, cx));
        }));
        cx.notify();
    }

    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.recorder = Recorder::default();
        cx.notify();
    }

    fn record(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        let Some((command, slot)) = self.recorder.target else { return };
        let plain = !keystroke.modifiers.modified();
        let mut keys = self.keys_of(command);
        let chord = match keystroke.key.as_str() {
            "escape" if plain => return self.stop_recording(cx),
            "backspace" | "delete" if plain => {
                if let Some(index) = slot.filter(|i| *i < keys.len()) {
                    keys.remove(index);
                    cx.emit(emit(KeymapEvent::Set(vec![(command, keys)])));
                }
                return self.stop_recording(cx);
            }
            _ => match Chord::from_keystroke(keystroke) {
                Some(chord) => chord,
                None => return,
            },
        };
        match slot.filter(|i| *i < keys.len()) {
            Some(index) => keys[index] = chord.clone(),
            None => keys.push(chord.clone()),
        }
        let keys = crate::keymap::dedup(keys);
        let others: Vec<Command> =
            self.snapshot.shortcuts.iter().filter(|s| s.command != command && s.keys.contains(&chord)).map(|s| s.command).collect();
        self.recorder = Recorder::default();
        if others.is_empty() {
            cx.emit(emit(KeymapEvent::Set(vec![(command, keys)])));
        } else {
            self.recorder.conflict = Some(Conflict { command, keys, chord, others });
        }
        cx.notify();
    }

    /// Give the recorded chord to its command, taking it from the others.
    fn replace(&mut self, cx: &mut Context<Self>) {
        let Some(conflict) = self.recorder.conflict.take() else { return };
        let mut changes = vec![(conflict.command, conflict.keys)];
        for other in conflict.others {
            let mut keys = self.keys_of(other);
            keys.retain(|k| *k != conflict.chord);
            changes.push((other, keys));
        }
        cx.emit(emit(KeymapEvent::Set(changes)));
        cx.notify();
    }
}

impl SettingsPanel {
    /// One chord as a clickable keycap, "None", or the recording prompt.
    fn chord_chip(&self, command: Command, slot: Option<usize>, chord: Option<&Chord>, cx: &mut Context<Self>) -> AnyElement {
        let recording = self.recorder.target == Some((command, slot));
        let text = |label: &'static str, color: u32| div().px(px(6.0)).text_size(px(11.5)).text_color(rgb(color)).child(label).into_any_element();
        let content = match chord {
            _ if recording => text("Type a shortcut", theme::accent()),
            Some(chord) => keycap(&chord.hint()),
            None => text("None", theme::muted()),
        };
        div()
            .id(SharedString::from(format!("shortcut-{}-{}", command.id(), slot.unwrap_or(usize::MAX))))
            .h(px(24.0))
            .px(px(2.0))
            .flex()
            .items_center()
            .continuous(6.0)
            .border_1()
            .border_color(if recording { rgb(theme::accent()) } else { gpui::transparent_black().into() })
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .child(content)
            .on_click(cx.listener(move |this, _, window, cx| {
                if recording {
                    this.stop_recording(cx);
                } else {
                    this.start_recording(command, slot, window, cx);
                }
            }))
            .into_any_element()
    }

    /// What the row says under its title, and in which color.
    fn shortcut_note(&self, shortcut: &super::SettingsShortcut) -> Option<(String, u32)> {
        let command = shortcut.command;
        if self.recorder.target.is_some_and(|(c, _)| c == command) {
            return Some(("Esc cancels, ⌫ removes it".into(), theme::muted()));
        }
        if let Some(conflict) = self.recorder.conflict.as_ref().filter(|c| c.command == command) {
            return Some((format!("{} is also used by {}", conflict.chord.hint(), names(&conflict.others)), theme::warning()));
        }
        shortcut.keys.iter().find_map(|chord| {
            let others: Vec<Command> =
                self.snapshot.shortcuts.iter().filter(|s| s.command != command && s.keys.contains(chord)).map(|s| s.command).collect();
            if !others.is_empty() {
                Some((format!("{} is also used by {}", chord.hint(), names(&others)), theme::warning()))
            } else {
                chord.warning().map(|w| (format!("{}: {w}", chord.hint()), theme::warning()))
            }
        })
    }

    fn shortcut_row(&self, shortcut: &super::SettingsShortcut, cx: &mut Context<Self>) -> AnyElement {
        let command = shortcut.command;
        let group = SharedString::from(format!("shortcut-row-{}", command.id()));
        let adding = self.recorder.target == Some((command, None));
        let mut chips: Vec<AnyElement> = shortcut.keys.iter().enumerate().map(|(i, k)| self.chord_chip(command, Some(i), Some(k), cx)).collect();
        if shortcut.keys.is_empty() || adding {
            chips.push(self.chord_chip(command, None, None, cx));
        }
        let add = (!shortcut.keys.is_empty() && !adding).then(|| {
            div()
                .id(SharedString::from(format!("shortcut-add-{}", command.id())))
                .size(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .continuous(6.0)
                .opacity(0.0)
                .group_hover(group.clone(), |s| s.opacity(1.0))
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
                .tooltip(crate::tooltip::Tooltip::text("Add Shortcut"))
                .child(icon("plus", 12.0, theme::muted()))
                .on_click(cx.listener(move |this, _, window, cx| this.start_recording(command, None, window, cx)))
        });
        let conflicted = self.recorder.conflict.as_ref().is_some_and(|c| c.command == command);
        let mut actions: Vec<AnyElement> = Vec::new();
        if conflicted {
            actions.push(Self::button("shortcut-replace", "Replace", ButtonKind::Secondary, Self::replace, cx));
            actions.push(Self::button("shortcut-cancel", "Cancel", ButtonKind::Secondary, Self::stop_recording, cx));
        } else if shortcut.custom {
            let reset = SettingsEvent::Keymap(KeymapEvent::Reset(command));
            actions.push(Self::emit_button(SharedString::from(format!("shortcut-reset-{}", command.id())), "Reset", reset, cx));
        }
        let note = self.shortcut_note(shortcut);
        div()
            .group(group)
            .w_full()
            .min_h(px(36.0))
            .px(px(14.0))
            .py(px(6.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(13.0)).child(shortcut.label.clone()))
                    .children(note.map(|(text, color)| div().text_size(px(11.5)).line_height(px(15.0)).text_color(rgb(color)).child(text))),
            )
            .children(actions)
            .children(add)
            .child(div().flex_none().flex().items_center().gap(px(4.0)).children(chips))
            .into_any_element()
    }
}

impl SettingsPanel {
    /// `keybindings.json` with reveal and edit icon buttons.
    fn keymap_file_line(&self, cx: &mut Context<Self>) -> AnyElement {
        let action = |id: &'static str, glyph: &'static str, tip: &'static str, event: KeymapEvent, cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .continuous(6.0)
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
                .tooltip(crate::tooltip::Tooltip::text(tip))
                .child(icon(glyph, 13.0, theme::muted()))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(emit(event.clone()))))
        };
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .flex_1()
                    .font_family(self.snapshot.terminal.font_in_use.clone())
                    .text_size(px(12.0))
                    .text_color(rgb(theme::muted()))
                    .child(crate::keymap::FILE),
            )
            .child(action("settings-keymap-reveal", "settings-search", "Show in Finder", KeymapEvent::RevealFile, cx))
            .child(action("settings-keymap-edit", "forward", "Edit keybindings.json", KeymapEvent::OpenFile, cx))
            .into_any_element()
    }

    pub(super) fn keyboard_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let section = super::Section::Keyboard;
        let show_all = self.query.is_empty() || self.text_matches(&format!("{} {}", section.label(), section.keywords()));
        let mut out = Vec::new();
        if show_all {
            let mut rows = vec![Self::row("Shortcuts file", None, Some(self.keymap_file_line(cx)), false)];
            if self.snapshot.shortcuts.iter().any(|s| s.custom) || !self.snapshot.shortcut_problems.is_empty() {
                let restore = Self::emit_button("settings-keymap-restore", "Restore", emit(KeymapEvent::RestoreDefaults), cx);
                rows.push(Self::row("Restore Defaults", Some("Every shortcut back to Silo's".into()), Some(restore), false));
            }
            out.push(self.group(None, rows));
            for problem in &self.snapshot.shortcut_problems {
                out.push(div().px(px(4.0)).text_size(px(11.5)).text_color(rgb(theme::warning())).child(problem.clone()).into_any_element());
            }
        }
        let mut groups: Vec<&'static str> = Vec::new();
        for command in Command::ALL {
            if !groups.contains(&command.group()) {
                groups.push(command.group());
            }
        }
        for group in groups {
            let rows: Vec<AnyElement> = self
                .snapshot
                .shortcuts
                .iter()
                .filter(|s| s.command.group() == group && (show_all || self.text_matches(&format!("{} {} {group}", s.label, s.hints()))))
                .map(|s| self.shortcut_row(s, cx))
                .collect();
            if !rows.is_empty() {
                out.push(self.group(Some(group), rows));
            }
        }
        // The Fn trigger is not a chord: shown, not remappable.
        if show_all || self.text_matches("voice typing dictation fn globe") {
            let row = Self::row("Voice Typing", Some("Hold, or press twice to keep talking".into()), Some(keycap("fn")), false);
            out.push(self.group(Some("Voice"), vec![row]));
        }
        out
    }
}
