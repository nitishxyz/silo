//! One browser block: a CEF page painted as a surface, a slim address strip,
//! engine state reported to the server (as owner) and server commands applied.

use std::time::{Duration, Instant};

use gpui::AppContext as _;
use silo_proto::{
    control::{BrowserCommand, Method},
    model::{BlockInfo, BlockKind, BrowserState, HistoryEntry},
};

/// Height of the address strip.
pub const STRIP: f32 = 28.0;
/// Engine changes are reported at most this often.
pub const REPORT_DEBOUNCE: Duration = Duration::from_millis(250);

/// Which pane a block gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Terminal,
    Browser,
}

#[must_use]
pub fn kind_of(info: Option<&BlockInfo>) -> Kind {
    match info.map(|i| i.kind) {
        Some(BlockKind::Browser) => Kind::Browser,
        _ => Kind::Terminal,
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
/// anything else a new navigation.
pub fn follow_url(state: &mut BrowserState, span: &mut Span, url: &str, back_forward: bool) {
    if let Some(target) = span.jump.take().filter(|&t| t < state.history.len()) {
        #[allow(clippy::cast_possible_wrap)]
        state.go(target as isize - state.index as isize);
        if state.url != url {
            url.clone_into(&mut state.history[target].url);
            url.clone_into(&mut state.url);
        }
        *span = Span::at(target);
        return;
    }
    if state.url == url {
        return;
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
}

/// ⌘L / ⌘R / ⌘. and ⌘X / ⌘Z / ⇧⌘Z on the page, and ⌘← / ⌘→ unless a page
/// text field has focus (there they move the caret). Nothing for the
/// address bar. ⌘C / ⌘V / ⌘A are app commands that reach the page too.
fn page_chord(key: &str, m: gpui::Modifiers, page_focused: bool, editable: bool) -> Option<Chord> {
    if !page_focused || !m.platform || m.control || m.alt {
        return None;
    }
    if m.shift {
        return key.eq_ignore_ascii_case("z").then_some(Chord::Edit(silo_web::Edit::Redo));
    }
    match key {
        "l" => Some(Chord::Address),
        "r" => Some(Chord::Reload),
        "." => Some(Chord::Stop),
        "x" => Some(Chord::Edit(silo_web::Edit::Cut)),
        "z" => Some(Chord::Edit(silo_web::Edit::Undo)),
        "left" if !editable => Some(Chord::Back),
        "right" if !editable => Some(Chord::Forward),
        _ => None,
    }
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
    debounce: Debounce,
    report_task: Option<gpui::Task<()>>,
    error: Option<String>,
    _subs: Vec<gpui::Subscription>,
}

impl BrowserPane {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block_id: String,
        rt: tokio::runtime::Handle,
        control: Option<silo_client::Control>,
        route: Route,
        home: std::path::PathBuf,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let address = cx.new(|cx| crate::text_field::TextField::new("Search or enter address", cx));
        let mut subs = vec![
            cx.on_focus(&focus, window, |this, _, cx| this.focused(true, cx)),
            cx.on_blur(&focus, window, |this, _, cx| this.focused(false, cx)),
        ];
        subs.push(cx.subscribe_in(&address, window, |this, _, event, window, cx| this.address_event(event, window, cx)));
        let address_focus = address.read(cx).focus.clone();
        subs.push(intercept(cx.weak_entity(), cx));
        subs.push(cx.on_blur(&address_focus, window, |this, _, cx| {
            this.editing = false;
            this.sync_address(cx);
        }));
        subs.push(cx.on_focus(&address_focus, window, |this, _, cx| {
            this.editing = true;
            this.own();
            cx.emit(BrowserPaneEvent::Activated);
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
            debounce: Debounce::default(),
            report_task: None,
            error: None,
            _subs: subs,
        }
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
            if fresh {
                self.ensure_page(cx);
            }
        }
        cx.notify();
    }

    fn is_owner(&self) -> bool {
        let me = self.control.as_ref().map(silo_client::Control::client_id);
        self.info.as_ref().is_some_and(|i| i.owner.is_some() && i.owner == me)
    }

    fn own(&self) {
        if self.is_owner() {
            return;
        }
        let Some(control) = self.control.clone() else { return };
        let block = self.block_id.clone();
        self.rt.spawn(async move {
            let mode = silo_proto::control::OwnMode::Take;
            let _ = control.call_value(Method::OwnBlock { block_ids: vec![block], mode }).await;
        });
    }

    fn focused(&mut self, focused: bool, cx: &mut gpui::Context<Self>) {
        if let Some(page) = &self.page {
            page.set_focus(focused);
        }
        if focused {
            self.own();
            cx.emit(BrowserPaneEvent::Activated);
        }
        cx.notify();
    }

    pub fn title(&self) -> String {
        let state = self.state.as_ref();
        match state.map(|s| s.title.trim()).filter(|t| !t.is_empty()) {
            Some(title) => title.to_owned(),
            None => state.map(|s| display_url(&s.url)).filter(|u| !u.is_empty()).unwrap_or_else(|| "New Tab".into()),
        }
    }

    pub fn favicon(&self) -> Option<String> {
        self.state.as_ref()?.favicon.clone()
    }

    pub fn bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.bounds
    }
}

impl BrowserPane {
    /// Open the page once both the server state and the layout are known.
    fn ensure_page(&mut self, cx: &mut gpui::Context<Self>) {
        if self.page.is_some() || self.starting || self.error.is_some() || self.state.is_none() || self.bounds.is_none() {
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
            let options = silo_web::BrowserOptions { url: &url, profile: &profile, size: self.view_size(), notify };
            anyhow::Ok((runtime.create_browser(options)?, dirty))
        })();
        self.starting = false;
        match opened {
            Ok((page, dirty)) => {
                let page = std::rc::Rc::new(page);
                crate::web::register(page.clone(), dirty, cx.weak_entity());
                self.page = Some(page);
                self.span = Span::at(state.index);
                self.error = None;
                tracing::info!(block = %self.block_id, url = %state.url, "browser opened");
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
        for event in page.take_events() {
            let Some(state) = self.state.as_mut() else { break };
            match event {
                silo_web::Event::UrlChanged { url, back_forward } => {
                    follow_url(state, &mut self.span, &url, back_forward);
                    changed = true;
                }
                silo_web::Event::TitleChanged(title) => {
                    follow_title(state, &title);
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
                    } else {
                        self.span.jump = None;
                    }
                    changed = true;
                }
                silo_web::Event::Progress(progress) => {
                    self.progress = progress;
                    cx.notify();
                }
                silo_web::Event::EditableFocus(editable) => self.editable = editable,
                silo_web::Event::Popup(url) => cx.emit(BrowserPaneEvent::Popup(url)),
                silo_web::Event::ImeBounds { x, y, width, height } => self.ime_rect = Some((x, y, width, height)),
                silo_web::Event::CursorChanged(cursor) => self.cursor = cursor,
                silo_web::Event::Closed => self.page = None,
            }
        }
        if changed {
            self.sync_address(cx);
            self.debounce.changed(Instant::now());
            self.schedule_report(cx);
        }
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
            BrowserCommand::Navigate { .. } => page.navigate(url.as_deref().unwrap_or_default()),
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
        }
        cx.notify();
    }
}

impl BrowserPane {
    /// Show the current URL unless the user is editing the address.
    fn sync_address(&mut self, cx: &mut gpui::Context<Self>) {
        let url = self.state.as_ref().map(|s| display_url(&s.url)).unwrap_or_default();
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
                let text = self.address.read(cx).text.clone();
                self.navigate(&text);
                self.focus.focus(window, cx);
            }
            FieldEvent::Cancel => {
                self.focus.focus(window, cx);
                self.sync_address(cx);
            }
            FieldEvent::Changed(_) | FieldEvent::Up | FieldEvent::Down => {}
        }
    }
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

    fn wheel(&self, event: &gpui::ScrollWheelEvent) {
        let (Some(page), Some((x, y))) = (self.page.as_ref(), self.local(event.position)) else { return };
        let delta = event.delta.pixel_delta(gpui::px(40.0));
        page.wheel(x, y, f32::from(delta.x), f32::from(delta.y), mods(event.modifiers));
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
            if let Some(frame) = entity.read(cx).page.as_ref().and_then(|p| p.frame()) {
                window.paint_surface(bounds, frame.buffer);
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

impl BrowserPane {
    fn strip(&self, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        use crate::theme::Continuous as _;
        use gpui::{div, prelude::*, px, rgb};
        let button = |id: &'static str, glyph: &'static str, tip: &'static str, enabled: bool| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(22.0))
                .continuous(crate::radius::CONTROL)
                .when(enabled, |el| el.cursor_pointer().hover(|s| s.bg(rgb(crate::theme::tab_hover_on(crate::theme::background())))))
                .tooltip(crate::tooltip::Tooltip::text(tip))
                .child(crate::assets::icon(glyph, 13.0, if enabled { crate::theme::foreground() } else { crate::theme::muted() }))
        };
        let loading = self.loading();
        let favicon = self.favicon().and_then(|f| favicon_image(&f));
        let reload = if loading {
            button("browser-reload", "close", "Stop  ⌘.", true).on_click(cx.listener(|this, _, _, _| this.stop()))
        } else {
            button("browser-reload", "reload", "Reload  ⌘R", self.page.is_some()).on_click(cx.listener(|this, _, _, _| this.reload()))
        };
        #[allow(clippy::cast_possible_truncation)]
        let progress = (loading && self.progress < 1.0).then(|| {
            div().absolute().left_0().bottom_0().h(px(2.0)).w(gpui::relative(self.progress.max(0.05) as f32)).bg(rgb(crate::theme::accent()))
        });
        div()
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .h(px(STRIP))
            .px(px(4.0))
            .border_b_1()
            .border_color(rgb(crate::theme::border()))
            .text_size(px(12.0))
            .child(button("browser-back", "back", "Back  ⌘←", self.history_step(-1)).on_click(cx.listener(|this, _, _, _| this.back())))
            .child(button("browser-forward", "forward", "Forward  ⌘→", self.history_step(1)).on_click(cx.listener(|this, _, _, _| this.forward())))
            .child(reload)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .flex_1()
                    .min_w_0()
                    .px(px(6.0))
                    .children(favicon.map(|f| gpui::img(f).size(px(14.0)).flex_none()))
                    .child(self.address.clone()),
            )
            .children(progress)
    }
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
    fn render(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
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
                if this.key(&e.keystroke, false) {
                    cx.stop_propagation()
                }
            }))
            .on_key_up(cx.listener(|this, e: &gpui::KeyUpEvent, _, cx| {
                if this.key(&e.keystroke, true) {
                    cx.stop_propagation()
                }
            }))
            .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _, _| this.mouse_move(e.position, e.modifiers)))
            .on_scroll_wheel(cx.listener(|this, e: &gpui::ScrollWheelEvent, _, _| this.wheel(e)))
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
                    cx.listener(|this, e: &gpui::MouseUpEvent, _, _| {
                        this.mouse_button(e.position, e.button, false, e.click_count, e.modifiers);
                    }),
                );
        }
        let page = page
            .on_mouse_down(gpui::MouseButton::Navigate(gpui::NavigationDirection::Back), cx.listener(|this, _, _, _| this.back()))
            .on_mouse_down(gpui::MouseButton::Navigate(gpui::NavigationDirection::Forward), cx.listener(|this, _, _, _| this.forward()));
        let page = page.child(page_canvas(cx.entity())).when_some(self.error.clone(), |el, _| {
            el.child(
                div().absolute().inset_0().flex().items_center().justify_center().text_color(rgb(crate::theme::muted())).child("Can't open the page"),
            )
        });
        div().flex().flex_col().size_full().min_w_0().min_h_0().child(self.strip(cx)).child(page)
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
        };
        let (id, rt, control, home) = (block.to_owned(), self.rt.clone(), self.control(), self.home.clone());
        let pane = cx.new(|cx| BrowserPane::new(id, rt, control, route, home, window, cx));
        let sub = cx.subscribe_in(&pane, window, |this, pane, event: &BrowserPaneEvent, _, cx| {
            let block = pane.read(cx).block_id.clone();
            match event {
                BrowserPaneEvent::Activated => this.pane_activated(block),
                BrowserPaneEvent::Popup(url) => this.split_browser(block, Some(url.clone())),
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
        let Some(control) = self.control() else { return };
        let method = browser_split(block.clone(), self.auto_axis(&block), None);
        let task = self.rt.spawn(async move { control.call::<silo_proto::control::Created>(method).await });
        cx.spawn_in(window, async move |this, cx| {
            let created = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| match created {
                Ok(created) => {
                    this.address_pending = Some(created.block_id);
                    this.focus_new_browser(window, cx);
                }
                Err(error) => tracing::warn!("new browser: {error:#}"),
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
    Method::BlockSplit { block_id: block, axis, spawn, kind: BlockKind::Browser, url }
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
            }
            cx.stop_propagation();
        });
    })
}

impl crate::workspace::Workspace {
    /// Content bounds of any pane, terminal or browser.
    pub(crate) fn block_bounds(&self, block: &str, cx: &gpui::App) -> Option<gpui::Bounds<gpui::Pixels>> {
        match self.browsers.get(block) {
            Some((pane, _)) => pane.read(cx).bounds(),
            None => self.panes.get(block).and_then(|(p, _)| p.read(cx).bounds()),
        }
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::model::{Lifecycle, TermSize};

    use super::*;

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
        }
    }

    #[test]
    fn pane_kind_follows_block_kind() {
        assert_eq!(kind_of(Some(&info(BlockKind::Browser))), Kind::Browser);
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
    }
}
