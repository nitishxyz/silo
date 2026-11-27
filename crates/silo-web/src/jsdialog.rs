//! JavaScript dialogs (`alert`, `confirm`, `prompt`, and "Leave site?" from
//! `beforeunload`). A windowless engine draws none and CEF's default for
//! one blocks the page forever, so every dialog is held here: the app shows
//! it on the page (`Event::JsDialog` → `Browser::answer_js_dialog`), and an
//! agent driving the page answers the same one (`Browser::answer_dialog`).
//! An agent's page nobody drives answers by itself: alerts and leaving are
//! accepted, confirms and prompts dismissed.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::sync::atomic::Ordering;

use cef::*;

use crate::shared::{Event, Shared, lock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsDialogKind {
    Alert,
    Confirm,
    Prompt,
    /// `beforeunload`: leave (or reload) the page with unsaved changes?
    Leave {
        reload: bool,
    },
}

/// A dialog the page waits on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsDialog {
    pub id: u32,
    pub kind: JsDialogKind,
    pub message: String,
    /// A prompt's default answer.
    pub default_text: String,
    /// The asking frame's URL (empty for a leave dialog).
    pub origin: String,
}

impl JsDialog {
    /// The card's headline: "example.com says", "Leave site?".
    #[must_use]
    pub fn headline(&self) -> String {
        match self.kind {
            JsDialogKind::Leave { reload: false } => "Leave site?".into(),
            JsDialogKind::Leave { reload: true } => "Reload site?".into(),
            _ => match site(&self.origin) {
                Some(site) => format!("{site} says"),
                None => "This page says".into(),
            },
        }
    }

    /// The text under the headline.
    #[must_use]
    pub fn body(&self) -> String {
        match self.kind {
            JsDialogKind::Leave { .. } => "Changes you made may not be saved.".into(),
            _ => self.message.clone(),
        }
    }

    /// The accepting button's label.
    #[must_use]
    pub fn accept_label(&self) -> &'static str {
        match self.kind {
            JsDialogKind::Leave { reload: false } => "Leave",
            JsDialogKind::Leave { reload: true } => "Reload",
            _ => "OK",
        }
    }

    /// The dismissing button's label; an alert has none.
    #[must_use]
    pub fn dismiss_label(&self) -> Option<&'static str> {
        match self.kind {
            JsDialogKind::Alert => None,
            JsDialogKind::Leave { .. } => Some("Stay"),
            JsDialogKind::Confirm | JsDialogKind::Prompt => Some("Cancel"),
        }
    }
}

/// `https://example.com:8443/x` → `example.com:8443`; `None` for no host
/// (`file:`, `data:`, `about:`).
fn site(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    (!host.is_empty()).then(|| host.to_owned())
}

/// A page opened a dialog: hold it (returns CEF's "handled").
pub(crate) fn open(
    shared: &Shared,
    kind: JsDialogKind,
    message: String,
    default_text: String,
    origin: String,
    callback: Option<&mut JsdialogCallback>,
) -> i32 {
    let Some(callback) = callback else { return 0 };
    let agents_only = shared.deny_prompts.load(Ordering::Acquire);
    if agents_only && !shared.devtools.page_enabled() {
        let accept = matches!(kind, JsDialogKind::Alert | JsDialogKind::Leave { .. });
        callback.cont(i32::from(accept), None);
        return 1;
    }
    let id = shared.next_dialog.fetch_add(1, Ordering::Relaxed);
    let previous = lock(&shared.js_dialog).replace((id, callback.clone()));
    if let Some((old, callback)) = previous {
        // A page waits on one dialog at a time; an orphan is dismissed.
        callback.cont(0, None);
        shared.push(Event::JsDialogClosed(old));
    }
    if !agents_only {
        shared.push(Event::JsDialog(JsDialog { id, kind, message, default_text, origin }));
    }
    1
}

/// Answer dialog `id` (`None`: whichever is showing); false if it isn't.
pub(crate) fn answer(shared: &Shared, id: Option<u32>, accept: bool, text: Option<&str>) -> bool {
    let held = {
        let mut held = lock(&shared.js_dialog);
        if held.as_ref().is_none_or(|(current, _)| id.is_some_and(|id| id != *current)) {
            return false;
        }
        held.take()
    };
    let Some((id, callback)) = held else { return false };
    callback.cont(i32::from(accept), text.map(CefString::from).as_ref());
    shared.push(Event::JsDialogClosed(id));
    true
}

/// The dialog went without an answer (navigation, crash, close).
pub(crate) fn reset(shared: &Shared) {
    let held = lock(&shared.js_dialog).take();
    if let Some((id, callback)) = held {
        drop(callback);
        shared.push(Event::JsDialogClosed(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(kind: JsDialogKind, origin: &str) -> JsDialog {
        JsDialog { id: 1, kind, message: "Hi".into(), default_text: String::new(), origin: origin.into() }
    }

    #[test]
    fn cards_say_who_asks_and_what_the_buttons_do() {
        let alert = dialog(JsDialogKind::Alert, "https://user:pw@example.com:8443/a?b#c");
        assert_eq!(alert.headline(), "example.com:8443 says");
        assert_eq!((alert.body().as_str(), alert.accept_label(), alert.dismiss_label()), ("Hi", "OK", None));
        assert_eq!(dialog(JsDialogKind::Confirm, "file:///tmp/x.html").headline(), "This page says");
        assert_eq!(dialog(JsDialogKind::Prompt, "").dismiss_label(), Some("Cancel"));
        let leave = dialog(JsDialogKind::Leave { reload: false }, "");
        assert_eq!((leave.headline().as_str(), leave.accept_label(), leave.dismiss_label()), ("Leave site?", "Leave", Some("Stay")));
        assert_eq!(leave.body(), "Changes you made may not be saved.", "pages no longer choose the text");
        assert_eq!(dialog(JsDialogKind::Leave { reload: true }, "").accept_label(), "Reload");
    }
}
