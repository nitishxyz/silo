//! Provider sign-in (`docs/AGENT_BLOCK.md` §4b "Sign-in"): the first-run
//! screen and the Providers view share one list; entering an API key, pasting
//! a login code or waiting for the browser happens in a dialog over the pane.

use std::{
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, BoxShadow, ClipboardItem, Context, Div, FocusHandle, FontWeight, Hsla, SharedString, Stateful, Window, div, point, prelude::*, px,
    rgb,
};
use otto_proto::{AuthMode, AuthStatus, LoginResult, Method};

use super::{
    AgentPane,
    pill_input::{self, PillInputEvent},
    rows,
    view::eased,
};
use crate::{
    assets::icon,
    motion,
    theme::{self, Continuous as _},
};

const WIDTH: f32 = 360.0;
/// Every row action (`API key`, `Sign in`) has this width, so rows line up.
const ROW_BUTTON: f32 = 84.0;
const DIALOG_BUTTON: f32 = 88.0;
/// Provider logos in the row's icon column.
const LOGO: f32 = 18.0;
const DIALOG_ENTER: Duration = Duration::from_millis(200);
const DIALOG_EXIT: Duration = Duration::from_millis(160);
/// A row settling into its Connected state.
const LAND: Duration = Duration::from_millis(260);

#[derive(Default)]
pub(super) struct SignIn {
    /// Shown while signed in too (add a provider, disconnect), with a way back.
    open: bool,
    dialog: Option<Dialog>,
    /// A row action in flight; every row action is disabled meanwhile.
    busy: Option<(String, Act)>,
    /// Signed in, waiting for the reloaded status: the button keeps spinning.
    landing: Option<(String, Act, Instant)>,
    /// Provider (empty: none in particular) and a short reason, shown in its row.
    error: Option<(String, String)>,
    /// Provider whose `Disconnect` asks once more.
    confirm: Option<String>,
    /// Rows that just turned Connected, for their fade-in.
    connected: HashMap<String, Instant>,
    /// Bumped on every start and cancel; stale results are dropped.
    attempt: u64,
    /// Focus the composer once it appears (after the first connection).
    pub(super) focus_composer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Subscribe,
    Key,
    Disconnect,
}

struct Dialog {
    provider: String,
    step: Step,
    error: Option<String>,
    busy: bool,
    /// The login behind `step` failed or expired: the primary action restarts it.
    spent: bool,
    shown: Instant,
    closing: Option<Instant>,
    /// Focus target for steps without a field.
    focus: FocusHandle,
    /// Move focus into the dialog on the next frame.
    focus_pending: bool,
    copied: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    ApiKey,
    /// A local or custom server's address (and, for `custom`, an optional key).
    Endpoint,
    Login {
        login_id: String,
        url: String,
        flow: Flow,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Flow {
    /// The provider shows a code to paste back (Anthropic).
    Code,
    /// The daemon waits for the browser's loopback redirect (ChatGPT).
    Browser,
    /// Device authorization: the user types `code` on the page.
    Device { code: String },
}

impl Flow {
    fn of(user_code: Option<String>, needs_code: bool) -> Self {
        match user_code {
            Some(code) => Self::Device { code },
            None if needs_code => Self::Code,
            None => Self::Browser,
        }
    }
}

/// Providers offered for sign-in, in daemon order; rows with no sign-in modes
/// (the test model) are left out.
fn providers(auth: &[AuthStatus]) -> Vec<&AuthStatus> {
    let mut seen: Vec<&str> = Vec::new();
    auth.iter()
        .filter(|a| !a.modes.is_empty())
        .filter(|a| {
            let new = !seen.contains(&a.provider.as_str());
            seen.push(&a.provider);
            new
        })
        .collect()
}

/// The subscription plan, shown as muted text (the button always says `Sign in`).
fn plan(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "Claude Pro/Max",
        "openai" => "ChatGPT Plus/Pro",
        _ => "Subscription",
    }
}

/// Providers reached at an address the user sets: their default, if any.
fn endpoint(provider: &str) -> Option<&'static str> {
    match provider {
        "ollama" => Some("http://127.0.0.1:11434"),
        "lmstudio" => Some("http://127.0.0.1:1234/v1"),
        "custom" => Some(""),
        _ => None,
    }
}

fn url_valid(url: &str) -> bool {
    let url = url.trim();
    (url.starts_with("http://") || url.starts_with("https://")) && url.len() > 10 && !url.contains(char::is_whitespace)
}

/// The row's second line when connected.
fn connection_label(status: &AuthStatus) -> &'static str {
    match status.provider.as_str() {
        "ollama" | "lmstudio" => "Connected · local",
        "custom" => "Connected",
        _ if status.mode == AuthMode::Subscription => "Connected · subscription",
        _ => "Connected · API key",
    }
}

/// The row's second line when not connected.
fn offer(status: &AuthStatus) -> &'static str {
    match status.provider.as_str() {
        "ollama" | "lmstudio" => "Local, no key needed",
        "custom" => "OpenAI-compatible server",
        p if status.modes.contains(&AuthMode::Subscription) => plan(p),
        _ => "API key",
    }
}

fn key_hint(provider: &str) -> String {
    match provider {
        "anthropic" => "Create one in the Anthropic Console".into(),
        "openai" => "Create one on the OpenAI platform".into(),
        "google" => "Create one in Google AI Studio".into(),
        "xai" => "Create one in the xAI Console".into(),
        "openrouter" => "Create one on OpenRouter".into(),
        "deepseek" => "Create one on the DeepSeek platform".into(),
        "moonshotai" => "Create one on the Moonshot platform".into(),
        "zai" => "Create one on the Z.ai platform".into(),
        "minimax" => "Create one on the MiniMax platform".into(),
        other => format!("Paste your {} key", super::view::provider_name(other)),
    }
}

/// A plausible key for `provider`: its prefix and a sane length.
fn key_valid(provider: &str, key: &str) -> bool {
    let key = key.trim();
    let ok = match provider {
        "anthropic" => key.starts_with("sk-ant-"),
        "openai" => key.starts_with("sk-"),
        _ => true,
    };
    ok && key.len() >= 20 && !key.contains(char::is_whitespace)
}

fn key_placeholder(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "sk-ant-…",
        "openai" => "sk-…",
        _ => "Paste an API key",
    }
}

fn logo(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "provider-anthropic",
        "openai" => "provider-openai",
        "google" => "provider-google",
        "xai" => "provider-xai",
        "openrouter" => "provider-openrouter",
        "deepseek" => "provider-deepseek",
        "moonshotai" => "provider-moonshotai",
        "zai" => "provider-zai",
        "minimax" => "provider-minimax",
        "ollama" => "provider-ollama",
        "lmstudio" => "provider-lmstudio",
        "custom" => "plug",
        _ => "agent",
    }
}

/// One line, no trailing period, at most ~100 characters.
fn short(error: &str) -> String {
    let line = error.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Sign-in failed");
    let line = line.trim_end_matches('.');
    // `Auth: …`, `Timeout: …`: the error class adds nothing here.
    let line = match line.split_once(": ") {
        Some((class, rest)) if !class.contains(' ') && class.chars().next().is_some_and(char::is_uppercase) && !rest.is_empty() => rest,
        _ => line,
    };
    if line.chars().count() > 100 { format!("{}…", line.chars().take(99).collect::<String>()) } else { line.to_owned() }
}

fn hsla(c: u32, alpha: f32) -> Hsla {
    let mut h: Hsla = rgb(c).into();
    h.a *= alpha;
    h
}

fn ink(amount: f32) -> u32 {
    theme::mix(theme::background(), theme::foreground(), amount)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Filled with the foreground: the one main action.
    Primary,
    /// Outlined pill.
    Outline,
    /// Text only, fill on hover.
    Quiet,
}

/// A button's state: `loading` keeps its width and swaps the label for a spinner.
#[derive(Clone, Copy)]
struct State {
    enabled: bool,
    loading: bool,
}

const READY: State = State { enabled: true, loading: false };

impl AgentPane {
    /// A pill with hover, pressed, keyboard-focus, disabled and loading states.
    /// Enter / Space activate it when focused (Tab reaches it).
    #[allow(clippy::too_many_arguments)]
    fn button(
        &self,
        id: impl Into<SharedString>,
        kind: Kind,
        label: impl Into<SharedString>,
        width: f32,
        state: State,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let id: SharedString = id.into();
        let (fill, text) = match kind {
            Kind::Primary => (Some(rgb(theme::foreground())), theme::background()),
            Kind::Outline | Kind::Quiet => (None, theme::foreground()),
        };
        let ring = BoxShadow {
            color: hsla(theme::accent(), 0.55),
            offset: point(px(0.0), px(0.0)),
            blur_radius: px(0.0),
            spread_radius: px(2.0),
            inset: false,
        };
        let content = if state.loading {
            crate::browser_pane::spinner(&format!("signin-{id}"), 13.0, text, cx)
        } else {
            div().child(label.into()).into_any_element()
        };
        let el = div()
            .id(gpui::ElementId::Name(id))
            .flex_none()
            // `0.0`: sized by the label.
            .when(width > 0.0, |el| el.w(px(width)))
            .when(width <= 0.0, |el| el.px(px(10.0)))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .capsule()
            .border_1()
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(text))
            .map(|el| match (kind, fill) {
                (_, Some(fill)) => el.bg(fill).border_color(fill),
                (Kind::Outline, None) => el.border_color(rgb(ink(0.2))),
                _ => el.border_color(hsla(0, 0.0)).text_color(rgb(theme::muted())),
            })
            .child(content);
        if !state.enabled || state.loading {
            return el.when(!state.enabled, |el| el.opacity(0.4));
        }
        let entity = cx.entity().downgrade();
        let on = Rc::new(on);
        let (click, key) = (on.clone(), on);
        let (e1, e2) = (entity.clone(), entity);
        el.tab_index(0)
            .cursor_pointer()
            .focus_visible(move |s| s.shadow(vec![ring]))
            .map(|el| match kind {
                Kind::Primary => el.hover(|s| s.opacity(0.88)),
                Kind::Outline => el.hover(|s| s.bg(theme::hover_fill())),
                Kind::Quiet => el.hover(|s| s.bg(theme::hover_fill()).text_color(rgb(theme::foreground()))),
            })
            .active(|s| s.scale(0.97).opacity(0.8))
            .on_click(move |_, window, cx| {
                let _ = e1.update(cx, |this, cx| click(this, window, cx));
            })
            .on_key_down(move |event, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    let _ = e2.update(cx, |this, cx| key(this, window, cx));
                }
            })
    }

    /// Show the Providers view even while signed in: add a provider or disconnect.
    pub(super) fn open_signin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.signin.open = true;
        self.set_overlay(None, cx);
        // The composer goes away: keep keys (Esc) on the pane.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The transcript area shows the sign-in screen.
    pub(super) fn showing_signin(&self) -> bool {
        self.catalog.as_ref().is_some_and(|c| self.signin.open || !c.any_signed_in())
    }

    /// The composer (with its tray and footer) exists only once a provider is
    /// connected, and not while the Providers view is open.
    pub(super) fn composer_visible(&self) -> bool {
        !self.signin.open && self.catalog.as_ref().is_some_and(super::client::Catalog::any_signed_in)
    }

    /// Per frame: drop a dialog that finished closing, move focus into a new
    /// dialog, and hand focus to the composer when it appears.
    pub(super) fn settle_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self.signin.dialog.as_ref().is_some_and(|d| d.closing.is_some_and(|at| motion::enter(at, DIALOG_EXIT, now, cx.reduce_motion()).is_none()))
        {
            self.signin.dialog = None;
            if self.composer_visible() {
                self.focus_composer(window, cx);
            } else {
                window.focus(&self.focus, cx);
            }
        }
        if let Some(dialog) = self.signin.dialog.as_mut().filter(|d| d.focus_pending && d.closing.is_none()) {
            dialog.focus_pending = false;
            let target = match &dialog.step {
                Step::ApiKey => self.key_field.read(cx).focus_handle(cx),
                Step::Endpoint => self.url_field.read(cx).focus_handle(cx),
                Step::Login { flow: Flow::Code, .. } => self.code_field.read(cx).focus_handle(cx),
                Step::Login { .. } => dialog.focus.clone(),
            };
            window.focus(&target, cx);
        }
        if self.composer_visible() && (std::mem::take(&mut self.signin.focus_composer) || self.focus.is_focused(window)) {
            self.focus_composer(window, cx);
        }
    }

    /// The Providers view was opened over a signed-in pane (Esc / Back return to the chat).
    pub(super) fn managing_providers(&self) -> bool {
        self.signin.open && self.catalog.as_ref().is_some_and(super::client::Catalog::any_signed_in)
    }

    /// Esc on the pane: close the dialog, else drop a pending confirm, else leave the Providers view.
    pub(super) fn signin_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.dialog_open() {
            self.dialog_cancel(cx);
        } else if self.signin.confirm.take().is_some() {
            cx.notify();
        } else if self.managing_providers() {
            self.close_signin(window, cx);
        } else {
            return false;
        }
        true
    }

    pub(super) fn close_signin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_login(cx);
        self.reset_signin(cx);
        self.focus_composer(window, cx);
    }

    fn reset_signin(&mut self, cx: &mut Context<Self>) {
        let attempt = self.signin.attempt + 1;
        self.signin = SignIn { attempt, ..SignIn::default() };
        self.key_field.update(cx, |e, cx| e.clear(cx));
        self.code_field.update(cx, |e, cx| e.clear(cx));
        self.url_field.update(cx, |e, cx| e.clear(cx));
        cx.notify();
    }

    fn dialog_open(&self) -> bool {
        self.signin.dialog.as_ref().is_some_and(|d| d.closing.is_none())
    }

    fn open_dialog(&mut self, provider: String, step: Step, cx: &mut Context<Self>) {
        if step == Step::ApiKey {
            let p = provider.clone();
            let placeholder = key_placeholder(&provider);
            self.key_field.update(cx, |i, cx| {
                i.clear(cx);
                i.set_placeholder(placeholder, cx);
                i.set_submit_circle(true, cx);
                i.set_validate(Some(Rc::new(move |key: &str| key_valid(&p, key))), cx);
            });
        }
        if step == Step::Endpoint {
            let current = self.catalog.as_ref().and_then(|c| c.auth.iter().find(|a| a.provider == provider)).and_then(|a| a.base_url.clone());
            let default = endpoint(&provider).unwrap_or_default();
            let url = current.unwrap_or_else(|| default.to_owned());
            self.url_field.update(cx, |i, cx| {
                i.clear(cx);
                i.set_placeholder(if default.is_empty() { "https://host/v1" } else { default }, cx);
                i.set_validate(Some(Rc::new(url_valid)), cx);
                i.set_text(&url, cx);
            });
            // `custom` may need a key; local servers don't.
            self.key_field.update(cx, |i, cx| {
                i.clear(cx);
                i.set_placeholder("API key (optional)", cx);
                i.set_submit_circle(false, cx);
                i.set_validate(None, cx);
            });
        }
        self.signin.dialog = Some(Dialog {
            provider,
            step,
            error: None,
            busy: false,
            spent: false,
            shown: Instant::now(),
            closing: None,
            focus: cx.focus_handle(),
            focus_pending: true,
            copied: false,
        });
        cx.notify();
    }

    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.signin.dialog.as_mut().filter(|d| d.closing.is_none()) {
            dialog.closing = Some(Instant::now());
        }
        cx.notify();
    }

    /// Cancel: the login (if any) is dropped on the daemon too.
    fn dialog_cancel(&mut self, cx: &mut Context<Self>) {
        if !self.dialog_open() {
            return;
        }
        self.cancel_login(cx);
        self.close_dialog(cx);
    }

    /// Enter in a dialog field.
    /// Events from the dialog's pill input: it submits only valid values.
    pub(super) fn dialog_field_event(&mut self, event: &PillInputEvent, cx: &mut Context<Self>) {
        match event {
            PillInputEvent::Submit(value) => self.dialog_submit(Some(value.clone()), cx),
            PillInputEvent::Cancel => self.dialog_cancel(cx),
            PillInputEvent::Changed => {
                if let Some(dialog) = self.signin.dialog.as_mut().filter(|d| d.error.is_some() && !d.spent) {
                    dialog.error = None;
                    cx.notify();
                }
            }
        }
    }

    /// The dialog's primary action.
    /// `value`: what the pill input submitted (already validated).
    fn dialog_submit(&mut self, value: Option<String>, cx: &mut Context<Self>) {
        let Some(dialog) = self.signin.dialog.as_ref().filter(|d| d.closing.is_none() && !d.busy) else { return };
        let provider = dialog.provider.clone();
        match (&dialog.step, dialog.spent) {
            (Step::Login { .. }, true) => self.start_subscription(provider, cx),
            // Either field's ⏎: the address must be valid; the key is optional.
            (Step::Endpoint, _) => {
                let url = self.url_field.read(cx).text(cx);
                if !url_valid(&url) {
                    self.url_field.update(cx, |i, cx| i.set_error(Some("Enter an http:// or https:// address".into()), cx));
                    return;
                }
                let key = (provider == "custom").then(|| self.key_field.read(cx).text(cx)).filter(|k| !k.is_empty());
                self.set_dialog_busy(true);
                self.save_endpoint(provider, url, key, cx);
            }
            (Step::ApiKey, _) => {
                let Some(key) = value else { return };
                self.set_dialog_busy(true);
                self.save_key(provider, key, cx);
            }
            (Step::Login { login_id, flow: Flow::Code, .. }, false) => {
                let Some(code) = value else { return };
                let login_id = login_id.clone();
                self.set_dialog_busy(true);
                self.finish_login(login_id, Some(code), cx);
            }
            (Step::Login { url, .. }, false) => cx.open_url(url),
        }
        cx.notify();
    }

    fn set_dialog_busy(&mut self, busy: bool) {
        if let Some(dialog) = self.signin.dialog.as_mut() {
            dialog.busy = busy;
            if busy {
                dialog.error = None;
            }
        }
    }

    fn dialog_error(&mut self, error: String, spent: bool, cx: &mut Context<Self>) {
        if let Some(dialog) = self.signin.dialog.as_mut().filter(|d| d.closing.is_none()) {
            dialog.busy = false;
            dialog.spent = spent;
            dialog.error = Some(error);
        }
        cx.notify();
    }

    /// Signed in: the dialog closes and the row keeps its spinner until the
    /// reloaded status shows it Connected.
    fn signed_in(&mut self, provider: String, act: Act, cx: &mut Context<Self>) {
        self.signin.attempt += 1;
        self.signin.busy = None;
        self.signin.error = None;
        self.signin.landing = Some((provider, act, Instant::now()));
        self.signin.focus_composer = true;
        self.close_dialog(cx);
        self.key_field.update(cx, |e, cx| e.clear(cx));
        self.code_field.update(cx, |e, cx| e.clear(cx));
        cx.set_global(super::client::AuthChanged);
    }

    fn save_key(&mut self, provider: String, key: String, cx: &mut Context<Self>) {
        let attempt = self.signin.attempt;
        let p = provider.clone();
        self.run(
            cx,
            move |c| async move {
                c.call_typed::<LoginResult>(Method::AuthLogin {
                    provider: p,
                    mode: AuthMode::ApiKey,
                    api_key: Some(key),
                    device: false,
                    base_url: None,
                })
                .await
            },
            move |this, result, cx| {
                if this.signin.attempt != attempt {
                    return;
                }
                match result {
                    Ok(_) => this.signed_in(provider, Act::Key, cx),
                    Err(error) => this.dialog_error(short(&error), false, cx),
                }
            },
        );
    }

    /// Save a local or custom server's address (and key); connected once it answers.
    fn save_endpoint(&mut self, provider: String, url: String, key: Option<String>, cx: &mut Context<Self>) {
        let attempt = self.signin.attempt;
        let p = provider.clone();
        self.run(
            cx,
            move |c| async move {
                c.call_typed::<LoginResult>(Method::AuthLogin {
                    provider: p,
                    mode: AuthMode::ApiKey,
                    api_key: key,
                    device: false,
                    base_url: Some(url),
                })
                .await
            },
            move |this, result, cx| {
                if this.signin.attempt != attempt {
                    return;
                }
                match result {
                    Ok(LoginResult::SignedIn { auth, .. }) if !auth.signed_in => {
                        this.dialog_error("Saved, but no server answers there".into(), false, cx);
                    }
                    Ok(_) => this.signed_in(provider, Act::Key, cx),
                    Err(error) => this.dialog_error(short(&error), false, cx),
                }
            },
        );
    }

    /// `Sign in`: start the subscription login; its next step opens in the dialog.
    fn start_subscription(&mut self, provider: String, cx: &mut Context<Self>) {
        self.cancel_login(cx);
        let attempt = self.signin.attempt;
        self.signin.error = None;
        self.signin.confirm = None;
        if self.dialog_open() {
            self.set_dialog_busy(true);
        } else {
            self.signin.busy = Some((provider.clone(), Act::Subscribe));
        }
        let p = provider.clone();
        self.run(
            cx,
            move |c| async move {
                c.call_typed::<LoginResult>(Method::AuthLogin {
                    provider: p,
                    mode: AuthMode::Subscription,
                    api_key: None,
                    device: false,
                    base_url: None,
                })
                .await
            },
            move |this, result, cx| {
                if this.signin.attempt != attempt {
                    if let Ok(LoginResult::Pending { login_id, .. }) = result {
                        this.send_cancel(login_id, cx);
                    }
                    return;
                }
                this.signin.busy = None;
                match result {
                    Ok(LoginResult::SignedIn { .. }) => this.signed_in(provider, Act::Subscribe, cx),
                    Ok(LoginResult::Pending { url, login_id, user_code, verification_uri, needs_code, .. }) => {
                        let flow = Flow::of(user_code, needs_code);
                        let url = match &flow {
                            Flow::Device { .. } => verification_uri.unwrap_or(url),
                            _ => url,
                        };
                        cx.open_url(&url);
                        this.code_field.update(cx, |e, cx| e.clear(cx));
                        let waits = flow != Flow::Code;
                        let step = Step::Login { login_id: login_id.clone(), url, flow };
                        match this.signin.dialog.as_mut().filter(|d| d.closing.is_none()) {
                            Some(dialog) => {
                                dialog.step = step;
                                dialog.busy = false;
                                dialog.spent = false;
                                dialog.error = None;
                                dialog.focus_pending = true;
                            }
                            None => this.open_dialog(provider, step, cx),
                        }
                        if waits {
                            this.finish_login(login_id, None, cx);
                        }
                    }
                    Err(error) => {
                        if this.dialog_open() {
                            this.dialog_error(short(&error), true, cx);
                        } else {
                            this.signin.error = Some((provider, short(&error)));
                        }
                    }
                }
            },
        );
        cx.notify();
    }

    /// `auth.login_complete`: with the pasted code, or waiting for the browser / device approval.
    fn finish_login(&mut self, login_id: String, code: Option<String>, cx: &mut Context<Self>) {
        let id = login_id.clone();
        self.run(
            cx,
            move |c| async move { c.call_typed::<AuthStatus>(Method::AuthLoginComplete { login_id, code }).await },
            move |this, result, cx| {
                let Some(dialog) = this.signin.dialog.as_ref().filter(|d| d.closing.is_none()) else { return };
                if !matches!(&dialog.step, Step::Login { login_id, .. } if *login_id == id) {
                    return;
                }
                let provider = dialog.provider.clone();
                match result {
                    Ok(_) => this.signed_in(provider, Act::Subscribe, cx),
                    // Each login is single-use: retrying starts a new one.
                    Err(error) => this.dialog_error(short(&error), true, cx),
                }
            },
        );
    }

    /// Abandon the shown login; the daemon drops it (and its browser listener).
    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        self.signin.attempt += 1;
        self.signin.busy = None;
        if let Some(Dialog { step: Step::Login { login_id, .. }, spent: false, closing: None, .. }) = &self.signin.dialog {
            self.send_cancel(login_id.clone(), cx);
        }
        cx.notify();
    }

    fn send_cancel(&self, login_id: String, cx: &mut Context<Self>) {
        self.run(cx, move |c| async move { c.call_typed::<serde_json::Value>(Method::AuthLoginCancel { login_id }).await }, |_, _, _| {});
    }

    /// Disconnect; a key from the environment stays in effect and is reported.
    fn logout(&mut self, provider: String, cx: &mut Context<Self>) {
        self.signin.confirm = None;
        self.signin.busy = Some((provider.clone(), Act::Disconnect));
        self.signin.error = None;
        let p = provider.clone();
        self.run(
            cx,
            move |c| async move {
                c.call_typed::<serde_json::Value>(Method::AuthLogout { provider: p.clone() }).await?;
                Ok(c.auth_status().await?.into_iter().any(|a| a.provider == p && a.signed_in))
            },
            move |this, result, cx| {
                this.signin.busy = None;
                this.signin.connected.remove(&provider);
                match result {
                    Ok(still) => {
                        if still {
                            this.signin.error = Some((provider, "Still connected by an environment variable".into()));
                        }
                        cx.set_global(super::client::AuthChanged);
                    }
                    Err(error) => this.signin.error = Some((provider, short(&error))),
                }
            },
        );
        cx.notify();
    }

    pub(super) fn render_signin(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let auth = self.catalog.as_ref().map(|c| c.auth.clone()).unwrap_or_default();
        let managing = auth.iter().any(|a| a.signed_in);
        let (title, subtitle) =
            if managing { ("Providers", "Connect the models otto can use") } else { ("Sign in to start", "Choose a model provider") };
        let shown = providers(&auth);
        let count = shown.len();
        let mut list = div().w_full().mt(px(22.0)).flex().flex_col();
        for (i, status) in shown.into_iter().enumerate() {
            list = list.child(self.render_provider(i, status, cx));
            if i + 1 < count {
                list = list.child(div().w_full().h(px(1.0)).bg(rgb(ink(0.08))));
            }
        }
        let general = self.signin.error.as_ref().filter(|(p, _)| p.is_empty()).map(|(_, e)| e.clone());
        // Every element starts at the column's left edge: `‹ Back` (Providers
        // only, a small text button), then the title and subtitle.
        let back = managing.then(|| {
            div().flex().ml(px(-8.0)).mb(px(10.0)).child(
                self.button("signin-back", Kind::Quiet, "", 0.0, READY, cx, |this, window, cx| this.close_signin(window, cx))
                    .pl(px(4.0))
                    .pr(px(10.0))
                    .child(icon("back", 14.0, muted))
                    .child(div().ml(px(2.0)).child("Back")),
            )
        });
        let header = div()
            .w_full()
            .flex()
            .flex_col()
            .children(back)
            .when(!managing, |el| el.child(div().mb(px(10.0)).child(icon("plug", 20.0, muted))))
            .child(div().text_size(px(15.0)).font_weight(FontWeight::MEDIUM).child(title))
            .child(div().mt(px(3.0)).text_size(px(12.5)).text_color(rgb(muted)).child(subtitle));
        let column = div()
            // The first-run screen sits in the middle; the Providers sub-view starts at the top.
            .when(!managing, |el| el.my_auto())
            .py(px(if managing { 28.0 } else { 24.0 }))
            .w(px(WIDTH))
            .max_w_full()
            .flex()
            .flex_col()
            .child(header)
            .child(list)
            .children(general.map(|e| div().mt(px(10.0)).text_size(px(12.0)).text_color(rgb(theme::danger())).child(e)));
        // Scrolls when the pane is shorter than the list.
        div()
            .size_full()
            .child(div().id("agent-signin").size_full().overflow_y_scroll().flex().flex_col().items_center().px(px(20.0)).child(column))
            .into_any_element()
    }

    fn render_provider(&mut self, i: usize, status: &AuthStatus, cx: &mut Context<Self>) -> AnyElement {
        let now = Instant::now();
        let provider = status.provider.clone();
        // The reloaded status shows it (or the reload failed): stop spinning.
        if self.signin.landing.as_ref().is_some_and(|(p, _, at)| *p == provider && (status.signed_in || now >= *at + Duration::from_secs(5))) {
            self.signin.landing = None;
            if status.signed_in {
                self.signin.connected.insert(provider.clone(), now);
            }
        }
        let landing = self.signin.landing.as_ref().map(|(p, a, _)| (p, *a));
        let spinning = self.signin.busy.as_ref().map(|(p, a)| (p, *a)).or(landing).filter(|(p, _)| **p == provider).map(|(_, a)| a);
        let locked = self.signin.busy.is_some() || self.signin.landing.is_some() || self.dialog_open();
        let state = |act: Act| State { enabled: !locked || spinning == Some(act), loading: spinning == Some(act) };
        let error = self.signin.error.as_ref().filter(|(p, _)| *p == provider).map(|(_, e)| e.clone());
        let fade = self.signin.connected.get(&provider).and_then(|at| eased(*at, LAND, now, cx));
        let second = match (&error, status.signed_in) {
            (Some(e), _) => div().text_color(rgb(theme::danger())).child(e.clone()),
            (None, true) => div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(div().size(px(6.0)).rounded_full().bg(rgb(theme::success())))
                .child(connection_label(status)),
            (None, false) => div().child(offer(status)),
        };
        let p = provider.clone();
        let actions = if endpoint(&provider).is_some() {
            // Local and custom servers: set or change the address (no sign-out:
            // a running server stays usable).
            div().child(
                self.button(
                    format!("signin-endpoint-{i}"),
                    if status.signed_in { Kind::Quiet } else { Kind::Outline },
                    if status.signed_in { "Change" } else { "Connect" },
                    ROW_BUTTON,
                    state(Act::Key),
                    cx,
                    move |this, _, cx| {
                        this.signin.error = None;
                        this.signin.confirm = None;
                        this.open_dialog(p.clone(), Step::Endpoint, cx);
                    },
                )
                .when_some(fade, |el, t| el.opacity(t)),
            )
        } else if status.signed_in {
            let asking = self.signin.confirm.as_deref() == Some(provider.as_str());
            let p2 = provider.clone();
            div()
                .child(
                    self.button(
                        format!("signin-logout-{i}"),
                        Kind::Quiet,
                        if asking { "Disconnect?" } else { "Disconnect" },
                        ROW_BUTTON,
                        state(Act::Disconnect),
                        cx,
                        move |this, _, cx| {
                            if this.signin.confirm.as_deref() == Some(p.as_str()) {
                                this.logout(p.clone(), cx);
                            } else {
                                this.signin.confirm = Some(p.clone());
                                cx.notify();
                            }
                        },
                    )
                    .when(asking, |el| el.text_color(rgb(theme::danger())))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if !hovered && this.signin.confirm.as_deref() == Some(p2.as_str()) {
                            this.signin.confirm = None;
                            cx.notify();
                        }
                    })),
                )
                .when_some(fade, |el, t| el.opacity(t))
        } else {
            let p2 = provider.clone();
            div()
                .flex()
                .gap(px(6.0))
                .when(status.modes.contains(&AuthMode::ApiKey), |el| {
                    el.child(self.button(format!("signin-key-{i}"), Kind::Outline, "API key", ROW_BUTTON, state(Act::Key), cx, move |this, _, cx| {
                        this.signin.error = None;
                        this.signin.confirm = None;
                        this.key_field.update(cx, |e, cx| e.clear(cx));
                        this.open_dialog(p.clone(), Step::ApiKey, cx);
                    }))
                })
                .when(status.modes.contains(&AuthMode::Subscription), |el| {
                    el.child(self.button(
                        format!("signin-sub-{i}"),
                        Kind::Primary,
                        "Sign in",
                        ROW_BUTTON,
                        state(Act::Subscribe),
                        cx,
                        move |this, _, cx| this.start_subscription(p2.clone(), cx),
                    ))
                })
        };
        // The shared list row, its logo glyph on the column's left edge
        // (the logo column centers it) and its buttons on the right edge.
        let inset = (rows::ICON_COL - LOGO) / 2.0;
        let row = rows::row(
            ("signin-row", i),
            false,
            Some(icon(logo(&provider), LOGO, theme::foreground()).into_any_element()),
            div().font_weight(FontWeight::MEDIUM).child(super::view::provider_name(&provider)),
            Some(second.when_some(fade.filter(|_| error.is_none()), |el, t| el.opacity(t)).into_any_element()),
            Some(div().ml(px(12.0)).child(actions).into_any_element()),
        )
        .pl_0()
        .pr(px(inset));
        div().mx(px(-inset)).child(row).into_any_element()
    }

    /// The dialog over the pane: fades in and scales from 0.98.
    pub(super) fn render_signin_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.signin.dialog.as_ref()?;
        let now = Instant::now();
        let t = match dialog.closing {
            Some(at) => 1.0 - eased(at, DIALOG_EXIT, now, cx).unwrap_or(1.0),
            None => eased(dialog.shown, DIALOG_ENTER, now, cx).unwrap_or(1.0),
        };
        if t > 0.0 && t < 1.0 || dialog.closing.is_some() {
            window.request_animation_frame();
        }
        let muted = theme::muted();
        let provider = dialog.provider.clone();
        let name = super::view::provider_name(&provider);
        let (busy, spent, copied) = (dialog.busy, dialog.spent, dialog.copied);
        let error = dialog.error.clone();
        let focus = dialog.focus.clone();
        let step = dialog.step.clone();
        let (title, line, primary) = match &step {
            Step::ApiKey => (format!("{name} API key"), key_hint(&provider), "Save"),
            Step::Endpoint if provider == "custom" => (format!("Connect {name}"), "Any OpenAI-compatible server".to_owned(), "Save"),
            Step::Endpoint => (format!("Connect {name}"), format!("Where {name} runs; no key needed"), "Save"),
            Step::Login { flow: Flow::Code, .. } => {
                ("Finish signing in".to_owned(), "Authorize in your browser, then paste the code".to_owned(), "Continue")
            }
            Step::Login { flow: Flow::Browser, .. } => {
                ("Finish signing in".to_owned(), "Continue in your browser; this closes when you're done".to_owned(), "Open again")
            }
            Step::Login { flow: Flow::Device { .. }, .. } => {
                ("Finish signing in".to_owned(), "Enter this code on the page that opened".to_owned(), "Open again")
            }
        };
        let primary = if spent { "Try again" } else { primary };
        let pill = match &step {
            Step::ApiKey => Some(self.key_field.clone()),
            Step::Endpoint => Some(self.url_field.clone()),
            Step::Login { flow: Flow::Code, .. } => Some(self.code_field.clone()),
            Step::Login { .. } => None,
        };
        // Every body is one pill tall, so the dialog is the same size for each step.
        let body_h = pill_input::HEIGHT + 10.0;
        let body: AnyElement = match (&step, pill) {
            (_, Some(pill)) => {
                let error = error.clone().map(SharedString::from);
                pill.update(cx, |p, cx| {
                    p.set_loading(busy, cx);
                    p.set_enabled(!spent, cx);
                    p.set_error(error, cx);
                });
                // `custom` adds an optional key under the address.
                let key = (step == Step::Endpoint && provider == "custom").then(|| {
                    self.key_field.update(cx, |p, cx| p.set_loading(busy, cx));
                    div().mt(px(6.0)).child(self.key_field.clone())
                });
                div().child(pill).children(key).into_any_element()
            }
            (Step::Login { flow: Flow::Device { code }, .. }, None) => {
                let copy = code.clone();
                div()
                    .h(px(body_h))
                    .pl(px(20.0))
                    .pr(px(10.0))
                    .flex()
                    .items_center()
                    .capsule()
                    .bg(theme::well_fill())
                    .when(spent, |el| el.opacity(0.5))
                    .child(div().flex_1().text_size(px(20.0)).font_weight(FontWeight::MEDIUM).font_family(theme::summary().1).child(code.clone()))
                    .child(self.button(
                        "signin-copy",
                        Kind::Outline,
                        if copied { "Copied" } else { "Copy" },
                        ROW_BUTTON,
                        State { enabled: !spent, loading: false },
                        cx,
                        move |this, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            if let Some(d) = this.signin.dialog.as_mut() {
                                d.copied = true;
                            }
                            cx.notify();
                        },
                    ))
                    .into_any_element()
            }
            _ => div()
                .h(px(body_h))
                .px(px(20.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .capsule()
                .bg(theme::well_fill())
                .text_size(px(13.0))
                .text_color(rgb(muted))
                .when(!spent, |el| el.child(crate::browser_pane::spinner("signin-wait", 16.0, muted, cx)).child("Waiting for the browser…"))
                .into_any_element(),
        };
        let has_pill = matches!(&step, Step::ApiKey | Step::Endpoint | Step::Login { flow: Flow::Code, .. });
        // The pill carries its own error line; other steps reserve one here.
        let error_line = (!has_pill).then(|| {
            div()
                .mt(px(4.0))
                .h(px(16.0))
                .px(px(20.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(12.0))
                .text_color(rgb(theme::danger()))
                .children(error.clone())
        });
        let waiting = matches!(&step, Step::Login { flow: Flow::Browser | Flow::Device { .. }, .. }) && !spent;
        // The pill is the primary action; a footer only for reopening the page or restarting.
        let footer = (spent || waiting).then(|| {
            let kind = if spent { Kind::Primary } else { Kind::Quiet };
            div().mt(px(10.0)).flex().justify_end().child(self.button(
                "signin-dialog-ok",
                kind,
                primary,
                DIALOG_BUTTON,
                State { enabled: true, loading: busy },
                cx,
                |this, _, cx| this.dialog_submit(None, cx),
            ))
        });
        let close = div()
            .id("signin-dialog-close")
            .flex_none()
            .size(px(26.0))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .active(|s| s.scale(0.94))
            .child(icon("close", 12.0, muted))
            .on_click(cx.listener(|this, _, _, cx| this.dialog_cancel(cx)));
        let card = div()
            .id("signin-dialog")
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| match event.keystroke.key.as_str() {
                "escape" => {
                    this.dialog_cancel(cx);
                    cx.stop_propagation();
                }
                "enter" => {
                    this.dialog_submit(None, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }))
            .occlude()
            .w(px(400.0))
            .max_w(gpui::relative(0.92))
            .flex()
            .flex_col()
            .pt(px(18.0))
            .px(px(20.0))
            .pb(px(16.0))
            .continuous(16.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .font_family(theme::ui_font())
            .text_size(px(13.0))
            .opacity(t)
            .scale(0.98 + 0.02 * t)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(icon(logo(&provider), 18.0, theme::foreground()))
                    .child(div().flex_1().text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(close.mr(px(-6.0))),
            )
            .child(div().mt(px(4.0)).text_size(px(12.5)).text_color(rgb(muted)).child(line))
            .child(div().mt(px(16.0)).child(body))
            .children(error_line)
            .children(footer);
        let closing = dialog.closing.is_some();
        Some(
            div()
                .id("signin-dialog-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .bg(hsla(0, (if theme::is_light() { 0.12 } else { 0.38 }) * t))
                .flex()
                .items_center()
                .justify_center()
                .when(!closing, |el| el.on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, _, cx| this.dialog_cancel(cx))))
                .child(card)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(provider: &str, modes: Vec<AuthMode>) -> AuthStatus {
        AuthStatus {
            provider: provider.into(),
            mode: AuthMode::ApiKey,
            signed_in: false,
            expires_at: None,
            account: None,
            modes,
            default_model: None,
            base_url: None,
        }
    }

    #[test]
    fn providers_skip_rows_without_sign_in_modes() {
        let both = vec![AuthMode::ApiKey, AuthMode::Subscription];
        let auth = vec![status("anthropic", both.clone()), status("openai", both.clone()), status("test", vec![]), status("openai", both)];
        let shown: Vec<&str> = providers(&auth).into_iter().map(|a| a.provider.as_str()).collect();
        assert_eq!(shown, ["anthropic", "openai"]);
    }

    #[test]
    fn every_provider_has_a_logo_and_endpoints_need_http() {
        let ids = ["anthropic", "openai", "google", "xai", "openrouter", "deepseek", "moonshotai", "zai", "minimax", "ollama", "lmstudio", "custom"];
        for provider in ids {
            assert!(crate::assets::has_icon(logo(provider)), "{provider}");
            assert_ne!(logo(provider), "agent", "{provider}");
        }
        assert_eq!(ids.iter().filter(|p| endpoint(p).is_some()).count(), 3);
        assert!(url_valid("http://127.0.0.1:11434"));
        assert!(!url_valid("127.0.0.1:11434"));
        assert!(!url_valid("http://a b"));
    }

    #[test]
    fn keys_need_their_provider_prefix() {
        assert!(key_valid("anthropic", " sk-ant-api03-abcdefghijklmnop "));
        assert!(!key_valid("anthropic", "sk-proj-abcdefghijklmnopqrstu"));
        assert!(!key_valid("anthropic", "sk-ant-short"));
        assert!(key_valid("openai", "sk-proj-abcdefghijklmnopqrstu"));
        assert!(!key_valid("openai", "pk-abcdefghijklmnopqrstuvwx"));
        assert!(!key_valid("openai", "sk-abc def ghi jkl mno pqr"));
        assert!(key_valid("other", "abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn pending_flow_follows_the_daemon() {
        assert_eq!(Flow::of(None, true), Flow::Code);
        assert_eq!(Flow::of(None, false), Flow::Browser);
        assert_eq!(Flow::of(Some("AB-CD".into()), false), Flow::Device { code: "AB-CD".into() });
    }

    #[test]
    fn labels_are_short_and_uniform() {
        assert_eq!(short("OAuth state mismatch; restart login."), "OAuth state mismatch; restart login");
        assert_eq!(short("\nfirst\nsecond"), "first");
        assert_eq!(short(""), "Sign-in failed");
        assert_eq!(short("Auth: OAuth state mismatch; restart login"), "OAuth state mismatch; restart login");
        assert_eq!(short("login cancelled: by user"), "login cancelled: by user");
        assert_eq!(short(&"x".repeat(300)).chars().count(), 100);
        assert_eq!(plan("anthropic"), "Claude Pro/Max");
        assert_eq!(plan("openai"), "ChatGPT Plus/Pro");
        let mut sub = status("openai", vec![]);
        sub.mode = AuthMode::Subscription;
        assert_eq!(connection_label(&sub), "Connected · subscription");
        assert_eq!(connection_label(&status("openai", vec![])), "Connected · API key");
    }
}
