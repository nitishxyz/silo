//! The Lua VM of Silo's config (`init.lua`, run by the server) and scripts
//! (`silo do script.lua`, run by the CLI): `docs/LUA.md` §3, §4, §9.
//!
//! It knows nothing of the server. It is handed an [`Api`] (a [`Backend`]
//! that calls control methods, usually a [`silo_client::Control`], plus
//! where ids default from) and the connection's [`Events`].
//!
//! Each [`Vm`] runs on a thread of its own (a current-thread runtime and a
//! `LocalSet`); the handle is cheap to clone and `Send + Sync`. Dropping the
//! last handle stops the VM. Every action run, event handler call and timer
//! tick is a Lua coroutine, so they interleave where they wait on Silo, a
//! command or a timer.
//!
//! # Config (the server)
//!
//! ```no_run
//! # async fn f(owner: silo_client::Control, owner_events: silo_client::Events,
//! #            caller: silo_client::Control, run: silo_proto::config::ActionRunParams) {
//! use silo_lua::{Api, Ctx, Invocation, Options, Source, Vm};
//! let path = std::path::Path::new("/home/me/.config/silo/init.lua");
//! let (vm, loaded) = Vm::load(Source::File(path.into()), Options { host: "mini".into(), ..Options::config(path) }).await;
//! if loaded.failed { /* keep the last good VM; report loaded.problems */ }
//! // Watch loaded.files for changes.
//! // Handlers and timers run as the owner from now on (subscribe first).
//! owner.subscribe().await.unwrap();
//! vm.attach(Api::control(owner), Some(owner_events));
//! // `action.run`, on a stand-in connection acting as the caller:
//! let call = Invocation { ctx: Ctx::from_run(&run), args: run.args.clone(), api: Api::control(caller) };
//! let result = vm.run_action(&run.name, call).await; // Ok(json) or Err(RpcError)
//! // On reload: load the new one; once `vm.idle()` resolves (or 30 s), `vm.stop()`.
//! # }
//! ```
//!
//! # Scripts (the CLI)
//!
//! ```no_run
//! # async fn f(control: silo_client::Control, events: silo_client::Events) {
//! use silo_lua::{Api, Mode, Options, Source, Vm};
//! let options = Options {
//!     mode: Mode::Script,
//!     dir: std::env::current_dir().unwrap(),
//!     source_name: "layout.lua".into(),
//!     args: serde_json::json!({"name": "web"}),
//!     session_id: Some("s1a2b3c4d".into()),
//!     ..Options::default()
//! };
//! let (vm, loaded) = Vm::load(Source::File("layout.lua".into()), options).await;
//! if loaded.failed { /* print loaded.problems, exit 1 */ }
//! control.subscribe().await.unwrap();
//! vm.attach(Api::control(control).with_session(Some("s1a2b3c4d".into())), Some(events)); // runs it
//! let exit = vm.finished().await; // exit.code, exit.value (the returned value), exit.error
//! # }
//! ```
//!
//! A script ends when its top level has returned and it has no handlers or
//! timers, on `silo.stop(code)`, on an error (code 1), or on [`Vm::stop`].
//!
//! [`check`] loads a config file with no server, for `silo config check`.

mod api;
mod args;
mod convert;
mod events;
mod exec;
mod layout;
mod silo;
mod vm;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use serde_json::Value;
pub use silo_client::Events;
use silo_proto::{
    config::{ActionInfo, ActionOrigin, ActionRunParams, ConfigProblem, HandlerInfo, KeyBinding, KeyMode, KeyUnbind},
    model::{BlockId, ClientId, SessionId, WindowId},
};

pub use api::{Api, Backend, BoxFuture, CLOSED, NO_CALLER, rpc_error};
pub use vm::Vm;

/// Code of an action that raised an error (`error(msg, 0)`).
pub const ACTION_FAILED: &str = "action_failed";
/// Code of a call into a VM that has stopped.
pub const STOPPED: &str = "stopped";

/// Limits (`docs/LUA.md` §3).
pub mod limits {
    use std::time::Duration;

    /// An action's time unless it sets `timeout`.
    pub const ACTION: Duration = Duration::from_secs(30);
    /// The most an action's `timeout` may ask for.
    pub const ACTION_MAX: Duration = Duration::from_secs(600);
    /// A handler call or a timer tick.
    pub const CALLBACK: Duration = Duration::from_secs(10);
    /// A config file's top level.
    pub const LOAD: Duration = Duration::from_secs(30);
    pub const MEMORY: usize = 256 << 20;
    pub const TIMERS: usize = 1024;
    pub const INVOKE_DEPTH: u32 = 16;
    pub const QUEUED_EVENTS: usize = 1024;
}

/// What the VM runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// `init.lua`: declares keys, modes, actions, handlers and timers;
    /// calls into Silo fail while it loads.
    #[default]
    Config,
    /// `silo do`: runs top to bottom with an [`Api`] once attached;
    /// `bind`, `unbind` and `mode` do nothing.
    Script,
}

/// Where the VM's `silo.log` (and a script's warnings) go, besides
/// `tracing`.
pub type LogSink = Arc<dyn Fn(LogLevel, &str) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// How a VM runs.
#[derive(Clone)]
pub struct Options {
    pub mode: Mode,
    /// `silo.config_dir`, and where `require` looks (`?.lua`, `?/init.lua`).
    pub dir: PathBuf,
    /// The chunk's name in sources and errors (`init.lua:12`).
    pub source_name: String,
    /// `silo.host` and `ctx.host`: the server's name.
    pub host: String,
    /// `silo.args` (scripts).
    pub args: Value,
    /// Handlers hear every session. Scripts otherwise hear only
    /// `session_id`'s (and events of no session); a config hears all
    /// sessions when it has no `session_id`.
    pub all_sessions: bool,
    /// `silo.session_id` (scripts: where it was run, or `-s`).
    pub session_id: Option<SessionId>,
    /// `silo.block_id` (scripts: where it was run, or `-b`).
    pub block_id: Option<BlockId>,
    /// Scripts: a failing handler or timer ends the script with code 1.
    pub fail_fast: bool,
    pub log: Option<LogSink>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Config,
            dir: PathBuf::from("."),
            source_name: "init.lua".into(),
            host: String::new(),
            args: Value::Null,
            all_sessions: false,
            session_id: None,
            block_id: None,
            fail_fast: false,
            log: None,
        }
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("mode", &self.mode)
            .field("dir", &self.dir)
            .field("source_name", &self.source_name)
            .field("host", &self.host)
            .field("all_sessions", &self.all_sessions)
            .field("session_id", &self.session_id)
            .field("block_id", &self.block_id)
            .field("fail_fast", &self.fail_fast)
            .finish_non_exhaustive()
    }
}

impl Options {
    /// A config VM for the file at `path`: hears every session, requires
    /// from the file's folder.
    #[must_use]
    pub fn config(path: &Path) -> Self {
        Self {
            mode: Mode::Config,
            dir: path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf),
            source_name: path.file_name().map_or_else(|| "init.lua".into(), |n| n.to_string_lossy().into_owned()),
            all_sessions: true,
            ..Self::default()
        }
    }
}

/// The Lua to run.
#[derive(Clone, Debug)]
pub enum Source {
    File(PathBuf),
    /// `silo do -e CODE`, `silo do -` (stdin), tests.
    Text {
        code: String,
    },
}

/// What a config declared while it loaded: the parts of
/// [`silo_proto::config::ConfigState`] Lua decides, and the files read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loaded {
    pub bindings: Vec<KeyBinding>,
    pub unbinds: Vec<KeyUnbind>,
    pub modes: Vec<KeyMode>,
    /// In the order defined (a redefinition keeps the first's place).
    pub actions: Vec<ActionInfo>,
    pub handlers: Vec<HandlerInfo>,
    /// Timers running when the load ended.
    pub timers: u32,
    pub problems: Vec<ConfigProblem>,
    /// The file did not compile or raised an error: use none of this.
    pub failed: bool,
    /// The file and every file `require` read (watch them for reloads).
    pub files: Vec<PathBuf>,
}

/// Where an action, handler or timer runs from: its `ctx`, and the ids
/// its calls default to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ctx {
    pub session_id: Option<SessionId>,
    pub window_id: Option<WindowId>,
    pub block_id: Option<BlockId>,
    /// The client whose `app.*` calls `silo.app.*` makes.
    pub client_id: Option<ClientId>,
    pub origin: Option<ActionOrigin>,
}

impl Ctx {
    /// The ctx of an `action.run` call.
    #[must_use]
    pub fn from_run(run: &ActionRunParams) -> Self {
        Self {
            session_id: run.session_id.clone(),
            window_id: run.window_id.clone(),
            block_id: run.block_id.clone(),
            client_id: run.client_id,
            origin: run.origin,
        }
    }
}

/// One action run: where from, its args, and what its calls go through
/// (the server: a stand-in connection acting as the caller).
#[derive(Clone, Debug)]
pub struct Invocation {
    pub ctx: Ctx,
    /// `null` is `{}`.
    pub args: Value,
    pub api: Api,
}

/// How a script ended.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exit {
    /// 0, 1 for an error, or `silo.stop`'s code.
    pub code: i32,
    /// What the top level returned, as JSON (`null`: nothing).
    pub value: Value,
    /// Why it failed (code 1).
    pub error: Option<String>,
}

/// Load the config file at `path` with no server: what it declares and its
/// problems (`silo config check`). Calls into Silo fail as they do at any
/// load; timers never fire. Blocks the calling thread while it loads.
#[must_use]
pub fn check(path: &Path) -> Loaded {
    let path = path.to_owned();
    let run = std::thread::Builder::new().name("silo-lua check".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        Some(runtime.block_on(async {
            let (vm, loaded) = Vm::load(Source::File(path.clone()), Options::config(&path)).await;
            vm.stop();
            loaded
        }))
    });
    run.ok().and_then(|t| t.join().ok().flatten()).unwrap_or_else(|| Loaded {
        failed: true,
        problems: vec![vm::problem(silo_proto::config::ProblemLevel::Error, "", "the Lua VM did not start")],
        ..Loaded::default()
    })
}
