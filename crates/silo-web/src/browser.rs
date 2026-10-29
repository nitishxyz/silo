//! One windowless browser (a browser block's engine side).

use std::sync::Arc;
use std::sync::atomic::Ordering;

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

/// Frames per second of a page nobody sees (the off-screen host): still
/// painting, so CDP screenshots and timers work. A page a window shows
/// paints at the display's rate ([`display_rate`]).
pub const HIDDEN_FRAME_RATE: u32 = 5;

/// The fastest refresh rate among the Mac's displays (a ProMotion panel's
/// 120 Hz), 60 when unknown: CEF paints at most this often. Main thread.
pub fn display_rate() -> u32 {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    let Some(class) = AnyClass::get(c"NSScreen") else { return 60 };
    let screens: *mut AnyObject = unsafe { msg_send![class, screens] };
    if screens.is_null() {
        return 60;
    }
    let count: usize = unsafe { msg_send![screens, count] };
    let fastest = (0..count)
        .map(|i| {
            let screen: *mut AnyObject = unsafe { msg_send![screens, objectAtIndex: i] };
            let fps: isize = unsafe { msg_send![screen, maximumFramesPerSecond] };
            fps
        })
        .max()
        .unwrap_or(60);
    clamp_rate(fastest)
}

fn clamp_rate(fps: isize) -> u32 {
    u32::try_from(fps).unwrap_or(60).clamp(60, 240)
}

/// The windowless frame rate of a page shown or kept off screen.
pub fn frame_rate(offscreen: bool) -> u32 {
    if offscreen { HIDDEN_FRAME_RATE } else { display_rate() }
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
        let shared = Shared::new(options.size, options.notify);
        shared.frame_rate.store(frame_rate(options.offscreen), Ordering::Relaxed);
        let window_info = window_info();
        let settings = settings(options.offscreen);
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

/// A windowless page's window: accelerated paint, Alloy (like every page
/// here), and frames only when asked for ([`Browser::begin_frame`]): CEF's
/// own frame timer, on its UI thread, misses most frames while the page
/// scrolls (30 fps or less, with 80 ms gaps, at any frame rate).
pub(crate) fn window_info() -> WindowInfo {
    WindowInfo {
        windowless_rendering_enabled: 1,
        shared_texture_enabled: 1,
        external_begin_frame_enabled: 1,
        runtime_style: RuntimeStyle::ALLOY,
        ..Default::default()
    }
}

pub(crate) fn settings(offscreen: bool) -> BrowserSettings {
    #[allow(clippy::cast_possible_wrap)]
    let rate = frame_rate(offscreen) as i32;
    BrowserSettings { windowless_frame_rate: rate, background_color: 0xFFFF_FFFF, ..Default::default() }
}

impl Browser {
    pub(crate) fn from_shared(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

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
        self.set_frame_rate(frame_rate(offscreen));
    }

    /// The most frames a second the page paints, asked for or not.
    pub fn set_frame_rate(&self, fps: u32) {
        self.shared.frame_rate.store(fps, Ordering::Relaxed);
        if let Some(host) = self.host() {
            host.set_windowless_frame_rate(i32::try_from(fps).unwrap_or(60));
        }
    }

    /// Asks the page for a frame (call once per display refresh while it
    /// shows): it runs its animations and paints what changed. A page no
    /// one asks gets frames from [`Runtime::drive_pages`].
    pub fn begin_frame(&self) {
        if let Some(host) = self.host() {
            *lock(&self.shared.frame_asked) = Some(std::time::Instant::now());
            host.send_external_begin_frame();
        }
    }

    /// Ask for a new frame now (a throttled page's screenshot is fresh).
    pub fn invalidate(&self) {
        if let Some(host) = self.host() {
            host.invalidate(PaintElementType::VIEW);
        }
        self.begin_frame();
    }

    /// Asks the page to close; `Event::Closed` follows once CEF is done with it.
    pub fn close(&self) {
        self.shared.closing.store(true, Ordering::Release);
        self.menu_command(None);
        if let Some(host) = self.host() {
            host.close_browser(1);
        }
    }

    /// Answers the open context menu: run `command` (in the frame and with
    /// the facts it opened on), or `None` when it closed with no engine
    /// command picked.
    pub fn menu_command(&self, command: Option<crate::MenuCommand>) {
        let Some(callback) = lock(&self.shared.menu).take() else { return };
        match command {
            Some(command) => {
                self.shared.input();
                callback.cont(command.id(), EventFlags::default());
            }
            None => callback.cancel(),
        }
    }

    /// The image at `url` as PNG bytes, loaded in this page's profile.
    pub fn image_png(&self, url: &str) -> tokio::sync::oneshot::Receiver<Option<Vec<u8>>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Some(host) = self.host() {
            let mut callback = crate::handlers::image_callback(tx);
            host.download_image(Some(&CefString::from(url)), 0, 0, 0, Some(&mut callback));
        }
        rx
    }

    /// `action` on the video or audio element the last context menu opened on.
    pub fn media(&self, action: crate::MediaAction) {
        let Some(frame) = lock(&self.shared.menu_frame).clone() else { return };
        self.shared.input();
        let script = crate::menu::media_script(&self.shared.menu_tag, action);
        frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
    }

    /// Download `url` in this page's profile, the user picking where
    /// (`Event::DownloadAsked` has `save_as`); `name` overrides the suggested name.
    pub fn save_as(&self, url: &str, name: Option<&str>) {
        let Some(host) = self.host() else { return };
        let mut asked = lock(&self.shared.save_as);
        // A request CEF never answered (a bad URL) leaves its marker: keep the last few.
        let excess = asked.len().saturating_sub(7);
        asked.drain(..excess);
        asked.push((url.to_owned(), name.map(str::to_owned)));
        drop(asked);
        host.start_download(Some(&CefString::from(url)));
    }

    /// Answer `Event::DownloadAsked`: write download `id` to `path`, or
    /// cancel it (`None`).
    pub fn download_to(&self, id: u32, path: Option<&std::path::Path>) {
        crate::download::answer(&self.shared, id, path);
    }

    pub fn cancel_download(&self, id: u32) {
        crate::download::cancel(&self.shared, id);
    }

    /// The system's print panel for the page.
    pub fn print(&self) {
        if let Some(host) = self.host() {
            host.print();
        }
    }

    /// DevTools for the page in a window of its own, inspecting the
    /// element at `at` (view points) when given.
    pub fn inspect(&self, at: Option<(i32, i32)>) {
        if let Some(host) = self.host() {
            crate::inspector::show(&host, at);
        }
    }

    /// Whether the page's DevTools window is open.
    pub fn inspecting(&self) -> bool {
        self.host().is_some_and(|host| host.has_dev_tools() != 0)
    }

    pub fn close_inspector(&self) {
        if let Some(host) = self.host() {
            host.close_dev_tools();
        }
    }
}

fn copy_operation() -> DragOperationsMask {
    DragOperationsMask::from(sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY)
}

/// `delta` as whole points to send now and the fraction left over.
#[allow(clippy::cast_possible_truncation)]
fn whole(delta: f32) -> (i32, f32) {
    let sent = delta.trunc();
    (sent as i32, delta - sent)
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
    /// page (gpui's and CEF's shared convention). CEF takes whole points: fractions carry over
    /// to the next event, so a slow trackpad scroll moves the page as far as the fingers.
    pub fn wheel(&self, x: f32, y: f32, dx: f32, dy: f32, mods: Modifiers) {
        let (rx, ry) = *lock(&self.shared.wheel_rest);
        let ((sx, rx), (sy, ry)) = (whole(rx + dx), whole(ry + dy));
        *lock(&self.shared.wheel_rest) = (rx, ry);
        if (sx, sy) == (0, 0) {
            return;
        }
        let mut event = mouse_event(x, y, mods, &[]);
        if self.shared.wheel_relatch.load(Ordering::Relaxed) {
            event.modifiers |= crate::input::NUM_LOCK;
        }
        if let Some(host) = self.host() {
            host.send_mouse_wheel_event(Some(&event), sx, sy);
        }
    }

    /// A pinch as two touch points: Chromium's gesture recognizer turns
    /// their spread into a page-scale (visual viewport) zoom.
    ///
    /// A wheel scroll just before or during the pinch stays latched through
    /// it and then takes no more wheel input until the wheel pauses for half
    /// a second (CEF adds wheel phases by timer). Each pinch's end flips the
    /// NumLock flag of later wheel events: a change of modifiers starts a
    /// new wheel scroll at once (Macs have no NumLock key; pages see it only
    /// through `getModifierState`).
    pub fn pinch(&self, step: Pinch, points: [(f32, f32); 2]) {
        let Some(host) = self.host() else { return };
        if matches!(step, Pinch::End | Pinch::Cancel) {
            self.shared.wheel_relatch.fetch_xor(true, Ordering::Relaxed);
        }
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
        assert_eq!(frame_rate(true), HIDDEN_FRAME_RATE);
        assert!((1..=5).contains(&HIDDEN_FRAME_RATE), "a few frames a second: screenshots and rAF keep working");
    }

    #[test]
    fn wheel_fractions_carry_over() {
        let mut rest = 0.0;
        let mut sent = 0;
        for _ in 0..10 {
            let (whole, left) = whole(rest + 0.4);
            sent += whole;
            rest = left;
        }
        assert_eq!(sent, 4, "ten 0.4 pt steps scroll 4 pt, not 0 (rounded) or 10");
        assert_eq!(whole(-2.7).0, -2);
        assert!((whole(-2.7).1 + 0.7).abs() < 1e-5);
    }

    #[test]
    fn shown_pages_paint_at_the_display_rate() {
        assert_eq!(clamp_rate(120), 120, "ProMotion");
        assert_eq!(clamp_rate(0), 60, "unknown");
        assert_eq!(clamp_rate(30), 60);
        assert_eq!(clamp_rate(500), 240);
    }
}
