//! One windowless browser (a browser block's engine side).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use cef::*;

use crate::frame::Frame as SurfaceFrame;
use crate::input::{Modifiers, MouseButton, mouse_event};
use crate::keyboard::{KeyInput, key_events};
use crate::shared::{Event, Shared, ViewSize, lock};
use crate::{Profile, Runtime};

/// `CefRange::InvalidRange()`: "no range". libcef rejects a null `const CefRange&`, so the
/// optional-in-spirit IME ranges must be passed explicitly.
const NO_RANGE: Range = Range { from: u32::MAX, to: u32::MAX };

/// Frames per second of a page a window shows, and of one nobody sees (the
/// off-screen host): still painting, so CDP screenshots and timers work.
pub const FRAME_RATE: u32 = 60;
pub const HIDDEN_FRAME_RATE: u32 = 5;

/// The windowless frame rate of a page shown or kept off screen.
pub fn frame_rate(offscreen: bool) -> u32 {
    if offscreen { HIDDEN_FRAME_RATE } else { FRAME_RATE }
}

pub struct BrowserOptions<'a> {
    pub url: &'a str,
    pub profile: &'a Profile,
    pub size: ViewSize,
    /// No window shows the page: it renders at [`HIDDEN_FRAME_RATE`].
    pub offscreen: bool,
    /// Called (on the main thread) after each new frame or event.
    pub notify: Arc<dyn Fn() + Send + Sync>,
}

/// Main-thread handle; dropping it does not close the page, call `close`.
pub struct Browser {
    shared: Arc<Shared>,
}

/// An edit command run by the page's focused frame, as from Chrome's Edit menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// A step of a two-finger touch pinch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pinch {
    Start,
    Move,
    End,
    Cancel,
}

impl Runtime {
    pub fn create_browser(&self, options: BrowserOptions) -> Result<Browser> {
        let shared = Arc::new(Shared {
            devtools: Arc::default(),
            view: Mutex::new(options.size),
            frame: Mutex::new(None),
            browser: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            notify: options.notify,
            focused: AtomicBool::new(false),
            last_input: Mutex::new(None),
            dropped: Mutex::new(None),
            theme_tag: crate::shared::theme_tag(),
            blank: AtomicBool::new(true),
            shown: AtomicBool::new(false),
            queued: Mutex::new(None),
            cert_failed: Mutex::new(None),
            scroll: Mutex::new((0.0, 0.0)),
        });
        let window_info =
            WindowInfo { windowless_rendering_enabled: 1, shared_texture_enabled: 1, runtime_style: RuntimeStyle::ALLOY, ..Default::default() };
        #[allow(clippy::cast_possible_wrap)]
        let rate = frame_rate(options.offscreen) as i32;
        let settings = BrowserSettings { windowless_frame_rate: rate, background_color: 0xFFFF_FFFF, ..Default::default() };
        let mut client = crate::handlers::client(&shared, &options.profile.tunnel);
        let mut context = options.profile.context.clone();
        let url = CefString::from(options.url);
        let created = browser_host_create_browser(Some(&window_info), Some(&mut client), Some(&url), Some(&settings), None, Some(&mut context));
        if created != 1 {
            bail!("browser_host_create_browser failed");
        }
        crate::runtime::browser_opened();
        Ok(Browser { shared })
    }
}

impl Browser {
    /// In-process CDP; poll this future on the main thread.
    pub async fn cdp(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let host = self.host().ok_or_else(|| anyhow::anyhow!("browser is not ready or has closed"))?;
        self.shared.devtools.call(host, method, params).await
    }

    pub fn subscribe_cdp(&self) -> tokio::sync::broadcast::Receiver<crate::DevToolsEvent> {
        self.shared.devtools.subscribe()
    }

    /// Answer the page's pending JavaScript dialog (held while CDP's `Page`
    /// domain is on, since a windowless engine shows none); false if none.
    pub fn answer_dialog(&self, accept: bool, prompt: Option<&str>) -> bool {
        self.shared.devtools.answer_dialog(accept, prompt)
    }

    /// Changes whenever the main frame navigates or its DevTools agent closes.
    pub fn document_generation(&self) -> u64 {
        self.shared.devtools.generation.load(Ordering::Relaxed)
    }

    /// CEF has created the page and it has not closed: CDP calls work.
    pub fn ready(&self) -> bool {
        self.host().is_some()
    }

    /// `None` until CEF has created the page (`on_after_created`) and after it closed.
    fn page(&self) -> Option<cef::Browser> {
        lock(&self.shared.browser).clone()
    }

    fn host(&self) -> Option<BrowserHost> {
        self.page().and_then(|page| page.host())
    }

    /// The latest painted frame (BGRA, pixel size = view size × scale).
    pub fn frame(&self) -> Option<SurfaceFrame> {
        lock(&self.shared.frame).clone()
    }

    /// Whether a real page committed and painted: until then the frame is
    /// a blank document's (or a previous page's).
    pub fn shown(&self) -> bool {
        self.shared.shown.load(Ordering::Acquire)
    }

    pub fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *lock(&self.shared.events))
    }

    pub fn navigate(&self, url: &str) {
        match self.page() {
            Some(page) => {
                if let Some(frame) = page.main_frame() {
                    frame.load_url(Some(&CefString::from(url)));
                }
            }
            None => *lock(&self.shared.queued) = Some(url.to_owned()),
        }
    }

    pub fn back(&self) {
        if let Some(page) = self.page() {
            page.go_back();
        }
    }

    pub fn forward(&self) {
        if let Some(page) = self.page() {
            page.go_forward();
        }
    }

    pub fn reload(&self, ignore_cache: bool) {
        if let Some(page) = self.page() {
            if ignore_cache { page.reload_ignore_cache() } else { page.reload() }
        }
    }

    pub fn stop(&self) {
        if let Some(page) = self.page() {
            page.stop_load();
        }
    }

    /// Runs `edit` in the focused frame, so the page sees its clipboard
    /// events and the edit joins its undo history.
    pub fn edit(&self, edit: Edit) {
        let Some(frame) = self.page().and_then(|page| page.focused_frame().or_else(|| page.main_frame())) else { return };
        self.shared.input();
        match edit {
            Edit::Undo => frame.undo(),
            Edit::Redo => frame.redo(),
            Edit::Cut => frame.cut(),
            Edit::Copy => frame.copy(),
            Edit::Paste => frame.paste(),
            Edit::SelectAll => frame.select_all(),
        }
    }

    /// CEF zoom level (0.0 = 100%, each step ≈ ×1.2).
    pub fn set_zoom(&self, level: f64) {
        if let Some(host) = self.host() {
            host.set_zoom_level(level);
        }
    }

    /// The page's root scroll offset in view points, as last reported.
    pub fn scroll_offset(&self) -> (f64, f64) {
        *lock(&self.shared.scroll)
    }

    pub fn resize(&self, size: ViewSize) {
        let previous = std::mem::replace(&mut *lock(&self.shared.view), size);
        if previous.scale != size.scale
            && let Some(host) = self.host()
        {
            host.notify_screen_info_changed();
        }
        if let Some(host) = self.host() {
            host.was_resized();
        }
    }

    pub fn set_focus(&self, focused: bool) {
        self.shared.focused.store(focused, Ordering::Relaxed);
        if let Some(host) = self.host() {
            host.set_focus(i32::from(focused));
        }
    }

    pub fn set_hidden(&self, hidden: bool) {
        if let Some(host) = self.host() {
            host.was_hidden(i32::from(hidden));
        }
    }

    /// Shown in a window (full rate) or kept off screen (throttled).
    pub fn set_offscreen(&self, offscreen: bool) {
        if let Some(host) = self.host() {
            #[allow(clippy::cast_possible_wrap)]
            host.set_windowless_frame_rate(frame_rate(offscreen) as i32);
        }
    }

    /// Ask for a new frame now (a throttled page's screenshot is fresh).
    pub fn invalidate(&self) {
        if let Some(host) = self.host() {
            host.invalidate(PaintElementType::VIEW);
        }
    }

    /// Asks the page to close; `Event::Closed` follows once CEF is done with it.
    pub fn close(&self) {
        if let Some(host) = self.host() {
            host.close_browser(1);
        }
    }
}

fn copy_operation() -> DragOperationsMask {
    DragOperationsMask::from(sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY)
}

/// Input, in view points (DIP) relative to the browser's top-left.
impl Browser {
    pub fn mouse_move(&self, x: f32, y: f32, mods: Modifiers, held: &[MouseButton]) {
        if let Some(host) = self.host() {
            host.send_mouse_move_event(Some(&mouse_event(x, y, mods, held)), 0);
        }
    }

    /// Files from outside the app dragged onto the page at `(x, y)`.
    pub fn file_drag_enter(&self, x: f32, y: f32, paths: &[std::path::PathBuf]) {
        let (Some(host), Some(mut data)) = (self.host(), drag_data_create()) else { return };
        for path in paths {
            data.add_file(Some(&CefString::from(path.to_string_lossy().as_ref())), None);
        }
        host.drag_target_drag_enter(Some(&mut data), Some(&mouse_event(x, y, Modifiers::default(), &[])), copy_operation());
    }

    pub fn file_drag_over(&self, x: f32, y: f32) {
        if let Some(host) = self.host() {
            host.drag_target_drag_over(Some(&mouse_event(x, y, Modifiers::default(), &[])), copy_operation());
        }
    }

    pub fn file_drag_leave(&self) {
        if let Some(host) = self.host() {
            host.drag_target_drag_leave();
        }
    }

    /// Ends a file drag at `(x, y)`; unhandled by the page, it does nothing.
    pub fn file_drop(&self, x: f32, y: f32) {
        if let Some(host) = self.host() {
            *lock(&self.shared.dropped) = Some(std::time::Instant::now());
            host.drag_target_drop(Some(&mouse_event(x, y, Modifiers::default(), &[])));
        }
    }

    pub fn mouse_leave(&self, mods: Modifiers) {
        if let Some(host) = self.host() {
            host.send_mouse_move_event(Some(&mouse_event(-1.0, -1.0, mods, &[])), 1);
        }
    }

    pub fn mouse_button(&self, x: f32, y: f32, button: MouseButton, down: bool, clicks: u32, mods: Modifiers) {
        if down {
            self.shared.input();
        }
        let held: &[MouseButton] = if down { &[button] } else { &[] };
        let event = mouse_event(x, y, mods, held);
        if let Some(host) = self.host() {
            host.send_mouse_click_event(Some(&event), button.cef(), i32::from(!down), clicks.max(1) as i32);
        }
    }

    /// Scroll by `(dx, dy)` points; positive `dy` moves content down, i.e. towards the top of the
    /// page (gpui's and CEF's shared convention).
    pub fn wheel(&self, x: f32, y: f32, dx: f32, dy: f32, mods: Modifiers) {
        let event = mouse_event(x, y, mods, &[]);
        if let Some(host) = self.host() {
            host.send_mouse_wheel_event(Some(&event), dx.round() as i32, dy.round() as i32);
        }
    }

    /// A pinch as two touch points: Chromium's gesture recognizer turns
    /// their spread into a page-scale (visual viewport) zoom.
    pub fn pinch(&self, step: Pinch, points: [(f32, f32); 2]) {
        let Some(host) = self.host() else { return };
        let type_ = match step {
            Pinch::Start => TouchEventType::PRESSED,
            Pinch::Move => TouchEventType::MOVED,
            Pinch::End => TouchEventType::RELEASED,
            Pinch::Cancel => TouchEventType::CANCELLED,
        };
        for (id, (x, y)) in points.into_iter().enumerate() {
            let event = TouchEvent {
                id: id as i32,
                x,
                y,
                radius_x: 0.0,
                radius_y: 0.0,
                rotation_angle: 0.0,
                pressure: 0.0,
                type_,
                modifiers: 0,
                pointer_type: PointerType::TOUCH,
            };
            host.send_touch_event(Some(&event));
        }
    }

    pub fn key(&self, input: &KeyInput) {
        if input.down {
            self.shared.input();
        }
        for event in key_events(input) {
            if let Some(host) = self.host() {
                host.send_key_event(Some(&event));
            }
        }
    }

    /// Inserts committed text (typing, IME commit, paste-like input).
    pub fn insert_text(&self, text: &str) {
        if let Some(host) = self.host() {
            host.ime_commit_text(Some(&CefString::from(text)), Some(&NO_RANGE), 0);
        }
    }

    /// Marked (composing) IME text with the caret/selection at `selection` (UTF-16 units).
    pub fn ime_set_composition(&self, text: &str, selection: std::ops::Range<u32>) {
        let underline = CompositionUnderline {
            range: Range { from: 0, to: text.encode_utf16().count() as u32 },
            color: 0xFF00_0000,
            background_color: 0,
            ..Default::default()
        };
        let selection = Range { from: selection.start, to: selection.end };
        if let Some(host) = self.host() {
            host.ime_set_composition(Some(&CefString::from(text)), Some(&[underline]), Some(&NO_RANGE), Some(&selection));
        }
    }

    pub fn ime_finish(&self) {
        if let Some(host) = self.host() {
            host.ime_finish_composing_text(0);
        }
    }

    pub fn ime_cancel(&self) {
        if let Some(host) = self.host() {
            host.ime_cancel_composition();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_nobody_sees_render_slowly_but_still_render() {
        assert_eq!(frame_rate(false), FRAME_RATE);
        assert_eq!(frame_rate(true), HIDDEN_FRAME_RATE);
        assert!((1..=5).contains(&HIDDEN_FRAME_RATE), "a few frames a second: screenshots and rAF keep working");
    }
}
