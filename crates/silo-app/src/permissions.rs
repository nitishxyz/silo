//! The permission helper: a small floating panel that helps the user grant
//! a macOS privacy permission to Silo Server (`app.permissions_show`,
//! Settings › Permissions › Grant).
//!
//! TCC charges every program in a Silo terminal to the server (its
//! responsible process), so the permission goes to Silo Server's bundle,
//! the runtime copy the server runs from (`permissions.status`'s
//! `bundle_path`; this app's own `Contents/Helpers` copy until it is
//! known). System Settings takes an app dropped into its list, so the panel
//! shows that bundle as a native drag tile ([`native`]: gpui can't drag out
//! of the app), with Open System Settings (which then moves the panel
//! beside the Settings window, [`place`]) and the status, polled every
//! second while the panel is open; once granted it says so and closes.
//!
//! One panel at a time ([`show`] retargets it). The window has a fixed
//! size and a fixed layout, so the native drag view gets a fixed frame
//! ([`TILE`]) instead of following gpui's layout.

use std::{path::PathBuf, time::Duration};

use gpui::{
    App, Bounds, Context, FontWeight, Global, IntoElement, Task, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, div, prelude::*, px, rgb, size,
};
use silo_proto::{
    control::Method,
    permissions::{PermissionId, PermissionStatus, Permissions},
};

use crate::{assets::icon, theme, theme::Continuous as _};

/// The panel's size, in points.
pub const PANEL_W: f32 = 340.0;
pub const PANEL_H: f32 = 200.0;
/// The drag tile, in window points from the top left. The native drag
/// view sits exactly here (the layout below is absolute and fixed).
pub const TILE: Frame = Frame { x: 16.0, y: 48.0, w: 104.0, h: 116.0 };
/// The bundle icon in the tile: this size, this far below its top.
const ICON: f32 = 56.0;
const ICON_TOP: f32 = 12.0;
// The tile, its icon and the note below it fit the panel.
const _: () = assert!(TILE.x + TILE.w < PANEL_W && TILE.y + TILE.h + 8.0 + 14.0 < PANEL_H && ICON_TOP + ICON < TILE.h);
/// Space between the panel and the System Settings window.
const GAP: f32 = 12.0;
/// How often the open panel checks the status.
const POLL: Duration = Duration::from_secs(1);
/// How long "Granted ✓" shows before the panel closes.
const CLOSE_AFTER: Duration = Duration::from_millis(1500);
/// How long to look for the System Settings window after opening it.
const SETTINGS_WAIT: Duration = Duration::from_secs(2);
const SETTINGS_EVERY: Duration = Duration::from_millis(150);

/// A rectangle with a top-left origin, y down: window points, or global
/// display points (CoreGraphics' space: the primary display's top left is
/// 0,0).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Frame {
    fn right(self) -> f32 {
        self.x + self.w
    }

    fn bottom(self) -> f32 {
        self.y + self.h
    }

    fn overlap(self, other: Self) -> f32 {
        let w = (self.right().min(other.right()) - self.x.max(other.x)).max(0.0);
        let h = (self.bottom().min(other.bottom()) - self.y.max(other.y)).max(0.0);
        w * h
    }
}

/// Where the panel (`panel`: width, height) goes beside the System Settings
/// window `settings`, on the screen it is mostly on (`screens`: their
/// visible frames, global): `GAP` to its right, else to its left, else
/// over its right edge; its middle level with the top third of the
/// window, kept on the screen. The panel's top-left corner, global.
#[must_use]
pub fn place(settings: Frame, panel: (f32, f32), screens: &[Frame]) -> (f32, f32) {
    let (w, h) = panel;
    // No screens known: nothing to keep it on.
    const ANYWHERE: Frame = Frame { x: -1.0e7, y: -1.0e7, w: 2.0e7, h: 2.0e7 };
    let screen = screens.iter().copied().max_by(|a, b| a.overlap(settings).total_cmp(&b.overlap(settings))).unwrap_or(ANYWHERE);
    let right = settings.right() + GAP;
    let left = settings.x - GAP - w;
    let x = if right + w <= screen.right() {
        right
    } else if left >= screen.x {
        left
    } else {
        (screen.right() - w).max(screen.x)
    };
    let y = settings.y + settings.h / 3.0 - h / 2.0;
    let y = y.min(screen.bottom() - h).max(screen.y);
    (x, y)
}

/// This Mac's server connection and the app's runtime, if connected.
fn local(cx: &App) -> Option<(silo_client::Control, tokio::runtime::Handle)> {
    let hosts = crate::hosts_state::HostsState::try_global(cx)?;
    let hosts = hosts.read(cx);
    let control = hosts.entries().iter().find(|h| h.info.local && !h.removed)?.control.clone()?;
    Some((control, hosts.rt.clone()))
}

/// `permissions.status` from this Mac's server; `None` while not connected.
pub fn fetch(cx: &App) -> Option<impl Future<Output = anyhow::Result<Permissions>> + use<>> {
    let (control, rt) = local(cx)?;
    let call = rt.spawn(async move { control.call::<Permissions>(Method::PermissionsStatus {}).await });
    Some(async move { call.await? })
}

/// Settings' Grant: `permissions.request` (macOS's own dialog for Screen
/// Recording, then the server has this app show the panel); the panel
/// right away if the server can't.
pub fn request(id: PermissionId, cx: &mut App) {
    let Some((control, rt)) = local(cx) else {
        show(id, cx);
        return;
    };
    let call = rt.spawn(async move { control.call_value(Method::PermissionsRequest { id }).await });
    cx.spawn(async move |cx| {
        let answered = call.await;
        if !matches!(answered, Ok(Ok(_))) {
            if let Ok(Err(error)) = &answered {
                tracing::debug!("permissions.request {id}: {error:#}");
            }
            cx.update(|cx| show(id, cx));
        }
    })
    .detach();
}

/// The open panel, if any.
#[derive(Default)]
struct Sidecar(Option<WindowHandle<PermissionsPanel>>);

impl Global for Sidecar {}

/// Show the panel for `id`: the open one, retargeted and brought forward,
/// or a new one.
pub fn show(id: PermissionId, cx: &mut App) {
    if id == PermissionId::Unknown {
        return;
    }
    if let Some(handle) = cx.try_global::<Sidecar>().and_then(|s| s.0) {
        let shown = handle.update(cx, |panel, window, cx| {
            panel.retarget(id, cx);
            window.activate_window();
        });
        if shown.is_ok() {
            cx.activate(true);
            return;
        }
    }
    let bounds = Bounds::centered(None, size(px(PANEL_W), px(PANEL_H)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(format!("{} permission", id.title()).into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        kind: WindowKind::Floating,
        is_resizable: false,
        is_minimizable: false,
        app_owns_titlebar_drag: true,
        window_background: WindowBackgroundAppearance::Opaque,
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        native::configure(window);
        let bundle = fallback_bundle();
        let drag = bundle.as_deref().and_then(|b| native::DragTile::attach(window, TILE, b));
        cx.new(|cx| PermissionsPanel::new(id, bundle, drag, window, cx))
    });
    match opened {
        Ok(handle) => {
            cx.set_global(Sidecar(Some(handle)));
            cx.activate(true);
        }
        Err(error) => tracing::warn!("permissions panel: {error:#}"),
    }
}

/// Close the panel, if open.
fn close(cx: &mut App) {
    if let Some(handle) = cx.try_global::<Sidecar>().and_then(|s| s.0) {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
    cx.set_global(Sidecar(None));
}

/// The server bundle before `permissions.status` names the runtime copy:
/// this app's `Contents/Helpers` one.
fn fallback_bundle() -> Option<PathBuf> {
    let exe = crate::conn::server_exe();
    silo_proto::permissions::server_bundle(&exe).filter(|b| b.exists())
}

pub struct PermissionsPanel {
    id: PermissionId,
    /// `None` until the first answer.
    status: Option<PermissionStatus>,
    /// Why the status can't be read (no server).
    problem: Option<&'static str>,
    bundle: Option<PathBuf>,
    bundle_name: String,
    drag: Option<native::DragTile>,
    _poll: Task<()>,
    closing: Option<Task<()>>,
    placing: Option<Task<()>>,
}

impl PermissionsPanel {
    fn new(id: PermissionId, bundle: Option<PathBuf>, drag: Option<native::DragTile>, window: &Window, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                let fetch = cx.update(|_, cx| fetch(cx)).ok().flatten();
                let answer = match fetch {
                    Some(call) => Some(call.await),
                    None => None,
                };
                if this.update_in(cx, |this, window, cx| this.apply(answer, window, cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(POLL).await;
            }
        });
        Self {
            id,
            status: None,
            problem: None,
            bundle,
            bundle_name: silo_proto::CHANNEL.server_name().into(),
            drag,
            _poll: poll,
            closing: None,
            placing: None,
        }
    }

    fn retarget(&mut self, id: PermissionId, cx: &mut Context<Self>) {
        if id != self.id {
            self.id = id;
            self.status = None;
        }
        self.closing = None;
        cx.notify();
    }

    fn apply(&mut self, answer: Option<anyhow::Result<Permissions>>, window: &Window, cx: &mut Context<Self>) {
        match answer {
            Some(Ok(permissions)) => {
                self.problem = None;
                self.status = Some(permissions.get(self.id).map_or(PermissionStatus::Unknown, |p| p.status));
                self.bundle_name = permissions.bundle_name.clone();
                if let Some(bundle) = permissions.bundle_path.filter(|b| b.exists())
                    && self.bundle.as_ref() != Some(&bundle)
                {
                    match &self.drag {
                        Some(drag) => drag.set_bundle(&bundle),
                        None => self.drag = native::DragTile::attach(window, TILE, &bundle),
                    }
                    self.bundle = Some(bundle);
                }
            }
            Some(Err(error)) => {
                tracing::debug!("permissions.status: {error:#}");
                self.problem = Some("Silo Server can't check permissions");
            }
            None => self.problem = Some("Silo Server is not running"),
        }
        if self.status.is_some_and(PermissionStatus::granted) && self.closing.is_none() {
            self.closing = Some(cx.spawn(async move |_, cx| {
                cx.background_executor().timer(CLOSE_AFTER).await;
                cx.update(close);
            }));
        }
        cx.notify();
    }

    /// Open System Settings at the permission, then move beside its window
    /// once it shows (or stay put).
    fn open_settings(&mut self, cx: &mut Context<Self>) {
        native::open_url(self.id.settings_url());
        self.placing = Some(cx.spawn(async move |_, cx| {
            let mut waited = Duration::ZERO;
            while waited < SETTINGS_WAIT {
                cx.background_executor().timer(SETTINGS_EVERY).await;
                waited += SETTINGS_EVERY;
                if let Some(settings) = native::settings_window() {
                    let (x, y) = place(settings, (PANEL_W, PANEL_H), &native::screens());
                    cx.update(|cx| {
                        if let Some(handle) = cx.try_global::<Sidecar>().and_then(|s| s.0) {
                            let _ = handle.update(cx, |_, window, _| native::move_window(window, x, y));
                        }
                    });
                    return;
                }
            }
        }));
    }

    fn status_line(&self) -> (String, u32) {
        match (self.status, self.problem) {
            (Some(PermissionStatus::Granted), _) => ("Granted ✓".into(), theme::success()),
            (_, Some(problem)) => (problem.into(), theme::warning()),
            (None, None) => ("Checking…".into(), theme::muted()),
            (Some(_), None) => ("Not granted yet".into(), theme::muted()),
        }
    }
}

impl Render for PermissionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.id;
        let (status, status_color) = self.status_line();
        let granted = self.status.is_some_and(PermissionStatus::granted);
        let tile = div()
            .absolute()
            .left(px(TILE.x))
            .top(px(TILE.y))
            .w(px(TILE.w))
            .h(px(TILE.h))
            .continuous(10.0)
            .border_1()
            .border_color(rgb(theme::border()))
            .bg(theme::well_fill())
            .flex()
            .flex_col()
            .items_center()
            .child(
                // The native view draws the bundle's icon here (see `native`).
                div()
                    .mt(px(ICON_TOP))
                    .size(px(ICON))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(self.drag.is_none(), |d| d.child(icon("lock", 28.0, theme::muted()))),
            )
            .child(div().mt(px(6.0)).text_size(px(11.5)).font_weight(FontWeight::MEDIUM).whitespace_nowrap().child(self.bundle_name.clone()))
            .child(div().mt(px(2.0)).text_size(px(10.5)).text_color(rgb(theme::muted())).whitespace_nowrap().child("Drag into the list"));
        let button = div()
            .id("permissions-open-settings")
            .h(px(28.0))
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .continuous(8.0)
            .bg(rgb(theme::accent()))
            .text_color(rgb(theme::on_accent()))
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .child("Open System Settings")
            .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)));
        div()
            .id("permissions-panel")
            .size_full()
            .relative()
            .overflow_hidden()
            .font_family(theme::ui_font())
            .text_size(px(12.0))
            .text_color(rgb(theme::foreground()))
            .bg(rgb(theme::popover()))
            // The top strip moves the window (there is no titlebar).
            .child(
                div()
                    .id("permissions-header")
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(px(40.0))
                    .pl(px(16.0))
                    .pr(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .on_mouse_down(gpui::MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(div().flex_1().min_w_0().text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).whitespace_nowrap().child(id.title()))
                    .child(
                        div()
                            .id("permissions-close")
                            .flex_none()
                            .size(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .continuous(11.0)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::hover_fill()))
                            .child(icon("close", 11.0, theme::muted()))
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(|_, _, cx| cx.defer(close)),
                    ),
            )
            .child(tile)
            .child(
                div()
                    .absolute()
                    .left(px(TILE.right() + 12.0))
                    .top(px(TILE.y))
                    .w(px(PANEL_W - TILE.right() - 12.0 - 16.0))
                    .h(px(TILE.h))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(div().text_size(px(12.0)).line_height(px(16.0)).text_color(rgb(theme::muted())).child(id.reason()))
                    .child(div().text_size(px(12.0)).font_weight(FontWeight::MEDIUM).text_color(rgb(status_color)).child(status))
                    .child(div().flex_1())
                    .when(!granted, |c| c.child(button)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(16.0))
                    .top(px(TILE.bottom() + 8.0))
                    .w(px(PANEL_W - 32.0))
                    .text_size(px(10.5))
                    .text_color(rgb(theme::muted()))
                    .whitespace_nowrap()
                    .child("Programs already running may need restarting."),
            )
    }
}

#[cfg(target_os = "macos")]
mod native {
    //! AppKit and CoreGraphics: the drag tile, the window's chrome, opening
    //! System Settings and finding its window.

    use std::{cell::RefCell, ffi::c_void, path::Path};

    use objc2::{
        AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
        rc::Retained,
        runtime::{AnyObject, NSObjectProtocol, ProtocolObject},
    };
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSImage,
        NSImageView, NSRunningApplication, NSScreen, NSView, NSWindow, NSWorkspace,
    };
    use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use super::{Frame, ICON, ICON_TOP};

    /// System Settings' bundle id (its window's owner).
    const SETTINGS_APP: &str = "com.apple.systempreferences";
    /// And its name, should the bundle id not be found.
    const SETTINGS_NAME: &str = "System Settings";

    pub struct DragIvars {
        /// A press in the view that has not started a drag yet.
        armed: std::cell::Cell<bool>,
        path: RefCell<String>,
        icon: RefCell<Option<Retained<NSImage>>>,
        image_view: RefCell<Option<Retained<NSImageView>>>,
    }

    define_class!(
        /// The drag tile's native view: above gpui's view, it takes every
        /// click in its frame, and a drag drags the bundle's file URL.
        #[unsafe(super(NSView))]
        #[thread_kind = MainThreadOnly]
        #[name = "SiloPermissionDragView"]
        #[ivars = DragIvars]
        pub struct DragView;

        impl DragView {
            #[unsafe(method(hitTest:))]
            fn hit_test(&self, point: NSPoint) -> *mut AnyObject {
                let frame = self.frame();
                let inside = point.x >= frame.origin.x
                    && point.x < frame.origin.x + frame.size.width
                    && point.y >= frame.origin.y
                    && point.y < frame.origin.y + frame.size.height;
                if inside { (self as *const Self).cast_mut().cast() } else { std::ptr::null_mut() }
            }

            #[unsafe(method(acceptsFirstMouse:))]
            fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
                true
            }

            #[unsafe(method(mouseDownCanMoveWindow))]
            fn mouse_down_can_move_window(&self) -> bool {
                false
            }

            // Never passed on to gpui's view or the window (which would
            // move): the press arms the drag, the first move starts it.
            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, _event: &NSEvent) {
                self.ivars().armed.set(true);
            }

            #[unsafe(method(mouseUp:))]
            fn mouse_up(&self, _event: &NSEvent) {
                self.ivars().armed.set(false);
            }

            #[unsafe(method(mouseDragged:))]
            fn mouse_dragged(&self, event: &NSEvent) {
                if self.ivars().armed.replace(false) {
                    self.begin_drag(event);
                }
            }
        }

        unsafe impl NSObjectProtocol for DragView {}

        unsafe impl NSDraggingSource for DragView {
            #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
            fn source_operation(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
                NSDragOperation::Copy
            }
        }
    );

    impl DragView {
        fn icon_frame(&self) -> NSRect {
            let bounds = self.bounds();
            let x = (bounds.size.width - f64::from(ICON)) / 2.0;
            let top = f64::from(ICON_TOP);
            let y = if self.isFlipped() { top } else { bounds.size.height - top - f64::from(ICON) };
            NSRect::new(NSPoint::new(x, y), NSSize::new(f64::from(ICON), f64::from(ICON)))
        }

        fn set_bundle(&self, path: &Path, mtm: MainThreadMarker) {
            let text = path.to_string_lossy().into_owned();
            let image = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&text));
            image.setSize(NSSize::new(f64::from(ICON), f64::from(ICON)));
            let view = self.ivars().image_view.borrow().clone();
            match view {
                Some(view) => view.setImage(Some(&image)),
                None => {
                    let view = NSImageView::imageViewWithImage(&image, mtm);
                    view.setFrame(self.icon_frame());
                    // Never a drop target of its own.
                    view.unregisterDraggedTypes();
                    self.addSubview(&view);
                    *self.ivars().image_view.borrow_mut() = Some(view);
                }
            }
            *self.ivars().path.borrow_mut() = text;
            *self.ivars().icon.borrow_mut() = Some(image);
        }

        fn begin_drag(&self, event: &NSEvent) {
            let path = self.ivars().path.borrow().clone();
            if path.is_empty() {
                return;
            }
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path));
            let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*url));
            let icon = self.ivars().icon.borrow().clone();
            // SAFETY: the contents is an NSImage, as the method expects.
            unsafe { item.setDraggingFrame_contents(self.icon_frame(), icon.as_deref().map(|i| i.as_ref())) };
            let items = NSArray::from_retained_slice(&[item]);
            let _session = self.beginDraggingSessionWithItems_event_source(&items, event, ProtocolObject::from_ref(self));
        }
    }

    /// The drag tile over a panel window's gpui view.
    pub struct DragTile(Retained<DragView>);

    impl DragTile {
        /// Add the view at `tile` (window points, top left) showing and
        /// dragging `bundle`: above gpui's view, a sibling in the content
        /// view (which gpui's view fills), so hit testing finds it first.
        pub fn attach(window: &gpui::Window, tile: Frame, bundle: &Path) -> Option<Self> {
            let mtm = MainThreadMarker::new()?;
            let gpui = gpui_view(window)?;
            // SAFETY: the live content view (gpui's view's superview).
            let parent = unsafe { gpui.superview() };
            let host: &NSView = parent.as_deref().unwrap_or(gpui);
            let bounds = host.bounds();
            let y = if host.isFlipped() { f64::from(tile.y) } else { bounds.size.height - f64::from(tile.y) - f64::from(tile.h) };
            let frame = NSRect::new(NSPoint::new(f64::from(tile.x), y), NSSize::new(f64::from(tile.w), f64::from(tile.h)));
            let ivars = DragIvars {
                armed: std::cell::Cell::new(false),
                path: RefCell::new(String::new()),
                icon: RefCell::new(None),
                image_view: RefCell::new(None),
            };
            // SAFETY: NSView's designated initializer, on fresh ivars.
            let view: Retained<DragView> = unsafe { msg_send![super(DragView::alloc(mtm).set_ivars(ivars)), initWithFrame: frame] };
            // Pinned to the top left as the window keeps its size.
            if !host.isFlipped() {
                view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
            }
            view.set_bundle(bundle, mtm);
            host.addSubview(&view);
            Some(Self(view))
        }

        pub fn set_bundle(&self, bundle: &Path) {
            if let Some(mtm) = MainThreadMarker::new() {
                self.0.set_bundle(bundle, mtm);
            }
        }
    }

    /// gpui's view of `window`.
    fn gpui_view(window: &gpui::Window) -> Option<&NSView> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else { return None };
        // SAFETY: the AppKit handle wraps the window's live NSView, which
        // lives as long as the window.
        unsafe { appkit.ns_view.cast::<NSView>().as_ref().into() }
    }

    fn ns_window(window: &gpui::Window) -> Option<Retained<NSWindow>> {
        gpui_view(window)?.window()
    }

    /// A floating panel's chrome: it stays up while System Settings is in
    /// front (panels hide when their app deactivates), without traffic
    /// lights (it has its own close button).
    pub fn configure(window: &gpui::Window) {
        let Some(ns_window) = ns_window(window) else { return };
        ns_window.setHidesOnDeactivate(false);
        for button in 0..3usize {
            // SAFETY: standardWindowButton: takes NSWindowButton (0 close,
            // 1 miniaturize, 2 zoom) and may return nil.
            unsafe {
                let button: *mut AnyObject = msg_send![&*ns_window, standardWindowButton: button];
                if let Some(button) = button.as_ref() {
                    let _: () = msg_send![button, setHidden: true];
                }
            }
        }
    }

    pub fn open_url(url: &str) {
        let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
        if !NSWorkspace::sharedWorkspace().openURL(&url) {
            tracing::warn!("could not open {url:?}");
        }
    }

    /// The primary screen's height: Cocoa's y runs up from its bottom.
    fn primary_height(mtm: MainThreadMarker) -> Option<f64> {
        NSScreen::screens(mtm).firstObject().map(|s| s.frame().size.height)
    }

    /// Every screen's visible frame, global (top left, y down).
    pub fn screens() -> Vec<Frame> {
        let Some(mtm) = MainThreadMarker::new() else { return Vec::new() };
        let Some(primary) = primary_height(mtm) else { return Vec::new() };
        NSScreen::screens(mtm)
            .iter()
            .map(|screen| {
                let f = screen.visibleFrame();
                #[allow(clippy::cast_possible_truncation)]
                Frame { x: f.origin.x as f32, y: (primary - f.origin.y - f.size.height) as f32, w: f.size.width as f32, h: f.size.height as f32 }
            })
            .collect()
    }

    /// Move `window`'s top-left corner to `x`, `y` (global).
    pub fn move_window(window: &gpui::Window, x: f32, y: f32) {
        let (Some(mtm), Some(ns_window)) = (MainThreadMarker::new(), ns_window(window)) else { return };
        let Some(primary) = primary_height(mtm) else { return };
        ns_window.setFrameTopLeftPoint(NSPoint::new(f64::from(x), primary - f64::from(y)));
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *const c_void;
    }

    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP: u32 = 1 << 4;

    /// The System Settings window's frame (global), from the window list:
    /// owners and bounds need no Screen Recording permission.
    pub fn settings_window() -> Option<Frame> {
        let pids: Vec<i32> = NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(SETTINGS_APP))
            .iter()
            .map(|a| {
                // SAFETY: `processIdentifier` returns a pid_t.
                let pid: i32 = unsafe { msg_send![&*a, processIdentifier] };
                pid
            })
            .collect();
        // SAFETY: a Copy-rule CFArray of CFDictionaries, toll-free bridged
        // to NSArray and NSDictionary; `Retained` releases it.
        let list: Retained<NSArray<AnyObject>> = unsafe {
            let raw = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
            Retained::from_raw(raw.cast_mut().cast())?
        };
        let key = |name: &str| NSString::from_str(name);
        let (owner_pid, owner_name, layer, bounds) =
            (key("kCGWindowOwnerPID"), key("kCGWindowOwnerName"), key("kCGWindowLayer"), key("kCGWindowBounds"));
        list.iter().find_map(|info| {
            // SAFETY: each entry is an NSDictionary of NSNumbers, NSStrings
            // and a bounds dictionary of NSNumbers; nil-checked.
            unsafe {
                let get = |dict: &AnyObject, key: &NSString| -> Option<&AnyObject> {
                    let value: *mut AnyObject = msg_send![dict, objectForKey: key];
                    value.as_ref()
                };
                let number = |dict: &AnyObject, key: &NSString| -> Option<f64> {
                    let value = get(dict, key)?;
                    Some(msg_send![value, doubleValue])
                };
                if number(&info, &layer)? != 0.0 {
                    return None;
                }
                #[allow(clippy::cast_possible_truncation)]
                let pid = number(&info, &owner_pid)? as i32;
                let named = || {
                    get(&info, &owner_name).is_some_and(|name| {
                        let name: &NSString = &*std::ptr::from_ref(name).cast::<NSString>();
                        name.to_string() == SETTINGS_NAME
                    })
                };
                if !pids.contains(&pid) && !named() {
                    return None;
                }
                let b = get(&info, &bounds)?;
                let (x, y, w, h) = (number(b, &key("X"))?, number(b, &key("Y"))?, number(b, &key("Width"))?, number(b, &key("Height"))?);
                // Tiny windows are not the settings window (a sheet's shadow).
                #[allow(clippy::cast_possible_truncation)]
                (w > 200.0 && h > 200.0).then_some(Frame { x: x as f32, y: y as f32, w: w as f32, h: h as f32 })
            }
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use std::path::Path;

    use super::Frame;

    pub struct DragTile;

    impl DragTile {
        pub fn attach(_window: &gpui::Window, _tile: Frame, _bundle: &Path) -> Option<Self> {
            None
        }
        pub fn set_bundle(&self, _bundle: &Path) {}
    }

    pub fn configure(_window: &gpui::Window) {}
    pub fn open_url(url: &str) {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    pub fn screens() -> Vec<Frame> {
        Vec::new()
    }
    pub fn move_window(_window: &gpui::Window, _x: f32, _y: f32) {}
    pub fn settings_window() -> Option<Frame> {
        None
    }
}

/// System Settings › Privacy & Security › Automation.
pub fn open_automation_settings() {
    native::open_url(silo_proto::permissions::AUTOMATION_URL);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Frame = Frame { x: 0.0, y: 25.0, w: 1728.0, h: 1080.0 };
    const PANEL: (f32, f32) = (PANEL_W, PANEL_H);

    #[test]
    fn the_panel_goes_right_of_settings_level_with_its_top_third() {
        let settings = Frame { x: 400.0, y: 100.0, w: 715.0, h: 600.0 };
        let (x, y) = place(settings, PANEL, &[SCREEN]);
        assert_eq!(x, 400.0 + 715.0 + GAP);
        assert_eq!(y, 100.0 + 200.0 - PANEL_H / 2.0, "its middle at the top third");
    }

    #[test]
    fn without_room_on_the_right_it_goes_left() {
        let settings = Frame { x: 1000.0, y: 100.0, w: 715.0, h: 600.0 };
        let (x, _) = place(settings, PANEL, &[SCREEN]);
        assert_eq!(x, 1000.0 - GAP - PANEL_W);
    }

    #[test]
    fn without_room_either_side_it_overlaps_the_right_edge() {
        let settings = Frame { x: 100.0, y: 100.0, w: 1500.0, h: 600.0 };
        let (x, _) = place(settings, PANEL, &[SCREEN]);
        assert_eq!(x, SCREEN.right() - PANEL_W);
    }

    #[test]
    fn it_stays_on_the_screen_vertically() {
        let high = Frame { x: 400.0, y: 25.0, w: 715.0, h: 150.0 };
        assert_eq!(place(high, PANEL, &[SCREEN]).1, SCREEN.y, "not under the menu bar");
        let low = Frame { x: 400.0, y: 1000.0, w: 715.0, h: 600.0 };
        assert_eq!(place(low, PANEL, &[SCREEN]).1, SCREEN.bottom() - PANEL_H);
    }

    #[test]
    fn it_uses_the_screen_settings_is_on() {
        // A second screen to the right of the first.
        let right = Frame { x: 1728.0, y: 0.0, w: 1920.0, h: 1055.0 };
        let settings = Frame { x: 2900.0, y: 200.0, w: 715.0, h: 600.0 };
        let (x, _) = place(settings, PANEL, &[SCREEN, right]);
        assert_eq!(x, 2900.0 - GAP - PANEL_W, "no room right of it on its own screen");
        // Settings on the first screen, room on its right before the edge.
        let settings = Frame { x: 900.0, y: 200.0, w: 715.0, h: 600.0 };
        let (x, _) = place(settings, PANEL, &[SCREEN, right]);
        assert_eq!(x, 900.0 - GAP - PANEL_W, "the right side would cross onto the other screen");
        // No screens known: to its right.
        assert_eq!(place(settings, PANEL, &[]), (900.0 + 715.0 + GAP, 200.0 + 200.0 - PANEL_H / 2.0));
    }
}
