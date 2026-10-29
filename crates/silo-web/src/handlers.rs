//! CEF client handlers for one windowless browser. All callbacks run on the CEF UI thread,
//! which with the external message pump is the app's main thread.

#![allow(clippy::transmute_ptr_to_ptr)]
// `wrap_client!`'s generated `new` takes one argument per handler.
#![allow(clippy::too_many_arguments)]

use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
use cef::*;

use crate::frame::Frame as SurfaceFrame;
use crate::profile::{SharedTunnel, proxy_credentials};
use crate::shared::{Cursor, Event, Security, Shared, lock};

/// Console messages from `FOCUS_SCRIPT`, swallowed by the display handler.
const FOCUS_TAG: &str = "\u{1}silo-editable:";

/// Run in every frame on load: reports whether the focused element takes
/// text. A frame whose focus is in a child frame leaves the report to it.
const FOCUS_SCRIPT: &str = r"(() => {
if (window.__siloFocus) return;
window.__siloFocus = true;
const log = console.log.bind(console);
const report = () => {
  if (window !== top && !document.hasFocus()) return;
  let a = document.activeElement;
  while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement;
  if (a && (a.tagName === 'IFRAME' || a.tagName === 'FRAME')) return;
  const text = !!a && (a.isContentEditable || a.tagName === 'TEXTAREA' || a.tagName === 'SELECT'
    || (a.tagName === 'INPUT' && !/^(button|checkbox|color|file|hidden|image|radio|range|reset|submit)$/i.test(a.type)));
  log('\u0001silo-editable:' + (text ? 1 : 0));
};
addEventListener('focusin', report, true);
addEventListener('focusout', () => setTimeout(report), true);
report();
})()";

/// Run in the main frame on load (`__TAG__` replaced by the browser's
/// theme tag): reports the page's colour as `#rrggbb`, or empty for none,
/// whenever it changes. The colour is a `theme-color` meta matching the
/// current appearance, else the background of the root or body when it is
/// a plain opaque colour. Any CSS colour is normalized by painting it.
const THEME_SCRIPT: &str = r#"(() => {
if (window !== top || window.__siloTheme) return;
window.__siloTheme = true;
const log = console.log.bind(console);
const canvas = document.createElement('canvas');
canvas.width = canvas.height = 1;
const pen = canvas.getContext('2d', { willReadFrequently: true });
const hex = c => {
  if (!c || /currentcolor/i.test(c) || !CSS.supports('color', c)) return '';
  pen.clearRect(0, 0, 1, 1);
  pen.fillStyle = '#000';
  pen.fillStyle = c;
  pen.fillRect(0, 0, 1, 1);
  const [r, g, b, a] = pen.getImageData(0, 0, 1, 1).data;
  if (a === 0) return '';
  const over = v => Math.round((v * a + 255 * (255 - a)) / 255);
  return '#' + [r, g, b].map(v => over(v).toString(16).padStart(2, '0')).join('');
};
const color = () => {
  for (const m of document.querySelectorAll('meta[name="theme-color" i]')) {
    const media = m.getAttribute('media');
    const c = hex((m.getAttribute('content') || '').trim());
    if (c && (!media || matchMedia(media).matches)) return c;
  }
  for (const el of [document.documentElement, document.body]) {
    if (!el) continue;
    const style = getComputedStyle(el);
    if (style.backgroundImage !== 'none') continue;
    const c = hex(style.backgroundColor);
    if (c) return c;
  }
  return '';
};
let last;
const report = () => {
  const c = color();
  if (c !== last) { last = c; log('__TAG__' + c); }
};
let queued = false;
const soon = () => {
  if (queued) return;
  queued = true;
  setTimeout(() => { queued = false; report(); }, 100);
};
const start = () => {
  const observer = new MutationObserver(soon);
  if (document.head) observer.observe(document.head, { childList: true, subtree: true, attributes: true });
  observer.observe(document.documentElement, { attributes: true });
  if (document.body) observer.observe(document.body, { attributes: true });
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', soon);
  setInterval(report, 2500);
  report();
};
if (document.readyState === 'loading') addEventListener('DOMContentLoaded', start); else start();
addEventListener('load', report);
})()"#;

wrap_render_handler! {
    pub(crate) struct ViewHandler {
        shared: Arc<Shared>,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let view = *lock(&self.shared.view);
                *rect = Rect { x: 0, y: 0, width: view.width.max(1) as c_int, height: view.height.max(1) as c_int };
            }
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> c_int {
            let Some(info) = info else { return 0 };
            let view = *lock(&self.shared.view);
            info.device_scale_factor = view.scale;
            info.depth = 24;
            info.depth_per_component = 8;
            info.rect = Rect { x: 0, y: 0, width: view.width as c_int, height: view.height as c_int };
            info.available_rect = info.rect.clone();
            1
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            if type_ != PaintElementType::VIEW {
                return;
            }
            let Some(info) = info else { return };
            match SurfaceFrame::from_io_surface(info.shared_texture_io_surface) {
                Ok(frame) => {
                    *lock(&self.shared.frame) = Some(frame);
                    self.shared.painted();
                    self.shared.notify();
                }
                Err(err) => tracing::error!("silo-web: accelerated paint dropped: {err:#}"),
            }
        }

        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            _selected_range: Option<&Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            if let Some(first) = character_bounds.and_then(|bounds| bounds.first()) {
                let (x, y, width, height) = (first.x, first.y, first.width, first.height);
                self.shared.push(Event::ImeBounds { x, y, width, height });
            }
        }

        fn on_scroll_offset_changed(&self, _browser: Option<&mut Browser>, x: f64, y: f64) {
            *lock(&self.shared.scroll) = (x, y);
        }
    }
}

wrap_navigation_entry_visitor! {
    struct BackForward {
        found: Arc<AtomicBool>,
    }

    impl NavigationEntryVisitor {
        fn visit(&self, entry: Option<&mut NavigationEntry>, _current: c_int, _index: c_int, _total: c_int) -> c_int {
            let flag = TransitionType::FORWARD_BACK_FLAG.get_raw();
            let back_forward = entry.is_some_and(|entry| entry.transition_type().get_raw() & flag != 0);
            self.found.store(back_forward, Ordering::Relaxed);
            0
        }
    }
}

/// Whether the browser's current entry was committed by its back/forward.
fn back_forward(browser: Option<&mut Browser>) -> bool {
    let Some(host) = browser.and_then(|browser| browser.host()) else { return false };
    let found = Arc::new(AtomicBool::new(false));
    host.navigation_entries(Some(&mut BackForward::new(Arc::clone(&found))), 1);
    found.load(Ordering::Relaxed)
}

/// The visible entry's `view-source:` address, when it shows one.
fn view_source(browser: Option<&mut Browser>) -> Option<String> {
    let entry = browser?.host()?.visible_navigation_entry()?;
    Some(CefString::from(&entry.display_url()).to_string()).filter(|shown| shown.starts_with("view-source:"))
}

wrap_display_handler! {
    pub(crate) struct PageDisplayHandler {
        shared: Arc<Shared>,
    }

    impl DisplayHandler {
        fn on_address_change(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|frame| frame.is_main() == 0) {
                return;
            }
            if let Some(url) = url {
                let mut browser = browser;
                // A `view-source:` page commits its page's URL; the entry keeps what it shows.
                let url = view_source(browser.as_deref_mut()).filter(|shown| shown.strip_prefix("view-source:") == Some(&url.to_string())).unwrap_or_else(|| url.to_string());
                self.shared.committed(&url);
                let back_forward = back_forward(browser.as_deref_mut());
                self.shared.push(Event::UrlChanged { url, back_forward });
                if let Some(security) = security(&self.shared, browser) {
                    self.shared.push(Event::Security(security));
                }
            }
        }

        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: c_int,
        ) -> c_int {
            let Some(message) = message.map(ToString::to_string) else { return 0 };
            if let Some(color) = message.strip_prefix(self.shared.theme_tag.as_str()) {
                self.shared.push(Event::ThemeColor(crate::color::parse(color)));
                return 1;
            }
            if let Some(link_text) = message.strip_prefix(self.shared.menu_tag.as_str()) {
                *lock(&self.shared.menu_report) = Some((std::time::Instant::now(), link_text.to_owned()));
                return 1;
            }
            let Some(state) = message.strip_prefix(FOCUS_TAG) else { return 0 };
            self.shared.push(Event::EditableFocus(state == "1"));
            1
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(title) = title {
                self.shared.push(Event::TitleChanged(title.to_string()));
            }
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: *mut u8,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> c_int {
            self.shared.push(Event::CursorChanged(cursor(type_)));
            1
        }

        fn on_loading_progress_change(&self, _browser: Option<&mut Browser>, progress: f64) {
            self.shared.push(Event::Progress(progress));
        }

        fn on_favicon_urlchange(&self, browser: Option<&mut Browser>, icon_urls: Option<&mut CefStringList>) {
            // cef-rs `CefStringList::clone` copies the opaque list handle by value (dangling), so
            // take the borrowed list instead; dropping a borrowed list does not free it.
            let url = icon_urls.and_then(|urls| std::mem::take(urls).into_iter().find(|url| !url.trim().is_empty()));
            let (Some(host), Some(url)) = (browser.and_then(|browser| browser.host()), url) else { return };
            let mut callback = FaviconCallback::new(Arc::clone(&self.shared));
            host.download_image(Some(&CefString::from(url.as_str())), 1, 32, 0, Some(&mut callback));
        }
    }
}

/// Favicons larger than this are dropped (server stores them in `BrowserState`).
const FAVICON_LIMIT: usize = 32 * 1024;

/// Chromium's certificate errors (`CERT_STATUS_ALL_ERRORS` without the
/// minor revocation-check ones).
const CERT_ERRORS: u32 = 0xFF00_FFFF & !(16 | 32);
/// Seconds from 1601-01-01 (`base::Time`) to the Unix epoch.
const WINDOWS_EPOCH: i64 = 11_644_473_600;

/// The connection facts of `browser`'s visible entry.
fn security(shared: &Shared, browser: Option<&mut Browser>) -> Option<Security> {
    let entry = browser?.host()?.visible_navigation_entry()?;
    let url = CefString::from(&entry.url()).to_string();
    let refused = lock(&shared.cert_failed).as_deref() == Some(url.as_str());
    let ssl = entry.sslstatus();
    let cert = ssl.as_ref().and_then(|s| s.x509_certificate());
    let content = ssl.as_ref().map_or(0, |s| s.content_status().get_raw());
    Some(Security {
        secure: ssl.as_ref().is_some_and(|s| s.is_secure_connection() != 0),
        cert_error: refused || ssl.as_ref().is_some_and(|s| s.cert_status().get_raw() & CERT_ERRORS != 0),
        mixed: content != 0,
        issuer: cert.as_ref().and_then(|c| c.issuer()).map(|p| CefString::from(&p.display_name()).to_string()).filter(|name| !name.is_empty()),
        valid_until: cert.map(|c| c.valid_expiry().val).filter(|&t| t > 0).map(|t| t / 1_000_000 - WINDOWS_EPOCH),
        url,
    })
}

wrap_download_image_callback! {
    pub(crate) struct FaviconCallback {
        shared: Arc<Shared>,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(&self, _url: Option<&CefString>, status: c_int, image: Option<&mut Image>) {
            if status != 0 && !(200..400).contains(&status) {
                return;
            }
            let Some(bytes) = image.and_then(png) else { return };
            if bytes.is_empty() || bytes.len() > FAVICON_LIMIT {
                return;
            }
            let url = format!("data:image/png;base64,{}", data_encoding::BASE64.encode(&bytes));
            self.shared.push(Event::FaviconChanged(url));
        }
    }
}

/// `image` encoded as PNG at 1x.
fn png(image: &mut Image) -> Option<Vec<u8>> {
    // libcef rejects null out-params, so the pixel size must be passed even though unused.
    let (mut width, mut height) = (0, 0);
    let png = image.as_png(1.0, 1, Some(&mut width), Some(&mut height))?;
    let mut bytes = vec![0; png.size()];
    let copied = png.data(Some(&mut bytes), 0);
    bytes.truncate(copied);
    Some(bytes)
}

type ImageReply = Arc<Mutex<Option<tokio::sync::oneshot::Sender<Option<Vec<u8>>>>>>;

wrap_download_image_callback! {
    pub(crate) struct ImageCallback {
        reply: ImageReply,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(&self, _url: Option<&CefString>, status: c_int, image: Option<&mut Image>) {
            let ok = status == 0 || (200..400).contains(&status);
            let bytes = image.filter(|_| ok).and_then(png).filter(|bytes| !bytes.is_empty());
            if let Some(reply) = lock(&self.reply).take() {
                let _ = reply.send(bytes);
            }
        }
    }
}

pub(crate) fn image_callback(reply: tokio::sync::oneshot::Sender<Option<Vec<u8>>>) -> DownloadImageCallback {
    ImageCallback::new(Arc::new(Mutex::new(Some(reply))))
}

wrap_load_handler! {
    pub(crate) struct PageLoadHandler {
        shared: Arc<Shared>,
    }

    impl LoadHandler {
        fn on_loading_state_change(&self, browser: Option<&mut Browser>, loading: c_int, _back: c_int, _forward: c_int) {
            self.shared.push(Event::Loading(loading != 0));
            if loading == 0
                && let Some(security) = security(&self.shared, browser)
            {
                self.shared.push(Event::Security(security));
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            _error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // net::IsCertificateError: -200..=-299.
            let cert = (-299..=-200).contains(&error_code.get_raw());
            if cert && frame.is_some_and(|frame| frame.is_main() != 0) {
                *lock(&self.shared.cert_failed) = failed_url.map(ToString::to_string);
            }
        }

        // A cross-document navigation gets a new render widget that starts unfocused, and Blink
        // drops IME input (not key events) for unfocused widgets, so re-apply the focus.
        fn on_load_start(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, _transition_type: TransitionType) {
            let Some(frame) = frame else { return };
            frame.execute_java_script(Some(&CefString::from(FOCUS_SCRIPT)), None, 0);
            frame.execute_java_script(Some(&CefString::from(crate::menu::menu_script(&self.shared.menu_tag).as_str())), None, 0);
            let main = frame.is_main() != 0;
            if main {
                // The old page's colour goes with it.
                self.shared.push(Event::ThemeColor(None));
                let script = THEME_SCRIPT.replace("__TAG__", &self.shared.theme_tag);
                frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
                self.shared.push(Event::EditableFocus(false));
            }
            if main && self.shared.focused.load(Ordering::Relaxed)
                && let Some(host) = browser.and_then(|browser| browser.host())
            {
                host.set_focus(1);
            }
        }
    }
}

/// Popups a page may have waiting for CEF to create them; more are refused.
const PENDING_POPUPS: usize = 8;

wrap_life_span_handler! {
    pub(crate) struct PageLifeSpanHandler {
        shared: Arc<Shared>,
        tunnel: SharedTunnel,
    }

    impl LifeSpanHandler {
        // A new tab: CEF creates the popup windowless with this crate's handlers (its opener link,
        // `postMessage` and `closed`, intact) and the app adopts it from `Event::NewTab`.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            popup_id: c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            _user_gesture: c_int,
            popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut c_int>,
        ) -> c_int {
            let Some(foreground) = crate::popup::tab_for(target_disposition) else { return 1 };
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            let size = popup_features.filter(|f| f.width_set != 0 && f.height_set != 0).map(|f| (f.width, f.height));
            let mut pending = lock(&self.shared.popups);
            pending.retain(|(_, popup)| lock(&popup.browser).is_none() && !popup.closing.load(Ordering::Acquire));
            let (Some(window_info), Some(client), Some(settings)) = (window_info, client, settings) else { return 1 };
            if pending.len() >= PENDING_POPUPS {
                return 1;
            }
            let popup = crate::popup::Popup::shared(*lock(&self.shared.view));
            *window_info = crate::browser::window_info();
            *settings = crate::browser::settings(!foreground);
            popup.frame_rate.store(crate::browser::frame_rate(!foreground), Ordering::Relaxed);
            *client = Some(crate::handlers::client(&popup, &self.tunnel));
            pending.push((popup_id, Arc::clone(&popup)));
            drop(pending);
            crate::runtime::browser_opened();
            self.shared.push(Event::NewTab(crate::NewTab { url, foreground, popup: Some(crate::Popup(popup)), size }));
            0
        }

        fn on_before_popup_aborted(&self, _browser: Option<&mut Browser>, popup_id: c_int) {
            let mut pending = lock(&self.shared.popups);
            let Some(index) = pending.iter().position(|(id, _)| *id == popup_id) else { return };
            let (_, popup) = pending.remove(index);
            drop(pending);
            popup.push(Event::Closed { by_page: true });
            crate::runtime::browser_closed();
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let queued = lock(&self.shared.queued).take();
            if let (Some(url), Some(frame)) = (queued, browser.as_ref().and_then(|browser| browser.main_frame())) {
                frame.load_url(Some(&CefString::from(url.as_str())));
            }
            // A popup the app closed before CEF had created it.
            if self.shared.closing.load(Ordering::Acquire)
                && let Some(host) = browser.as_ref().and_then(|browser| browser.host())
            {
                host.close_browser(1);
            }
            *lock(&self.shared.browser) = browser.map(|browser| browser.clone());
            self.shared.notify();
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.shared.devtools.close();
            lock(&self.shared.browser).take();
            if let Some(menu) = lock(&self.shared.menu).take() {
                menu.cancel();
            }
            lock(&self.shared.menu_frame).take();
            self.shared.push(Event::Closed { by_page: !self.shared.closing.load(Ordering::Acquire) });
            crate::runtime::browser_closed();
        }
    }
}

wrap_context_menu_handler! {
    pub(crate) struct PageMenuHandler {
        shared: Arc<Shared>,
    }

    impl ContextMenuHandler {
        // The app draws the menu (`Event::ContextMenu`) and answers with `Browser::menu_command`.
        fn run_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            _model: Option<&mut MenuModel>,
            callback: Option<&mut RunContextMenuCallback>,
        ) -> c_int {
            let (Some(params), Some(callback)) = (params, callback) else { return 0 };
            let menu = crate::menu::ContextMenu::from_params(params, self.shared.take_menu_report());
            *lock(&self.shared.menu_frame) = frame.map(|frame| frame.clone());
            if let Some(previous) = lock(&self.shared.menu).replace(callback.clone()) {
                previous.cancel();
            }
            self.shared.push(Event::ContextMenu(menu));
            1
        }
    }
}

wrap_request_handler! {
    pub(crate) struct PageRequestHandler {
        shared: Arc<Shared>,
        tunnel: SharedTunnel,
    }

    impl RequestHandler {
        // A navigation asking for another tab that CEF doesn't make a popup of (e.g. a ⌘-clicked
        // link): it opens fresh in a new tab, this page stays.
        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            _user_gesture: c_int,
        ) -> c_int {
            let Some(foreground) = crate::popup::tab_for(target_disposition) else { return 0 };
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            self.shared.push(Event::NewTab(crate::NewTab { url, foreground, popup: None, size: None }));
            1
        }

        // UI thread. A file drop the page didn't handle never navigates the block.
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: c_int,
            _is_redirect: c_int,
        ) -> c_int {
            let url = request.map(|r| CefString::from(&r.url()).to_string()).unwrap_or_default();
            c_int::from(self.shared.drop_navigation(&url))
        }

        // IO thread. Answers only proxy challenges from this profile's own tunnel proxy; anything
        // else (origin 401s, other proxies) is cancelled, never prompted or auto-filled.
        fn auth_credentials(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            is_proxy: c_int,
            host: Option<&CefString>,
            port: c_int,
            _realm: Option<&CefString>,
            _scheme: Option<&CefString>,
            callback: Option<&mut AuthCallback>,
        ) -> c_int {
            let host = host.map(ToString::to_string).unwrap_or_default();
            let creds = (is_proxy != 0).then(|| proxy_credentials(&self.tunnel, &host, port)).flatten();
            let (Some((user, pass)), Some(callback)) = (creds, callback) else { return 0 };
            callback.cont(Some(&CefString::from(user.as_str())), Some(&CefString::from(pass.as_str())));
            1
        }
    }
}

wrap_permission_handler! {
    pub(crate) struct PagePermissionHandler {
        shared: Arc<Shared>,
    }

    impl PermissionHandler {
        // A clipboard read (`navigator.clipboard.read*`) is allowed once per user input to
        // the page and others are dismissed. Chromium persists the grant and counts
        // dismissals toward an embargo, so both are cleared and every read asks again.
        // Other prompts keep CEF's default.
        fn on_show_permission_prompt(
            &self,
            browser: Option<&mut Browser>,
            _prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> c_int {
            let clipboard = PermissionRequestTypes::CLIPBOARD.get_raw();
            let Some(callback) = callback.filter(|_| requested_permissions == clipboard) else { return 0 };
            let gesture = self.shared.take_gesture();
            callback.cont(if gesture { PermissionRequestResult::ACCEPT } else { PermissionRequestResult::DISMISS });
            let context = browser.as_ref().and_then(|b| b.host()).and_then(|h| h.request_context());
            if let (Some(context), Some(origin)) = (context, requesting_origin) {
                // Owned copies: the borrowed origin is only valid during this call.
                let origin = CefString::from(origin.to_string().as_str());
                let top = browser.and_then(|b| b.main_frame()).map_or_else(|| origin.to_string(), |f| CefString::from(&f.url()).to_string());
                // Chromium checks the setting again when it reads, so keep the grant briefly.
                let mut task = ForgetClipboard::new(context, origin, CefString::from(top.as_str()));
                post_delayed_task(ThreadId::UI, Some(&mut task), FORGET_CLIPBOARD_MS);
            }
            1
        }
    }
}

const FORGET_CLIPBOARD_MS: i64 = 1000;

wrap_task! {
    struct ForgetClipboard {
        context: RequestContext,
        origin: CefString,
        top: CefString,
    }

    impl Task {
        fn execute(&self) {
            let (origin, top) = (Some(&self.origin), Some(&self.top));
            self.context.set_content_setting(origin, top, ContentSettingTypes::CLIPBOARD_READ_WRITE, ContentSettingValues::DEFAULT);
            self.context.set_website_setting(origin, top, ContentSettingTypes::PERMISSION_AUTOBLOCKER_DATA, None);
        }
    }
}

wrap_jsdialog_handler! {
    struct AgentDialogs {
        shared: Arc<Shared>,
    }

    impl JsdialogHandler {
        fn on_jsdialog(
            &self, _browser: Option<&mut Browser>, _origin_url: Option<&CefString>, _dialog_type: JsdialogType,
            _message_text: Option<&CefString>, _default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>, _suppress_message: Option<&mut c_int>,
        ) -> c_int {
            self.shared.devtools.hold_dialog(callback)
        }

        fn on_before_unload_dialog(
            &self, _browser: Option<&mut Browser>, _message_text: Option<&CefString>, _is_reload: c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> c_int {
            self.shared.devtools.hold_dialog(callback)
        }

        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            self.shared.devtools.reset_dialog();
        }
    }
}

// Keys the page leaves unhandled stop here. Without it CEF offers each to
// `[[NSApp mainMenu] performKeyEquivalent:]`, where a bare `f` matches
// AppKit's fn-F "Enter Full Screen" (typing toggled full screen); the app has
// already run its own shortcuts.
wrap_keyboard_handler! {
    struct PageKeys {}

    impl KeyboardHandler {
        fn on_key_event(&self, _browser: Option<&mut Browser>, _event: Option<&KeyEvent>, _os_event: *mut u8) -> c_int {
            1
        }
    }
}

wrap_client! {
    pub(crate) struct PageClient {
        view: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
        request: RequestHandler,
        permission: PermissionHandler,
        dialogs: JsdialogHandler,
        menu: ContextMenuHandler,
        downloads: DownloadHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.view.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }

        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(self.permission.clone())
        }

        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(self.dialogs.clone())
        }

        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(PageKeys::new())
        }

        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.menu.clone())
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.downloads.clone())
        }
    }
}

pub(crate) fn client(shared: &Arc<Shared>, tunnel: &SharedTunnel) -> Client {
    PageClient::new(
        ViewHandler::new(Arc::clone(shared)),
        PageDisplayHandler::new(Arc::clone(shared)),
        PageLoadHandler::new(Arc::clone(shared)),
        PageLifeSpanHandler::new(Arc::clone(shared), Arc::clone(tunnel)),
        PageRequestHandler::new(Arc::clone(shared), Arc::clone(tunnel)),
        PagePermissionHandler::new(Arc::clone(shared)),
        AgentDialogs::new(Arc::clone(shared)),
        PageMenuHandler::new(Arc::clone(shared)),
        crate::download::handler(shared),
    )
}

fn cursor(kind: CursorType) -> Cursor {
    match kind {
        CursorType::HAND => Cursor::Hand,
        CursorType::IBEAM | CursorType::VERTICALTEXT => Cursor::IBeam,
        CursorType::CROSS | CursorType::CELL => Cursor::Crosshair,
        CursorType::WAIT | CursorType::PROGRESS => Cursor::Wait,
        CursorType::NOTALLOWED | CursorType::NODROP => Cursor::NotAllowed,
        CursorType::EASTRESIZE | CursorType::WESTRESIZE | CursorType::EASTWESTRESIZE | CursorType::COLUMNRESIZE => Cursor::ResizeColumn,
        CursorType::NORTHRESIZE | CursorType::SOUTHRESIZE | CursorType::NORTHSOUTHRESIZE | CursorType::ROWRESIZE => Cursor::ResizeRow,
        CursorType::GRAB => Cursor::Grab,
        CursorType::GRABBING => Cursor::Grabbing,
        CursorType::MOVE => Cursor::Move,
        CursorType::NONE => Cursor::None,
        _ => Cursor::Arrow,
    }
}
