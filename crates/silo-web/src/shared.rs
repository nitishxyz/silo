//! State shared between a `Browser` handle and its CEF handlers.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::frame::Frame;

/// Engine-side changes, drained by the app with `Browser::take_events`.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    UrlChanged(String),
    TitleChanged(String),
    /// `data:image/png;base64,…` of the page's 32 px favicon.
    FaviconChanged(String),
    Loading {
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
    },
    /// The page asked for a new window; open it as a new browser block.
    Popup(String),
    CursorChanged(Cursor),
    /// IME candidate anchor in view points: `(x, y, w, h)` of the selection's first character.
    ImeBounds {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// `on_before_close` ran; the handle is dead.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    Hand,
    IBeam,
    Crosshair,
    Wait,
    NotAllowed,
    ResizeColumn,
    ResizeRow,
    Grab,
    Grabbing,
    Move,
    None,
}

/// View size in points (DIP) and the backing scale factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewSize {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

pub(crate) type Notify = Arc<dyn Fn() + Send + Sync>;

pub(crate) struct Shared {
    pub(crate) view: Mutex<ViewSize>,
    pub(crate) frame: Mutex<Option<Frame>>,
    /// Set by `on_after_created`, cleared by `on_before_close`.
    pub(crate) browser: Mutex<Option<cef::Browser>>,
    pub(crate) events: Mutex<Vec<Event>>,
    pub(crate) notify: Notify,
    /// Last `Browser::set_focus`; re-applied to each new page's widget.
    pub(crate) focused: AtomicBool,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    pub(crate) fn push(&self, event: Event) {
        lock(&self.events).push(event);
        (self.notify)();
    }
}
