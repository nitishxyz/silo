//! CEF client handlers for one windowless browser. All callbacks run on the CEF UI thread,
//! which with the external message pump is the app's main thread.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cef::rc::Rc as _;
use cef::*;

use crate::frame::Frame as SurfaceFrame;
use crate::profile::{SharedTunnel, proxy_credentials};
use crate::shared::{Cursor, Event, Shared, lock};

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
                    (self.shared.notify)();
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
    }
}

wrap_display_handler! {
    pub(crate) struct PageDisplayHandler {
        shared: Arc<Shared>,
    }

    impl DisplayHandler {
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|frame| frame.is_main() == 0) {
                return;
            }
            if let Some(url) = url {
                self.shared.push(Event::UrlChanged(url.to_string()));
            }
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

wrap_download_image_callback! {
    pub(crate) struct FaviconCallback {
        shared: Arc<Shared>,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(&self, _url: Option<&CefString>, status: c_int, image: Option<&mut Image>) {
            if status != 0 && !(200..400).contains(&status) {
                return;
            }
            // libcef rejects null out-params, so the pixel size must be passed even though unused.
            let (mut width, mut height) = (0, 0);
            let png = image.and_then(|image| image.as_png(1.0, 1, Some(&mut width), Some(&mut height)));
            let Some(png) = png else { return };
            let mut bytes = vec![0; png.size()];
            let copied = png.data(Some(&mut bytes), 0);
            bytes.truncate(copied);
            if bytes.is_empty() || bytes.len() > FAVICON_LIMIT {
                return;
            }
            let url = format!("data:image/png;base64,{}", data_encoding::BASE64.encode(&bytes));
            self.shared.push(Event::FaviconChanged(url));
        }
    }
}

wrap_load_handler! {
    pub(crate) struct PageLoadHandler {
        shared: Arc<Shared>,
    }

    impl LoadHandler {
        fn on_loading_state_change(&self, _browser: Option<&mut Browser>, loading: c_int, back: c_int, forward: c_int) {
            self.shared.push(Event::Loading { loading: loading != 0, can_go_back: back != 0, can_go_forward: forward != 0 });
        }

        // A cross-document navigation gets a new render widget that starts unfocused, and Blink
        // drops IME input (not key events) for unfocused widgets, so re-apply the focus.
        fn on_load_start(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, _transition_type: TransitionType) {
            let main = frame.is_some_and(|frame| frame.is_main() != 0);
            if main && self.shared.focused.load(Ordering::Relaxed)
                && let Some(host) = browser.and_then(|browser| browser.host())
            {
                host.set_focus(1);
            }
        }
    }
}

wrap_life_span_handler! {
    pub(crate) struct PageLifeSpanHandler {
        shared: Arc<Shared>,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut c_int>,
        ) -> c_int {
            if let Some(url) = target_url.map(ToString::to_string).filter(|url| !url.is_empty()) {
                self.shared.push(Event::Popup(url));
            }
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            *lock(&self.shared.browser) = browser.map(|browser| browser.clone());
            (self.shared.notify)();
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            lock(&self.shared.browser).take();
            self.shared.push(Event::Closed);
            crate::runtime::browser_closed();
        }
    }
}

wrap_request_handler! {
    pub(crate) struct PageRequestHandler {
        tunnel: SharedTunnel,
    }

    impl RequestHandler {
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

wrap_client! {
    pub(crate) struct PageClient {
        view: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
        request: RequestHandler,
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
    }
}

pub(crate) fn client(shared: &Arc<Shared>, tunnel: &SharedTunnel) -> Client {
    PageClient::new(
        ViewHandler::new(Arc::clone(shared)),
        PageDisplayHandler::new(Arc::clone(shared)),
        PageLoadHandler::new(Arc::clone(shared)),
        PageLifeSpanHandler::new(Arc::clone(shared)),
        PageRequestHandler::new(Arc::clone(tunnel)),
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
