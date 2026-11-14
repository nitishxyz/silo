//! The VM's thread: its state, the invocation context, actions, the event
//! pump, handlers and timers.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use mlua::{Function, HookTriggers, IntoLuaMulti, Lua, LuaOptions, MultiValue, StdLib, Table, Value as LuaValue, VmState, chunk::ChunkMode};
use serde_json::{Value, json};
use silo_proto::{
    config::{ActionInfo, ActionOrigin, ConfigProblem, HandlerInfo, KeyBinding, KeyMode, KeyUnbind, ProblemLevel},
    control::RpcError,
    model::{SessionSummary, SessionView},
};
use tokio::{
    sync::{Notify, mpsc, oneshot, watch},
    time::Instant,
};

use crate::{
    ACTION_FAILED, Api, Ctx, Events, Exit, Invocation, Loaded, LogLevel, Mode, Options, STOPPED, Source, args,
    convert::{from_lua, to_lua},
    events::{EventCtx, Heard, Tracker},
    exec::GroupGuard,
    limits,
};

/// Instructions between two looks at the clock.
const HOOK_EVERY: u32 = 10_000;

enum Cmd {
    Run { name: String, call: Box<Invocation>, reply: oneshot::Sender<Result<Value, RpcError>> },
    Attach { api: Api, events: Option<Events> },
    Stop,
}

/// A handle to a VM running on its own thread. Cheap to clone; the VM
/// stops when the last clone is dropped or on [`Vm::stop`].
#[derive(Clone)]
pub struct Vm {
    inner: Arc<Handle>,
}

struct Handle {
    tx: mpsc::UnboundedSender<Cmd>,
    exit: watch::Receiver<Option<Exit>>,
    running: watch::Receiver<usize>,
    files: Arc<Mutex<Vec<PathBuf>>>,
}

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vm").field("finished", &self.is_finished()).field("running_actions", &self.running_actions()).finish()
    }
}

fn stopped() -> RpcError {
    RpcError::new(STOPPED, "the Lua VM has stopped")
}

impl Vm {
    /// Start a VM on a thread of its own and load `source` into it.
    ///
    /// [`Mode::Config`]: runs the file top to bottom (calls into Silo fail
    /// with a warning) and returns what it declared. [`Mode::Script`]:
    /// only compiles it; [`Vm::attach`] runs it.
    pub async fn load(source: Source, options: Options) -> (Vm, Loaded) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (exit_tx, exit) = watch::channel(None);
        let (running_tx, running) = watch::channel(0usize);
        let files = Arc::new(Mutex::new(Vec::new()));
        let (loaded_tx, loaded_rx) = oneshot::channel();
        let shared = files.clone();
        let name = format!("lua {}", options.source_name);
        let spawned = std::thread::Builder::new().name(name).stack_size(8 << 20).spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                return;
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, main(source, options, rx, exit_tx, running_tx, shared, loaded_tx));
            // Dropping `local` drops the tasks still running, which kills
            // their commands.
        });
        let vm = Vm { inner: Arc::new(Handle { tx, exit, running, files }) };
        let loaded = match spawned {
            Ok(_) => loaded_rx.await.ok(),
            Err(_) => None,
        };
        let loaded = loaded.unwrap_or_else(|| Loaded {
            failed: true,
            problems: vec![problem(ProblemLevel::Error, "", "the Lua VM stopped while loading")],
            ..Loaded::default()
        });
        (vm, loaded)
    }

    /// Run action `name` with `call`'s args, ctx and API.
    ///
    /// # Errors
    /// `not_found` (no such action), `bad_request` (args), `timeout`,
    /// [`ACTION_FAILED`] (it raised), [`STOPPED`].
    pub async fn run_action(&self, name: &str, call: Invocation) -> Result<Value, RpcError> {
        let (reply, answer) = oneshot::channel();
        self.inner.tx.send(Cmd::Run { name: name.to_owned(), call: Box::new(call), reply }).map_err(|_| stopped())?;
        answer.await.unwrap_or_else(|_| Err(stopped()))
    }

    /// Give handlers and timers `api` and start hearing `events` (the
    /// caller subscribes the connection). A config's timers start now. A
    /// script starts running, with `api`. Call once.
    pub fn attach(&self, api: Api, events: Option<Events>) {
        let _ = self.inner.tx.send(Cmd::Attach { api, events });
    }

    /// How the VM ended: a script's exit; a config VM's once stopped.
    pub async fn finished(&self) -> Exit {
        let mut exit = self.inner.exit.clone();
        loop {
            let now = exit.borrow().clone();
            if let Some(done) = now {
                return done;
            }
            if exit.changed().await.is_err() {
                let last = exit.borrow().clone();
                return last.unwrap_or(Exit { code: 1, value: Value::Null, error: Some("the Lua VM stopped".into()) });
            }
        }
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.inner.exit.borrow().is_some() || self.inner.tx.is_closed()
    }

    /// Stop the VM: running actions fail, handlers and timers end, commands
    /// it started (not detached) are killed. A script not yet ended exits 0.
    pub fn stop(&self) {
        let _ = self.inner.tx.send(Cmd::Stop);
    }

    /// The loaded file and every file `require` has read so far.
    #[must_use]
    pub fn files(&self) -> Vec<PathBuf> {
        self.inner.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Actions running now.
    #[must_use]
    pub fn running_actions(&self) -> usize {
        *self.inner.running.borrow()
    }

    /// Resolves once no action is running (at once if none is).
    pub async fn idle(&self) {
        let mut running = self.inner.running.clone();
        let _ = running.wait_for(|n| *n == 0).await;
    }
}

pub(crate) fn problem(level: ProblemLevel, source: &str, message: impl Into<String>) -> ConfigProblem {
    ConfigProblem { level, source: source.to_owned(), message: message.into() }
}

/// An action as defined.
pub(crate) struct Action {
    pub info: ActionInfo,
    pub run: Function,
    pub schema: Value,
    pub timeout: Duration,
}

pub(crate) struct Handler {
    pub event: String,
    pub func: Function,
}

pub(crate) struct TimerShared {
    pub active: Cell<bool>,
    pub cancel: Notify,
}

impl TimerShared {
    pub fn cancel(&self) {
        self.active.set(false);
        self.cancel.notify_one();
    }
}

pub(crate) enum Filter {
    Any,
    Fields(serde_json::Map<String, Value>),
    Func(Function),
}

pub(crate) struct Waiter {
    pub event: String,
    pub filter: Filter,
    pub tx: oneshot::Sender<Heard>,
}

/// What a config declares, in order.
#[derive(Default)]
pub(crate) struct Decl {
    pub bindings: Vec<KeyBinding>,
    pub unbinds: Vec<KeyUnbind>,
    pub modes: Vec<KeyMode>,
    pub action_order: Vec<String>,
    pub handlers: Vec<HandlerInfo>,
    pub problems: Vec<ConfigProblem>,
    pub bind_fns: u32,
}

/// The VM's state, shared by its tasks (and found from Lua through the
/// state's app data).
pub(crate) struct State {
    pub options: Options,
    /// The config's top level is running: declarations count, calls fail.
    pub loading: Cell<bool>,
    pub decl: RefCell<Decl>,
    pub actions: RefCell<HashMap<String, Action>>,
    pub handlers: RefCell<Vec<Handler>>,
    pub timers: RefCell<HashMap<u64, Rc<TimerShared>>>,
    pub next_id: Cell<u64>,
    pub waiters: RefCell<Vec<Waiter>>,
    /// What handlers and timers call through, once attached.
    pub attached: RefCell<Option<Api>>,
    pub attached_tx: watch::Sender<bool>,
    pub files: Arc<Mutex<Vec<PathBuf>>>,
    pub exit: watch::Sender<Option<Exit>>,
    /// A script's top level has returned this.
    pub top: RefCell<Option<Value>>,
    pub stopping: Arc<AtomicBool>,
    pub queue: RefCell<VecDeque<Heard>>,
    pub queued: Notify,
    pub dropped_logged: Cell<bool>,
    /// A script's compiled top level, until attached.
    pub main: RefCell<Option<Function>>,
}

impl State {
    pub fn log(&self, level: LogLevel, message: &str) {
        let name = &self.options.source_name;
        match level {
            LogLevel::Debug => tracing::debug!(target: "silo_lua", "{name}: {message}"),
            LogLevel::Info => tracing::info!(target: "silo_lua", "{name}: {message}"),
            LogLevel::Warn => tracing::warn!(target: "silo_lua", "{name}: {message}"),
            LogLevel::Error => tracing::error!(target: "silo_lua", "{name}: {message}"),
        }
        if let Some(sink) = &self.options.log {
            sink(level, message);
        }
    }

    /// A problem with a `silo.*` call: kept while a config loads, else
    /// logged.
    pub fn report(&self, level: ProblemLevel, source: &str, message: String) {
        if self.loading.get() {
            self.decl.borrow_mut().problems.push(problem(level, source, message));
        } else {
            let level = if level == ProblemLevel::Error { LogLevel::Error } else { LogLevel::Warn };
            let shown = if source.is_empty() { message } else { format!("{source}: {message}") };
            self.log(level, &shown);
        }
    }

    pub fn next_id(&self) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        id
    }

    pub fn track_file(&self, path: PathBuf) {
        let mut files = self.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !files.contains(&path) {
            files.push(path);
        }
    }

    /// End the VM with `exit`, unless it has ended.
    pub fn finish(&self, exit: Exit) {
        self.stopping.store(true, Ordering::Relaxed);
        self.exit.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(exit);
            true
        });
    }

    pub fn finished(&self) -> bool {
        self.exit.borrow().is_some()
    }

    /// A script whose top level returned and that has no handlers or
    /// timers left ends.
    pub fn check_done(&self) {
        if self.options.mode != Mode::Script || !self.timers.borrow().is_empty() || !self.handlers.borrow().is_empty() {
            return;
        }
        let top = self.top.borrow().clone();
        if let Some(value) = top {
            self.finish(Exit { code: 0, value, error: None });
        }
    }

    /// The API handlers and timers call through.
    pub fn callback_api(&self) -> Api {
        self.attached.borrow().clone().unwrap_or_default()
    }

    /// Handlers and timers hear this event.
    fn in_scope(&self, ctx: &EventCtx) -> bool {
        if self.options.all_sessions {
            return true;
        }
        match (&self.options.session_id, &ctx.session_id) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => true,
        }
    }

    fn loaded(&self, failed: bool) -> Loaded {
        let decl = self.decl.borrow();
        let actions = self.actions.borrow();
        Loaded {
            bindings: decl.bindings.clone(),
            unbinds: decl.unbinds.clone(),
            modes: decl.modes.clone(),
            actions: decl.action_order.iter().filter_map(|n| actions.get(n)).map(|a| a.info.clone()).collect(),
            handlers: decl.handlers.clone(),
            timers: u32::try_from(self.timers.borrow().len()).unwrap_or(u32::MAX),
            problems: decl.problems.clone(),
            failed,
            files: self.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
        }
    }
}

/// The VM's state from Lua.
pub(crate) fn state(lua: &Lua) -> mlua::Result<Rc<State>> {
    lua.app_data_ref::<Rc<State>>().map(|s| Rc::clone(&s)).ok_or_else(|| mlua::Error::runtime("silo: the VM is shutting down"))
}

/// The action, handler call or timer tick running now.
pub(crate) struct Current {
    pub api: Api,
    pub ctx: Ctx,
    pub deadline: Option<Instant>,
    pub depth: u32,
    /// Process groups of its commands, killed when it ends.
    pub guards: RefCell<Vec<GroupGuard>>,
    /// The hook has seen it past its deadline.
    pub overdue: Cell<bool>,
}

impl Current {
    pub fn new(api: Api, ctx: Ctx, limit: Option<Duration>) -> Self {
        Self { api, ctx, deadline: limit.map(|l| Instant::now() + l), depth: 0, guards: RefCell::default(), overdue: Cell::new(false) }
    }
}

tokio::task_local! {
    pub(crate) static CURRENT: Rc<Current>;
}

pub(crate) fn current() -> Option<Rc<Current>> {
    CURRENT.try_with(Rc::clone).ok()
}

pub(crate) enum Failure {
    Timeout,
    Lua(String),
}

/// The message of a Lua error, without mlua's wrapping and traceback.
pub(crate) fn message(error: &mlua::Error) -> String {
    let text = match error {
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => return message(cause),
        mlua::Error::RuntimeError(m) | mlua::Error::MemoryError(m) => m.clone(),
        mlua::Error::SyntaxError { message, .. } => message.clone(),
        other => other.to_string(),
    };
    // A coroutine's error carries the traceback Lua took when it was raised.
    match text.split_once("\nstack traceback:") {
        Some((head, _)) => head.to_owned(),
        None => text,
    }
}

/// Run `f(args)` as a coroutine with `current` as its context, until its
/// deadline.
pub(crate) async fn invoke(current: Current, f: Function, args: MultiValue) -> Result<MultiValue, Failure> {
    let deadline = current.deadline;
    let current = Rc::new(current);
    let mut call = Box::pin(f.call_async::<MultiValue>(args));
    // The hook may yield once per poll: a second firing within one poll
    // means the code could not yield. A loop that catches the error it then
    // gets (`pcall`) still yields on the next poll, so the timeout fires.
    let seen = current.clone();
    let call = std::future::poll_fn(move |cx| {
        seen.overdue.set(false);
        call.as_mut().poll(cx)
    });
    CURRENT
        .scope(current, async move {
            let result = match deadline {
                Some(at) => tokio::time::timeout_at(at, call).await.map_err(|_| Failure::Timeout)?,
                None => call.await,
            };
            result.map_err(|e| if deadline.is_some_and(|at| Instant::now() >= at) { Failure::Timeout } else { Failure::Lua(message(&e)) })
        })
        .await
}

/// `ctx` as Lua sees it.
pub(crate) fn ctx_table(lua: &Lua, ctx: &Ctx, host: &str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("session_id", ctx.session_id.clone())?;
    t.set("window_id", ctx.window_id.clone())?;
    t.set("block_id", ctx.block_id.clone())?;
    t.set("client_id", ctx.client_id)?;
    let origin = ctx.origin.and_then(|o| serde_json::to_value(o).ok()).and_then(|v| v.as_str().map(str::to_owned));
    t.set("origin", origin)?;
    t.set("host", host)?;
    Ok(t)
}

/// `"init.lua:12: message"` as its source and message.
fn split_source(text: &str, fallback: &str) -> (String, String) {
    let mut parts = text.splitn(3, ':');
    if let (Some(file), Some(line), Some(rest)) = (parts.next(), parts.next(), parts.next())
        && !file.is_empty()
        && !file.contains(char::is_whitespace)
        && line.parse::<u32>().is_ok()
    {
        return (format!("{file}:{line}"), rest.trim_start().to_owned());
    }
    (fallback.to_owned(), text.to_owned())
}

fn new_lua(state: &Rc<State>) -> mlua::Result<Lua> {
    let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::new())?;
    lua.set_memory_limit(limits::MEMORY)?;
    let stopping = state.stopping.clone();
    lua.set_global_hook(HookTriggers::new().every_nth_instruction(HOOK_EVERY), move |_, _| {
        // Past the deadline (or stopping), yield first: that leaves even a
        // `pcall`, and the invocation's timeout (or the VM's end) then drops
        // the coroutine. Firing again means it could not yield (a C
        // boundary): raise instead.
        let stop = stopping.load(Ordering::Relaxed);
        let overdue =
            CURRENT.try_with(|c| (stop || c.deadline.is_some_and(|at| Instant::now() >= at)).then(|| c.overdue.replace(true))).ok().flatten();
        match overdue {
            None if stop => Err(mlua::Error::runtime("stopped")),
            None => Ok(VmState::Continue),
            Some(false) => Ok(VmState::Yield),
            Some(true) => Err(mlua::Error::runtime(if stop { "stopped" } else { "time limit exceeded" })),
        }
    })?;
    let globals = lua.globals();
    if let Ok(io) = globals.get::<Table>("io") {
        io.set("popen", LuaValue::Nil)?;
    }
    if let Ok(os) = globals.get::<Table>("os") {
        os.set("execute", LuaValue::Nil)?;
        os.set("exit", LuaValue::Nil)?;
    }
    let package: Table = globals.get("package")?;
    package.set("loadlib", LuaValue::Nil)?;
    package.set("cpath", "")?;
    let dir = state.options.dir.to_string_lossy();
    package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))?;
    let searchers: Table = package.get("searchers")?;
    let preload: LuaValue = searchers.get(1)?;
    let ours = LuaValue::Function(lua.create_function(search)?);
    package.set("searchers", lua.create_sequence_from([preload, ours])?)?;
    if state.options.mode == Mode::Config {
        globals.set(
            "print",
            lua.create_function(|lua, values: mlua::Variadic<LuaValue>| {
                let line = values.iter().map(|v| crate::silo::show(lua, v)).collect::<Vec<_>>().join("\t");
                self::state(lua)?.log(LogLevel::Info, &line);
                Ok(())
            })?,
        )?;
    }
    lua.set_app_data(Rc::clone(state));
    crate::silo::install(&lua, state)?;
    Ok(lua)
}

/// `require`'s searcher: Lua files on `package.path` only, named by their
/// path under the config's folder, and remembered for watching.
fn search(lua: &Lua, name: String) -> mlua::Result<MultiValue> {
    let state = state(lua)?;
    let path_spec: String = lua.globals().get::<Table>("package")?.get("path")?;
    let rel = name.replace('.', "/");
    let mut tried = String::new();
    for template in path_spec.split(';').filter(|t| !t.is_empty()) {
        let file = template.replace('?', &rel);
        let path = PathBuf::from(&file);
        if path.is_file() {
            state.track_file(path.clone());
            let code = std::fs::read(&path).map_err(|e| mlua::Error::runtime(format!("{file}: {e}")))?;
            let shown = path.strip_prefix(&state.options.dir).map_or_else(|_| file.clone(), |p| p.to_string_lossy().into_owned());
            let chunk = lua.load(code).set_name(format!("@{shown}")).set_mode(ChunkMode::Text).into_function()?;
            return (chunk, file).into_lua_multi(lua);
        }
        tried.push_str(&format!("\n\tno file '{file}'"));
    }
    tried.into_lua_multi(lua)
}

/// The VM's thread.
async fn main(
    source: Source,
    options: Options,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    exit: watch::Sender<Option<Exit>>,
    running: watch::Sender<usize>,
    files: Arc<Mutex<Vec<PathBuf>>>,
    loaded_tx: oneshot::Sender<Loaded>,
) {
    let name = options.source_name.clone();
    let mode = options.mode;
    let state = Rc::new(State {
        options,
        loading: Cell::new(false),
        decl: RefCell::default(),
        actions: RefCell::default(),
        handlers: RefCell::default(),
        timers: RefCell::default(),
        next_id: Cell::new(0),
        waiters: RefCell::default(),
        attached: RefCell::default(),
        attached_tx: watch::channel(false).0,
        files,
        exit,
        top: RefCell::default(),
        stopping: Arc::default(),
        queue: RefCell::default(),
        queued: Notify::new(),
        dropped_logged: Cell::new(false),
        main: RefCell::default(),
    });
    let failed = |message: String| {
        let (source, message) = split_source(&message, &name);
        Loaded { failed: true, problems: vec![problem(ProblemLevel::Error, &source, message)], ..Loaded::default() }
    };
    let lua = match new_lua(&state) {
        Ok(lua) => lua,
        Err(e) => {
            let _ = loaded_tx.send(failed(format!("the Lua VM did not start: {}", message(&e))));
            state.finish(Exit { code: 1, value: Value::Null, error: Some("the Lua VM did not start".into()) });
            return;
        }
    };
    let code = match source {
        Source::File(path) => {
            state.track_file(path.clone());
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))
        }
        Source::Text { code } => Ok(code.into_bytes()),
    };
    let compiled =
        code.and_then(|code| lua.load(code).set_name(format!("@{name}")).set_mode(ChunkMode::Text).into_function().map_err(|e| message(&e)));
    let loaded = match compiled {
        Err(e) => {
            let mut loaded = state.loaded(true);
            loaded.problems.extend(failed(e).problems);
            loaded
        }
        Ok(chunk) if mode == Mode::Script => {
            *state.main.borrow_mut() = Some(chunk);
            state.loaded(false)
        }
        Ok(chunk) => {
            state.loading.set(true);
            let ran = invoke(Current::new(Api::none(), Ctx::default(), Some(limits::LOAD)), chunk, MultiValue::new()).await;
            state.loading.set(false);
            match ran {
                Ok(_) => state.loaded(false),
                Err(failure) => {
                    let text = match failure {
                        Failure::Timeout => format!("{name} took longer than {}s to load", limits::LOAD.as_secs()),
                        Failure::Lua(m) => m,
                    };
                    let mut loaded = state.loaded(true);
                    loaded.problems.extend(failed(text).problems);
                    loaded
                }
            }
        }
    };
    let failed_load = loaded.failed;
    let _ = loaded_tx.send(loaded);
    tokio::task::spawn_local(run_handlers(state.clone(), lua.clone()));

    let mut exit = state.exit.subscribe();
    let running = Rc::new(running);
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Cmd::Run { name, call, reply }) => {
                    let (state, lua, running) = (state.clone(), lua.clone(), running.clone());
                    tokio::task::spawn_local(async move {
                        running.send_modify(|n| *n += 1);
                        let _count = Decrement(running);
                        let result = run_action(&state, &lua, &name, *call).await;
                        let _ = reply.send(result);
                    });
                }
                Some(Cmd::Attach { api, events }) => attach(&state, &lua, api, events, failed_load),
                Some(Cmd::Stop) | None => break,
            },
            _ = exit.wait_for(Option::is_some) => break,
        }
    }
    state.finish(Exit::default());
    // Break the state's links to Lua functions before the tasks go.
    lua.remove_app_data::<Rc<State>>();
    state.waiters.borrow_mut().clear();
    for timer in state.timers.borrow().values() {
        timer.cancel();
    }
}

/// Counts an action as done when dropped (finished or cancelled).
struct Decrement(Rc<watch::Sender<usize>>);

impl Drop for Decrement {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

async fn run_action(state: &Rc<State>, lua: &Lua, name: &str, call: Invocation) -> Result<Value, RpcError> {
    let found = state.actions.borrow().get(name).map(|a| (a.run.clone(), a.schema.clone(), a.timeout));
    let Some((run, schema, timeout)) = found else {
        return Err(RpcError::not_found(format!("action {name}")));
    };
    let args = if call.args.is_null() { json!({}) } else { call.args };
    args::check(&schema, &args).map_err(|e| RpcError::bad_request(format!("{name}: {e}")))?;
    let lua_args = (ctx_table(lua, &call.ctx, &state.options.host), to_lua(lua, &args));
    let (Ok(ctx), Ok(args)) = lua_args else {
        return Err(RpcError::internal("out of memory"));
    };
    let lua_args = (ctx, args).into_lua_multi(lua).map_err(|e| RpcError::internal(message(&e)))?;
    match invoke(Current::new(call.api, call.ctx, Some(timeout)), run, lua_args).await {
        Ok(values) => {
            result_json(lua, values.into_iter().next().unwrap_or(LuaValue::Nil)).map_err(|e| RpcError::internal(format!("{name} returned {e}")))
        }
        Err(Failure::Timeout) => Err(RpcError::new(RpcError::TIMEOUT, format!("action {name} did not finish in {}s", timeout.as_secs_f64()))),
        Err(Failure::Lua(m)) => Err(RpcError::new(ACTION_FAILED, m)),
    }
}

/// What `run` returned as a result: nothing, `nil` or `true` is none.
fn result_json(lua: &Lua, value: LuaValue) -> Result<Value, String> {
    match value {
        LuaValue::Nil | LuaValue::Boolean(true) => Ok(Value::Null),
        other => from_lua(lua, &other),
    }
}

fn attach(state: &Rc<State>, lua: &Lua, api: Api, events: Option<Events>, failed_load: bool) {
    if state.attached.borrow().is_some() {
        state.log(LogLevel::Warn, "attached twice; ignored");
        return;
    }
    *state.attached.borrow_mut() = Some(api.clone());
    state.attached_tx.send_replace(true);
    if let Some(events) = events {
        tokio::task::spawn_local(pump(state.clone(), lua.clone(), api.clone(), events));
    }
    if state.options.mode != Mode::Script {
        return;
    }
    let Some(main) = state.main.borrow_mut().take() else {
        let why = if failed_load { "the script did not load" } else { "the script has already run" };
        state.finish(Exit { code: 1, value: Value::Null, error: Some(why.into()) });
        return;
    };
    let (state, lua) = (state.clone(), lua.clone());
    tokio::task::spawn_local(async move {
        let options = &state.options;
        let ctx = Ctx {
            session_id: options.session_id.clone().or_else(|| api.session_id.clone()),
            window_id: api.window_id.clone(),
            block_id: options.block_id.clone().or_else(|| api.block_id.clone()),
            client_id: api.client_id,
            origin: Some(ActionOrigin::Script),
        };
        match invoke(Current::new(api, ctx, None), main, MultiValue::new()).await {
            Ok(values) => match from_lua(&lua, &values.into_iter().next().unwrap_or(LuaValue::Nil)) {
                Ok(value) => {
                    *state.top.borrow_mut() = Some(value);
                    state.check_done();
                }
                Err(e) => state.finish(Exit { code: 1, value: Value::Null, error: Some(format!("the script returned {e}")) }),
            },
            Err(Failure::Lua(m)) => state.finish(Exit { code: 1, value: Value::Null, error: Some(m) }),
            Err(Failure::Timeout) => state.finish(Exit { code: 1, value: Value::Null, error: Some("time limit exceeded".into()) }),
        }
    });
}

/// Hear the connection's events: work out the derived ones, wake waiters
/// and queue what handlers listen for.
async fn pump(state: Rc<State>, lua: Lua, api: Api, mut events: Events) {
    let mut tracker = Tracker::default();
    seed(&state, &api, &mut tracker).await;
    while let Some((_, event)) = events.recv().await {
        if state.finished() {
            return;
        }
        for heard in tracker.handle(&event) {
            if !state.in_scope(&heard.ctx) {
                continue;
            }
            wake_waiters(&state, &lua, &heard);
            if state.handlers.borrow().iter().any(|h| h.event == heard.name) {
                let mut queue = state.queue.borrow_mut();
                if queue.len() >= limits::QUEUED_EVENTS {
                    queue.pop_front();
                    if !state.dropped_logged.replace(true) {
                        state.log(LogLevel::Warn, &format!("more than {} events waiting for handlers; dropping the oldest", limits::QUEUED_EVENTS));
                    }
                }
                queue.push_back(heard);
                state.queued.notify_one();
            }
        }
    }
    if state.options.mode == Mode::Script {
        state.finish(Exit { code: 1, value: Value::Null, error: Some("the connection to the server closed".into()) });
    }
}

/// Know the sessions as they are, so the first change is told apart.
async fn seed(state: &State, api: &Api, tracker: &mut Tracker) {
    let Ok(list) = api.call("session.list", json!({})).await else { return };
    let Ok(list) = serde_json::from_value::<Vec<SessionSummary>>(list) else { return };
    tracker.seed_list(&list);
    for summary in list {
        if !state.in_scope(&EventCtx { session_id: Some(summary.id.clone()), ..EventCtx::default() }) {
            continue;
        }
        if let Ok(view) = api.call("session.view", json!({"session_id": summary.id})).await
            && let Ok(view) = serde_json::from_value::<SessionView>(view)
        {
            tracker.seed_view(&view);
        }
    }
}

fn ctx_of(heard: &Heard, origin: ActionOrigin) -> Ctx {
    Ctx {
        session_id: heard.ctx.session_id.clone(),
        window_id: heard.ctx.window_id.clone(),
        block_id: heard.ctx.block_id.clone(),
        client_id: None,
        origin: Some(origin),
    }
}

fn wake_waiters(state: &State, lua: &Lua, heard: &Heard) {
    let waiters = std::mem::take(&mut *state.waiters.borrow_mut());
    let mut keep = Vec::with_capacity(waiters.len());
    for waiter in waiters {
        if waiter.tx.is_closed() {
            continue;
        }
        if waiter.event == heard.name && matches(state, lua, &waiter.filter, heard) {
            let _ = waiter.tx.send(heard.clone());
        } else {
            keep.push(waiter);
        }
    }
    state.waiters.borrow_mut().extend(keep);
}

/// Whether `heard` passes `silo.wait`'s filter: fields of the event (else
/// of its ctx) to equal, or a function `(ev, ctx)`.
fn matches(state: &State, lua: &Lua, filter: &Filter, heard: &Heard) -> bool {
    match filter {
        Filter::Any => true,
        Filter::Fields(fields) => fields.iter().all(|(key, want)| {
            let ctx = match key.as_str() {
                "session_id" => heard.ctx.session_id.clone(),
                "window_id" => heard.ctx.window_id.clone(),
                "block_id" => heard.ctx.block_id.clone(),
                _ => None,
            };
            heard.ev.get(key).cloned().or_else(|| ctx.map(Value::String)).as_ref() == Some(want)
        }),
        Filter::Func(f) => {
            let ctx = ctx_of(heard, ActionOrigin::Event);
            let call = || -> mlua::Result<bool> {
                let ev = to_lua(lua, &heard.ev)?;
                let ctx = ctx_table(lua, &ctx, &state.options.host)?;
                let got: LuaValue = f.call((ev, ctx))?;
                Ok(!matches!(got, LuaValue::Nil | LuaValue::Boolean(false)))
            };
            let current = Rc::new(Current::new(Api::none(), ctx.clone(), Some(limits::CALLBACK)));
            CURRENT.sync_scope(current, call).unwrap_or_else(|e| {
                state.log(LogLevel::Warn, &format!("silo.wait filter for {}: {}", heard.name, message(&e)));
                false
            })
        }
    }
}

/// Run handlers, one event at a time.
async fn run_handlers(state: Rc<State>, lua: Lua) {
    loop {
        let next = state.queue.borrow_mut().pop_front();
        let Some(heard) = next else {
            state.queued.notified().await;
            continue;
        };
        let handlers: Vec<Function> = state.handlers.borrow().iter().filter(|h| h.event == heard.name).map(|h| h.func.clone()).collect();
        for func in handlers {
            let ctx = ctx_of(&heard, ActionOrigin::Event);
            let args = (ctx_table(&lua, &ctx, &state.options.host), to_lua(&lua, &heard.ev));
            let (Ok(ctx_t), Ok(ev)) = args else { continue };
            let Ok(args) = (ctx_t, ev).into_lua_multi(&lua) else { continue };
            let ran = invoke(Current::new(state.callback_api(), ctx, Some(limits::CALLBACK)), func, args).await;
            callback_failed(&state, &format!("a handler for {}", heard.name), ran.err());
        }
    }
}

/// Report a handler's or timer's failure; with `fail_fast`, a script ends.
pub(crate) fn callback_failed(state: &State, what: &str, failure: Option<Failure>) {
    let Some(failure) = failure else { return };
    if state.finished() {
        return;
    }
    let text = match failure {
        Failure::Timeout => format!("{what} did not finish in {}s", limits::CALLBACK.as_secs()),
        Failure::Lua(m) => format!("{what} failed: {m}"),
    };
    state.log(LogLevel::Error, &text);
    if state.options.mode == Mode::Script && state.options.fail_fast {
        state.finish(Exit { code: 1, value: Value::Null, error: Some(text) });
    }
}

/// Start a timer calling `func(ctx)` after `period`, and every `period`
/// when `repeat`. A config's timers wait for [`Vm::attach`].
pub(crate) fn start_timer(state: &Rc<State>, lua: &Lua, func: Function, period: Duration, repeat: bool, ctx: Ctx) -> Result<Rc<TimerShared>, String> {
    if state.timers.borrow().len() >= limits::TIMERS {
        return Err(format!("at most {} timers", limits::TIMERS));
    }
    let period = period.max(Duration::from_millis(1));
    let id = state.next_id();
    let shared = Rc::new(TimerShared { active: Cell::new(true), cancel: Notify::new() });
    state.timers.borrow_mut().insert(id, shared.clone());
    let (state, lua, timer) = (state.clone(), lua.clone(), shared.clone());
    tokio::task::spawn_local(async move {
        if state.options.mode == Mode::Config {
            let mut attached = state.attached_tx.subscribe();
            tokio::select! {
                _ = attached.wait_for(|a| *a) => {}
                () = timer.cancel.notified() => {}
            }
        }
        if repeat {
            let mut interval = tokio::time::interval_at(Instant::now() + period, period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while timer.active.get() {
                tokio::select! {
                    _ = interval.tick() => {}
                    () = timer.cancel.notified() => break,
                }
                if !timer.active.get() {
                    break;
                }
                timer_tick(&state, &lua, &ctx, &func).await;
            }
        } else if timer.active.get() {
            tokio::select! {
                () = tokio::time::sleep(period) => timer_tick(&state, &lua, &ctx, &func).await,
                () = timer.cancel.notified() => {}
            }
        }
        timer.active.set(false);
        state.timers.borrow_mut().remove(&id);
        state.check_done();
    });
    Ok(shared)
}

async fn timer_tick(state: &State, lua: &Lua, ctx: &Ctx, func: &Function) {
    let ctx_t = ctx_table(lua, ctx, &state.options.host);
    let Ok(args) = ctx_t.and_then(|t| t.into_lua_multi(lua)) else { return };
    let ran = invoke(Current::new(state.callback_api(), ctx.clone(), Some(limits::CALLBACK)), func.clone(), args).await;
    callback_failed(state, "a timer", ran.err());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_split_off_messages() {
        assert_eq!(split_source("init.lua:12: boom", "init.lua"), ("init.lua:12".into(), "boom".into()));
        assert_eq!(split_source("keys.lua:3: unexpected symbol", "init.lua"), ("keys.lua:3".into(), "unexpected symbol".into()));
        assert_eq!(split_source("plain failure", "init.lua"), ("init.lua".into(), "plain failure".into()));
    }
}
