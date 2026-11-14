//! Key modes (`docs/LUA.md` §4.1): a per-window stack `app.mode_enter`
//! pushes and `app.mode_exit` (or Escape, unless the mode binds it) pops.
//! Only the top mode's bindings apply, ahead of the others: the workspace's
//! key context names every other declared mode `silo_off_<mode>`, and a
//! mode's bindings are `!silo_off_<mode>` (`keymap::mode_off`). An
//! exclusive mode adds `silo_exclusive`, which turns off the bindings
//! outside modes, and this module's key listener swallows the keys left
//! before they reach a terminal, page or field.

use gpui::{AnyElement, Context, Div, KeyContext, KeyDownEvent, Stateful, Window, div, prelude::*, px, rgb};
use silo_proto::config::KeyMode;

use crate::{
    keymap::{self, Chord},
    theme::Continuous as _,
    workspace::Workspace,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entered {
    pub name: String,
    /// Leave after the next bound key runs.
    pub once: bool,
}

/// A window's modes, the top last.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Modes {
    stack: Vec<Entered>,
}

/// What a key press does with a mode on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// What it does outside modes.
    Pass,
    /// Escape: leave the mode.
    Leave,
    /// Nothing (an exclusive mode's unbound key).
    Swallow,
}

impl Modes {
    /// Enter mode `name` of `known` (again: it moves to the top).
    ///
    /// # Errors
    /// No such mode.
    pub(crate) fn enter(&mut self, name: &str, once: bool, known: &[KeyMode]) -> Result<(), String> {
        if !known.iter().any(|m| m.name == name) {
            return Err(format!("No mode named “{name}”"));
        }
        self.stack.retain(|e| e.name != name);
        self.stack.push(Entered { name: name.to_owned(), once });
        Ok(())
    }

    /// Leave the top mode, or `all`; whether one was on.
    pub(crate) fn exit(&mut self, all: bool) -> bool {
        if all { !std::mem::take(&mut self.stack).is_empty() } else { self.stack.pop().is_some() }
    }

    #[must_use]
    pub(crate) fn top(&self) -> Option<&Entered> {
        self.stack.last()
    }

    /// A binding of `mode` ran: a `once` mode on top leaves. Whether it did.
    pub(crate) fn ran(&mut self, mode: Option<&str>) -> bool {
        let leaves = self.top().is_some_and(|top| top.once && Some(top.name.as_str()) == mode);
        if leaves {
            self.stack.pop();
        }
        leaves
    }

    /// The top mode is exclusive.
    #[must_use]
    pub(crate) fn exclusive(&self, known: &[KeyMode]) -> bool {
        self.top().is_some_and(|top| known.iter().any(|m| m.name == top.name && m.exclusive))
    }

    /// Drop modes the config no longer declares; whether any went.
    pub(crate) fn forget_unknown(&mut self, known: &[KeyMode]) -> bool {
        let before = self.stack.len();
        self.stack.retain(|e| known.iter().any(|m| m.name == e.name));
        self.stack.len() != before
    }

    /// The workspace's key context.
    #[must_use]
    pub(crate) fn context(&self, known: &[KeyMode]) -> KeyContext {
        let mut context = KeyContext::default();
        context.add("Workspace");
        let top = self.top().map(|t| t.name.as_str());
        for mode in known.iter().filter(|m| Some(m.name.as_str()) != top) {
            context.add(keymap::mode_off(&mode.name));
        }
        if self.exclusive(known) {
            context.add(keymap::EXCLUSIVE);
        }
        context
    }

    /// A key no binding took: `escape` is a plain Escape, `binds_escape`
    /// whether the top mode binds it.
    #[must_use]
    pub(crate) fn key(&self, escape: bool, binds_escape: bool, known: &[KeyMode]) -> Key {
        if self.top().is_none() {
            Key::Pass
        } else if escape && !binds_escape {
            Key::Leave
        } else if self.exclusive(known) {
            Key::Swallow
        } else {
            Key::Pass
        }
    }
}

/// The mode actions and key listener, on the workspace's root.
pub(crate) fn handle(element: Stateful<Div>, cx: &mut Context<Workspace>) -> Stateful<Div> {
    element.on_action(cx.listener(Workspace::run_binding)).capture_key_down(cx.listener(Workspace::mode_key))
}

impl Workspace {
    pub(crate) fn mode_context(&self) -> KeyContext {
        self.modes.context(&keymap::modes())
    }

    /// `app.mode_enter`.
    ///
    /// # Errors
    /// No such mode.
    pub(crate) fn enter_mode(&mut self, name: &str, once: bool, cx: &mut Context<Self>) -> Result<(), String> {
        self.modes.enter(name, once, &keymap::modes())?;
        cx.notify();
        Ok(())
    }

    /// `app.mode_exit`.
    pub(crate) fn exit_mode(&mut self, all: bool, cx: &mut Context<Self>) {
        if self.modes.exit(all) {
            cx.notify();
        }
    }

    /// A modal sheet has the keys: modes leave them alone.
    fn modal_open(&self) -> bool {
        self.palette.is_some()
            || self.settings.is_some()
            || self.switcher.is_some()
            || self.picker.is_some()
            || self.add_host.is_some()
            || self.share.is_some()
            || self.close_confirm.is_some()
    }

    /// Bindings come first; a key none took ends here in a mode that
    /// leaves on Escape or swallows the rest.
    fn mode_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(top) = self.modes.top().map(|t| t.name.clone()) else { return };
        if self.modal_open() {
            return;
        }
        let keystroke = &event.keystroke;
        let escape = keystroke.key == "escape" && !keystroke.modifiers.modified();
        let binds_escape = Chord::parse("escape").is_ok_and(|c| keymap::mode_binds(&top, &silo_proto::keys::Sequence(vec![c])));
        match self.modes.key(escape, binds_escape, &keymap::modes()) {
            Key::Pass => {}
            Key::Leave => {
                self.modes.exit(false);
                cx.stop_propagation();
                cx.notify();
            }
            Key::Swallow => cx.stop_propagation(),
        }
    }

    /// The title bar's chip naming the mode on; a click leaves it.
    pub(crate) fn render_mode_chip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let name = self.modes.top()?.name.clone();
        let chrome = self.chrome;
        Some(
            div()
                .id("mode-chip")
                .flex()
                .flex_none()
                .items_center()
                .ml(px(6.0))
                .h(px(24.0))
                .px(px(9.0))
                .capsule()
                .border_1()
                .border_color(rgb(chrome.accent))
                .text_size(px(11.5))
                .text_color(rgb(chrome.accent))
                .whitespace_nowrap()
                .cursor_pointer()
                .hover(|s| s.bg(chrome.hover()))
                .child(name)
                .tooltip(crate::tooltip::Tooltip::text("Leave mode (esc)"))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, _, cx| this.exit_mode(false, cx)))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests;
