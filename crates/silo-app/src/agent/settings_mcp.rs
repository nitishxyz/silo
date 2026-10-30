//! Otto settings › MCP: the MCP servers of a pane's otto for its project
//! (`mcp.list`), global and project, with their status and tool count; add,
//! edit, remove, on/off, and an HTTP server's browser sign-in
//! (`docs/AGENT_BLOCK.md` §9). Live from `mcp.status` and `mcp.changed`.

use std::{collections::BTreeMap, future::Future, time::Duration};

use gpui::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription, Task, Window, div, prelude::*, px, rgb,
};
use otto_client::{ClientError, Connection};
use otto_proto::{Event, McpScope, McpServer, McpStatus, McpTransport, ServerMessage};
use silo_client::callback::CallbackForward;

use super::{
    client::{self, Otto},
    editor::{Editor, EditorColors, EditorEvent},
    settings_agents::{AgentsSettings as Ui, hsla, mono, short_error},
};
use crate::{
    controls::{Segmented, Toggle},
    text_field::{FieldEvent, TextField},
    theme::{self, Continuous as _},
    tooltip::Tooltip,
};

// ---- the model ----

/// How otto reaches a server, as the form edits it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Kind {
    #[default]
    Stdio,
    Http,
}

/// The editor's fields, as typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Form {
    pub name: String,
    pub scope: McpScope,
    pub kind: Kind,
    pub command: String,
    /// Shell-style words: quotes group, `\` escapes.
    pub args: String,
    /// `KEY=VALUE` lines.
    pub env: String,
    pub url: String,
    /// `Name: value` lines.
    pub headers: String,
}

impl Default for Form {
    fn default() -> Self {
        Self::blank()
    }
}

impl Form {
    pub(crate) fn blank() -> Self {
        let empty = String::new;
        Self {
            name: empty(),
            scope: McpScope::Global,
            kind: Kind::Stdio,
            command: empty(),
            args: empty(),
            env: empty(),
            url: empty(),
            headers: empty(),
        }
    }

    pub(crate) fn of(server: &McpServer) -> Self {
        let mut form = Self { name: server.name.clone(), scope: server.scope, ..Self::blank() };
        match &server.transport {
            McpTransport::Stdio { command, args, env } => {
                form.command.clone_from(command);
                form.args = join_args(args);
                form.env = env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n");
            }
            McpTransport::Http { url, headers } => {
                form.kind = Kind::Http;
                form.url.clone_from(url);
                form.headers = headers.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
            }
        }
        form
    }

    /// What `mcp.add` / `mcp.update` send, or why it can't be saved.
    pub(crate) fn transport(&self) -> Result<McpTransport, &'static str> {
        match self.kind {
            Kind::Stdio => {
                let command = self.command.trim();
                if command.is_empty() {
                    return Err("Enter a command");
                }
                let args = split_args(&self.args).ok_or("Close the quote in the arguments")?;
                let env =
                    pairs(&self.env, '=', |k| k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).ok_or("Environment lines are KEY=VALUE")?;
                Ok(McpTransport::Stdio { command: command.to_owned(), args, env })
            }
            Kind::Http => {
                let url = self.url.trim();
                if !(url.starts_with("http://") || url.starts_with("https://")) || url.ends_with("//") {
                    return Err("The URL starts with http:// or https://");
                }
                let headers = pairs(&self.headers, ':', |k| k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                    .ok_or("Header lines are Name: value")?;
                Ok(McpTransport::Http { url: url.to_owned(), headers })
            }
        }
    }

    /// Why it can't be saved; `taken`: a server of that name in that scope (not this one).
    pub(crate) fn problem(&self, taken: impl Fn(&str, McpScope) -> bool) -> Option<&'static str> {
        let name = self.name.trim();
        if name.is_empty() {
            return Some("Name the server");
        }
        if name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Some("Names use letters, digits, -, _ and .");
        }
        if taken(name, self.scope) {
            return Some("That name is taken");
        }
        self.transport().err()
    }
}

/// Shell-style words: whitespace separates, quotes group, `\` escapes (not in
/// single quotes). `None`: a quote is left open.
pub(crate) fn split_args(text: &str) -> Option<Vec<String>> {
    let (mut out, mut word, mut started, mut quote) = (Vec::new(), String::new(), false, None::<char>);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"') | None, '\\') => {
                word.extend(chars.next());
                started = true;
            }
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => (quote, started) = (Some(c), true),
            (None, c) if c.is_whitespace() => {
                if std::mem::take(&mut started) {
                    out.push(std::mem::take(&mut word));
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    quote.is_none().then_some(out)
}

/// [`split_args`]'s inverse: words with spaces or quotes are quoted.
pub(crate) fn join_args(args: &[String]) -> String {
    let quoted = |a: &String| {
        if !a.is_empty() && !a.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\')) {
            return a.clone();
        }
        if a.contains('\'') { format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\"")) } else { format!("'{a}'") }
    };
    args.iter().map(quoted).collect::<Vec<_>>().join(" ")
}

/// `KEY<sep>VALUE` lines (blank lines skipped, the first `sep` splits, the key
/// trimmed and checked); `None`: a line isn't one.
pub(crate) fn pairs(text: &str, sep: char, key_ok: impl Fn(&str) -> bool) -> Option<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (key, value) = line.split_once(sep)?;
        let key = key.trim();
        if key.is_empty() || !key_ok(key) {
            return None;
        }
        out.insert(key.to_owned(), value.trim().to_owned());
    }
    Some(out)
}

/// An error for a row: its first line, no paths, at most about 60 characters.
pub(crate) fn brief(message: &str) -> String {
    let text = short_error(message.lines().next().unwrap_or_default());
    if text.chars().count() <= 60 {
        return text;
    }
    let cut: String = text.chars().take(60).collect();
    format!("{}…", cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head).trim_end_matches([',', ':', ';', '.']))
}

pub(crate) fn status_label(status: &McpStatus) -> String {
    match status {
        McpStatus::Connected => "Connected".into(),
        McpStatus::Connecting => "Connecting…".into(),
        McpStatus::Error { message } => format!("Error · {}", brief(message)),
        McpStatus::NeedsSignIn => "Needs sign-in".into(),
        McpStatus::Disabled => "Off".into(),
    }
}

/// The status dot's color.
fn status_color(status: &McpStatus) -> u32 {
    match status {
        McpStatus::Connected => theme::success(),
        McpStatus::Connecting | McpStatus::NeedsSignIn => theme::warning(),
        McpStatus::Error { .. } => theme::danger(),
        McpStatus::Disabled => theme::muted(),
    }
}

/// A connected server's tool count.
pub(crate) fn tools_label(server: &McpServer) -> Option<String> {
    (server.status == McpStatus::Connected).then(|| if server.tools == 1 { "1 tool".to_owned() } else { format!("{} tools", server.tools) })
}

/// What a row says the server is, without paths: the command's name and its
/// plain arguments (an npm package keeps its scope), or the URL's host.
pub(crate) fn summary(transport: &McpTransport) -> String {
    match transport {
        McpTransport::Stdio { command, args, .. } => {
            let name = command.trim().rsplit(['/', '\\']).next().unwrap_or_default();
            let plain = args.iter().filter(|a| a.starts_with('@') || !a.contains(['/', '\\'])).map(String::as_str);
            std::iter::once(name).chain(plain).collect::<Vec<_>>().join(" ")
        }
        McpTransport::Http { url, .. } => {
            let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
            let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
            host.rsplit_once('@').map_or(host, |(_, h)| h).to_owned()
        }
    }
}

pub(crate) fn scope_label(scope: McpScope) -> &'static str {
    match scope {
        McpScope::Global => "Global",
        McpScope::Project => "Project",
    }
}

/// A `mcp.status` event on the list: whether it named a listed server.
pub(crate) fn apply_status(servers: &mut [McpServer], name: &str, status: McpStatus, tools: u32) -> bool {
    let Some(server) = servers.iter_mut().find(|s| s.name == name) else { return false };
    server.enabled = status != McpStatus::Disabled;
    (server.status, server.tools) = (status, tools);
    true
}

/// After the browser: wait for its redirect, or have the user paste the
/// address it was sent to (a remote otto whose callback couldn't be forwarded here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Finish {
    Wait,
    Paste,
}

/// `forward`: for a remote otto, whether its callback port listens here now.
pub(crate) fn finish(remote: bool, forwarded: bool) -> Finish {
    if remote && !forwarded { Finish::Paste } else { Finish::Wait }
}

/// The server being edited and what it was when opened.
#[derive(Clone, Debug)]
pub(crate) struct Draft {
    pub form: Form,
    /// As saved: its name, scope, form and on/off; `None`: new.
    pub saved: Option<(String, McpScope, Form, bool)>,
}

impl Draft {
    pub(crate) fn new_server() -> Self {
        Self { form: Form::blank(), saved: None }
    }

    pub(crate) fn open(server: &McpServer) -> Self {
        let form = Form::of(server);
        Self { saved: Some((server.name.clone(), server.scope, form.clone(), server.enabled)), form }
    }

    pub(crate) fn dirty(&self) -> bool {
        self.saved.as_ref().map_or_else(|| self.form != Form::blank(), |(.., form, _)| *form != self.form)
    }

    /// Saved under another name or scope: added there, then removed here.
    pub(crate) fn moved_from(&self) -> Option<(String, McpScope)> {
        self.saved.as_ref().filter(|(name, scope, ..)| *name != self.form.name.trim() || *scope != self.form.scope).map(|(n, s, ..)| (n.clone(), *s))
    }

    /// Why it can't be saved, against the listed servers.
    pub(crate) fn problem(&self, servers: &[McpServer]) -> Option<&'static str> {
        let own = self.saved.as_ref().map(|(n, s, ..)| (n.as_str(), *s));
        self.form.problem(|name, scope| own != Some((name, scope)) && servers.iter().any(|s| s.name == name && s.scope == scope))
    }
}

// ---- the view ----

/// How long a remote sign-in's callback forward listens (otto waits 10 minutes).
const FORWARD_TTL: Duration = Duration::from_secs(600);

#[derive(Clone)]
enum Load {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// A global event the list follows.
enum Heard {
    Status {
        name: String,
        status: McpStatus,
        tools: u32,
    },
    /// `mcp.changed`, a lag, or a new connection: list again.
    Changed,
}

struct Edit {
    draft: Draft,
    name: Entity<TextField>,
    command: Entity<TextField>,
    args: Entity<TextField>,
    url: Entity<TextField>,
    env: Entity<Editor>,
    headers: Entity<Editor>,
    error: Option<String>,
    saving: bool,
    /// Leaving with changes: asking whether to discard them.
    leaving: bool,
    focus_name: bool,
    /// Sign out: `None` not asked, `Some(false)` in flight, `Some(true)` done.
    signed_out: Option<bool>,
    _subs: Vec<Subscription>,
}

/// A server's browser sign-in.
struct Login {
    name: String,
    /// `mcp.login`'s id once it answered.
    id: Option<String>,
    finish: Finish,
    paste: Entity<TextField>,
    /// A pasted address is being checked.
    busy: bool,
    /// A remote otto's callback port, listening here while the sign-in waits.
    _forward: Option<CallbackForward>,
    _sub: Subscription,
}

pub struct McpSettings {
    rt: tokio::runtime::Handle,
    target: Otto,
    /// The pane's folder, and its project root once loaded.
    cwd: Option<String>,
    root: Option<String>,
    load: Load,
    servers: Vec<McpServer>,
    /// The server whose Remove waits for a confirmation.
    confirm: Option<(String, McpScope)>,
    /// A failed list action.
    notice: Option<String>,
    edit: Option<Edit>,
    login: Option<Login>,
    /// Bumped by each sign-in started or cancelled, and each list asked for.
    attempt: u64,
    listing: u64,
    _watch: Option<Task<()>>,
}

impl Drop for McpSettings {
    /// Settings closed mid sign-in: otto stops waiting for the browser.
    fn drop(&mut self) {
        if let Some(id) = self.login.as_ref().and_then(|l| l.id.clone()) {
            let target = self.target.clone();
            self.rt.spawn(async move {
                if let Ok(c) = client::connection(&target).await {
                    let _ = c.mcp_login_cancel(&id).await;
                }
            });
        }
    }
}

impl McpSettings {
    pub fn new(rt: tokio::runtime::Handle, target: Otto, cwd: Option<String>) -> Self {
        Self {
            rt,
            target,
            cwd,
            root: None,
            load: Load::Idle,
            servers: Vec::new(),
            confirm: None,
            notice: None,
            edit: None,
            login: None,
            attempt: 0,
            listing: 0,
            _watch: None,
        }
    }

    /// Load once the page shows: listing starts the enabled servers.
    fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if matches!(self.load, Load::Idle) {
            self.reload(cx);
            self.watch(cx);
        }
    }

    /// Esc: the editor (its discard question first), else a confirmation or a
    /// sign-in. Whether it was used.
    pub fn escape(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(edit) = &mut self.edit {
            if edit.leaving {
                edit.leaving = false;
                cx.notify();
            } else if self.may_leave(cx) {
                cx.notify();
            }
            return true;
        }
        if self.confirm.take().is_some() {
            cx.notify();
            return true;
        }
        if self.login.is_some() {
            self.cancel_login(cx);
            return true;
        }
        false
    }

    /// Whether the page may go away now; with unsaved changes it first asks (asked already: they go).
    pub fn may_leave(&mut self, cx: &mut Context<Self>) -> bool {
        match &mut self.edit {
            Some(edit) if edit.draft.dirty() && !edit.leaving => {
                edit.leaving = true;
                cx.notify();
                false
            }
            _ => {
                self.edit = None;
                true
            }
        }
    }

    /// Run `work` on the shared connection, then `done` here.
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
        if matches!(self.load, Load::Idle | Load::Failed(_)) {
            self.load = Load::Loading;
        }
        self.listing += 1;
        let (listing, cwd) = (self.listing, self.cwd.clone());
        self.run(
            cx,
            move |c| async move {
                let root = client::project(&c, cwd.as_deref()).await?.root;
                let servers = c.mcp_list(Some(root.clone())).await?;
                Ok((root, servers))
            },
            move |this, result, _| {
                if this.listing != listing {
                    return;
                }
                match result {
                    Ok((root, servers)) => {
                        this.root = Some(root);
                        this.servers = servers;
                        this.load = Load::Ready;
                    }
                    Err(error) => this.load = Load::Failed(error),
                }
            },
        );
    }

    /// Follow `mcp.status` and `mcp.changed` (and list again after a reconnect).
    fn watch(&mut self, cx: &mut Context<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Heard>();
        let target = self.target.clone();
        self.rt.spawn(async move {
            let mut first = true;
            loop {
                if let Ok(connection) = client::connection(&target).await {
                    let mut events = connection.events();
                    if !std::mem::take(&mut first) && tx.send(Heard::Changed).is_err() {
                        return;
                    }
                    loop {
                        let heard = match events.recv().await {
                            Ok(Ok(ServerMessage::Event { session_id: None, event, .. })) => match *event {
                                Event::McpStatus { name, status, tools } => Some(Heard::Status { name, status, tools }),
                                Event::McpChanged {} => Some(Heard::Changed),
                                _ => None,
                            },
                            Ok(Ok(_)) => None,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some(Heard::Changed),
                            Ok(Err(_)) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        };
                        if heard.is_some_and(|h| tx.send(h).is_err()) {
                            return;
                        }
                    }
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        self._watch = Some(cx.spawn(async move |this, cx| {
            while let Some(heard) = rx.recv().await {
                let applied = this.update(cx, |this, cx| {
                    match heard {
                        Heard::Status { name, status, tools } => {
                            if !apply_status(&mut this.servers, &name, status, tools) {
                                this.reload(cx);
                            }
                        }
                        Heard::Changed => this.reload(cx),
                    }
                    cx.notify();
                });
                if applied.is_err() {
                    return;
                }
            }
        }));
    }

    /// A server as otto now reports it.
    fn replace(&mut self, server: McpServer) {
        match self.servers.iter_mut().find(|s| s.name == server.name) {
            Some(row) => *row = server,
            None => self.servers.push(server),
        }
    }

    fn set_enabled(&mut self, name: String, scope: McpScope, on: bool, cx: &mut Context<Self>) {
        if let Some(row) = self.servers.iter_mut().find(|s| s.name == name) {
            row.enabled = on;
            row.status = if on { McpStatus::Connecting } else { McpStatus::Disabled };
        }
        self.notice = None;
        let root = self.root.clone();
        self.run(
            cx,
            move |c| async move { c.mcp_update(root, scope, &name, None, Some(on)).await },
            |this, result, cx| match result {
                Ok(server) => this.replace(server),
                Err(error) => {
                    this.notice = Some(brief(&error));
                    this.reload(cx);
                }
            },
        );
        cx.notify();
    }

    fn remove(&mut self, name: String, scope: McpScope, cx: &mut Context<Self>) {
        self.confirm = None;
        let root = self.root.clone();
        self.run(
            cx,
            move |c| async move { c.mcp_remove(root, scope, &name).await },
            |this, result, cx| match result {
                Ok(_) => this.reload(cx),
                Err(error) => this.notice = Some(brief(&error)),
            },
        );
    }

    // ---- sign-in ----

    /// `Sign in`: `mcp.login`, a remote otto's callback forwarded here, the
    /// page opened, then `mcp.login_complete` waits for it (or for a pasted address).
    fn sign_in(&mut self, name: String, cx: &mut Context<Self>) {
        self.cancel_login(cx);
        let attempt = self.attempt;
        let paste = cx.new(|cx| TextField::new("Paste the address you were sent to", cx));
        let sub = cx.subscribe(&paste, |this, _, event: &FieldEvent, cx| match event {
            FieldEvent::Submit => this.submit_paste(cx),
            FieldEvent::Cancel => this.cancel_login(cx),
            _ => {}
        });
        self.notice = None;
        self.login = Some(Login { name: name.clone(), id: None, finish: Finish::Wait, paste, busy: true, _forward: None, _sub: sub });
        let (root, target) = (self.root.clone(), self.target.clone());
        self.run(
            cx,
            move |c| async move {
                let started = c.mcp_login(root, &name).await?;
                let forward = match &target {
                    Otto::Remote { id } => match client::dialer(id).await {
                        Ok(dialer) => {
                            let callback = &started.callback;
                            CallbackForward::start(dialer, callback.port, callback.port, callback.path.clone(), FORWARD_TTL).ok()
                        }
                        Err(_) => None,
                    },
                    Otto::Local => None,
                };
                Ok((started, forward))
            },
            move |this, result, cx| {
                if this.attempt != attempt {
                    if let Ok((started, _)) = result {
                        this.send_cancel(started.login_id, cx);
                    }
                    return;
                }
                match result {
                    Ok((started, forward)) => {
                        let finish = finish(matches!(this.target, Otto::Remote { .. }), forward.is_some());
                        cx.open_url(&started.url);
                        if let Some(login) = &mut this.login {
                            (login.id, login.finish, login.busy, login._forward) = (Some(started.login_id.clone()), finish, false, forward);
                        }
                        if finish == Finish::Wait {
                            this.complete(started.login_id, None, cx);
                        }
                    }
                    Err(error) => {
                        this.login = None;
                        this.notice = Some(brief(&error));
                    }
                }
            },
        );
        cx.notify();
    }

    /// `mcp.login_complete`: waiting for the browser, or with the pasted address.
    fn complete(&mut self, id: String, code: Option<String>, cx: &mut Context<Self>) {
        let login_id = id.clone();
        self.run(
            cx,
            move |c| async move { c.mcp_login_complete(&login_id, code).await },
            move |this, result, _| {
                if this.login.as_ref().and_then(|l| l.id.as_deref()) != Some(id.as_str()) {
                    return;
                }
                this.login = None;
                match result {
                    Ok(server) => this.replace(server),
                    Err(error) => this.notice = Some(format!("Sign-in failed · {}", brief(&error))),
                }
            },
        );
    }

    fn submit_paste(&mut self, cx: &mut Context<Self>) {
        let Some(login) = &mut self.login else { return };
        let text = login.paste.read(cx).text.trim().to_owned();
        let Some(id) = login.id.clone().filter(|_| !text.is_empty() && !login.busy) else { return };
        login.busy = true;
        self.complete(id, Some(text), cx);
        cx.notify();
    }

    /// Abandon the sign-in; otto drops it (and its browser listener).
    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        self.attempt += 1;
        if let Some(id) = self.login.take().and_then(|l| l.id) {
            self.send_cancel(id, cx);
        }
        cx.notify();
    }

    fn send_cancel(&self, id: String, cx: &mut Context<Self>) {
        self.run(cx, move |c| async move { c.mcp_login_cancel(&id).await }, |_, _, _| {});
    }

    /// The editor's Sign out: the server's tokens go; it asks to sign in again.
    fn sign_out(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.edit else { return };
        edit.signed_out = Some(false);
        let root = self.root.clone();
        self.run(
            cx,
            move |c| async move { c.mcp_logout(root, &name).await },
            |this, result, _| {
                let Some(edit) = &mut this.edit else { return };
                match result {
                    Ok(_) => edit.signed_out = Some(true),
                    Err(error) => (edit.signed_out, edit.error) = (None, Some(brief(&error))),
                }
            },
        );
        cx.notify();
    }

    // ---- the editor ----

    fn start_edit(&mut self, draft: Draft, cx: &mut Context<Self>) {
        let field = |placeholder: &'static str, cx: &mut Context<Self>| cx.new(|cx| TextField::new(placeholder, cx));
        let (name, command, args, url) = (field("name", cx), field("npx", cx), field("-y package", cx), field("https://", cx));
        let lines = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut e = Editor::new(placeholder, cx);
                e.multiline = true;
                e
            })
        };
        let (env, headers) = (lines("KEY=value", cx), lines("Authorization: Bearer ${TOKEN}", cx));
        let on_field = |set: fn(&mut Form, String)| {
            move |this: &mut Self, _: Entity<TextField>, event: &FieldEvent, cx: &mut Context<Self>| match event {
                FieldEvent::Changed(text) => this.update_form(cx, |f| set(f, text.clone())),
                FieldEvent::Submit => this.save(cx),
                FieldEvent::Cancel => _ = this.escape(cx),
                FieldEvent::Up | FieldEvent::Down => {}
            }
        };
        let on_lines = |set: fn(&mut Form, String)| {
            move |this: &mut Self, editor: Entity<Editor>, event: &EditorEvent, cx: &mut Context<Self>| match event {
                EditorEvent::Changed => {
                    let text = editor.read(cx).text().to_owned();
                    this.update_form(cx, |f| set(f, text));
                }
                EditorEvent::Submit => this.save(cx),
                EditorEvent::Escape => _ = this.escape(cx),
                _ => {}
            }
        };
        let subs = vec![
            cx.subscribe(&name, on_field(|f, t| f.name = t)),
            cx.subscribe(&command, on_field(|f, t| f.command = t)),
            cx.subscribe(&args, on_field(|f, t| f.args = t)),
            cx.subscribe(&url, on_field(|f, t| f.url = t)),
            cx.subscribe(&env, on_lines(|f, t| f.env = t)),
            cx.subscribe(&headers, on_lines(|f, t| f.headers = t)),
        ];
        let form = draft.form.clone();
        name.update(cx, |f, cx| f.replace_text(form.name.clone(), cx));
        command.update(cx, |f, cx| f.replace_text(form.command.clone(), cx));
        args.update(cx, |f, cx| f.replace_text(form.args.clone(), cx));
        url.update(cx, |f, cx| f.replace_text(form.url.clone(), cx));
        let focus_name = draft.saved.is_none();
        self.edit = Some(Edit {
            draft,
            name,
            command,
            args,
            url,
            env,
            headers,
            error: None,
            saving: false,
            leaving: false,
            focus_name,
            signed_out: None,
            _subs: subs,
        });
        // `set_text` reports a change, which writes the same text back.
        if let Some(edit) = &self.edit {
            let (env, headers) = (edit.env.clone(), edit.headers.clone());
            env.update(cx, |e, cx| e.set_text(form.env.clone(), cx));
            headers.update(cx, |e, cx| e.set_text(form.headers.clone(), cx));
        }
        (self.confirm, self.notice) = (None, None);
        cx.notify();
    }

    fn update_form(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Form)) {
        if let Some(edit) = &mut self.edit {
            f(&mut edit.draft.form);
            edit.error = None;
        }
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.edit else { return };
        if edit.saving || !edit.draft.dirty() {
            return;
        }
        let transport = match edit.draft.problem(&self.servers).map_or_else(|| edit.draft.form.transport(), Err) {
            Ok(transport) => transport,
            Err(problem) => {
                edit.error = Some(problem.to_owned());
                cx.notify();
                return;
            }
        };
        (edit.saving, edit.error) = (true, None);
        let (form, moved, new) = (edit.draft.form.clone(), edit.draft.moved_from(), edit.draft.saved.is_none());
        let enabled = edit.draft.saved.as_ref().is_none_or(|(.., on)| *on);
        let root = self.root.clone();
        self.run(
            cx,
            move |c| async move {
                let name = form.name.trim();
                if new || moved.is_some() {
                    c.mcp_add(root.clone(), form.scope, name, transport, enabled).await?;
                } else {
                    c.mcp_update(root.clone(), form.scope, name, Some(transport), None).await?;
                }
                if let Some((old, scope)) = moved {
                    c.mcp_remove(root, scope, &old).await?;
                }
                Ok(())
            },
            |this, result, cx| match result {
                Ok(()) => {
                    this.edit = None;
                    this.reload(cx);
                }
                Err(error) => {
                    if let Some(edit) = &mut this.edit {
                        (edit.saving, edit.error) = (false, Some(brief(&error)));
                    }
                    this.reload(cx);
                }
            },
        );
        cx.notify();
    }

    // ---- the list ----

    fn project_name(&self) -> Option<String> {
        self.root.as_deref().map(super::project_name)
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let about = match self.project_name() {
            Some(project) => format!("Global, and {project}'s"),
            None => "Global and project".to_owned(),
        };
        let header = div()
            .w_full()
            .min_h(px(44.0))
            .px(px(14.0))
            .py(px(10.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child("MCP servers"))
                    .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(about)),
            )
            .child(Ui::button("mcp-add", "Add", false, true).on_click(cx.listener(|this, _, _, cx| this.start_edit(Draft::new_server(), cx))));
        out.push(Ui::card(vec![header.into_any_element()]));
        out.extend(self.notice.clone().map(Ui::error_line));
        match self.load.clone() {
            Load::Idle | Load::Loading if self.servers.is_empty() => out.push(Ui::note("Loading…")),
            Load::Failed(error) => out.push(Ui::error_line(brief(&error))),
            Load::Ready if self.servers.is_empty() => out.push(Ui::note("No servers yet. Add one to give agents its tools.")),
            _ => {}
        }
        let rows: Vec<AnyElement> = self.servers.clone().into_iter().enumerate().map(|(i, server)| self.server_row(i, server, cx)).collect();
        if !rows.is_empty() {
            out.push(Ui::card(rows));
        }
        out
    }

    fn server_row(&self, index: usize, server: McpServer, cx: &mut Context<Self>) -> AnyElement {
        let (name, scope) = (server.name.clone(), server.scope);
        let confirming = self.confirm.as_ref().is_some_and(|(n, s)| *n == name && *s == scope);
        let login = self.login.as_ref().filter(|l| l.name == name);
        let mut line: Vec<String> = vec![summary(&server.transport), status_label(&server.status)];
        line.extend(tools_label(&server));
        let detail: SharedString = match login {
            _ if confirming => format!("Remove {name}?").into(),
            Some(l) if l.id.is_none() => "Starting sign-in…".into(),
            Some(l) if l.finish == Finish::Paste => "Sign in in the browser, then paste the address it ends on".into(),
            Some(_) => "Finish signing in in the browser".into(),
            None => line.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ").into(),
        };
        let tip = match &server.status {
            McpStatus::Error { message } => Some(short_error(message)),
            _ => None,
        };
        let actions = if confirming {
            let n = name.clone();
            div()
                .flex()
                .gap(px(6.0))
                .child(Ui::button(("mcp-cancel", index), "Cancel", false, true).on_click(cx.listener(|this, _, _, cx| {
                    this.confirm = None;
                    cx.notify();
                })))
                .child(
                    Ui::danger_button(("mcp-confirm", index), "Remove")
                        .on_click(cx.listener(move |this, _, _, cx| this.remove(n.clone(), scope, cx))),
                )
                .into_any_element()
        } else if login.is_some() {
            Ui::button(("mcp-login-cancel", index), "Cancel", false, true)
                .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx)))
                .into_any_element()
        } else {
            let (sign, toggle_name, edit, remove) = (name.clone(), name.clone(), server.clone(), name.clone());
            let entity = cx.entity().downgrade();
            let toggle =
                Toggle::new(gpui::ElementId::Name(format!("mcp-enabled-{}-{name}", scope_label(scope)).into()), format!("{name} on"), server.enabled)
                    .on_toggle(move |on, _, cx| {
                        let _ = entity.update(cx, |this, cx| this.set_enabled(toggle_name.clone(), scope, on, cx));
                    });
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .when(server.enabled && server.status == McpStatus::NeedsSignIn, |el| {
                    el.child(
                        Ui::button(("mcp-sign-in", index), "Sign in", true, true)
                            .on_click(cx.listener(move |this, _, _, cx| this.sign_in(sign.clone(), cx))),
                    )
                })
                .child(toggle)
                .child(
                    Ui::icon_button(("mcp-edit", index), "pencil", "Edit")
                        .on_click(cx.listener(move |this, _, _, cx| this.start_edit(Draft::open(&edit), cx))),
                )
                .child(Ui::icon_button(("mcp-remove", index), "close", "Remove").on_click(cx.listener(move |this, _, _, cx| {
                    this.confirm = Some((remove.clone(), scope));
                    cx.notify();
                })))
                .into_any_element()
        };
        let open = server.clone();
        let text = div()
            .id(("mcp-row", index))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.start_edit(Draft::open(&open), cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().flex_none().size(px(7.0)).rounded_full().bg(rgb(status_color(&server.status))))
                    .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child(name.clone()))
                    .child(Ui::badge(scope_label(scope))),
            )
            .child(
                div()
                    .id(("mcp-detail", index))
                    .pl(px(13.0))
                    .text_size(px(11.5))
                    .text_color(rgb(if confirming { theme::foreground() } else { theme::muted() }))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .when_some(tip.filter(|_| !confirming && login.is_none()), |el, tip| el.tooltip(Tooltip::text(tip)))
                    .child(detail),
            );
        let paste = login.filter(|l| l.finish == Finish::Paste && l.id.is_some()).map(|l| {
            let busy = l.busy;
            div().w_full().flex().items_center().gap(px(8.0)).child(div().flex_1().min_w_0().child(Ui::field_box(&l.paste, 600.0))).child(
                Ui::button(("mcp-paste", index), if busy { "Checking…" } else { "Done" }, true, !busy)
                    .on_click(cx.listener(|this, _, _, cx| this.submit_paste(cx))),
            )
        });
        div()
            .w_full()
            .min_h(px(48.0))
            .px(px(14.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().w_full().flex().items_center().gap(px(10.0)).child(text).child(actions))
            .children(paste)
            .into_any_element()
    }

    fn lines_box(editor: &Entity<Editor>, cx: &mut Context<Self>) -> AnyElement {
        let colors = EditorColors {
            text: rgb(theme::foreground()).into(),
            placeholder: rgb(theme::muted()).into(),
            caret: rgb(theme::foreground()).into(),
            selection: hsla(theme::selection(), 0.6),
        };
        div()
            .w_full()
            .min_h(px(64.0))
            .px(px(12.0))
            .py(px(8.0))
            .continuous(10.0)
            .bg(theme::well_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .font_family(mono())
            .text_size(px(12.5))
            .line_height(px(18.0))
            .child(Editor::element(editor, 12, colors, cx))
            .into_any_element()
    }

    fn render_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let project = self.project_name();
        let servers = self.servers.clone();
        let Some(edit) = &mut self.edit else { return Vec::new() };
        if std::mem::take(&mut edit.focus_name) {
            let field = edit.name.clone();
            window.focus(&field.read(cx).focus.clone(), cx);
        }
        let draft = edit.draft.clone();
        let form = draft.form.clone();
        let can_save = draft.dirty() && !edit.saving;
        let problem = draft.problem(&servers);
        let (saving, leaving, signed_out, error) = (edit.saving, edit.leaving, edit.signed_out, edit.error.clone());
        let (name, command, args, url, env, headers) =
            (edit.name.clone(), edit.command.clone(), edit.args.clone(), edit.url.clone(), edit.env.clone(), edit.headers.clone());
        let mut out = Vec::new();

        let title: SharedString = if form.name.trim().is_empty() { "New server".into() } else { form.name.trim().to_owned().into() };
        let toolbar = div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(Ui::icon_button("mcp-back", "back", "MCP servers").on_click(cx.listener(|this, _, _, cx| {
                if this.may_leave(cx) {
                    cx.notify();
                }
            })))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .when(draft.saved.is_none(), |el| el.child(Ui::badge("New")))
            .child(
                Ui::button("mcp-save", if saving { "Saving…" } else { "Save" }, true, can_save)
                    .when(can_save, |b| b.on_click(cx.listener(|this, _, _, cx| this.save(cx))))
                    .when_some(problem.filter(|_| can_save), |b, p| b.tooltip(Tooltip::text(p))),
            );
        out.push(toolbar.into_any_element());
        if leaving {
            out.push(
                div()
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .continuous(8.0)
                    .bg(theme::hover_fill())
                    .child(div().flex_1().text_size(px(12.5)).child("Discard your changes?"))
                    .child(Ui::button("mcp-keep", "Keep editing", false, true).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(edit) = &mut this.edit {
                            edit.leaving = false;
                        }
                        cx.notify();
                    })))
                    .child(Ui::danger_button("mcp-discard", "Discard").on_click(cx.listener(|this, _, _, cx| {
                        this.edit = None;
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        }
        out.extend(error.map(Ui::error_line));

        let entity = cx.entity().downgrade();
        let scope = Segmented::new(
            "mcp-scope",
            ["Global".to_owned(), project.as_ref().map_or_else(|| "Project".to_owned(), |p| format!("Project · {p}"))],
            usize::from(form.scope == McpScope::Project),
        )
        .segment_width(if project.is_some() { 120.0 } else { 80.0 })
        .when(project.is_none(), |s| s.unavailable(1, "Open a project first"))
        .on_select(move |index, _, cx| {
            let scope = if index == 1 { McpScope::Project } else { McpScope::Global };
            let _ = entity.update(cx, |this, cx| this.update_form(cx, |f| f.scope = scope));
        });
        let entity = cx.entity().downgrade();
        let kind = Segmented::new("mcp-kind", ["Command", "HTTP"], usize::from(form.kind == Kind::Http)).on_select(move |index, _, cx| {
            let kind = if index == 1 { Kind::Http } else { Kind::Stdio };
            let _ = entity.update(cx, |this, cx| this.update_form(cx, |f| f.kind = kind));
        });
        let mut rows = vec![
            Ui::row("Name", Ui::field_box(&name, 220.0)),
            Ui::row("Saved in", scope.into_any_element()),
            Ui::row("Type", kind.into_any_element()),
        ];
        match form.kind {
            Kind::Stdio => rows.extend([Ui::row("Command", Ui::field_box(&command, 300.0)), Ui::row("Arguments", Ui::field_box(&args, 300.0))]),
            Kind::Http => rows.push(Ui::row("URL", Ui::field_box(&url, 300.0))),
        }
        out.push(Ui::card(rows));
        let (label, field, note) = match form.kind {
            Kind::Stdio => ("Environment", env, "One KEY=value per line. ${VAR} reads your environment."),
            Kind::Http => ("Headers", headers, "One Name: value per line. ${VAR} reads your environment."),
        };
        let body = div().w_full().flex().flex_col().gap(px(6.0)).child(Self::lines_box(&field, cx)).child(Ui::note(note));
        out.push(Ui::titled(label, None, body.into_any_element()));

        let saved_http = draft.saved.as_ref().filter(|(.., f, _)| f.kind == Kind::Http).map(|(n, ..)| n.clone());
        if let Some(server) = saved_http {
            let label = match signed_out {
                None => "Sign out",
                Some(false) => "Signing out…",
                Some(true) => "Signed out",
            };
            let button = Ui::button("mcp-sign-out", label, false, signed_out.is_none())
                .when(signed_out.is_none(), |b| b.on_click(cx.listener(move |this, _, _, cx| this.sign_out(server.clone(), cx))));
            let row = Ui::row("Forget its sign-in", button.into_any_element());
            out.push(Ui::titled("Sign-in", None, Ui::card(vec![row])));
        }
        out
    }
}

impl Render for McpSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_loaded(cx);
        let children = if self.edit.is_some() { self.render_editor(window, cx) } else { self.render_list(cx) };
        div().w_full().flex().flex_col().gap(px(14.0)).children(children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn stdio(command: &str, args: &[&str]) -> McpTransport {
        McpTransport::Stdio { command: command.into(), args: strings(args), env: BTreeMap::new() }
    }

    fn server(name: &str, scope: McpScope, transport: McpTransport) -> McpServer {
        McpServer { name: name.into(), scope, transport, enabled: true, status: McpStatus::Connected, tools: 3 }
    }

    #[test]
    fn mcp_args_split_like_a_shell_and_join_back() {
        assert_eq!(split_args("  -y  @scope/pkg --flag "), Some(strings(&["-y", "@scope/pkg", "--flag"])));
        assert_eq!(split_args(r#"--name "two words" 'it''s' a\ b "q\"t" ''"#), Some(strings(&["--name", "two words", "its", "a b", "q\"t", ""])));
        assert_eq!(split_args(r"'a\b'"), Some(strings(&[r"a\b"])), "no escapes in single quotes");
        assert_eq!(split_args(""), Some(Vec::new()));
        assert_eq!(split_args("\"open"), None);
        assert_eq!(split_args("it's"), None);
        for args in [strings(&["-y", "@scope/pkg"]), strings(&["two words", "", "it's", r"back\slash", "q\"t"])] {
            assert_eq!(split_args(&join_args(&args)), Some(args.clone()), "{args:?}");
        }
        assert_eq!(join_args(&strings(&["-y", "a b"])), "-y 'a b'");
    }

    #[test]
    fn mcp_env_and_header_lines() {
        let env = |t: &str| pairs(t, '=', |k| k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        let parsed = env("TOKEN=${GITHUB_TOKEN}\n\n  MODE = a=b  \n").unwrap();
        assert_eq!(parsed.into_iter().collect::<Vec<_>>(), [("MODE".into(), "a=b".into()), ("TOKEN".into(), "${GITHUB_TOKEN}".into())]);
        assert_eq!(env("NOPE"), None);
        assert_eq!(env("=x"), None);
        assert_eq!(env("BAD KEY=x"), None);
        let form = Form { kind: Kind::Http, url: "https://x.dev/mcp".into(), headers: "Authorization: Bearer a:b=".into(), ..Form::blank() };
        let McpTransport::Http { headers, .. } = form.transport().unwrap() else { panic!() };
        assert_eq!(headers.get("Authorization").map(String::as_str), Some("Bearer a:b="));
    }

    #[test]
    fn mcp_forms_validate_and_round_trip() {
        let mut form = Form { name: "github".into(), command: " npx ".into(), args: "-y 'a b'".into(), env: "K=v".into(), ..Form::blank() };
        let transport = form.transport().unwrap();
        let env = BTreeMap::from([("K".to_owned(), "v".to_owned())]);
        assert_eq!(transport, McpTransport::Stdio { command: "npx".into(), args: strings(&["-y", "a b"]), env });
        let listed = server("github", McpScope::Global, transport);
        assert_eq!(Form::of(&listed).transport(), Ok(listed.transport.clone()));
        assert_eq!(form.problem(|_, _| false), None);
        assert_eq!(form.problem(|n, s| n == "github" && s == McpScope::Global), Some("That name is taken"));
        form.command.clear();
        assert_eq!(form.problem(|_, _| false), Some("Enter a command"));
        form.kind = Kind::Http;
        assert_eq!(form.problem(|_, _| false), Some("The URL starts with http:// or https://"));
        form.url = "ftp://x".into();
        assert!(form.transport().is_err());
        form.url = "https://api.example.com/mcp".into();
        form.headers = "no colon".into();
        assert_eq!(form.problem(|_, _| false), Some("Header lines are Name: value"));
        form.headers.clear();
        assert_eq!(form.problem(|_, _| false), None, "an HTTP form ignores the command fields");
        for (name, problem) in [("", Some("Name the server")), ("a b", Some("Names use letters, digits, -, _ and .")), ("a.b-c_d", None)] {
            assert_eq!(Form { name: name.into(), ..form.clone() }.problem(|_, _| false), problem, "{name:?}");
        }
        assert_eq!(
            Form { args: "'open".into(), ..Form { name: "x".into(), command: "c".into(), ..Form::blank() } }.problem(|_, _| false),
            Some("Close the quote in the arguments")
        );
    }

    #[test]
    fn mcp_drafts_track_changes_and_moves() {
        let listed = vec![server("github", McpScope::Global, stdio("npx", &["-y", "gh"])), server("linear", McpScope::Project, stdio("uvx", &[]))];
        assert!(!Draft::new_server().dirty());
        assert_eq!(Draft::new_server().problem(&listed), Some("Name the server"));
        let mut draft = Draft::open(&listed[0]);
        assert!(!draft.dirty() && draft.moved_from().is_none());
        assert_eq!(draft.problem(&listed), None, "its own name is fine");
        draft.form.args = "-y gh2".into();
        assert!(draft.dirty() && draft.moved_from().is_none());
        draft.form.scope = McpScope::Project;
        assert_eq!(draft.moved_from(), Some(("github".into(), McpScope::Global)));
        draft.form.name = "linear".into();
        assert_eq!(draft.problem(&listed), Some("That name is taken"));
        draft.form.scope = McpScope::Global;
        assert_eq!(draft.problem(&listed), None, "the same name in the other scope");
    }

    #[test]
    fn mcp_status_labels_are_short() {
        assert_eq!(status_label(&McpStatus::Connected), "Connected");
        assert_eq!(status_label(&McpStatus::Connecting), "Connecting…");
        assert_eq!(status_label(&McpStatus::NeedsSignIn), "Needs sign-in");
        assert_eq!(status_label(&McpStatus::Disabled), "Off");
        assert_eq!(status_label(&McpStatus::Error { message: "spawn /usr/bin/nope: not found\nmore".into() }), "Error · spawn not found");
        let long = brief(&"word ".repeat(30));
        assert!(long.ends_with('…') && long.chars().count() <= 61, "{long}");
        let mut s = server("x", McpScope::Global, stdio("x", &[]));
        assert_eq!(tools_label(&s).as_deref(), Some("3 tools"));
        s.tools = 1;
        assert_eq!(tools_label(&s).as_deref(), Some("1 tool"));
        s.status = McpStatus::Connecting;
        assert_eq!(tools_label(&s), None);
        assert_eq!((scope_label(McpScope::Global), scope_label(McpScope::Project)), ("Global", "Project"));
    }

    #[test]
    fn mcp_summaries_never_show_paths() {
        assert_eq!(summary(&stdio("/usr/local/bin/node", &["/Users/me/srv/index.js", "--port", "3"])), "node --port 3");
        assert_eq!(summary(&stdio("npx", &["-y", "@modelcontextprotocol/server-github"])), "npx -y @modelcontextprotocol/server-github");
        let http = |url: &str| summary(&McpTransport::Http { url: url.into(), headers: BTreeMap::new() });
        assert_eq!(http("https://user:pw@api.example.com:8443/mcp?x=1"), "api.example.com:8443");
        assert_eq!(http("https://mcp.linear.app/sse"), "mcp.linear.app");
    }

    #[test]
    fn mcp_status_events_update_their_row() {
        let mut list = vec![server("a", McpScope::Global, stdio("a", &[])), server("b", McpScope::Project, stdio("b", &[]))];
        assert!(apply_status(&mut list, "b", McpStatus::Disabled, 0));
        assert_eq!((list[1].enabled, list[1].tools, list[0].tools), (false, 0, 3));
        assert!(apply_status(&mut list, "b", McpStatus::NeedsSignIn, 0));
        assert!(list[1].enabled);
        assert!(!apply_status(&mut list, "c", McpStatus::Connected, 1), "an unknown server lists again");
    }

    #[test]
    fn mcp_remote_sign_in_pastes_only_without_a_forward() {
        assert_eq!(finish(false, false), Finish::Wait);
        assert_eq!(finish(true, true), Finish::Wait);
        assert_eq!(finish(true, false), Finish::Paste);
    }
}
