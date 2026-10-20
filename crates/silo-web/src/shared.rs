//! State shared between a `Browser` handle and its CEF handlers.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::frame::Frame;

/// Engine-side changes, drained by the app with `Browser::take_events`.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The main frame's committed URL; `back_forward` when the engine's own
    /// history navigation committed it.
    UrlChanged {
        url: String,
        back_forward: bool,
    },
    TitleChanged(String),
    /// `data:image/png;base64,…` of the page's 32 px favicon.
    FaviconChanged(String),
    Loading(bool),
    /// Page load progress, 0.0–1.0.
    Progress(f64),
    /// Whether the page's focused element takes text (field, contenteditable).
    EditableFocus(bool),
    /// The page's colour (`theme-color`, else its opaque background) as
    /// `0x00RRGGBB`; `None` when it has none.
    ThemeColor(Option<u32>),
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
    /// Last mouse press or key press sent to the page.
    pub(crate) last_input: Mutex<Option<Instant>>,
    /// Last external file drop on the page: the navigation Chromium starts
    /// when the page ignores it is cancelled.
    pub(crate) dropped: Mutex<Option<Instant>>,
    /// Console prefix of the main frame's theme reports: random, so no
    /// other frame or page script can forge one.
    pub(crate) theme_tag: String,
}

/// A fresh random theme tag.
pub(crate) fn theme_tag() -> String {
    use std::hash::{BuildHasher, Hasher};
    format!("\u{1}silo-theme-{:016x}:", std::collections::hash_map::RandomState::new().build_hasher().finish())
}

/// How long a user input counts as a gesture (Chromium's user activation).
const GESTURE: Duration = Duration::from_secs(5);

/// How long after a file drop a `file:` navigation counts as caused by it.
const DROP_NAVIGATION: Duration = Duration::from_secs(2);

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    pub(crate) fn push(&self, event: Event) {
        lock(&self.events).push(event);
        (self.notify)();
    }

    pub(crate) fn input(&self) {
        *lock(&self.last_input) = Some(Instant::now());
    }

    /// Whether the user interacted with the page just now; each input counts once.
    pub(crate) fn take_gesture(&self) -> bool {
        lock(&self.last_input).take().is_some_and(|at| at.elapsed() < GESTURE)
    }

    /// Whether a navigation to `url` is the fallback of a file drop just now.
    pub(crate) fn drop_navigation(&self, url: &str) -> bool {
        url.starts_with("file:") && lock(&self.dropped).is_some_and(|at| at.elapsed() < DROP_NAVIGATION)
    }
}
