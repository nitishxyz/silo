//! Otto settings, inside the agent pane (`docs/AGENT_BLOCK.md` §9): the
//! pane's own otto (`connection(&self.target)`), shown like the Providers
//! screen. Tabs: Providers (the sign-in list), Models (defaults for new
//! sessions), Agents (the agent editor) and Search; more go in [`Tab::ALL`].

use std::future::Future;

use gpui::{
    Anchor, AnyElement, AppContext as _, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription, Window, anchored, deferred,
    div, prelude::*, px, rgb,
};
use otto_client::{ClientError, Connection};
use otto_proto::{ApprovalMode, ConfigValue, Effort, Method};
use serde_json::Value;

use super::{
    AgentPane, client,
    settings_agents::{AgentsSettings, short_error},
    signin::{Kind, READY},
};
use crate::surface::Glassy as _;
use crate::{
    assets::icon,
    controls::{Segmented, Toggle},
    theme::{self, Continuous as _},
};

/// A tab of the otto settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Providers,
    Models,
    Agents,
    Search,
}

impl Tab {
    pub(crate) const ALL: [Self; 4] = [Self::Providers, Self::Models, Self::Agents, Self::Search];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Models => "Models",
            Self::Agents => "Agents",
            Self::Search => "Search",
        }
    }
}

/// Where the otto settings were opened from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// The pane header's settings button.
    Header,
    /// `/settings`.
    Slash,
    /// The palette's Otto Settings.
    Palette,
    /// `/providers` and the ⋮ menu's Providers.
    Providers,
}

/// The tab `entry` opens: Providers for providers, else Models (Providers
/// while nothing is signed in: there are no models yet).
pub(crate) fn entry_tab(entry: Entry, signed_in: bool) -> Tab {
    match entry {
        Entry::Providers => Tab::Providers,
        _ if !signed_in => Tab::Providers,
        Entry::Header | Entry::Slash | Entry::Palette => Tab::Models,
    }
}

/// The longest line of a description, however wide the pane.
pub(super) const DESCRIPTION_MAX: f32 = 560.0;

/// The view's title: the host it configures, for a remote one.
pub(crate) fn title(host: Option<&str>) -> String {
    host.map_or_else(|| "Otto".to_owned(), |host| format!("Otto on {host}"))
}

/// Default effort choices; `None` is the agent's, else the model's.
pub(crate) const EFFORTS: [(Option<Effort>, &str); 5] =
    [(None, "Default"), (Some(Effort::Low), "Low"), (Some(Effort::Medium), "Medium"), (Some(Effort::High), "High"), (Some(Effort::Max), "Max")];
pub(crate) const MODES: [(ApprovalMode, &str); 3] =
    [(ApprovalMode::Auto, "Auto"), (ApprovalMode::AskOnEffects, "Ask"), (ApprovalMode::AskAll, "Ask all")];
/// `search_engine` values; the first is otto's default.
pub(crate) const ENGINES: [(&str, &str); 3] = [("duckduckgo", "DuckDuckGo"), ("bing", "Bing"), ("google", "Google")];
/// otto's agent when `agent` is unset.
const DEFAULT_AGENT: &str = "build";

/// The `config.json` values the Models and Search tabs show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Defaults {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub agent: Option<String>,
    pub approval: ApprovalMode,
    pub native_compaction: bool,
    /// Snapshot the work tree at each turn's start and end for review (otto's default: on).
    pub turn_snapshots: bool,
    /// An [`ENGINES`] key.
    pub search: &'static str,
}

impl Defaults {
    /// From `config.get` (the whole object); unknown values read as unset.
    pub(crate) fn of(config: &Value) -> Self {
        let text = |key: &str| config.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_owned);
        let search = text("search_engine").and_then(|s| ENGINES.iter().find(|(k, _)| *k == s)).map_or(ENGINES[0].0, |(k, _)| k);
        Self {
            provider: text("provider"),
            model: text("model"),
            effort: config.get("effort").cloned().and_then(|v| serde_json::from_value(v).ok()),
            agent: text("agent"),
            approval: config.get("approval_mode").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default(),
            native_compaction: config.get("native_compaction").and_then(Value::as_bool).unwrap_or(false),
            turn_snapshots: config.get("turn_snapshots").and_then(Value::as_bool).unwrap_or(true),
            search,
        }
    }

    /// The model picker's label.
    pub(crate) fn model_label(&self) -> String {
        match (&self.provider, &self.model) {
            (_, None) => "Automatic".to_owned(),
            (Some(p), Some(m)) => format!("{m} · {}", super::provider_name(p)),
            (None, Some(m)) => m.clone(),
        }
    }

    pub(crate) fn effort_label(&self) -> &'static str {
        EFFORTS.iter().find(|(e, _)| *e == self.effort).map_or("Default", |(_, l)| l)
    }

    pub(crate) fn agent_label(&self) -> &str {
        self.agent.as_deref().unwrap_or(DEFAULT_AGENT)
    }

    pub(crate) fn mode_index(&self) -> usize {
        MODES.iter().position(|(m, _)| *m == self.approval).unwrap_or_default()
    }

    pub(crate) fn search_index(&self) -> usize {
        ENGINES.iter().position(|(k, _)| *k == self.search).unwrap_or_default()
    }
}

/// One change on the Models or Search tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// A provider and model; `None` is Automatic (the signed-in default).
    Model(Option<(String, String)>),
    Effort(Option<Effort>),
    Agent(String),
    Approval(ApprovalMode),
    Compaction(bool),
    Snapshots(bool),
    Search(&'static str),
}

impl Change {
    /// Its `config.set` calls, in order.
    pub(crate) fn writes(&self) -> Vec<(&'static str, Value)> {
        match self {
            Self::Model(Some((p, m))) => vec![("provider", Value::from(p.as_str())), ("model", Value::from(m.as_str()))],
            Self::Model(None) => vec![("provider", Value::Null), ("model", Value::Null)],
            Self::Effort(effort) => vec![("effort", effort.as_ref().map_or(Value::Null, json))],
            Self::Agent(name) => vec![("agent", Value::from(name.as_str()))],
            Self::Approval(mode) => vec![("approval_mode", json(mode))],
            Self::Compaction(on) => vec![("native_compaction", Value::Bool(*on))],
            Self::Snapshots(on) => vec![("turn_snapshots", Value::Bool(*on))],
            Self::Search(engine) => vec![("search_engine", Value::from(*engine))],
        }
    }

    /// Shown at once, before otto confirms.
    pub(crate) fn apply(&self, d: &mut Defaults) {
        match self.clone() {
            Self::Model(choice) => (d.provider, d.model) = choice.map_or((None, None), |(p, m)| (Some(p), Some(m))),
            Self::Effort(effort) => d.effort = effort,
            Self::Agent(name) => d.agent = Some(name),
            Self::Approval(mode) => d.approval = mode,
            Self::Compaction(on) => d.native_compaction = on,
            Self::Snapshots(on) => d.turn_snapshots = on,
            Self::Search(engine) => d.search = engine,
        }
    }
}

fn json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("plain enum")
}

/// The open otto settings of a pane.
pub(super) struct OttoSettings {
    tab: Tab,
    /// Opened before the catalog loaded: its tab is chosen again once it has.
    unsure: Option<Entry>,
    defaults: Entity<DefaultsView>,
    agents: Entity<AgentsSettings>,
}

impl AgentPane {
    /// Show the otto settings on the tab `entry` opens.
    pub fn open_otto_settings(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        let tab = entry_tab(entry, self.catalog.as_ref().is_some_and(client::Catalog::any_signed_in));
        if self.otto_settings.is_none() {
            let (rt, target) = (self.rt.clone(), self.target.clone());
            let defaults = cx.new(|cx| DefaultsView::new(rt.clone(), target.clone(), cx));
            let agents = cx.new(|_| AgentsSettings::new(rt, target, self.cwd.clone()));
            self.otto_settings = Some(OttoSettings { tab, unsure: None, defaults, agents });
        }
        if let Some(open) = &mut self.otto_settings {
            open.unsure = self.catalog.is_none().then_some(entry);
        }
        self.select_tab(tab, cx);
        self.set_overlay(None, cx);
        // The composer goes away: keys (Esc) stay on the pane.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The catalog loaded: settings opened before it land on the tab their entry opens.
    pub(super) fn catalog_loaded(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.otto_settings.as_mut().and_then(|open| open.unsure.take()) else { return };
        self.select_tab(entry_tab(entry, self.catalog.as_ref().is_some_and(client::Catalog::any_signed_in)), cx);
    }

    /// The pane header's settings button.
    pub fn open_settings_from_header(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_otto_settings(Entry::Header, window, cx);
    }

    pub(super) fn showing_otto_settings(&self) -> bool {
        self.otto_settings.is_some()
    }

    fn select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Some(open) = &mut self.otto_settings else { return };
        if open.tab == Tab::Agents && tab != Tab::Agents && !open.agents.update(cx, |a, cx| a.may_leave(cx)) {
            return;
        }
        open.tab = tab;
        cx.notify();
    }

    /// Back to the chat (an agent with unsaved changes asks first).
    pub(super) fn close_otto_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.otto_settings else { return };
        if !open.agents.update(cx, |a, cx| a.may_leave(cx)) {
            return;
        }
        self.otto_settings = None;
        self.close_signin(window, cx);
        cx.notify();
    }

    /// Esc: the agent editor's first, else back to the chat.
    pub(super) fn otto_settings_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(open) = &self.otto_settings else { return false };
        let agents = open.agents.clone();
        if open.tab == Tab::Agents && agents.update(cx, |a, cx| a.escape(cx)) {
            return true;
        }
        if let Some(open) = &self.otto_settings {
            open.defaults.update(cx, |d, cx| d.close_menu(cx));
        }
        self.close_otto_settings(window, cx);
        true
    }

    pub(super) fn render_otto_settings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(open) = &self.otto_settings else { return div().into_any_element() };
        let (tab, defaults, agents) = (open.tab, open.defaults.clone(), open.agents.clone());
        let muted = theme::muted();
        let back = div().flex().ml(px(-8.0)).mb(px(10.0)).child(
            self.button("otto-settings-back", Kind::Quiet, "", 0.0, READY, cx, |this, window, cx| this.close_otto_settings(window, cx))
                .pl(px(4.0))
                .pr(px(10.0))
                .child(icon("back", 14.0, muted))
                .child(div().ml(px(2.0)).child("Back")),
        );
        let mut tabs = div().flex().items_center().gap(px(2.0));
        for (i, t) in Tab::ALL.into_iter().enumerate() {
            let selected = t == tab;
            tabs = tabs.child(
                div()
                    .id(("otto-settings-tab", i))
                    .h(px(26.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .capsule()
                    .text_size(px(12.5))
                    .cursor_pointer()
                    .when(selected, |el| el.bg(theme::row_fill()).font_weight(FontWeight::MEDIUM))
                    .when(!selected, |el| el.text_color(rgb(muted)).hover(|s| s.bg(theme::hover_fill())))
                    .child(t.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.select_tab(t, cx))),
            );
        }
        let body = match tab {
            Tab::Providers => self.provider_list(cx),
            Tab::Models | Tab::Search => {
                defaults.update(cx, |d, _| d.page = tab);
                defaults.into_any_element()
            }
            Tab::Agents => agents.into_any_element(),
        };
        let column = div()
            .w_full()
            .py(px(28.0))
            .flex()
            .flex_col()
            .child(back)
            .child(div().text_size(px(15.0)).font_weight(FontWeight::MEDIUM).child(title(self.host.as_deref())))
            .child(div().mt(px(12.0)).mb(px(18.0)).child(tabs))
            .child(body);
        div()
            .size_full()
            .child(div().id("otto-settings").size_full().overflow_y_scroll().flex().flex_col().px(px(self.fit().pad)).child(column))
            .into_any_element()
    }
}

// ---- Models and Search ----

/// A picker of the Models tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Menu {
    Model,
    Effort,
    Agent,
}

/// A menu row: a label, whether it's the current value, and its change
/// (`None`: a section header).
type MenuRow = (SharedString, bool, Option<Change>);

/// The Models and Search tabs: otto's defaults for new sessions, loaded with
/// `config.get` and saved with `config.set` on every change.
pub(super) struct DefaultsView {
    rt: tokio::runtime::Handle,
    target: client::Otto,
    /// The tab shown.
    page: Tab,
    loaded: bool,
    defaults: Defaults,
    catalog: client::Catalog,
    error: Option<String>,
    menu: Option<Menu>,
    _subs: Vec<Subscription>,
}

impl DefaultsView {
    fn new(rt: tokio::runtime::Handle, target: client::Otto, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.observe_global::<client::AuthChanged>(Self::reload),
            // Another pane on this otto saved an agent or a default.
            cx.observe_global::<client::OttoChanged>(|this, cx| {
                if cx.global::<client::OttoChanged>().concerns(&this.target) {
                    this.reload(cx);
                }
            }),
        ];
        let mut view = Self {
            rt,
            target,
            page: Tab::Models,
            loaded: false,
            defaults: Defaults::of(&Value::Null),
            catalog: client::Catalog::default(),
            error: None,
            menu: None,
            _subs: subs,
        };
        view.reload(cx);
        view
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    fn run<T, F, Fut>(&self, cx: &mut Context<Self>, work: F, done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static)
    where
        T: Send + 'static,
        F: FnOnce(Connection) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    {
        let target = self.target.clone();
        let task = self.rt.spawn(async move { work(client::connection(&target).await?).await });
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(result) => result.map_err(|e| super::describe(&e)),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.run(
            cx,
            |c| async move {
                let catalog = client::catalog(&c).await?;
                let config = c.call_typed::<ConfigValue>(Method::ConfigGet { key: None }).await?.value;
                Ok((catalog, config))
            },
            |this, result, _| match result {
                Ok((catalog, config)) => {
                    this.catalog = catalog;
                    this.defaults = Defaults::of(&config);
                    this.loaded = true;
                }
                Err(error) => this.error = Some(short_error(&error)),
            },
        );
    }

    /// Show `change` at once and save it.
    fn change(&mut self, change: Change, cx: &mut Context<Self>) {
        self.menu = None;
        self.error = None;
        change.apply(&mut self.defaults);
        let writes = change.writes();
        self.run(
            cx,
            move |c| async move {
                for (key, value) in writes {
                    c.call(Method::ConfigSet { key: key.to_owned(), value }).await?;
                }
                Ok(())
            },
            |this, result, cx| match result {
                Ok(()) => cx.set_global(client::OttoChanged(this.target.clone())),
                Err(error) => {
                    this.error = Some(short_error(&error));
                    this.reload(cx);
                }
            },
        );
        cx.notify();
    }

    fn menu_rows(&self, menu: Menu) -> Vec<MenuRow> {
        let d = &self.defaults;
        let mut out: Vec<MenuRow> = Vec::new();
        match menu {
            Menu::Model => {
                out.push(("Automatic".into(), d.model.is_none(), Some(Change::Model(None))));
                let mut group: Option<String> = None;
                for m in self.catalog.usable() {
                    let provider = m.provider.as_str().to_owned();
                    if group.as_deref() != Some(provider.as_str()) {
                        out.push((super::provider_name(&provider).into(), false, None));
                        group = Some(provider.clone());
                    }
                    let checked = d.model.as_deref() == Some(m.id.as_str()) && d.provider.as_deref().is_none_or(|p| p == provider);
                    out.push((m.id.clone().into(), checked, Some(Change::Model(Some((provider, m.id.clone()))))));
                }
            }
            Menu::Effort => {
                out.extend(EFFORTS.iter().map(|(e, label)| ((*label).into(), d.effort == *e, Some(Change::Effort(*e)))));
            }
            Menu::Agent => {
                let mut names: Vec<String> = self.catalog.agents.iter().map(|a| a.name.clone()).collect();
                if names.is_empty() {
                    names.push(DEFAULT_AGENT.to_owned());
                }
                out.extend(names.into_iter().map(|n| (n.clone().into(), n == d.agent_label(), Some(Change::Agent(n)))));
            }
        }
        out
    }

    fn menu_trigger(&self, menu: Menu, label: String, cx: &mut Context<Self>) -> AnyElement {
        let open = self.menu == Some(menu);
        let trigger = div()
            .id(("otto-defaults-menu", menu as usize))
            .h(px(26.0))
            .min_w(px(180.0))
            .max_w(px(260.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .continuous(7.0)
            .bg(theme::control_fill())
            .border_1()
            .border_color(rgb(if open { theme::accent() } else { theme::border() }))
            .cursor_pointer()
            .hover(|s| s.bg(theme::row_fill()))
            .child(div().flex_1().min_w_0().text_ellipsis().whitespace_nowrap().overflow_hidden().text_size(px(12.5)).child(label))
            .child(icon("chevron", 11.0, theme::muted()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.menu = if this.menu == Some(menu) { None } else { Some(menu) };
                cx.notify();
            }));
        let mut root = div().relative().child(trigger);
        if open {
            root = root.child(div().absolute().top(px(30.0)).right_0().child(
                deferred(anchored().anchor(Anchor::TopRight).snap_to_window_with_margin(px(8.0)).child(self.menu_list(menu, cx))).with_priority(1),
            ));
        }
        root.into_any_element()
    }

    fn menu_list(&self, menu: Menu, cx: &mut Context<Self>) -> AnyElement {
        let _glass = crate::surface::enter();
        let mut list = div()
            .id(("otto-defaults-list", menu as usize))
            .w(px(240.0))
            .max_h(px(300.0))
            .overflow_y_scroll()
            .p(px(4.0))
            .flex()
            .flex_col()
            .gap(px(1.0))
            .glass(9.0)
            .font_family(theme::ui_font())
            .text_color(rgb(theme::foreground()))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)));
        for (i, (label, checked, change)) in self.menu_rows(menu).into_iter().enumerate() {
            let Some(change) = change else {
                list = list.child(
                    div().h(px(24.0)).px(px(8.0)).flex().items_end().pb(px(3.0)).text_size(px(11.0)).text_color(rgb(theme::muted())).child(label),
                );
                continue;
            };
            list = list.child(
                div()
                    .id(("otto-defaults-option", i))
                    .h(px(28.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .continuous(6.0)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::row_fill()))
                    .child(div().flex_1().min_w_0().text_ellipsis().whitespace_nowrap().overflow_hidden().text_size(px(12.5)).child(label))
                    .child(div().w(px(12.0)).children(checked.then(|| icon("check", 12.0, theme::accent()))))
                    .on_click(cx.listener(move |this, _, _, cx| this.change(change.clone(), cx))),
            );
        }
        list.into_any_element()
    }

    /// A row with an optional muted line under its title.
    fn row(title: &'static str, about: Option<&'static str>, control: AnyElement) -> AnyElement {
        div()
            .w_full()
            .min_h(px(44.0))
            .px(px(14.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child(title))
                    .children(about.map(|a| div().max_w(px(DESCRIPTION_MAX)).text_size(px(11.5)).text_color(rgb(theme::muted())).child(a))),
            )
            .child(div().flex_none().flex().items_center().justify_end().child(control))
            .into_any_element()
    }

    fn segmented(
        &self,
        id: &'static str,
        labels: Vec<&'static str>,
        selected: usize,
        pick: fn(usize) -> Change,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity().downgrade();
        Segmented::new(id, labels, selected)
            .on_select(move |index, _, cx| {
                let _ = entity.update(cx, |this, cx| this.change(pick(index), cx));
            })
            .into_any_element()
    }

    fn render_models(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let d = self.defaults.clone();
        let entity = cx.entity().downgrade();
        let compaction = Toggle::new("otto-native-compaction", "Native compaction", d.native_compaction)
            .on_toggle(move |on, _, cx| {
                let _ = entity.update(cx, |this, cx| this.change(Change::Compaction(on), cx));
            })
            .into_any_element();
        let entity = cx.entity().downgrade();
        let snapshots = Toggle::new("otto-turn-snapshots", "Snapshot each turn for review", d.turn_snapshots)
            .on_toggle(move |on, _, cx| {
                let _ = entity.update(cx, |this, cx| this.change(Change::Snapshots(on), cx));
            })
            .into_any_element();
        let card = AgentsSettings::card(vec![
            Self::row("Model", None, self.menu_trigger(Menu::Model, d.model_label(), cx)),
            Self::row("Effort", Some("An agent's own effort wins"), self.menu_trigger(Menu::Effort, d.effort_label().to_owned(), cx)),
            Self::row("Agent", None, self.menu_trigger(Menu::Agent, d.agent_label().to_owned(), cx)),
            Self::row(
                "Approvals",
                None,
                self.segmented("otto-default-mode", MODES.iter().map(|(_, l)| *l).collect(), d.mode_index(), |i| Change::Approval(MODES[i].0), cx),
            ),
            Self::row("Native compaction", Some("Let the provider compact long sessions when it can"), compaction),
            Self::row(
                "Snapshot each turn for review",
                Some("In a git repo, keep the work tree at each turn's start and end so its changes can be reviewed"),
                snapshots,
            ),
        ]);
        vec![AgentsSettings::titled("Defaults", None, card), AgentsSettings::note("Applies to new sessions.")]
    }

    fn render_search(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let engines = self.segmented(
            "otto-search-engine",
            ENGINES.iter().map(|(_, l)| *l).collect(),
            self.defaults.search_index(),
            |i| Change::Search(ENGINES[i].0),
            cx,
        );
        let card = AgentsSettings::card(vec![Self::row("Engine", Some("The results page web search reads"), engines)]);
        vec![AgentsSettings::titled("Web search", None, card)]
    }
}

impl Render for DefaultsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut children = Vec::new();
        children.extend(self.error.clone().map(AgentsSettings::error_line));
        if self.loaded {
            children.extend(if self.page == Tab::Search { self.render_search(cx) } else { self.render_models(cx) });
        } else if self.error.is_none() {
            children.push(AgentsSettings::note("Loading…"));
        }
        div().w_full().flex().flex_col().gap(px(14.0)).children(children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tabs_in_order() {
        let labels: Vec<_> = Tab::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(labels, ["Providers", "Models", "Agents", "Search"]);
    }

    #[test]
    fn titles_name_a_remote_host() {
        assert_eq!(title(None), "Otto");
        assert_eq!(title(Some("mini")), "Otto on mini");
    }

    #[test]
    fn each_entry_point_opens_its_tab() {
        for entry in [Entry::Header, Entry::Slash, Entry::Palette] {
            assert_eq!(entry_tab(entry, true), Tab::Models, "{entry:?}");
            assert_eq!(entry_tab(entry, false), Tab::Providers, "{entry:?}: nothing signed in yet");
        }
        assert_eq!(entry_tab(Entry::Providers, true), Tab::Providers);
        assert_eq!(entry_tab(Entry::Providers, false), Tab::Providers);
    }

    #[test]
    fn config_values_become_picker_states() {
        let unset = Defaults::of(&json!({}));
        assert_eq!((unset.model_label(), unset.effort_label(), unset.agent_label()), ("Automatic".to_owned(), "Default", "build"));
        assert_eq!((unset.mode_index(), unset.search_index(), unset.native_compaction), (0, 0, false));
        assert!(unset.turn_snapshots, "otto snapshots unless told not to");
        assert!(!Defaults::of(&json!({"turn_snapshots": false})).turn_snapshots);
        let set = Defaults::of(&json!({
            "provider": "anthropic", "model": "claude-x", "effort": "high", "agent": "plan",
            "approval_mode": "ask_all", "native_compaction": true, "search_engine": "google",
        }));
        assert_eq!(set.model_label(), "claude-x · Anthropic");
        assert_eq!((set.effort_label(), set.agent_label(), set.mode_index(), set.search_index()), ("High", "plan", 2, 2));
        assert!(set.native_compaction);
        // Unknown values read as unset rather than failing the page.
        let odd = Defaults::of(&json!({"effort": "loud", "approval_mode": 3, "search_engine": "altavista", "model": ""}));
        assert_eq!((odd.effort, odd.mode_index(), odd.search, odd.model), (None, 0, "duckduckgo", None));
    }

    #[test]
    fn changes_write_config_keys_that_read_back_the_same() {
        let mut config = json!({});
        let mut shown = Defaults::of(&config);
        for change in [
            Change::Model(Some(("openai".into(), "gpt-x".into()))),
            Change::Effort(Some(Effort::Max)),
            Change::Agent("plan".into()),
            Change::Approval(ApprovalMode::AskOnEffects),
            Change::Compaction(true),
            Change::Snapshots(false),
            Change::Search("bing"),
            Change::Effort(None),
            Change::Model(None),
        ] {
            change.apply(&mut shown);
            for (key, value) in change.writes() {
                config[key] = value;
            }
            assert_eq!(Defaults::of(&config), shown, "{change:?}");
        }
        assert_eq!(config["effort"], Value::Null);
        assert_eq!((config["provider"].clone(), config["model"].clone()), (Value::Null, Value::Null));
        assert_eq!(config["approval_mode"], "ask_on_effects");
        assert_eq!(Change::Effort(Some(Effort::Low)).writes(), [("effort", json!("low"))]);
        assert_eq!(Change::Model(Some(("a".into(), "m".into()))).writes(), [("provider", json!("a")), ("model", json!("m"))]);
    }
}
