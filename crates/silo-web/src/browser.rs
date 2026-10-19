//! One windowless browser (a browser block's engine side).

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use cef::*;

use crate::frame::Frame as SurfaceFrame;
use crate::input::{Modifiers, MouseButton, mouse_event};
use crate::keyboard::{KeyInput, key_events};
use crate::shared::{Event, Shared, ViewSize, lock};
use crate::{Profile, Runtime};

pub struct BrowserOptions<'a> {
    pub url: &'a str,
    pub profile: &'a Profile,
    pub size: ViewSize,
    /// Called (on the main thread) after each new frame or event.
    pub notify: Arc<dyn Fn() + Send + Sync>,
}

/// Main-thread handle; dropping it does not close the page, call `close`.
pub struct Browser {
    shared: Arc<Shared>,
}

impl Runtime {
    pub fn create_browser(&self, options: BrowserOptions) -> Result<Browser> {
        let shared = Arc::new(Shared {
            view: Mutex::new(options.size),
            frame: Mutex::new(None),
            browser: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            notify: options.notify,
        });
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            shared_texture_enabled: 1,
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        };
        let settings = BrowserSettings {
            windowless_frame_rate: 60,
            background_color: 0xFFFF_FFFF,
            ..Default::default()
        };
        let mut client = crate::handlers::client(&shared);
        let mut context = options.profile.context.clone();
        let url = CefString::from(options.url);
        let created = browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&url),
            Some(&settings),
            None,
            Some(&mut context),
        );
        if created != 1 {
            bail!("browser_host_create_browser failed");
        }
        crate::runtime::browser_opened();
        Ok(Browser { shared })
    }
}

impl Browser {
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

    pub fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *lock(&self.shared.events))
    }

    pub fn navigate(&self, url: &str) {
        if let Some(frame) = self.page().and_then(|page| page.main_frame()) {
            frame.load_url(Some(&CefString::from(url)));
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
            if ignore_cache {
                page.reload_ignore_cache()
            } else {
                page.reload()
            }
        }
    }

    pub fn stop(&self) {
        if let Some(page) = self.page() {
            page.stop_load();
        }
    }

    /// CEF zoom level (0.0 = 100%, each step ≈ ×1.2).
    pub fn set_zoom(&self, level: f64) {
        if let Some(host) = self.host() {
            host.set_zoom_level(level);
        }
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
        if let Some(host) = self.host() {
            host.set_focus(i32::from(focused));
        }
    }

    pub fn set_hidden(&self, hidden: bool) {
        if let Some(host) = self.host() {
            host.was_hidden(i32::from(hidden));
        }
    }

    /// Asks the page to close; `Event::Closed` follows once CEF is done with it.
    pub fn close(&self) {
        if let Some(host) = self.host() {
            host.close_browser(1);
        }
    }
}

/// Input, in view points (DIP) relative to the browser's top-left.
impl Browser {
    pub fn mouse_move(&self, x: f32, y: f32, mods: Modifiers, held: &[MouseButton]) {
        if let Some(host) = self.host() {
            host.send_mouse_move_event(Some(&mouse_event(x, y, mods, held)), 0);
        }
    }

    pub fn mouse_leave(&self, mods: Modifiers) {
        if let Some(host) = self.host() {
            host.send_mouse_move_event(Some(&mouse_event(-1.0, -1.0, mods, &[])), 1);
        }
    }

    pub fn mouse_button(
        &self,
        x: f32,
        y: f32,
        button: MouseButton,
        down: bool,
        clicks: u32,
        mods: Modifiers,
    ) {
        let held: &[MouseButton] = if down { &[button] } else { &[] };
        let event = mouse_event(x, y, mods, held);
        if let Some(host) = self.host() {
            host.send_mouse_click_event(
                Some(&event),
                button.cef(),
                i32::from(!down),
                clicks.max(1) as i32,
            );
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

    pub fn key(&self, input: &KeyInput) {
        for event in key_events(input) {
            if let Some(host) = self.host() {
                host.send_key_event(Some(&event));
            }
        }
    }

    /// Inserts committed text (typing, IME commit, paste-like input).
    pub fn insert_text(&self, text: &str) {
        if let Some(host) = self.host() {
            host.ime_commit_text(Some(&CefString::from(text)), None, 0);
        }
    }

    /// Marked (composing) IME text with the caret/selection at `selection` (UTF-16 units).
    pub fn ime_set_composition(&self, text: &str, selection: std::ops::Range<u32>) {
        let underline = CompositionUnderline {
            range: Range {
                from: 0,
                to: text.encode_utf16().count() as u32,
            },
            color: 0xFF00_0000,
            background_color: 0,
            ..Default::default()
        };
        let selection = Range {
            from: selection.start,
            to: selection.end,
        };
        if let Some(host) = self.host() {
            host.ime_set_composition(
                Some(&CefString::from(text)),
                Some(&[underline]),
                None,
                Some(&selection),
            );
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
