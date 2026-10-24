//! One browser block: a CEF page painted as a surface, navigation and the
//! address in the pane header ([`crate::pane_header::BlockHeader`]), engine
//! state reported to the server (as owner) and server commands applied.

use std::time::{Duration, Instant};

use gpui::AppContext as _;
use silo_proto::{
    control::{BrowserCommand, Method},
    model::{BlockInfo, BlockKind, BrowserState, HistoryEntry},
};

/// Engine changes are reported at most this often.
pub const REPORT_DEBOUNCE: Duration = Duration::from_millis(250);

/// Which pane a block gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Terminal,
    Browser,
    /// An otto session; a "Not supported" card on remote hosts.
    Agent,
    /// A newer server's block kind: no pane, a "Not supported" card.
    Unsupported,
}

#[must_use]
pub fn kind_of(info: Option<&BlockInfo>) -> Kind {
    match info.map(|i| i.kind) {
        Some(BlockKind::Browser) => Kind::Browser,
        Some(BlockKind::Agent) => Kind::Agent,
        Some(BlockKind::Unknown) => Kind::Unsupported,
        _ => Kind::Terminal,
    }
}

/// What fills a block's icon slot (pane header, tab).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Spinner,
    Favicon,
    Glyph(&'static str),
}

/// Loading beats the page's favicon, which beats the kind's glyph.
#[must_use]
pub fn icon_for(kind: Kind, loading: bool, favicon: bool) -> Icon {
    match kind {
        Kind::Terminal => Icon::Glyph("terminal"),
        Kind::Browser if loading => Icon::Spinner,
        Kind::Browser if favicon => Icon::Favicon,
        Kind::Browser => Icon::Glyph("globe"),
        Kind::Agent => Icon::Glyph(crate::agent_block::ICON),
        Kind::Unsupported => Icon::Glyph("info"),
    }
}

/// The address strip's text for `url`: no scheme for http(s), nothing for
/// `about:blank`.
#[must_use]
pub fn display_url(url: &str) -> String {
    if url == "about:blank" {
        return String::new();
    }
    let bare = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    bare.strip_suffix('/').filter(|b| !b.contains('/')).unwrap_or(bare).to_owned()
}

/// Debounces engine-state reports: the latest state is sent once changes
/// have settled for [`REPORT_DEBOUNCE`] (or after 4× that under churn).
#[derive(Default)]
pub struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    pub fn changed(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// Whether to send now; resets when it says yes.
    pub fn due(&mut self, now: Instant) -> bool {
        let (Some(first), Some(last)) = (self.first, self.last) else { return false };
        let ready = now - last >= REPORT_DEBOUNCE || now - first >= REPORT_DEBOUNCE * 4;
        if ready {
            *self = Self::default();
        }
        ready
    }

    pub fn pending(&self) -> bool {
        self.last.is_some()
    }
}

/// The block's history entries `lo..=hi` the live engine reaches with its
/// own back/forward; outside them an entry is loaded afresh (`jump`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    lo: usize,
    hi: usize,
    jump: Option<usize>,
}

impl Span {
    /// An engine just opened at entry `index`.
    #[must_use]
    pub fn at(index: usize) -> Self {
        Self { lo: index, hi: index, jump: None }
    }
}

/// How Back (`-1`) or Forward (`1`) moves through the block's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The engine has the entry: its own back/forward.
    Engine,
    /// Load this entry.
    Load(usize),
}

/// Back/Forward are bounded by the block's history alone.
#[must_use]
pub fn step(state: &BrowserState, span: Span, delta: isize) -> Option<Step> {
    let target = state.index.checked_add_signed(delta).filter(|&i| i < state.history.len())?;
    Some(if (span.lo..=span.hi).contains(&target) { Step::Engine } else { Step::Load(target) })
}

/// Follow the engine's committed URL in `state`'s history: the loaded
/// entry of a jump (a redirect replaces it), a neighbour entry for the
/// engine's back/forward, the current one a reload or same-page change,
/// anything else a new navigation. A blank commit while a page entry is
/// loading is the engine's initial document: ignored (false).
pub fn follow_url(state: &mut BrowserState, span: &mut Span, url: &str, back_forward: bool) -> bool {
    let loading_page = span.jump.and_then(|t| state.history.get(t)).is_some_and(|e| !silo_web::is_blank(&e.url));
    if loading_page && silo_web::is_blank(url) {
        return false;
    }
    if let Some(target) = span.jump.take().filter(|&t| t < state.history.len()) {
        #[allow(clippy::cast_possible_wrap)]
        state.go(target as isize - state.index as isize);
        if state.url != url {
            url.clone_into(&mut state.history[target].url);
            url.clone_into(&mut state.url);
        }
        *span = Span::at(target);
        return true;
    }
    if state.url == url {
        return true;
    }
    let engine = span.lo..=span.hi;
    let at = |i: Option<usize>| {
        back_forward && i.filter(|i| engine.contains(i)).and_then(|i| state.history.get(i)).is_some_and(|e: &HistoryEntry| e.url == url)
    };
    if at(state.index.checked_sub(1)) {
        state.go(-1);
        span.lo = span.lo.min(state.index);
    } else if at(state.index.checked_add(1)) {
        state.go(1);
        span.hi = span.hi.max(state.index);
    } else {
        let removed = (state.index + 2).saturating_sub(BrowserState::HISTORY);
        state.push(url.to_owned());
        *span = Span { lo: span.lo.saturating_sub(removed), hi: state.index, jump: None };
    }
    true
}

/// An app chord on a focused page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chord {
    Address,
    Back,
    Forward,
    Reload,
    Stop,
    Edit(silo_web::Edit),
    /// ⌘+ (1), ⌘− (-1), ⌘0 (0): page zoom.
    Zoom(i8),
}

/// ⌘L / ⌘R / ⌘. / ⌘+ / ⌘− / ⌘0 and ⌘X / ⌘Z / ⇧⌘Z on the page, and ⌘← / ⌘→
/// unless a page text field has focus (there they move the caret). Nothing
/// for the address bar. ⌘C / ⌘V / ⌘A are app commands that reach the page too.
fn page_chord(key: &str, m: gpui::Modifiers, page_focused: bool, editable: bool) -> Option<Chord> {
    if !page_focused || !m.platform || m.control || m.alt {
        return None;
    }
    if m.shift {
        return match key {
            "=" | "+" => Some(Chord::Zoom(1)),
            _ => key.eq_ignore_ascii_case("z").then_some(Chord::Edit(silo_web::Edit::Redo)),
        };
    }
    match key {
        "l" => Some(Chord::Address),
        "r" => Some(Chord::Reload),
        "." => Some(Chord::Stop),
        "=" | "+" => Some(Chord::Zoom(1)),
        "-" => Some(Chord::Zoom(-1)),
        "0" => Some(Chord::Zoom(0)),
        "x" => Some(Chord::Edit(silo_web::Edit::Cut)),
        "z" => Some(Chord::Edit(silo_web::Edit::Undo)),
        "left" if !editable => Some(Chord::Back),
        "right" if !editable => Some(Chord::Forward),
        _ => None,
    }
}

/// CEF page zoom levels ⌘+ / ⌘− step through (≈ 58%–249%).
const ZOOM_LEVELS: std::ops::RangeInclusive<f32> = -3.0..=5.0;

/// The zoom level after `step` (0 resets) from `level`, on half-level steps.
fn zoom_step(level: f32, step: i8) -> f32 {
    if step == 0 {
        return 0.0;
    }
    let next = ((level * 2.0).round() + f32::from(step)) / 2.0;
    next.clamp(*ZOOM_LEVELS.start(), *ZOOM_LEVELS.end())
}

pub fn follow_title(state: &mut BrowserState, title: &str) {
    state.title = title.to_owned();
    if let Some(entry) = state.history.get_mut(state.index) {
        entry.title = title.to_owned();
    }
}

/// The owner's `browser.report` for `state`.
#[must_use]
pub fn report(block_id: &str, state: &BrowserState) -> Method {
    Method::BrowserReport {
        block_id: block_id.to_owned(),
        url: state.url.clone(),
        title: state.title.clone(),
        favicon: state.favicon.clone(),
        loading: state.loading,
        history: state.history.clone(),
        index: state.index,
    }
}

pub enum BrowserPaneEvent {
    Activated,
    /// The page asked for a new window.
    Popup(String),
    /// The page's colour changed.
    Backdrop,
    /// The header's settings button: Settings › Browser.
    OpenSettings,
}

impl gpui::EventEmitter<BrowserPaneEvent> for BrowserPane {}

/// Where the pane's engine profile comes from.
#[derive(Clone)]
pub enum Route {
    Local,
    Remote { host_id: String, home: std::path::PathBuf, addr: silo_client::transport::EndpointAddr },
}

pub struct BrowserPane {
    pub block_id: String,
    rt: tokio::runtime::Handle,
    control: Option<silo_client::Control>,
    route: Route,
    home: std::path::PathBuf,
    pub info: Option<BlockInfo>,
    /// The engine's state as this client sees it (reported when owner).
    state: Option<BrowserState>,
    page: Option<std::rc::Rc<silo_web::Browser>>,
    actions: Option<std::rc::Rc<crate::browser_actions::Executor>>,
    starting: bool,
    pub focus: gpui::FocusHandle,
    pub address: gpui::Entity<crate::text_field::TextField>,
    /// Page bounds from the last paint (window coordinates).
    bounds: Option<gpui::Bounds<gpui::Pixels>>,
    scale: f32,
    held: Vec<silo_web::MouseButton>,
    /// An external file drag is over the page (CEF saw drag-enter).
    file_drag: bool,
    composing: Option<String>,
    gate: crate::composition::InputGate,
    /// IME candidate anchor in page points.
    ime_rect: Option<(i32, i32, i32, i32)>,
    cursor: silo_web::Cursor,
    /// The address strip has focus: server/engine URLs don't overwrite it.
    editing: bool,
    span: Span,
    /// A page text field has focus.
    editable: bool,
    /// Load progress, 0.0–1.0.
    progress: f64,
    /// The page's colour (`theme-color` or background).
    theme_color: Option<u32>,
    /// The colours of the card behind the toolbar.
    palette: crate::adaptive::Chrome,
    /// Radii of the page's bottom corners: the card's, inside its border.
    corners: (f32, f32),
    debounce: Debounce,
    report_task: Option<gpui::Task<()>>,
    error: Option<String>,
    /// No page to show: Silo's empty state instead of the engine's surface,
    /// kept until a real page has painted.
    empty: bool,
    /// The recent page ↓/↑ picked from the address bar.
    selected: Option<usize>,
    /// What the engine shows or is loading.
    engine_url: String,
    /// The decoded favicon of `state.favicon`.
    favicon_image: Option<(String, std::sync::Arc<gpui::Image>)>,
    /// The engine's facts about its page's connection.
    security: Option<silo_web::Security>,
    /// The security popover is open.
    security_open: bool,
    /// A URL the engine refused for its certificate (its interstitial
    /// shows, nothing commits): the bar shows it instead.
    blocked: Option<String>,
    /// Two-finger horizontal swipes that navigate.
    swipe: crate::gesture::SwipeTracker,
    /// "Swipe between pages" is on, as read when the gesture began.
    swipe_enabled: bool,
    swipe_arrow: Option<SwipeArrow>,
    /// Whether an element under the pointer can scroll toward its start or
    /// end, as the page answered for the current gesture, and that query.
    swipe_scrollers: Option<[bool; 2]>,
    swipe_query: u64,
    /// The touch points of a pinch or smart zoom in flight.
    pinch: Option<crate::gesture::PinchTouch>,
    smart_zoom: Option<gpui::Task<()>>,
    /// The page scale as pinched here, reset by navigation.
    page_scale: f32,
    /// In the off-screen host (`browser_host.rs`): the page opens without
    /// a layout, at the last (or a default) size.
    offscreen: bool,
    /// In a window, but in a tab it doesn't show: rendered slowly like the host's.
    hidden: bool,
    /// The page's first document committed (or its first load ended):
    /// actions before it would act on the engine's blank start page.
    committed: bool,
    _subs: Vec<gpui::Subscription>,
    /// Focus subscriptions of the window showing the pane.
    window_subs: Vec<gpui::Subscription>,
}

/// The back/forward arrow a navigation swipe pulls in from the page's edge.
struct SwipeArrow {
    direction: crate::gesture::Direction,
    progress: f32,
    shown: crate::motion::Toggle,
}

impl BrowserPane {
    /// A pane in no window yet: [`Self::attach`] shows it in one, the
    /// off-screen host runs it without one.
    pub fn new(
        block_id: String,
        rt: tokio::runtime::Handle,
        control: Option<silo_client::Control>,
        route: Route,
        home: std::path::PathBuf,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let address = cx.new(|cx| crate::text_field::TextField::new("Search or enter address", cx));
        let mut subs = vec![intercept(cx.weak_entity(), cx)];
        subs.push(cx.observe(&crate::hosts_state::HostsState::global(cx), |this, _, cx| {
            if this.empty {
                cx.notify();
            }
        }));
        Self {
            block_id,
            rt,
            control,
            route,
            home,
            info: None,
            state: None,
            page: None,
            actions: None,
            starting: false,
            focus,
            address,
            bounds: None,
            scale: 2.0,
            held: Vec::new(),
            file_drag: false,
            composing: None,
            gate: Default::default(),
            ime_rect: None,
            cursor: silo_web::Cursor::Arrow,
            editing: false,
            span: Span::default(),
            editable: false,
            progress: 1.0,
            theme_color: None,
            palette: crate::adaptive::Chrome::on(crate::theme::background()),
            corners: (0.0, 0.0),
            debounce: Debounce::default(),
            report_task: None,
            error: None,
            empty: false,
            selected: None,
            engine_url: String::new(),
            favicon_image: None,
            security: None,
            security_open: false,
            blocked: None,
            swipe: crate::gesture::SwipeTracker::default(),
            swipe_enabled: false,
            swipe_arrow: None,
            swipe_scrollers: None,
            swipe_query: 0,
            pinch: None,
            smart_zoom: None,
            page_scale: 1.0,
            offscreen: false,
            hidden: false,
            committed: false,
            _subs: subs,
            window_subs: Vec::new(),
        }
    }

    /// Show the pane in `window` (on creation, or adopted from the host).
    pub fn attach(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let address_focus = self.address.read(cx).focus.clone();
        self.window_subs = vec![
            cx.on_focus(&self.focus, window, |this, _, cx| this.focused(true, cx)),
            cx.on_blur(&self.focus, window, |this, _, cx| this.focused(false, cx)),
            cx.subscribe_in(&self.address, window, |this, _, event, window, cx| this.address_event(event, window, cx)),
            cx.on_blur(&address_focus, window, |this, _, cx| {
                this.editing = false;
                this.sync_address(cx);
            }),
            cx.on_focus(&address_focus, window, |this, _, cx| {
                this.editing = true;
                this.security_open = false;
                this.address.update(cx, |field, cx| field.select_all(cx));
                this.own(silo_proto::control::OwnMode::Take);
                cx.emit(BrowserPaneEvent::Activated);
            }),
        ];
        self.offscreen = false;
        if let Some(page) = &self.page {
            page.set_offscreen(self.hidden);
        }
    }

    /// Whether the window's shown tab has this pane (a page in another tab renders slowly).
    pub(crate) fn set_hidden(&mut self, hidden: bool) {
        if self.hidden != hidden {
            self.hidden = hidden;
            if let Some(page) = self.page.as_ref().filter(|_| !self.offscreen) {
                page.set_offscreen(hidden);
            }
        }
    }

    /// Run in the off-screen host: no window; a page not open yet opens on
    /// the next server state or [`Self::open_offscreen`].
    pub(crate) fn detach(&mut self) {
        self.window_subs.clear();
        self.held.clear();
        self.editing = false;
        if let Some(page) = &self.page {
            page.set_focus(false);
            page.set_offscreen(true);
        }
        self.offscreen = true;
    }

    /// Open the page without a layout (an agent acts on it).
    pub(crate) fn open_offscreen(&mut self, cx: &mut gpui::Context<Self>) {
        self.offscreen = true;
        self.ensure_page(cx);
    }

    /// Whether the page exists, answers CDP and shows its first document.
    pub(crate) fn page_ready(&self) -> bool {
        self.committed && self.page.as_ref().is_some_and(|p| p.ready())
    }

    /// The page could not open (bad address, engine failure).
    pub(crate) fn page_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// A block of this Mac's server (only those are hosted off screen).
    pub(crate) fn is_local(&self) -> bool {
        matches!(self.route, Route::Local) && self.control.is_some()
    }
}

impl BrowserPane {
    /// A reconnect: retry a page that failed to open.
    pub fn set_control(&mut self, control: silo_client::Control, cx: &mut gpui::Context<Self>) {
        self.control = Some(control);
        self.retry(cx);
    }

    fn retry(&mut self, cx: &mut gpui::Context<Self>) {
        if self.page.is_none() && self.error.take().is_some() {
            self.ensure_page(cx);
            cx.notify();
        }
    }

    /// Server state: the first one opens the page; later ones are only
    /// adopted while another client owns the engine state.
    pub fn set_info(&mut self, info: BlockInfo, cx: &mut gpui::Context<Self>) {
        if self.info.as_ref() == Some(&info) {
            return;
        }
        let server = info.browser.clone();
        self.info = Some(info);
        if (self.state.is_none() || (!self.is_owner() && !self.debounce.pending()))
            && let Some(server) = server
        {
            let fresh = self.state.is_none();
            self.span = Span::at(server.index);
            self.state = Some(server);
            self.sync_address(cx);
            self.refresh_empty(cx);
            self.decode_favicon();
            if fresh {
                self.ensure_page(cx);
            } else if let Some(page) = self.page.clone()
                && self.state.as_ref().is_some_and(|s| s.url != self.engine_url)
            {
                // The engine shows other state (it opened from the cached
                // tree, or the owner followed a link): load the server's.
                self.load_entry(self.span.lo, &page);
            }
        }
        cx.notify();
    }

    pub(crate) fn is_owner(&self) -> bool {
        let me = self.control.as_ref().map(silo_client::Control::client_id);
        self.info.as_ref().is_some_and(|i| i.owner.is_some() && i.owner == me)
    }

    pub(crate) fn agent_executor(&mut self, cx: &mut gpui::Context<Self>) -> anyhow::Result<std::rc::Rc<crate::browser_actions::Executor>> {
        if !self.is_owner() {
            anyhow::bail!("this app does not own the browser block");
        }
        self.executor(cx)
    }

    /// The page's executor, whoever owns the block (the off-screen host
    /// has just claimed it).
    pub(crate) fn executor(&mut self, cx: &mut gpui::Context<Self>) -> anyhow::Result<std::rc::Rc<crate::browser_actions::Executor>> {
        if let Some(actions) = &self.actions {
            return Ok(actions.clone());
        }
        let page = self.page.clone().ok_or_else(|| anyhow::anyhow!("browser is not ready"))?;
        let actions = crate::browser_actions::Executor::new(page, cx);
        self.actions = Some(actions.clone());
        Ok(actions)
    }

    /// Nobody owns the block (its last owner left).
    pub(crate) fn unowned(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.owner.is_none())
    }

    /// Own the block if nobody does.
    pub(crate) fn claim(&self) {
        self.own(silo_proto::control::OwnMode::Request);
    }

    /// Take the engine state (`Take`), or claim it only if nobody holds it
    /// (`Request`): a page this app runs serves agents' browser actions even
    /// before the user focuses it.
    fn own(&self, mode: silo_proto::control::OwnMode) {
        if self.is_owner() {
            return;
        }
        let Some(control) = self.control.clone() else { return };
        let block = self.block_id.clone();
        self.rt.spawn(async move {
            let _ = control.call_value(Method::OwnBlock { block_ids: vec![block], mode }).await;
        });
    }

    fn focused(&mut self, focused: bool, cx: &mut gpui::Context<Self>) {
        if let Some(page) = &self.page {
            page.set_focus(focused);
        }
        if focused {
            self.own(silo_proto::control::OwnMode::Take);
            cx.emit(BrowserPaneEvent::Activated);
        }
        cx.notify();
    }

    pub fn title(&self) -> String {
        let state = self.state.as_ref().filter(|s| !silo_web::is_blank(&s.url));
        match state.map(|s| s.title.trim()).filter(|t| !t.is_empty()) {
            Some(title) => title.to_owned(),
            None => state.map(|s| display_url(&s.url)).filter(|u| !u.is_empty()).unwrap_or_else(|| "New Tab".into()),
        }
    }

    /// The page's favicon as an image, decoded once per favicon.
    pub fn favicon_image(&self) -> Option<std::sync::Arc<gpui::Image>> {
        let current = self.state.as_ref()?.favicon.as_deref()?;
        self.favicon_image.as_ref().filter(|(url, _)| url == current).map(|(_, image)| image.clone())
    }

    fn decode_favicon(&mut self) {
        let current = self.state.as_ref().and_then(|s| s.favicon.clone());
        if self.favicon_image.as_ref().map(|(url, _)| url) != current.as_ref() {
            self.favicon_image = current.and_then(|url| favicon_image(&url).map(|image| (url, image)));
        }
    }

    /// The icon slot's content for this block.
    pub fn icon(&self) -> Icon {
        icon_for(Kind::Browser, self.loading(), self.favicon_image().is_some())
    }

    /// The page's colour, while it has one.
    pub fn backdrop(&self) -> Option<u32> {
        self.theme_color.filter(|_| !self.empty)
    }

    /// The card's colours behind the toolbar, set by the workspace.
    pub fn set_palette(&mut self, palette: crate::adaptive::Chrome, cx: &mut gpui::Context<Self>) {
        if self.palette != palette {
            self.palette = palette;
            self.address.update(cx, |address, cx| address.set_palette(Some(palette), cx));
            cx.notify();
        }
    }

    /// The page's bottom corner radii (left, right), set by the workspace.
    pub fn set_corners(&mut self, corners: (f32, f32), cx: &mut gpui::Context<Self>) {
        if self.corners != corners {
            self.corners = corners;
            cx.notify();
        }
    }

    pub fn bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.bounds
    }
}

impl BrowserPane {
    /// The host the recent pages are remembered under.
    fn host_key(&self) -> &str {
        match &self.route {
            Route::Local => "local",
            Route::Remote { host_id, .. } => host_id,
        }
    }

    /// Blank: the empty state; a real page replaces it once painted.
    fn refresh_empty(&mut self, cx: &mut gpui::Context<Self>) {
        let blank = self.state.as_ref().is_none_or(|s| silo_web::is_blank(&s.url));
        let empty = blank || (self.empty && !self.page.as_ref().is_some_and(|p| p.shown()));
        if self.empty != empty {
            self.empty = empty;
            self.selected = None;
            cx.emit(BrowserPaneEvent::Backdrop);
        }
    }

    fn recents(&self, cx: &gpui::App) -> Vec<crate::browser_home::RecentPage> {
        crate::browser_home::listed(&crate::hosts_state::HostsState::global(cx).read(cx).app_state().recent_pages, self.host_key())
    }

    fn remember(&self, url: &str, title: Option<&str>, cx: &mut gpui::Context<Self>) {
        let host = self.host_key().to_owned();
        crate::hosts_state::HostsState::global(cx).update(cx, |hosts, cx| {
            hosts.update_state(
                |s| match title {
                    Some(title) => crate::browser_home::retitle(&mut s.recent_pages, &host, url, title),
                    None => crate::browser_home::visit(&mut s.recent_pages, &host, url, ""),
                },
                cx,
            );
        });
    }

    fn open_recent(&mut self, index: usize, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let Some(page) = self.recents(cx).into_iter().nth(index) else { return };
        self.selected = None;
        self.navigate(&page.url);
        self.focus.focus(window, cx);
    }
}

impl BrowserPane {
    /// Open the page once both the server state and the layout are known.
    fn ensure_page(&mut self, cx: &mut gpui::Context<Self>) {
        if self.page.is_some() || self.starting || self.error.is_some() || self.state.is_none() || (self.bounds.is_none() && !self.offscreen) {
            return;
        }
        self.starting = true;
        match self.route.clone() {
            Route::Local => self.open_page(BrowserState::PROFILE, None, cx),
            Route::Remote { host_id, home, addr } => {
                let task = self.rt.spawn(async move { crate::web::tunnel(&home, addr).await });
                cx.spawn(async move |this, cx| {
                    let tunnel = task.await.map_err(anyhow::Error::from).and_then(|r| r);
                    let _ = this.update(cx, |this, cx| match tunnel {
                        Ok(tunnel) => this.open_page(&host_id, Some(tunnel), cx),
                        Err(error) => this.failed(format!("{error:#}"), cx),
                    });
                })
                .detach();
            }
        }
    }

    fn failed(&mut self, error: String, cx: &mut gpui::Context<Self>) {
        tracing::warn!(block = %self.block_id, "browser: {error}");
        self.starting = false;
        self.error = Some(error);
        cx.notify();
    }
}

impl BrowserPane {
    fn view_size(&self) -> silo_web::ViewSize {
        let size = self.bounds.map_or(gpui::size(gpui::px(800.), gpui::px(600.)), |b| b.size);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (width, height) = (f32::from(size.width).max(1.0) as u32, f32::from(size.height).max(1.0) as u32);
        silo_web::ViewSize { width, height, scale: self.scale }
    }

    fn open_page(&mut self, profile: &str, tunnel: Option<silo_web::Tunnel>, cx: &mut gpui::Context<Self>) {
        let Some(state) = self.state.clone() else { return };
        let Some(url) = silo_proto::model::browser_url(&state.url) else {
            return self.failed("blocked address".into(), cx);
        };
        let opened = (|| {
            let runtime = crate::web::runtime(&self.home, cx)?;
            // A cached profile keeps its first tunnel: re-point it (a
            // re-added host gets a new proxy port and password).
            let profile = runtime.profile(profile, tunnel.clone())?;
            if tunnel.is_some() {
                profile.set_tunnel(tunnel)?;
            }
            let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = dirty.clone();
            let notify = std::sync::Arc::new(move || flag.store(true, std::sync::atomic::Ordering::Release));
            let options =
                silo_web::BrowserOptions { url: &url, profile: &profile, size: self.view_size(), offscreen: self.offscreen || self.hidden, notify };
            anyhow::Ok((runtime.create_browser(options)?, dirty))
        })();
        self.starting = false;
        match opened {
            Ok((page, dirty)) => {
                let page = std::rc::Rc::new(page);
                crate::web::register(page.clone(), dirty, Some(cx.weak_entity()));
                self.page = Some(page);
                self.committed = false;
                self.span = Span::at(state.index);
                self.engine_url = state.url.clone();
                self.error = None;
                tracing::info!(block = %self.block_id, url = %state.url, "browser opened");
                self.own(silo_proto::control::OwnMode::Request);
            }
            Err(error) => self.failed(format!("{error:#}"), cx),
        }
        cx.notify();
    }
}

impl BrowserPane {
    /// New frame or events from the engine (called by the pump).
    pub fn engine_changed(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(page) = self.page.clone() else { return };
        let mut changed = false;
        let mut visits = Vec::new();
        for event in page.take_events() {
            let Some(state) = self.state.as_mut() else { break };
            match event {
                silo_web::Event::UrlChanged { url, back_forward } => {
                    self.committed = true;
                    if follow_url(state, &mut self.span, &url, back_forward) {
                        self.engine_url.clone_from(&url);
                    }
                    self.page_scale = 1.0;
                    self.security_open = false;
                    self.blocked = None;
                    visits.push((url, None));
                    changed = true;
                }
                silo_web::Event::Security(security) => {
                    if security.cert_error && security.url != state.url {
                        self.blocked = Some(security.url.clone());
                        changed = true;
                    }
                    self.security = Some(security);
                    cx.notify();
                }
                silo_web::Event::TitleChanged(title) => {
                    follow_title(state, &title);
                    visits.push((state.url.clone(), Some(title)));
                    changed = true;
                }
                silo_web::Event::FaviconChanged(icon) => {
                    state.favicon = (icon.len() <= BrowserState::FAVICON).then_some(icon);
                    changed = true;
                }
                silo_web::Event::Loading(loading) => {
                    // Zoom is per page in CEF: re-apply the block's level.
                    page.set_zoom(f64::from(state.zoom));
                    state.loading = loading;
                    if loading {
                        self.progress = 0.0;
                        self.blocked = None;
                    } else {
                        self.committed = true;
                        self.span.jump = None;
                    }
                    changed = true;
                }
                silo_web::Event::Progress(progress) => {
                    self.progress = progress;
                    cx.notify();
                }
                silo_web::Event::EditableFocus(editable) => self.editable = editable,
                silo_web::Event::ThemeColor(color) => {
                    if self.theme_color != color {
                        self.theme_color = color;
                        cx.emit(BrowserPaneEvent::Backdrop);
                    }
                }
                silo_web::Event::Popup(url) => cx.emit(BrowserPaneEvent::Popup(url)),
                silo_web::Event::ImeBounds { x, y, width, height } => self.ime_rect = Some((x, y, width, height)),
                silo_web::Event::CursorChanged(cursor) => self.cursor = cursor,
                silo_web::Event::Closed => {
                    self.page = None;
                    self.actions = None;
                    self.theme_color = None;
                }
            }
        }
        if changed {
            self.sync_address(cx);
            self.debounce.changed(Instant::now());
            self.schedule_report(cx);
        }
        for (url, title) in visits {
            self.remember(&url, title.as_deref(), cx);
        }
        self.decode_favicon();
        self.refresh_empty(cx);
        cx.notify();
    }
}

impl BrowserPane {
    fn schedule_report(&mut self, cx: &mut gpui::Context<Self>) {
        if self.report_task.is_some() {
            return;
        }
        self.report_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REPORT_DEBOUNCE / 4).await;
                let sent = this.update(cx, |this, _| {
                    if !this.debounce.due(Instant::now()) {
                        return false;
                    }
                    this.send_report();
                    this.report_task = None;
                    true
                });
                if sent.unwrap_or(true) {
                    return;
                }
            }
        }));
    }

    /// Only the owner's report is accepted; viewers keep their engine local.
    fn send_report(&self) {
        let (Some(control), Some(state)) = (self.control.clone(), self.state.as_ref()) else { return };
        if !self.is_owner() {
            return;
        }
        let method = report(&self.block_id, state);
        self.rt.spawn(async move {
            if let Err(error) = control.call_value(method).await {
                tracing::debug!("browser.report: {error:#}");
            }
        });
    }
}

impl BrowserPane {
    /// User actions go to the server, which sends `browser.command` back to
    /// every attached client (this one included) and keeps offline state.
    fn send(&self, method: Method) {
        let Some(control) = self.control.clone() else { return };
        self.rt.spawn(async move {
            if let Err(error) = control.call_value(method).await {
                tracing::warn!("browser command: {error:#}");
            }
        });
    }

    pub fn navigate(&self, input: &str) {
        if let Some(url) = silo_proto::model::browser_url(input) {
            self.send(Method::BrowserNavigate { block_id: self.block_id.clone(), url });
        }
    }

    /// Copy, Paste, Undo, …: the page's own edit commands (windowless
    /// pages don't map ⌘ chords to them).
    pub fn edit(&self, edit: silo_web::Edit) {
        if let Some(page) = &self.page {
            page.edit(edit);
        }
    }

    pub fn back(&self) {
        if self.history_step(-1) {
            self.send(Method::BrowserBack { block_id: self.block_id.clone() });
        }
    }

    pub fn forward(&self) {
        if self.history_step(1) {
            self.send(Method::BrowserForward { block_id: self.block_id.clone() });
        }
    }

    pub fn reload(&self) {
        self.send(Method::BrowserReload { block_id: self.block_id.clone(), ignore_cache: false });
    }

    fn load_entry(&mut self, target: usize, page: &silo_web::Browser) {
        let entry = self.state.as_ref().and_then(|s| s.history.get(target));
        if let Some(url) = entry.and_then(|e| silo_proto::model::browser_url(&e.url)) {
            self.span.jump = Some(target);
            page.navigate(&url);
            self.engine_url = url;
        }
    }

    pub fn stop(&mut self) {
        self.span.jump = None;
        if let Some(page) = &self.page {
            page.stop();
        }
    }

    fn history_step(&self, delta: isize) -> bool {
        self.state.as_ref().is_some_and(|s| step(s, self.span, delta).is_some())
    }

    pub(crate) fn loading(&self) -> bool {
        self.page.is_some() && self.state.as_ref().is_some_and(|s| s.loading)
    }

    /// `browser.command` from the server.
    pub fn apply_command(&mut self, command: &BrowserCommand, cx: &mut gpui::Context<Self>) {
        let url = match command {
            BrowserCommand::Navigate { url } => match silo_proto::model::browser_url(url) {
                Some(url) => Some(url),
                None => return,
            },
            _ => None,
        };
        if self.page.is_none() && matches!(command, BrowserCommand::Navigate { .. } | BrowserCommand::Reload { .. }) {
            if let (Some(url), Some(state)) = (&url, self.state.as_mut()) {
                state.url.clone_from(url);
            }
            self.retry(cx);
            return;
        }
        let Some(page) = self.page.clone() else { return };
        match command {
            BrowserCommand::Navigate { .. } => {
                let url = url.unwrap_or_default();
                page.navigate(&url);
                self.engine_url = url;
            }
            BrowserCommand::Back | BrowserCommand::Forward => {
                let delta = if matches!(command, BrowserCommand::Back) { -1 } else { 1 };
                match self.state.as_ref().and_then(|s| step(s, self.span, delta)) {
                    Some(Step::Engine) if delta < 0 => page.back(),
                    Some(Step::Engine) => page.forward(),
                    Some(Step::Load(target)) => self.load_entry(target, &page),
                    None => {}
                }
            }
            BrowserCommand::Reload { ignore_cache } => page.reload(*ignore_cache),
            BrowserCommand::Zoom { zoom } => {
                page.set_zoom(f64::from(*zoom));
                if let Some(state) = self.state.as_mut() {
                    state.zoom = *zoom;
                }
            }
            BrowserCommand::Unknown => {}
        }
        cx.notify();
    }
}

impl BrowserPane {
    /// Put the full current URL in the field unless the user is editing it.
    fn sync_address(&mut self, cx: &mut gpui::Context<Self>) {
        let url = Some(self.url().to_owned()).filter(|u| !silo_web::is_blank(u)).unwrap_or_default();
        let editing = self.editing;
        self.address.update(cx, |field, cx| {
            if field.text != url && !editing {
                field.replace_text(url, cx);
            }
        });
    }

    /// ⌘L: edit the address, everything selected.
    pub fn focus_address(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let focus = self.address.read(cx).focus.clone();
        focus.focus(window, cx);
        self.address.update(cx, |field, cx| field.select_all(cx));
    }

    fn address_event(&mut self, event: &crate::text_field::FieldEvent, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        use crate::text_field::FieldEvent;
        match event {
            FieldEvent::Submit => {
                if let Some(index) = self.selected.filter(|_| self.empty) {
                    return self.open_recent(index, window, cx);
                }
                let text = self.address.read(cx).text.clone();
                if text.trim().is_empty() {
                    if !self.empty {
                        self.navigate("about:blank");
                    }
                    return;
                }
                self.navigate(&text);
                self.focus.focus(window, cx);
            }
            FieldEvent::Cancel => {
                if close_on_escape(&mut self.security_open, "escape") {
                    return cx.notify();
                }
                self.selected = None;
                self.focus.focus(window, cx);
                self.sync_address(cx);
            }
            FieldEvent::Up | FieldEvent::Down if self.empty => {
                let delta = if matches!(event, FieldEvent::Up) { -1 } else { 1 };
                self.selected = crate::browser_home::select(self.selected, self.recents(cx).len(), delta);
                cx.notify();
            }
            FieldEvent::Changed(_) => {
                if self.selected.take().is_some() {
                    cx.notify();
                }
            }
            FieldEvent::Up | FieldEvent::Down => {}
        }
    }
}

/// Esc closes an open security popover, and goes no further.
fn close_on_escape(open: &mut bool, key: &str) -> bool {
    let close = *open && key == "escape";
    *open &= !close;
    close
}

fn mods(m: gpui::Modifiers) -> silo_web::Modifiers {
    silo_web::Modifiers { shift: m.shift, control: m.control, alt: m.alt, command: m.platform }
}

fn button(b: gpui::MouseButton) -> Option<silo_web::MouseButton> {
    match b {
        gpui::MouseButton::Left => Some(silo_web::MouseButton::Left),
        gpui::MouseButton::Right => Some(silo_web::MouseButton::Right),
        gpui::MouseButton::Middle => Some(silo_web::MouseButton::Middle),
        _ => None,
    }
}

impl BrowserPane {
    /// `position` in page points.
    fn local(&self, position: gpui::Point<gpui::Pixels>) -> Option<(f32, f32)> {
        let b = self.bounds?;
        Some((f32::from(position.x - b.origin.x), f32::from(position.y - b.origin.y)))
    }

    fn mouse_button(&mut self, position: gpui::Point<gpui::Pixels>, b: gpui::MouseButton, down: bool, clicks: usize, m: gpui::Modifiers) {
        let (Some(page), Some((x, y)), Some(b)) = (self.page.clone(), self.local(position), button(b)) else { return };
        self.held.retain(|h| *h != b);
        if down {
            self.held.push(b);
        }
        #[allow(clippy::cast_possible_truncation)]
        page.mouse_button(x, y, b, down, clicks as u32, mods(m));
    }

    fn mouse_move(&self, position: gpui::Point<gpui::Pixels>, m: gpui::Modifiers) {
        let (Some(page), Some((x, y))) = (self.page.as_ref(), self.local(position)) else { return };
        page.mouse_move(x, y, mods(m), &self.held);
    }

    /// Trackpad scrolls go to the page unless they are a navigation swipe.
    fn wheel(&mut self, event: &gpui::ScrollWheelEvent, cx: &mut gpui::Context<Self>) {
        use crate::gesture::{Direction, Swipe};
        let (Some(page), Some((x, y))) = (self.page.clone(), self.local(event.position)) else { return };
        let delta = event.delta.pixel_delta(gpui::px(40.0));
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        if event.delta.precise() && event.touch_phase == gpui::TouchPhase::Started {
            self.swipe_enabled = gpui_platform::swipe_navigation_enabled();
            if self.swipe_enabled {
                self.query_scrollers(&page, (x, y), cx);
            }
        }
        if !event.delta.precise() || !self.swipe_enabled {
            page.wheel(x, y, dx, dy, mods(event.modifiers));
            return;
        }
        // History that way, and no scroller under the pointer that would take
        // the swipe (unknown until the page answered: then the page keeps it).
        let scrollers = self.swipe_scrollers;
        let back = self.history_step(-1) && scrollers.is_some_and(|[back, _]| !back);
        let forward = self.history_step(1) && scrollers.is_some_and(|[_, forward]| !forward);
        let can = |d: Direction| if d == Direction::Back { back } else { forward };
        let now = Instant::now();
        let reduce = cx.reduce_motion();
        match self.swipe.scroll(event.touch_phase, (dx, dy), page.scroll_offset().0, can) {
            Swipe::Pass => page.wheel(x, y, dx, dy, mods(event.modifiers)),
            Swipe::Track { direction, progress } => {
                let arrow = self.swipe_arrow.get_or_insert_with(|| SwipeArrow {
                    direction,
                    progress,
                    shown: crate::motion::Toggle::new(false, crate::motion::UI_FAST),
                });
                arrow.direction = direction;
                arrow.progress = progress;
                arrow.shown.set(true, now, reduce);
                cx.notify();
            }
            Swipe::Commit(direction) => {
                if let Some(arrow) = &mut self.swipe_arrow {
                    arrow.shown.set(false, now, reduce);
                }
                if direction == Direction::Back {
                    self.back()
                } else {
                    self.forward()
                }
                cx.notify();
            }
            Swipe::Abort => {
                if let Some(arrow) = &mut self.swipe_arrow {
                    arrow.progress = 0.0;
                    arrow.shown.set(false, now, reduce);
                }
                cx.notify();
            }
            Swipe::Drop => {}
        }
    }

    /// Ask the page whether an element under `point` can still scroll
    /// horizontally toward its start (a back swipe) or its end (forward).
    fn query_scrollers(&mut self, page: &std::rc::Rc<silo_web::Browser>, (x, y): (f32, f32), cx: &mut gpui::Context<Self>) {
        const PROBE: &str = "(x, y, s) => { const v = visualViewport, d = devicePixelRatio; \
            let e = document.elementFromPoint(v.offsetLeft + x * s / d / v.scale, v.offsetTop + y * s / d / v.scale); \
            let back = false, forward = false; \
            for (; e && e !== document.body && e !== document.documentElement; e = e.parentElement) { \
              const o = getComputedStyle(e).overflowX, max = e.scrollWidth - e.clientWidth; \
              if ((o === 'auto' || o === 'scroll') && max > 0) { const l = Math.abs(e.scrollLeft); \
                if (l > 0) back = true; if (l < max - 1) forward = true; } } \
            return [back, forward]; }";
        self.swipe_scrollers = None;
        self.swipe_query += 1;
        let query = self.swipe_query;
        let page = page.clone();
        let expression = format!("({PROBE})({x}, {y}, {})", self.scale);
        cx.spawn(async move |this, cx| {
            let reply = page.cdp("Runtime.evaluate", serde_json::json!({"expression": expression, "returnByValue": true})).await;
            let value = reply.ok().map(|r| r["result"]["value"].clone());
            let answer = value.and_then(|v| Some([v[0].as_bool()?, v[1].as_bool()?]));
            let _ = this.update(cx, |this, _| {
                if this.swipe_query == query {
                    this.swipe_scrollers = answer;
                }
            });
        })
        .detach();
    }

    /// A trackpad pinch, as two touch points the page zooms its visual
    /// viewport by.
    fn pinch(&mut self, event: &gpui::PinchEvent) {
        let Some(page) = self.page.clone() else { return };
        match event.phase {
            gpui::TouchPhase::Started => {
                self.end_pinch(silo_web::Pinch::Cancel);
                let Some(center) = self.local(event.position) else { return };
                let mut touch = crate::gesture::PinchTouch::new(center);
                page.pinch(silo_web::Pinch::Start, touch.start_points());
                page.pinch(silo_web::Pinch::Move, touch.points());
                if event.delta != 0.0 {
                    self.page_scale = crate::gesture::scaled(self.page_scale, touch.magnify(event.delta));
                    page.pinch(silo_web::Pinch::Move, touch.points());
                }
                self.pinch = Some(touch);
            }
            gpui::TouchPhase::Moved => {
                if self.smart_zoom.is_some() {
                    return;
                }
                let Some(touch) = &mut self.pinch else { return };
                self.page_scale = crate::gesture::scaled(self.page_scale, touch.magnify(event.delta));
                page.pinch(silo_web::Pinch::Move, touch.points());
            }
            gpui::TouchPhase::Ended if self.smart_zoom.is_none() => self.end_pinch(silo_web::Pinch::End),
            gpui::TouchPhase::Ended => {}
            gpui::TouchPhase::Cancelled => self.end_pinch(silo_web::Pinch::Cancel),
        }
    }

    /// Lifts (or cancels) the touch points of a pinch or smart zoom.
    fn end_pinch(&mut self, step: silo_web::Pinch) {
        self.smart_zoom = None;
        if let (Some(touch), Some(page)) = (self.pinch.take(), &self.page) {
            page.pinch(step, touch.points());
        }
    }

    /// Smart zoom (two-finger double tap): to 2× at the pointer, or back to 1×.
    fn smart_zoom(&mut self, event: &gpui::SmartMagnifyEvent, cx: &mut gpui::Context<Self>) {
        let (Some(page), Some(center)) = (self.page.clone(), self.local(event.position)) else { return };
        self.end_pinch(silo_web::Pinch::Cancel);
        let touch = crate::gesture::PinchTouch::new(center);
        page.pinch(silo_web::Pinch::Start, touch.start_points());
        page.pinch(silo_web::Pinch::Move, touch.points());
        self.pinch = Some(touch);
        let steps = crate::gesture::smart_zoom_steps(self.page_scale, if cx.reduce_motion() { 2 } else { 12 });
        self.smart_zoom = Some(cx.spawn(async move |this, cx| {
            for spread in steps {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                let moved = this.update(cx, |this, _| {
                    let (Some(touch), Some(page)) = (this.pinch.as_mut(), this.page.as_ref()) else { return false };
                    this.page_scale = crate::gesture::scaled(this.page_scale, touch.spread_to(spread));
                    page.pinch(silo_web::Pinch::Move, touch.points());
                    true
                });
                if !moved.unwrap_or(false) {
                    return;
                }
            }
            let _ = this.update(cx, |this, _| {
                if let (Some(touch), Some(page)) = (this.pinch.take(), this.page.as_ref()) {
                    page.pinch(silo_web::Pinch::End, touch.points());
                }
                this.smart_zoom = None;
            });
        }));
    }

    fn zoom(&self, step: i8) {
        let level = self.state.as_ref().map_or(0.0, |s| s.zoom);
        let zoom = zoom_step(level, step);
        if zoom != level || step == 0 {
            self.send(Method::BrowserZoom { block_id: self.block_id.clone(), zoom });
        }
    }

    /// The swipe arrow, over the page at its leading or trailing edge.
    fn swipe_arrow(&mut self, window: &mut gpui::Window, cx: &gpui::App) -> Option<gpui::AnyElement> {
        use gpui::{div, prelude::*, px, rgb};
        let (now, reduce) = (Instant::now(), cx.reduce_motion());
        if self.swipe_arrow.as_ref().is_some_and(|a| a.shown.hidden(now, reduce)) {
            self.swipe_arrow = None;
        }
        let arrow = self.swipe_arrow.as_ref()?;
        if arrow.shown.moving(now, reduce) {
            window.request_animation_frame();
        }
        const SIZE: f32 = 36.0;
        let full = arrow.progress >= 1.0;
        let inset = -SIZE + (SIZE + 16.0) * crate::motion::ease_out(arrow.progress);
        let (icon, bg) = if full {
            (crate::theme::accent_on(crate::theme::accent()), rgb(crate::theme::accent()))
        } else {
            (crate::theme::foreground(), crate::theme::overlay_fill())
        };
        let back = arrow.direction == crate::gesture::Direction::Back;
        let chip = div()
            .absolute()
            .top_1_2()
            .mt(px(-SIZE / 2.0))
            .size(px(SIZE))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(bg)
            .border_1()
            .border_color(rgb(crate::theme::border()))
            .shadow_md()
            .opacity(arrow.shown.value(now, reduce))
            .child(crate::assets::icon(if back { "back" } else { "forward" }, 16.0, icon));
        Some(if back { chip.left(px(inset)) } else { chip.right(px(inset)) }.into_any_element())
    }
}

impl BrowserPane {
    /// Keys go to the page as key events; while an IME composes, printable
    /// keys go to the platform's text input instead. ⌘ chords bound by the
    /// app never get here.
    fn key(&mut self, key: &gpui::Keystroke, release: bool) -> bool {
        let Some(page) = self.page.clone() else { return false };
        let printable = key.key_char.as_ref().is_some_and(|t| !t.is_empty()) && !key.modifiers.control && !key.modifiers.platform;
        use crate::composition::RawDisposition;
        match self.gate.route(&key.key, printable, release, self.composing.is_some()) {
            RawDisposition::Encode => {}
            RawDisposition::PlatformText => return false,
            RawDisposition::Ignore => return true,
            RawDisposition::Cancel => {
                page.ime_cancel();
                self.composing = None;
                return true;
            }
        }
        // RAWKEYDOWN + CHAR: the key event types its own text.
        let input =
            silo_web::KeyInput { key: &key.key, key_char: key.key_char.as_deref(), modifiers: mods(key.modifiers), down: !release, repeat: false };
        page.key(&input);
        true
    }
}

impl gpui::EntityInputHandler for BrowserPane {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted: &mut Option<std::ops::Range<usize>>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<String> {
        let text = self.composing.as_deref()?;
        let units: Vec<u16> = text.encode_utf16().collect();
        let range = range.start.min(units.len())..range.end.min(units.len());
        *adjusted = Some(range.clone());
        String::from_utf16(&units[range]).ok()
    }

    fn selected_text_range(&mut self, _: bool, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> Option<gpui::UTF16Selection> {
        let end = self.composing.as_deref().map_or(0, |t| t.encode_utf16().count());
        Some(gpui::UTF16Selection { range: end..end, reversed: false })
    }

    fn marked_text_range(&self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> Option<std::ops::Range<usize>> {
        self.composing.as_deref().map(|t| 0..t.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) {
        if self.composing.take().is_some()
            && let Some(page) = &self.page
        {
            page.ime_finish();
        }
    }

    fn replace_text_in_range(&mut self, _: Option<std::ops::Range<usize>>, text: &str, _: &mut gpui::Window, _: &mut gpui::Context<Self>) {
        self.composing = None;
        if let Some(page) = &self.page {
            page.insert_text(text);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        selected: Option<std::ops::Range<usize>>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) {
        let Some(page) = &self.page else { return };
        let len = text.encode_utf16().count();
        let selected = selected.unwrap_or(len..len);
        #[allow(clippy::cast_possible_truncation)]
        page.ime_set_composition(text, selected.start as u32..selected.end as u32);
        self.composing = (!text.is_empty()).then(|| text.to_owned());
    }

    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: gpui::Bounds<gpui::Pixels>,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        let (b, (x, y, w, h)) = (self.bounds?, self.ime_rect?);
        #[allow(clippy::cast_precision_loss)]
        let rect = gpui::Bounds::new(
            b.origin + gpui::point(gpui::px(x as f32), gpui::px(y as f32)),
            gpui::size(gpui::px(w.max(1) as f32), gpui::px(h.max(1) as f32)),
        );
        Some(rect)
    }

    fn character_index_for_point(&mut self, _: gpui::Point<gpui::Pixels>, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> Option<usize> {
        None
    }
}

impl BrowserPane {
    /// Layout of the page area: open or resize the page to it.
    fn layout(&mut self, bounds: gpui::Bounds<gpui::Pixels>, scale: f32, cx: &mut gpui::Context<Self>) {
        let changed = self.bounds.is_none_or(|b| b.size != bounds.size) || self.scale != scale;
        self.bounds = Some(bounds);
        self.scale = scale;
        if changed && let Some(page) = &self.page {
            page.resize(self.view_size());
        }
        if self.page.is_none() && !self.starting && self.error.is_none() {
            // Not during paint: creating the engine pumps the run loop.
            cx.spawn(async move |this, cx| _ = this.update(cx, |this, cx| this.ensure_page(cx))).detach();
        }
    }
}

fn page_canvas(entity: gpui::Entity<BrowserPane>) -> impl gpui::IntoElement {
    use gpui::Styled as _;
    gpui::canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            let scale = window.scale_factor();
            entity.update(cx, |pane, cx| pane.layout(bounds, scale, cx));
            if let Some(frame) = entity.read(cx).page.as_ref().filter(|_| !entity.read(cx).empty).and_then(|p| p.frame()) {
                let (left, right) = entity.read(cx).corners;
                let corners = gpui::Corners { bottom_left: gpui::px(left), bottom_right: gpui::px(right), ..Default::default() };
                window.paint_rounded_surface(bounds, frame.buffer, corners, crate::theme::CORNER_SMOOTHING);
            }
            let focus = entity.read(cx).focus.clone();
            window.handle_input(&focus, gpui::ElementInputHandler::new(bounds, entity.clone()), cx);
            let entity = entity.downgrade();
            window.on_mouse_event(move |e: &gpui::FileDropEvent, phase, _, cx| {
                if phase == gpui::DispatchPhase::Bubble && matches!(e, gpui::FileDropEvent::Exited | gpui::FileDropEvent::Ended) {
                    let _ = entity.update(cx, |pane, _| {
                        if std::mem::take(&mut pane.file_drag)
                            && let Some(page) = &pane.page
                        {
                            page.file_drag_leave();
                        }
                    });
                }
            });
        },
    )
    .size_full()
}

impl Drop for BrowserPane {
    fn drop(&mut self) {
        if let Some(page) = self.page.take() {
            crate::web::close(&page);
        }
    }
}

/// A `data:image/png;base64,…` favicon as an image.
#[must_use]
pub fn favicon_image(data_url: &str) -> Option<std::sync::Arc<gpui::Image>> {
    let b64 = data_url.strip_prefix("data:image/png;base64,")?;
    let bytes = data_encoding::BASE64.decode(b64.as_bytes()).ok()?;
    Some(std::sync::Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes)))
}

fn cursor_style(cursor: silo_web::Cursor) -> gpui::CursorStyle {
    use gpui::CursorStyle as S;
    use silo_web::Cursor as C;
    match cursor {
        C::Hand => S::PointingHand,
        C::IBeam => S::IBeam,
        C::Crosshair => S::Crosshair,
        C::NotAllowed => S::OperationNotAllowed,
        C::ResizeColumn => S::ResizeLeftRight,
        C::ResizeRow => S::ResizeUpDown,
        C::Grab => S::OpenHand,
        C::Grabbing | C::Move => S::ClosedHand,
        C::Arrow | C::Wait | C::None => S::Arrow,
    }
}

/// The browser's header: navigation and the address in the middle, the page
/// load as the header's progress line. No title: the tab shows it.
impl crate::pane_header::BlockHeader for BrowserPane {
    fn header(
        &mut self,
        style: crate::pane_header::HeaderStyle,
        _: Option<&BlockInfo>,
        cx: &mut gpui::Context<Self>,
    ) -> crate::pane_header::HeaderParts {
        use gpui::{IntoElement as _, ParentElement as _, Styled as _, div, px};
        let middle = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .flex_1()
            .min_w_0()
            .text_size(px(12.0))
            .children(self.nav_buttons(style, cx))
            .child(self.address_box(cx))
            .into_any_element();
        let mut parts = crate::pane_header::HeaderParts::new(middle, self.title());
        #[allow(clippy::cast_possible_truncation)]
        let progress = (self.loading() && self.progress < 1.0).then_some(self.progress as f32);
        parts.progress = progress;
        parts.rule = true;
        parts.actions = vec![self.settings_button(style, cx)];
        parts
    }
}

impl BrowserPane {
    /// The header's actions by id, before the pane buttons (shown only in a full header).
    pub(crate) const ACTIONS: [&'static str; 1] = ["browser-settings"];

    fn settings_button(&self, style: crate::pane_header::HeaderStyle, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use crate::pane_header::{button, keep_press};
        use gpui::{IntoElement as _, StatefulInteractiveElement as _};
        keep_press(button(Self::ACTIONS[0], "settings", "Browser settings", self.palette.icon, true, style))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(BrowserPaneEvent::OpenSettings)))
            .into_any_element()
    }

    /// Back, Forward, and Reload or Stop.
    fn nav_buttons(&self, style: crate::pane_header::HeaderStyle, cx: &mut gpui::Context<Self>) -> [gpui::AnyElement; 3] {
        use crate::pane_header::{button, keep_press};
        use gpui::{IntoElement as _, StatefulInteractiveElement as _};
        let palette = self.palette;
        let nav = |id: &'static str, glyph: &'static str, tip: &'static str, enabled: bool| {
            keep_press(button(id, glyph, tip, if enabled { palette.ink } else { palette.icon }, enabled, style))
        };
        let reload = if self.loading() {
            nav("browser-reload", "close", "Stop  ⌘.", true).on_click(cx.listener(|this, _, _, _| this.stop()))
        } else {
            nav("browser-reload", "reload", "Reload  ⌘R", self.page.is_some()).on_click(cx.listener(|this, _, _, _| this.reload()))
        };
        [
            nav("browser-back", "back", "Back  ⌘←", self.history_step(-1)).on_click(cx.listener(|this, _, _, _| this.back())).into_any_element(),
            nav("browser-forward", "forward", "Forward  ⌘→", self.history_step(1))
                .on_click(cx.listener(|this, _, _, _| this.forward()))
                .into_any_element(),
            reload.into_any_element(),
        ]
    }

    /// The security indicator, the address and the zoom pill. Presses stay
    /// here: the field takes them, never a pane drag.
    fn address_box(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use crate::theme::Continuous as _;
        use gpui::{div, prelude::*, px};
        let palette = self.palette;
        crate::pane_header::keep_press(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .flex_1()
                .min_w_0()
                .px(px(6.0))
                .when(palette.alpha < 1.0, |el| el.h(px(22.0)).continuous(crate::radius::CONTROL).bg(palette.tint(0.07))),
        )
        .child(self.security_indicator(cx))
        .child(self.address_text(cx))
        .children(self.zoom_pill(cx))
        .into_any_element()
    }

    fn url(&self) -> &str {
        self.blocked.as_deref().or_else(|| self.state.as_ref().map(|s| s.url.as_str())).unwrap_or_default()
    }

    /// Lock, open lock + "Not secure", or info; opens the popover under it.
    fn security_indicator(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use crate::address::Security;
        use crate::theme::Continuous as _;
        use gpui::{div, prelude::*, px, rgb};
        let palette = self.palette;
        let security = crate::address::security(self.url(), self.security.as_ref());
        let glyph = match security {
            Security::Secure => "lock",
            Security::NotSecure => "lock-off",
            Security::Neutral => "info",
        };
        let label = (security == Security::NotSecure && !self.editing).then_some("Not secure");
        div()
            .id("browser-security")
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.0))
            .h(px(18.0))
            .min_w(px(18.0))
            .justify_center()
            .px(px(2.0))
            .continuous(crate::radius::CONTROL)
            .cursor_pointer()
            .hover(|s| s.bg(palette.hover()))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| {
                this.security_open = !this.security_open;
                cx.notify();
            }))
            .child(crate::assets::icon(glyph, 12.0, palette.icon))
            .children(label.map(|l| div().pr(px(2.0)).text_size(px(11.0)).text_color(rgb(palette.muted)).child(l)))
            .children(self.security_open.then(|| self.security_popover(cx)))
            .into_any_element()
    }

    /// The field while editing; at rest the host, with scheme and path muted.
    fn address_text(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use gpui::{div, prelude::*, rgb};
        let palette = self.palette;
        let shown = crate::address::shown(self.url());
        let at_rest = !self.editing && !shown.host.is_empty();
        let part = |text: String, color: u32| div().flex_none().text_color(rgb(color)).child(text);
        div()
            .relative()
            .flex()
            .items_center()
            .flex_1()
            .min_w_0()
            .child(div().w_full().when(at_rest, |el| el.opacity(0.0)).child(self.address.clone()))
            .when(at_rest, |el| {
                el.child(
                    div()
                        .id("browser-address-shown")
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .cursor(gpui::CursorStyle::IBeam)
                        .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.focus_address(window, cx)))
                        .when(!shown.scheme.is_empty(), |el| el.child(part(shown.scheme, palette.muted)))
                        .child(part(shown.host, palette.ink))
                        .child(div().min_w_0().truncate().text_color(rgb(palette.muted)).child(shown.path)),
                )
            })
            .into_any_element()
    }

    /// The page zoom when not 100%; a click resets it.
    fn zoom_pill(&self, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        use crate::theme::Continuous as _;
        use gpui::{div, prelude::*, px, rgb};
        let label = crate::address::zoom_label(self.state.as_ref()?.zoom)?;
        let palette = self.palette;
        Some(
            div()
                .id("browser-zoom")
                .flex_none()
                .px(px(5.0))
                .h(px(16.0))
                .flex()
                .items_center()
                .capsule()
                .bg(palette.tint(0.08))
                .hover(|s| s.bg(palette.tint(0.14)))
                .cursor_pointer()
                .text_size(px(10.5))
                .text_color(rgb(palette.muted))
                .tooltip(crate::tooltip::Tooltip::text("Reset zoom"))
                .on_click(cx.listener(|this, _, _, _| this.send(Method::BrowserZoom { block_id: this.block_id.clone(), zoom: 0.0 })))
                .child(label)
                .into_any_element(),
        )
    }

    /// Over the page (painted after it), so opaque.
    fn security_popover(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use crate::theme;
        use crate::theme::Continuous as _;
        use gpui::{div, prelude::*, px, rgb};
        let url = self.url();
        let info = self.security.as_ref().filter(|i| i.url == url);
        let host = crate::address::shown(url).host;
        let detail = |text: String| div().text_size(px(11.0)).text_color(rgb(theme::muted())).child(text);
        let cert = info.filter(|i| i.secure && !i.cert_error);
        let card = div()
            .id("browser-security-popover")
            .absolute()
            .top(px(21.0))
            .left(px(-2.0))
            .w(px(260.0))
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .continuous(10.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_lg()
            .occlude()
            .font_family(theme::ui_font())
            .text_size(px(12.5))
            .text_color(rgb(theme::foreground()))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.security_open = false;
                cx.notify();
            }))
            .child(div().child(crate::address::headline(url, info)))
            .when(!host.is_empty(), |el| el.child(detail(host)))
            .children(cert.and_then(|i| i.issuer.clone()).map(|issuer| detail(format!("Issued by {issuer}"))))
            .children(cert.and_then(|i| i.valid_until).map(|t| detail(format!("Valid until {}", crate::address::date(t)))));
        gpui::deferred(card).with_priority(1).into_any_element()
    }

    /// No page: a hint and this host's recent pages, on the card itself.
    fn empty_state(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use crate::theme::Continuous as _;
        use gpui::{div, prelude::*, px, rgb};
        let palette = self.palette;
        let rows: Vec<_> = self
            .recents(cx)
            .into_iter()
            .enumerate()
            .map(|(i, page)| {
                let title = if page.title.trim().is_empty() { display_url(&page.url) } else { page.title.clone() };
                div()
                    .id(("browser-recent", i))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(28.0))
                    .px(px(8.0))
                    .continuous(crate::radius::CONTROL)
                    .cursor_pointer()
                    .when(self.selected == Some(i), |el| el.bg(palette.selected()))
                    .hover(|s| s.bg(palette.hover()))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_recent(i, window, cx)))
                    .child(crate::assets::icon("globe", 12.0, palette.icon))
                    .child(div().flex_1().min_w_0().truncate().text_color(rgb(palette.ink)).child(title))
                    .child(
                        div().flex_none().max_w(gpui::relative(0.5)).truncate().text_color(rgb(palette.muted)).child(page_host(&page.url).to_owned()),
                    )
            })
            .collect();
        div()
            .id("browser-empty")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .p(px(24.0))
            .text_size(px(12.0))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.focus_address(window, cx)))
            .child(crate::assets::icon("globe", 24.0, palette.icon))
            .child(div().text_size(px(13.0)).text_color(rgb(palette.muted)).child("Search or enter an address"))
            .when(!rows.is_empty(), |el| el.child(div().flex().flex_col().gap(px(1.0)).w_full().max_w(px(380.0)).mt(px(6.0)).children(rows)))
            .into_any_element()
    }
}

/// `url`'s host (and port).
fn page_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// The loading spinner, in an icon's bounds: one turn a second, still
/// under Reduce Motion.
pub(crate) fn spinner(id: &str, size: f32, color: u32, cx: &gpui::App) -> gpui::AnyElement {
    use gpui::{AnimationExt as _, IntoElement as _};
    let icon = crate::assets::icon("spinner", size, color);
    if cx.reduce_motion() {
        return icon.into_any_element();
    }
    icon.with_animation(
        gpui::ElementId::Name(format!("spinner-{id}").into()),
        gpui::Animation::new(Duration::from_secs(1)).repeat_synced(),
        |icon, delta| icon.with_transformation(gpui::Transformation::rotate(gpui::radians(delta * std::f32::consts::TAU))),
    )
    .into_any_element()
}

impl gpui::Render for BrowserPane {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        use gpui::{div, prelude::*, rgb};
        let mut page = div()
            .id(gpui::ElementId::Name(format!("browser-{}", self.block_id).into()))
            .key_context("Browser")
            .track_focus(&self.focus)
            .relative()
            .flex_1()
            .min_h_0()
            .cursor(cursor_style(self.cursor))
            .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _, cx| {
                if close_on_escape(&mut this.security_open, &e.keystroke.key) {
                    cx.notify();
                    cx.stop_propagation()
                } else if this.key(&e.keystroke, false) {
                    cx.stop_propagation()
                }
            }))
            .on_key_up(cx.listener(|this, e: &gpui::KeyUpEvent, _, cx| {
                if this.key(&e.keystroke, true) {
                    cx.stop_propagation()
                }
            }))
            // Not during an app drag (a divider resize, a pane): the page
            // never saw its press, so it gets neither the moves nor the release.
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, cx| {
                if !cx.has_active_drag() {
                    this.mouse_move(e.position, e.modifiers);
                }
            }))
            .on_scroll_wheel(cx.listener(|this, e: &gpui::ScrollWheelEvent, _, cx| this.wheel(e, cx)))
            .on_pinch(cx.listener(|this, e: &gpui::PinchEvent, _, _| this.pinch(e)))
            .on_smart_magnify(cx.listener(|this, e: &gpui::SmartMagnifyEvent, _, cx| this.smart_zoom(e, cx)))
            .on_drag_move(cx.listener(|this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                let inside = e.bounds.contains(&e.event.position);
                let (Some(page), Some((x, y))) = (this.page.clone(), this.local(e.event.position)) else { return };
                match (inside, this.file_drag) {
                    (true, false) => page.file_drag_enter(x, y, e.drag(cx).paths()),
                    (true, true) => page.file_drag_over(x, y),
                    (false, true) => page.file_drag_leave(),
                    (false, false) => {}
                }
                this.file_drag = inside;
            }))
            .on_drop(cx.listener(|this, _: &gpui::ExternalPaths, window, cx| {
                this.focus.focus(window, cx);
                if std::mem::take(&mut this.file_drag)
                    && let (Some(page), Some((x, y))) = (this.page.as_ref(), this.local(window.mouse_position()))
                {
                    page.file_drop(x, y);
                }
            }));
        for b in [gpui::MouseButton::Left, gpui::MouseButton::Right, gpui::MouseButton::Middle] {
            page = page
                .on_mouse_down(
                    b,
                    cx.listener(|this, e: &gpui::MouseDownEvent, window, cx| {
                        this.focus.focus(window, cx);
                        this.mouse_button(e.position, e.button, true, e.click_count, e.modifiers);
                    }),
                )
                .on_mouse_up(
                    b,
                    cx.listener(|this, e: &gpui::MouseUpEvent, _, cx| {
                        if !cx.has_active_drag() {
                            this.mouse_button(e.position, e.button, false, e.click_count, e.modifiers);
                        }
                    }),
                );
        }
        let page = page
            .on_mouse_down(gpui::MouseButton::Navigate(gpui::NavigationDirection::Back), cx.listener(|this, _, _, _| this.back()))
            .on_mouse_down(gpui::MouseButton::Navigate(gpui::NavigationDirection::Forward), cx.listener(|this, _, _, _| this.forward()));
        let empty = (self.empty && self.error.is_none()).then(|| self.empty_state(cx));
        let arrow = self.swipe_arrow(window, cx);
        let page = page
            .child(page_canvas(cx.entity()))
            .children(empty)
            .when_some(self.error.clone(), |el, _| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(crate::theme::muted()))
                        .child("Can't open the page"),
                )
            })
            .children(arrow);
        div().flex().flex_col().size_full().min_w_0().min_h_0().child(page)
    }
}

impl crate::workspace::Workspace {
    /// The browser pane of `block`, created on first sight.
    pub(crate) fn ensure_browser(&mut self, block: &str, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        if self.browsers.contains_key(block) {
            return;
        }
        let host = &self.hosts[self.active_host].info;
        let route = match &host.transport {
            crate::hosts::Transport::Local { .. } => Route::Local,
            crate::hosts::Transport::Iroh { home, addr } => Route::Remote { host_id: addr.id.to_string(), home: home.clone(), addr: addr.clone() },
            // Tunnels need control of the server: a link's browser never loads.
            crate::hosts::Transport::Shared { home, ticket } => {
                Route::Remote { host_id: ticket.addr.id.to_string(), home: home.clone(), addr: ticket.addr.clone() }
            }
        };
        // A page the off-screen host runs is shown as it is: no reload.
        let adopted = host.local.then(|| crate::browser_host::BrowserHost::take(block, cx)).flatten();
        let pane = adopted.unwrap_or_else(|| {
            let (id, rt, control, home) = (block.to_owned(), self.rt.clone(), self.control(), self.home.clone());
            cx.new(|cx| BrowserPane::new(id, rt, control, route, home, cx))
        });
        pane.update(cx, |pane, cx| pane.attach(window, cx));
        let sub = cx.subscribe_in(&pane, window, |this, pane, event: &BrowserPaneEvent, window, cx| {
            let block = pane.read(cx).block_id.clone();
            match event {
                BrowserPaneEvent::Activated => this.pane_activated(block),
                BrowserPaneEvent::Popup(url) => this.split_browser(block, Some(url.clone())),
                BrowserPaneEvent::Backdrop => {}
                BrowserPaneEvent::OpenSettings => this.open_settings_section(crate::settings_panel::Section::Browser, window, cx),
            }
            cx.notify();
        });
        self.browsers.insert(block.to_owned(), (pane, sub));
    }

    pub(crate) fn browser_focused(&self, window: &gpui::Window, cx: &gpui::App) -> Option<String> {
        self.browsers
            .iter()
            .find(|(_, (p, _))| p.read(cx).focus.is_focused(window) || p.read(cx).address.read(cx).focus.is_focused(window))
            .map(|(id, _)| id.clone())
    }

    /// Split `block` with a new browser (popup, New Browser, Open URL).
    pub(crate) fn split_browser(&mut self, block: String, url: Option<String>) {
        let axis = self.auto_axis(&block);
        self.call(browser_split(block, axis, url));
    }

    /// New Browser: a blank browser beside `block`, address bar focused.
    pub(crate) fn new_browser(&mut self, block: String, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let method = browser_split(block.clone(), self.auto_axis(&block), None);
        self.create_browser(method, window, cx);
    }

    /// New Browser Tab: a tab holding a blank browser, address bar focused.
    pub(crate) fn new_browser_tab(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let Some(view) = &self.view else { return };
        let spawn = silo_proto::control::SpawnSpec { cwd: None, command: Vec::new(), size: None };
        let method = Method::WindowCreate { session_id: view.session.id.clone(), spawn, kind: BlockKind::Browser, url: None };
        self.create_browser(method, window, cx);
    }

    /// Create a browser block with `method`, then focus its address bar.
    pub(crate) fn create_browser(&mut self, method: Method, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let Some(control) = self.control() else { return };
        let task = self.rt.spawn(async move { control.call::<silo_proto::control::Created>(method).await });
        cx.spawn_in(window, async move |this, cx| {
            let created = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| match created {
                Ok(created) => {
                    this.address_pending = Some(created.block_id);
                    this.focus_new_browser(window, cx);
                }
                Err(error) => {
                    tracing::warn!("new browser: {error:#}");
                    this.flash(this.failed(&error, "Couldn't open a browser"), cx);
                }
            });
        })
        .detach();
    }

    /// Focus the pending new browser's address bar once its pane exists.
    pub(crate) fn focus_new_browser(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let Some(pane) = self.address_pending.as_ref().and_then(|b| self.browsers.get(b)).map(|(p, _)| p.clone()) else { return };
        self.address_pending = None;
        pane.update(cx, |pane, cx| pane.focus_address(window, cx));
    }
}

fn browser_split(block: String, axis: silo_proto::model::Axis, url: Option<String>) -> Method {
    let spawn = silo_proto::control::SpawnSpec { cwd: None, command: Vec::new(), size: None };
    Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Browser, url, agent: None }
}

/// Page chords ([`page_chord`]), intercepted ahead of the keymap and the page.
fn intercept(pane: gpui::WeakEntity<BrowserPane>, cx: &mut gpui::App) -> gpui::Subscription {
    cx.intercept_keystrokes(move |event, window, cx| {
        let k = &event.keystroke;
        let _ = pane.update(cx, |pane, cx| {
            let Some(chord) = page_chord(&k.key, k.modifiers, pane.focus.is_focused(window), pane.editable) else { return };
            match chord {
                Chord::Address => pane.focus_address(window, cx),
                Chord::Back => pane.back(),
                Chord::Forward => pane.forward(),
                Chord::Reload => pane.reload(),
                Chord::Stop => pane.stop(),
                Chord::Edit(edit) => pane.edit(edit),
                Chord::Zoom(step) => pane.zoom(step),
            }
            cx.stop_propagation();
        });
    })
}

impl crate::workspace::Workspace {
    /// Content bounds of any pane, terminal or browser.
    pub(crate) fn block_bounds(&self, block: &str, cx: &gpui::App) -> Option<gpui::Bounds<gpui::Pixels>> {
        if let Some((pane, _)) = self.browsers.get(block) {
            return pane.read(cx).bounds();
        }
        if let Some((pane, _)) = self.agents.get(block) {
            return pane.read(cx).bounds();
        }
        self.panes.get(block).and_then(|(p, _)| p.read(cx).bounds())
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::model::{Lifecycle, TermSize};

    use super::*;

    #[test]
    fn escape_closes_only_an_open_security_popover() {
        let mut open = true;
        assert!(!close_on_escape(&mut open, "a") && open, "other keys leave it open");
        assert!(close_on_escape(&mut open, "escape") && !open, "Esc closes it and stops there");
        assert!(!close_on_escape(&mut open, "escape"), "closed: Esc goes on (the address cancels, the page gets it)");
    }

    fn info(kind: BlockKind) -> BlockInfo {
        BlockInfo {
            id: "b".into(),
            session_id: "s".into(),
            kind,
            title: String::new(),
            cwd: None,
            exited: None,
            lifecycle: Lifecycle::Running,
            size: TermSize::new(80, 24),
            owner: None,
            owner_name: None,
            pid: None,
            browser: (kind == BlockKind::Browser).then(|| BrowserState::new("about:blank".into())),
            agent: (kind == BlockKind::Agent).then(Default::default),
        }
    }

    #[test]
    fn pane_kind_follows_block_kind() {
        assert_eq!(kind_of(Some(&info(BlockKind::Browser))), Kind::Browser);
        assert_eq!(kind_of(Some(&info(BlockKind::Agent))), Kind::Agent);
        assert_eq!(kind_of(Some(&info(BlockKind::Terminal))), Kind::Terminal);
        assert_eq!(kind_of(None), Kind::Terminal);
    }

    #[test]
    fn address_display_drops_http_schemes() {
        assert_eq!(display_url("https://example.com/"), "example.com");
        assert_eq!(display_url("http://localhost:3000/a/b"), "localhost:3000/a/b");
        assert_eq!(display_url("about:blank"), "");
        assert_eq!(display_url("data:text/html,hi"), "data:text/html,hi");
        let typed = silo_proto::model::browser_url("localhost:5173").unwrap();
        assert_eq!(display_url(&typed), "localhost:5173");
    }

    #[test]
    fn reports_are_debounced() {
        let t = Instant::now();
        let mut d = Debounce::default();
        assert!(!d.due(t));
        d.changed(t);
        assert!(!d.due(t + REPORT_DEBOUNCE / 2));
        d.changed(t + REPORT_DEBOUNCE / 2);
        assert!(!d.due(t + REPORT_DEBOUNCE));
        assert!(d.due(t + REPORT_DEBOUNCE * 2));
        assert!(!d.pending());
        // Under constant churn a report still goes out.
        for i in 0..8 {
            d.changed(t + REPORT_DEBOUNCE / 2 * i);
        }
        assert!(d.due(t + REPORT_DEBOUNCE * 4));
    }

    #[test]
    fn engine_urls_follow_history() {
        let mut s = BrowserState::new("a".into());
        let mut span = Span::at(0);
        follow_url(&mut s, &mut span, "b", false);
        follow_url(&mut s, &mut span, "c", false);
        follow_url(&mut s, &mut span, "b", true);
        assert_eq!((s.index, s.history.len()), (1, 3), "back");
        follow_url(&mut s, &mut span, "c", true);
        assert_eq!(s.index, 2, "forward");
        follow_title(&mut s, "C");
        assert_eq!(s.history[2].title, "C");
        follow_url(&mut s, &mut span, "d", false);
        assert_eq!((s.index, s.url.as_str()), (3, "d"));
        assert_eq!(span, Span::at(0).with_hi(3));
    }

    #[test]
    fn a_link_to_the_previous_page_is_a_new_entry() {
        let mut s = BrowserState::new("a".into());
        let mut span = Span::at(0);
        follow_url(&mut s, &mut span, "b", false);
        follow_url(&mut s, &mut span, "a", false);
        assert_eq!((s.index, s.history.len()), (2, 3));
    }

    #[test]
    fn icon_prefers_spinner_then_favicon_then_glyph() {
        assert_eq!(icon_for(Kind::Browser, true, true), Icon::Spinner);
        assert_eq!(icon_for(Kind::Browser, false, true), Icon::Favicon);
        assert_eq!(icon_for(Kind::Browser, false, false), Icon::Glyph("globe"));
        assert_eq!(icon_for(Kind::Terminal, true, true), Icon::Glyph("terminal"));
    }

    /// Reopened from a stale cached `about:blank`, then given the server's
    /// page: the engine's initial blank commit must not become history.
    #[test]
    fn restored_page_ignores_the_initial_blank_commit() {
        let mut s = BrowserState::new("about:blank".into());
        s.push("http://a/".into());
        let mut span = Span::at(s.index);
        span.jump = Some(s.index);
        assert!(!follow_url(&mut s, &mut span, "about:blank", false));
        assert_eq!((s.index, s.url.as_str(), s.history.len()), (1, "http://a/", 2));
        assert!(follow_url(&mut s, &mut span, "http://a/", false));
        assert_eq!((s.index, s.history.len(), span), (1, 2, Span::at(1)));
        // Without a pending load, about:blank is a navigation like any other.
        assert!(follow_url(&mut s, &mut span, "about:blank", false));
        assert_eq!((s.index, s.url.as_str()), (2, "about:blank"));
    }

    impl Span {
        fn with_hi(mut self, hi: usize) -> Self {
            self.hi = hi;
            self
        }
    }

    /// A block reopened at the last of a/b/c: its engine has one entry.
    fn reopened() -> (BrowserState, Span) {
        let mut s = BrowserState::new("a".into());
        s.push("b".into());
        s.push("c".into());
        let span = Span::at(s.index);
        (s, span)
    }

    /// Simulates a client: Back/Forward and the engine's URL commits.
    fn go(s: &mut BrowserState, span: &mut Span, engine: &mut Vec<String>, at: &mut usize, delta: isize) -> bool {
        let Some(step) = step(s, *span, delta) else { return false };
        match step {
            Step::Engine => *at = at.checked_add_signed(delta).unwrap(),
            Step::Load(target) => {
                span.jump = Some(target);
                engine.truncate(*at + 1);
                engine.push(s.history[target].url.clone());
                *at = engine.len() - 1;
            }
        }
        let url = engine[*at].clone();
        follow_url(s, span, &url, step == Step::Engine);
        true
    }

    #[test]
    fn back_is_bounded_by_the_block_history() {
        let (mut s, mut span) = reopened();
        let (mut engine, mut at) = (vec!["c".to_owned()], 0);
        assert!(go(&mut s, &mut span, &mut engine, &mut at, -1));
        assert_eq!(s.url, "b");
        assert!(go(&mut s, &mut span, &mut engine, &mut at, -1));
        assert_eq!((s.url.as_str(), s.index), ("a", 0));
        assert!(!go(&mut s, &mut span, &mut engine, &mut at, -1), "first entry");
        assert_eq!(step(&s, span, -1), None);
        assert_eq!(s.history.len(), 3);
        assert!(go(&mut s, &mut span, &mut engine, &mut at, 1));
        assert!(go(&mut s, &mut span, &mut engine, &mut at, 1));
        assert_eq!((s.url.as_str(), s.index, s.history.len()), ("c", 2, 3));
        assert_eq!(step(&s, span, 1), None);
    }

    #[test]
    fn live_engine_history_is_used_directly() {
        let mut s = BrowserState::new("a".into());
        let mut span = Span::at(0);
        follow_url(&mut s, &mut span, "b", false);
        assert_eq!(step(&s, span, -1), Some(Step::Engine));
        assert_eq!(step(&s, span, 1), None);
        let (mut s, mut span) = reopened();
        follow_url(&mut s, &mut span, "b", false);
        assert_eq!((s.index, s.history.len()), (3, 4), "typing the previous entry's URL in a reopened page is a new entry");
        let (mut s, span) = reopened();
        assert_eq!(step(&s, span, -1), Some(Step::Load(1)));
        let mut span = Span { jump: Some(1), ..span };
        follow_url(&mut s, &mut span, "redirected", false);
        assert_eq!((s.index, s.history[1].url.as_str(), s.history.len(), span), (1, "redirected", 3, Span::at(1)), "a redirect replaces the entry");
    }

    #[test]
    fn page_chords_respect_text_fields_and_the_address_bar() {
        let cmd = gpui::Modifiers { platform: true, ..Default::default() };
        assert_eq!(page_chord("left", cmd, true, false), Some(Chord::Back));
        assert_eq!(page_chord("right", cmd, true, false), Some(Chord::Forward));
        assert_eq!(page_chord("left", cmd, true, true), None, "caret in a page field");
        assert_eq!(page_chord("right", cmd, true, true), None);
        assert_eq!(page_chord("left", cmd, false, false), None, "address bar");
        assert_eq!(page_chord("l", cmd, true, true), Some(Chord::Address));
        assert_eq!(page_chord(".", cmd, true, false), Some(Chord::Stop));
        assert_eq!(page_chord("[", cmd, true, false), None, "pane focus");
        assert_eq!(page_chord("]", cmd, true, false), None);
        let shift = gpui::Modifiers { shift: true, ..cmd };
        assert_eq!(page_chord("left", shift, true, false), None);
        assert_eq!(page_chord("x", cmd, true, true), Some(Chord::Edit(silo_web::Edit::Cut)));
        assert_eq!(page_chord("z", cmd, true, true), Some(Chord::Edit(silo_web::Edit::Undo)));
        assert_eq!(page_chord("z", shift, true, true), Some(Chord::Edit(silo_web::Edit::Redo)));
        assert_eq!(page_chord("x", cmd, false, false), None, "the address bar cuts its own text");
        assert_eq!(page_chord("=", cmd, true, true), Some(Chord::Zoom(1)));
        assert_eq!(page_chord("=", shift, true, false), Some(Chord::Zoom(1)), "⌘+ is ⇧⌘= on US layouts");
        assert_eq!(page_chord("-", cmd, true, false), Some(Chord::Zoom(-1)));
        assert_eq!(page_chord("0", cmd, true, false), Some(Chord::Zoom(0)));
        assert_eq!(page_chord("0", cmd, false, false), None, "the address bar");
    }

    #[test]
    fn zoom_steps_by_half_levels_within_limits() {
        assert!((zoom_step(0.0, 1) - 0.5).abs() < f32::EPSILON);
        assert!((zoom_step(0.3, -1) - 0.0).abs() < f32::EPSILON, "snaps to half levels");
        assert!((zoom_step(5.0, 1) - 5.0).abs() < f32::EPSILON);
        assert!((zoom_step(-3.0, -1) + 3.0).abs() < f32::EPSILON);
        assert!(zoom_step(2.5, 0).abs() < f32::EPSILON);
    }
}
