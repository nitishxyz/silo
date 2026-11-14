//! The servers' `init.lua` as the app sees it (`docs/LUA.md` §6): each
//! host's [`ConfigState`] (feature `config`), fetched on connect and on
//! `config.changed`; running what an `init.lua` binding names; the actions
//! ⌘P lists; and the reserved actions `open_file` and `open_url`.
//!
//! Only the home server (This Mac) sets keys (`keymap::set_config`);
//! every connected host's actions are offered.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, RwLock},
};

use gpui::{App, Context, Window};
use serde_json::{Map, Value, json};
use silo_client::Control;
use silo_proto::{
    config::{ActionInfo, ActionOrigin, ActionResult, ActionRunParams, ConfigState},
    control::{Method, RpcError},
};

use crate::{
    commands::Command,
    finder,
    keymap::{self, Bound, RunBinding, Target},
    status::Status,
    workspace::Workspace,
};

/// A host's config and the fetch it came from (a later fetch wins).
struct Known {
    generation: u64,
    state: Option<Arc<ConfigState>>,
}

static CONFIGS: LazyLock<RwLock<HashMap<usize, Known>>> = LazyLock::new(RwLock::default);

/// Host `host`'s config: `None` without the feature, refused (a share
/// link), or not answered yet.
#[must_use]
pub(crate) fn get(host: usize) -> Option<Arc<ConfigState>> {
    CONFIGS.read().ok()?.get(&host)?.state.clone()
}

/// Fetch `host`'s config (it connected, or said `config.changed`); `home`:
/// it is This Mac's, whose keys apply.
pub(crate) fn fetch(rt: &tokio::runtime::Handle, host: usize, home: bool, control: Control, cx: &mut App) {
    let generation = {
        let Ok(mut configs) = CONFIGS.write() else { return };
        let known = configs.entry(host).or_insert(Known { generation: 0, state: None });
        known.generation += 1;
        known.generation
    };
    if !control.peer_supports(silo_proto::compat::feature::CONFIG) {
        // Deferred: this runs inside `HostsState`'s update, and applying
        // refreshes the windows, which read it.
        cx.defer(move |cx| apply(host, home, generation, None, cx));
        return;
    }
    let task = rt.spawn(async move { control.call::<ConfigState>(Method::ConfigGet {}).await });
    cx.spawn(async move |cx| {
        let state = match task.await {
            Ok(Ok(state)) => Some(state),
            // Gone before it answered: what we had stays until it is back.
            Ok(Err(error))
                if error.is::<silo_client::Closed>()
                    || error.is::<silo_client::Offline>()
                    || error.is::<silo_client::TimedOut>()
                    || error.is::<silo_client::Upgrading>() =>
            {
                return;
            }
            // Refused (no `control`) or unknown (an older server): no config.
            Ok(Err(error)) => {
                tracing::debug!(host, "config.get: {error:#}");
                None
            }
            Err(_) => return,
        };
        cx.update(|cx| apply(host, home, generation, state, cx));
    })
    .detach();
}

fn apply(host: usize, home: bool, generation: u64, state: Option<ConfigState>, cx: &mut App) {
    {
        let Ok(mut configs) = CONFIGS.write() else { return };
        let Some(known) = configs.get_mut(&host).filter(|k| k.generation == generation) else { return };
        known.state = state.clone().map(Arc::new);
    }
    if home {
        keymap::set_config(state, cx);
    }
    let modes = keymap::modes();
    for window in cx.windows() {
        if let Some(handle) = window.downcast::<Workspace>() {
            let _ = handle.update(cx, |workspace, _, cx| {
                if home {
                    workspace.modes.forget_unknown(&modes);
                }
                cx.notify();
            });
        }
    }
}

/// `config.reload` on the home server; its `config.changed` brings the result.
///
/// # Errors
/// No home server connected, or one without `init.lua`.
pub(crate) fn reload_home(cx: &App) -> Result<(), String> {
    let hosts = crate::hosts_state::HostsState::try_global(cx).ok_or("Silo is starting.")?;
    let hosts = hosts.read(cx);
    let control =
        hosts.entries().iter().find(|h| h.info.local && !h.removed).and_then(|h| h.control.clone()).ok_or("This Mac's server isn't running.")?;
    if !control.peer_supports(silo_proto::compat::feature::CONFIG) {
        return Err("This Mac's server doesn't load init.lua.".into());
    }
    hosts.rt.spawn(async move {
        if let Err(error) = control.call_value(Method::ConfigReload {}).await {
            tracing::warn!("config.reload: {error:#}");
        }
    });
    Ok(())
}

// ---- ⌘P groups ----

/// Action categories seen, for their group labels (a handful per run).
static CATEGORIES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// The group of action category `category` (empty: "Actions").
pub(crate) fn category_id(category: &str) -> u16 {
    let label = if category.trim().is_empty() { "Actions" } else { category.trim() };
    let Ok(mut known) = CATEGORIES.lock() else { return 0 };
    let index = known.iter().position(|c| *c == label).unwrap_or_else(|| {
        known.push(Box::leak(label.to_owned().into_boxed_str()));
        known.len() - 1
    });
    u16::try_from(index).unwrap_or(u16::MAX)
}

#[must_use]
pub(crate) fn category_label(id: u16) -> &'static str {
    CATEGORIES.lock().ok().and_then(|k| k.get(usize::from(id)).copied()).unwrap_or("Actions")
}

/// The actions ⌘P offers from `sources` (the focused session's host's
/// config first, then the home server's): not hidden, the first of a name,
/// grouped by category (in order of names, the unnamed last).
#[must_use]
pub(crate) fn palette_actions<'a>(sources: &[(usize, &'a ConfigState)]) -> Vec<(usize, &'a ActionInfo)> {
    let mut out: Vec<(usize, &ActionInfo)> = Vec::new();
    for (host, config) in sources {
        for action in config.actions.iter().filter(|a| !a.hidden) {
            if !out.iter().any(|(_, a)| a.name == action.name) {
                out.push((*host, action));
            }
        }
    }
    out.sort_by_key(|(_, a)| (a.category.trim().is_empty(), a.category.trim().to_lowercase()));
    out
}

/// ⌘P's row for `action` of `host` (`elsewhere`: the name of a host other
/// than the home server it comes from).
#[must_use]
pub(crate) fn action_item(host: usize, action: &ActionInfo, elsewhere: Option<&str>, hint: Option<String>) -> finder::Item {
    let title = if action.title.is_empty() { action.name.clone() } else { action.title.clone() };
    let target = finder::Target::Action { host, name: action.name.clone() };
    let mut item = finder::Item::new(target, finder::Group::Action(category_id(&action.category)), title, format!("action:{}", action.name));
    item.keywords = format!("{} {} {}", action.name, action.description, action.keywords.join(" "));
    let detail = [Some(action.description.as_str()).filter(|d| !d.is_empty()), elsewhere].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    item.detail = (!detail.is_empty()).then_some(detail);
    item.right = hint.map(finder::Right::Shortcut);
    item
}

// ---- what a binding runs ----

/// The ids of what has focus, as a method or action takes them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Focus {
    pub session_id: Option<String>,
    pub window_id: Option<String>,
    pub block_id: Option<String>,
}

/// Where an action runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum On {
    /// The focused session's host.
    Here,
    Home,
}

/// Where action `name` runs: the focused session's host if its config
/// has it, else the home server (`home`: `None` when that is the same host).
#[must_use]
pub(crate) fn action_on(name: &str, here: Option<&ConfigState>, home: Option<&ConfigState>) -> Option<On> {
    let defines = |config: Option<&ConfigState>| config.is_some_and(|c| c.actions.iter().any(|a| a.name == name));
    if defines(here) {
        Some(On::Here)
    } else if defines(home) {
        Some(On::Home)
    } else {
        None
    }
}

/// What a binding comes to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Run {
    Nothing,
    Command(Command),
    /// An `app.*` method (`{method, params}`), run in this window.
    App(Value),
    /// A method of the focused session's host.
    Call {
        method: String,
        params: Value,
    },
    Action {
        on: On,
        params: ActionRunParams,
    },
    /// Shown as a toast.
    Missing(String),
}

/// `params` as an object (`null` is none).
fn object(params: Value) -> Value {
    if params.is_null() { json!({}) } else { params }
}

/// Which of `session_id`, `window_id`, `block_id` method `name` takes; `None`
/// for a method this build does not know. From its schema, once per method.
fn takes(method: &str) -> Option<[bool; 3]> {
    static CACHE: LazyLock<Mutex<HashMap<String, Option<[bool; 3]>>>> = LazyLock::new(Mutex::default);
    if let Some(found) = CACHE.lock().ok().and_then(|c| c.get(method).copied()) {
        return found;
    }
    let found = silo_proto::api::describe(method).map(|d| {
        let has = |key: &str| d.params_schema.get("properties").and_then(|p| p.get(key)).is_some();
        [has("session_id"), has("window_id"), has("block_id")]
    });
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(method.to_owned(), found);
    }
    found
}

/// `params` with the focus ids `method` takes and leaves out; a method this
/// build does not know is sent as given.
#[must_use]
pub(crate) fn fill_focus(method: &str, params: Value, focus: &Focus) -> Value {
    let mut map = match object(params) {
        Value::Object(map) => map,
        other => return other,
    };
    if let Some(takes) = takes(method) {
        for ((key, value), wanted) in
            [("session_id", &focus.session_id), ("window_id", &focus.window_id), ("block_id", &focus.block_id)].into_iter().zip(takes)
        {
            if wanted
                && !map.contains_key(key)
                && let Some(value) = value
            {
                map.insert(key.to_owned(), Value::String(value.clone()));
            }
        }
    }
    Value::Object(map)
}

/// `action.run`'s params; `focus` only when it is on the host that runs it.
#[must_use]
pub(crate) fn action_params(name: &str, args: Value, focus: Option<&Focus>, origin: Option<ActionOrigin>) -> ActionRunParams {
    let focus = focus.cloned().unwrap_or_default();
    ActionRunParams {
        name: name.to_owned(),
        args: object(args),
        session_id: focus.session_id,
        window_id: focus.window_id,
        block_id: focus.block_id,
        client_id: None,
        origin,
    }
}

/// What `bound` runs with `focus`: `here` is the focused session's host's
/// config, `home` the home server's when that is another host.
#[must_use]
pub(crate) fn resolve(bound: &Bound, focus: &Focus, here: Option<&ConfigState>, home: Option<&ConfigState>) -> Run {
    match &bound.target {
        Target::Blocked => Run::Nothing,
        Target::Command(command) => Run::Command(*command),
        Target::Method(name) if name.starts_with("app.") => Run::App(json!({ "method": name, "params": object(bound.args.clone()) })),
        Target::Method(name) => Run::Call { method: name.clone(), params: fill_focus(name, bound.args.clone(), focus) },
        Target::Action(name) => match action_on(name, here, home) {
            Some(On::Here) => Run::Action { on: On::Here, params: action_params(name, bound.args.clone(), Some(focus), Some(ActionOrigin::Key)) },
            Some(On::Home) => Run::Action { on: On::Home, params: action_params(name, bound.args.clone(), None, Some(ActionOrigin::Key)) },
            None => Run::Missing(format!("No action named “{name}”")),
        },
    }
}

// ---- the args prompt ----

/// The properties `schema` requires.
#[must_use]
pub(crate) fn required_args(schema: &Value) -> Vec<String> {
    schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}

/// The args prompt's placeholder. An action with one required argument is
/// asked for its value alone: `name`, with any others after it
/// (`name (optional: ratio=…)`). Else `file=… ratio=…`, the required first.
#[must_use]
pub(crate) fn args_placeholder(schema: &Value) -> String {
    let required = required_args(schema);
    let mut names: Vec<&String> = schema.get("properties").and_then(Value::as_object).map(|p| p.keys().collect()).unwrap_or_default();
    // By name: the order of a schema's properties is not kept everywhere.
    names.sort_by_key(|n| (!required.contains(n), n.as_str()));
    if let Some(sole) = silo_proto::config::sole_arg(schema) {
        let others: Vec<String> = names.iter().filter(|n| n.as_str() != sole).map(|n| format!("{n}=…")).collect();
        return if others.is_empty() { sole.to_owned() } else { format!("{sole} (optional: {})", others.join(" ")) };
    }
    let mut words: Vec<String> = names.iter().map(|n| format!("{n}=…")).collect();
    // Required names the schema lists no properties for.
    words.extend(required.iter().filter(|r| !names.contains(r)).map(|r| format!("{r}=…")));
    if words.is_empty() { "key=value …".into() } else { words.join(" ") }
}

/// A typed `key=value` value, as `silo do` reads it: `true`, `false`,
/// `null`, a number, else the text.
fn typed(text: &str) -> Value {
    match text {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => match serde_json::from_str::<Value>(text) {
            Ok(number @ Value::Number(_)) if text.trim() == text => number,
            _ => Value::String(text.to_owned()),
        },
    }
}

/// `text`'s words: spaces split them except inside double quotes.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut word));
                }
                started = false;
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// The args typed in the prompt, typed like `silo do`'s (a string where
/// `schema` declares one). `key=value` words name arguments (`"…"` keeps
/// spaces). Where one argument takes a bare value
/// ([`silo_proto::config::sole_arg`]), everything that is not a declared
/// `key=value` is that argument's value: `my new tab`, `app.log ratio=0.4`.
///
/// # Errors
/// A word without `=` when no argument takes a bare value, the bare
/// argument given twice, or a required one missing.
pub(crate) fn parse_args(text: &str, schema: &Value) -> Result<Value, String> {
    let property = |key: &str| schema.get("properties").and_then(|p| p.get(key));
    let value = |key: &str, raw: &str| {
        let string = property(key).and_then(|p| p.get("type")).and_then(Value::as_str) == Some("string");
        if string { Value::String(raw.to_owned()) } else { typed(raw) }
    };
    let sole = silo_proto::config::sole_arg(schema);
    let mut map = Map::new();
    let mut bare: Vec<String> = Vec::new();
    for word in words(text) {
        match word.split_once('=').filter(|(k, _)| !k.is_empty()) {
            // With a bare argument, only declared names are `key=value`:
            // a value may hold an `=` of its own.
            Some((key, raw)) if sole.is_none() || property(key).is_some() => {
                map.insert(key.to_owned(), value(key, raw));
            }
            _ if sole.is_some() => bare.push(word),
            _ => return Err(format!("“{word}” isn't key=value")),
        }
    }
    if let Some(key) = sole.filter(|_| !bare.is_empty()) {
        if map.contains_key(key) {
            return Err(format!("“{}” isn't key=value ({key} is already given)", bare[0]));
        }
        map.insert(key.to_owned(), value(key, &bare.join(" ")));
    }
    let missing: Vec<String> = required_args(schema).into_iter().filter(|r| !map.contains_key(r)).collect();
    if !missing.is_empty() {
        return Err(format!("Missing {}", missing.join(", ")));
    }
    Ok(Value::Object(map))
}

/// What a failed call says.
fn message(error: &anyhow::Error) -> String {
    error.downcast_ref::<RpcError>().map_or_else(|| format!("{error:#}"), |e| e.message.clone())
}

/// A connected host's config, with the host.
type HostConfig = (usize, Arc<ConfigState>);

impl Workspace {
    /// This Mac's host: the home server.
    pub(crate) fn home_host(&self) -> Option<usize> {
        self.hosts.iter().position(|h| h.info.local && !h.removed)
    }

    /// Host `host`'s config while it is connected.
    pub(crate) fn live_config(&self, host: usize) -> Option<Arc<ConfigState>> {
        self.hosts.get(host).filter(|h| h.control.is_some() && !h.removed)?;
        get(host)
    }

    /// The focused session, tab and block.
    pub(crate) fn focus_ids(&self, window: &Window, cx: &App) -> Focus {
        Focus {
            session_id: self.view.as_ref().map(|v| v.session.id.clone()),
            window_id: self.active_window().map(|w| w.id.clone()),
            block_id: self.focused_block(window, cx),
        }
    }

    /// The configs actions resolve against: the focused session's host's,
    /// and the home server's when that is another host.
    fn action_configs(&self) -> (Option<Arc<ConfigState>>, Option<HostConfig>) {
        let here = self.live_config(self.active_host);
        let home = self.home_host().filter(|h| *h != self.active_host).and_then(|h| Some((h, self.live_config(h)?)));
        (here, home)
    }

    /// An `init.lua` binding's key was pressed.
    pub(crate) fn run_binding(&mut self, action: &RunBinding, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bound) = keymap::bound(action.index) else { return };
        let focus = self.focus_ids(window, cx);
        let (here, home) = self.action_configs();
        let run = resolve(&bound, &focus, here.as_deref(), home.as_ref().map(|(_, c)| c.as_ref()));
        let home = home.map(|(h, _)| h);
        if bound.target != Target::Blocked && self.modes.ran(bound.mode.as_deref()) {
            cx.notify();
        }
        self.run_resolved(run, home, window, cx);
    }

    fn run_resolved(&mut self, run: Run, home: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        match run {
            Run::Nothing => {}
            Run::Command(command) => self.run(command, window, cx),
            Run::App(call) => match serde_json::from_value::<Method>(call) {
                Ok(Method::AppQuit { .. }) => crate::files_block::request_quit(cx),
                Ok(method) => {
                    if let Err(error) = self.run_app_method(self.active_host, method, window, cx) {
                        self.flash(Status::error(error.message), cx);
                    }
                }
                Err(error) => self.flash(Status::error(format!("Bad shortcut: {error}")), cx),
            },
            Run::Call { method, params } => {
                let Some(control) = self.control() else { return self.flash(Status::warning("Not connected"), cx) };
                let task = self.rt.spawn(async move { control.call_raw(&method, params).await });
                self.toast_failure(task, cx);
            }
            Run::Action { on, params } => {
                let host = match on {
                    On::Here => self.active_host,
                    On::Home => home.unwrap_or(self.active_host),
                };
                self.run_action(host, params, cx);
            }
            Run::Missing(text) => self.flash(Status::warning(text), cx),
        }
    }

    /// `action.run` on `host`; a failure is a toast, a result isn't shown.
    pub(crate) fn run_action(&mut self, host: usize, params: ActionRunParams, cx: &mut Context<Self>) {
        let Some(control) = self.hosts.get(host).and_then(|h| h.control.clone()) else {
            return self.flash(Status::warning("Not connected"), cx);
        };
        let task = self.rt.spawn(async move { control.call_value(Method::ActionRun(params)).await });
        self.toast_failure(task, cx);
    }

    fn toast_failure(&self, task: tokio::task::JoinHandle<anyhow::Result<Value>>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) = task.await {
                let _ = this.update(cx, |this, cx| this.flash(Status::error(message(&error)), cx));
            }
        })
        .detach();
    }

    /// ⌘P's actions: the focused session's host's, then the home server's.
    pub(crate) fn action_items(&self) -> Vec<finder::Item> {
        let (here, home) = self.action_configs();
        let mut sources: Vec<(usize, &ConfigState)> = Vec::new();
        if let Some(config) = &here {
            sources.push((self.active_host, config));
        }
        if let Some((host, config)) = &home {
            sources.push((*host, config));
        }
        let home = self.home_host();
        palette_actions(&sources)
            .into_iter()
            .map(|(host, action)| {
                let elsewhere = (Some(host) != home && host != self.active_host).then(|| self.hosts[host].info.name.as_str());
                action_item(host, action, elsewhere, keymap::action_hint(&action.name))
            })
            .collect()
    }

    /// Run reserved action `name` (`open_file`, `open_url`) with `args` when
    /// the focused session's host (else the home server) defines it, else,
    /// or when it returns `false` or fails, `fallback`. `block`: the block
    /// it is about, when not the focused one.
    #[allow(clippy::too_many_arguments, reason = "the reserved action's whole call")]
    pub(crate) fn reserved_or(
        &mut self,
        name: &'static str,
        args: Value,
        block: Option<String>,
        origin: Option<ActionOrigin>,
        fallback: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (here, home) = self.action_configs();
        let (host, focused) = match action_on(name, here.as_deref(), home.as_ref().map(|(_, c)| c.as_ref())) {
            Some(On::Here) => (self.active_host, true),
            Some(On::Home) => (home.map_or(self.active_host, |(h, _)| h), false),
            None => return fallback(self, window, cx),
        };
        let Some(control) = self.hosts.get(host).and_then(|h| h.control.clone()) else { return fallback(self, window, cx) };
        let mut focus = self.focus_ids(window, cx);
        if block.is_some() {
            focus.block_id = block;
        }
        let params = action_params(name, args, focused.then_some(&focus), origin);
        let task = self.rt.spawn(async move { control.call::<ActionResult>(Method::ActionRun(params)).await });
        cx.spawn_in(window, async move |this, cx| {
            let handled = match task.await {
                Ok(Ok(done)) => done.result != Value::Bool(false),
                Ok(Err(error)) => {
                    tracing::warn!("{name}: {error:#}");
                    false
                }
                Err(_) => false,
            };
            if !handled {
                let _ = this.update_in(cx, |this, window, cx| fallback(this, window, cx));
            }
        })
        .detach();
    }

    /// A ⌘-clicked link in terminal `block`: a `file://` path goes to
    /// `open_file`, anything else to `open_url`, before the default.
    pub(crate) fn open_terminal_link(&mut self, block: String, url: String, external: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (name, args) = match crate::terminal_links::file_path(&url) {
            Some(path) => ("open_file", json!({ "path": path })),
            None => ("open_url", json!({ "url": url })),
        };
        let beside = block.clone();
        let fallback = move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
            if external {
                cx.open_url(&url);
            } else {
                this.open_page_beside_block(&beside, url, cx);
            }
        };
        self.reserved_or(name, args, Some(block), None, fallback, window, cx);
    }
}

#[cfg(test)]
mod tests;
