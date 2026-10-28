//! Pages a page opens: modified link clicks, `window.open`, `target=_blank`.
//! Each becomes a new tab. A page CEF creates as a popup is handed to the app
//! as a [`Popup`] (the opener keeps its `window.opener` link to it); a URL
//! CEF only asks about (`on_open_urlfrom_tab`) opens fresh.

use std::sync::Arc;

use cef::WindowOpenDisposition;

use crate::shared::{Shared, lock};
use crate::{Browser, ViewSize};

/// A new tab the page asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct NewTab {
    /// The page to open (empty: `about:blank`, e.g. `window.open('')`).
    pub url: String,
    /// Shown at once (Shift-⌘-click, `window.open`), else opened behind
    /// (⌘-click, middle click).
    pub foreground: bool,
    /// The engine page CEF created for it, its opener link intact; `None`:
    /// open `url` in a new page.
    pub popup: Option<Popup>,
    /// The size `window.open` asked for, in points (a tab ignores it).
    pub size: Option<(i32, i32)>,
}

/// A popup page CEF is creating or has created; [`Popup::open`] adopts it.
#[derive(Clone)]
pub struct Popup(pub(crate) Arc<Shared>);

impl std::fmt::Debug for Popup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Popup")
    }
}

impl PartialEq for Popup {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Popup {
    /// The page's handle; `notify` replaces the opener's until now (events
    /// so far are kept: drain them right away).
    #[must_use]
    pub fn open(self, notify: Arc<dyn Fn() + Send + Sync>) -> Browser {
        *lock(&self.0.notify) = notify;
        Browser::from_shared(self.0)
    }

    pub(crate) fn shared(view: ViewSize) -> Arc<Shared> {
        Shared::new(view, Arc::new(|| {}))
    }
}

/// How a page asked to open: `Some(foreground)` for a new tab, `None` for
/// what isn't one (the current tab, a download, PiP, an error).
#[must_use]
pub(crate) fn tab_for(disposition: WindowOpenDisposition) -> Option<bool> {
    match disposition {
        WindowOpenDisposition::NEW_BACKGROUND_TAB => Some(false),
        WindowOpenDisposition::NEW_FOREGROUND_TAB
        | WindowOpenDisposition::NEW_POPUP
        | WindowOpenDisposition::NEW_WINDOW
        | WindowOpenDisposition::OFF_THE_RECORD
        | WindowOpenDisposition::SINGLETON_TAB
        | WindowOpenDisposition::SWITCH_TO_TAB => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispositions_map_to_background_or_foreground_tabs() {
        assert_eq!(tab_for(WindowOpenDisposition::NEW_BACKGROUND_TAB), Some(false), "⌘-click, middle click");
        assert_eq!(tab_for(WindowOpenDisposition::NEW_FOREGROUND_TAB), Some(true), "⇧⌘-click");
        assert_eq!(tab_for(WindowOpenDisposition::NEW_POPUP), Some(true), "window.open with features");
        assert_eq!(tab_for(WindowOpenDisposition::NEW_WINDOW), Some(true), "⇧-click");
        assert_eq!(tab_for(WindowOpenDisposition::OFF_THE_RECORD), Some(true));
        for not_a_tab in [
            WindowOpenDisposition::CURRENT_TAB,
            WindowOpenDisposition::SAVE_TO_DISK,
            WindowOpenDisposition::IGNORE_ACTION,
            WindowOpenDisposition::NEW_PICTURE_IN_PICTURE,
            WindowOpenDisposition::UNKNOWN,
        ] {
            assert_eq!(tab_for(not_a_tab), None, "{not_a_tab:?}");
        }
    }
}
