//! Lua config and actions (`docs/LUA.md` §8): the server's `init.lua`, its
//! VM ([`silo_lua::Vm`]), the watcher that reloads it, and `config.*` and
//! `action.*`.
//!
//! - Handlers and timers call Silo on a stand-in connection acting as the
//!   owner, labelled `init.lua`, opened for each VM and closed with it.
//! - An action calls Silo on a stand-in acting as its caller
//!   ([`StandIns::caller`]), so every call inside has the caller's rights,
//!   pane and approvals.
//! - A load that fails keeps the VM and state it had and reports why. A
//!   missing file is an empty config, with no VM.
//! - A hot upgrade hands nothing over: the successor loads afresh.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use silo_client::Control;
use silo_lua::{Api, Ctx, Invocation, Loaded, Options, Source, Vm};
use silo_proto::{
    api::method_name,
    config::{ActionList, ActionOrigin, ActionResult, ActionRunParams, ConfigState},
    control::{ClientRole, Event, Method, RpcError},
    model::ClientId,
};
use tokio::sync::{Notify, mpsc};

use crate::{
    Server,
    access::Principal,
    tools::{StandIn, StandIns},
};

/// Quiet time after a change to the config before it is loaded.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// How often a config folder that does not exist is looked for.
const POLL: Duration = Duration::from_secs(3);
/// Longest wait for a replaced VM's running actions before it is stopped.
const RETIRE_WAIT: Duration = Duration::from_secs(30);
/// The owner stand-in's label (`owner_name` of what handlers make).
const LABEL: &str = "init.lua";

/// The config: what is loaded and how to load it again. On [`Server`].
pub struct Scripting {
    /// The config file; `None`: scripting is off.
    path: Option<PathBuf>,
    current: Mutex<Current>,
    /// One load at a time.
    loads: tokio::sync::Mutex<()>,
    /// A load finished: the watcher looks at what to watch again.
    loaded: Notify,
    stopped: AtomicBool,
}

#[derive(Default)]
struct Current {
    vm: Option<Vm>,
    /// The connection `vm`'s handlers and timers call through.
    owner: Option<Control>,
    state: ConfigState,
    /// What the last failed load read, watched so that fixing a required
    /// file reloads too.
    failed_files: Vec<PathBuf>,
}

impl Scripting {
    /// Scripting for the server at `home`; off when `enabled` is false or
    /// `SILO_CONFIG=off`. Nothing loads until [`run`].
    #[must_use]
    pub fn new(enabled: bool, home: &Path) -> Self {
        let off = std::env::var_os("SILO_CONFIG").is_some_and(|v| v == "off");
        let path = (enabled && !off).then(|| silo_proto::paths::config_file(home));
        let state = ConfigState { path: path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(), ..ConfigState::default() };
        Self {
            path,
            current: Mutex::new(Current { state, ..Current::default() }),
            loads: tokio::sync::Mutex::new(()),
            loaded: Notify::new(),
            stopped: AtomicBool::new(false),
        }
    }

    /// Whether `init.lua` is loaded at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.path.is_some()
    }

    fn current(&self) -> std::sync::MutexGuard<'_, Current> {
        self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// What `config.get` answers.
    #[must_use]
    pub fn state(&self) -> ConfigState {
        if !self.enabled() {
            return ConfigState::default();
        }
        self.current().state.clone()
    }

    /// Stop the VM (its timers, handlers and the commands it started, not
    /// detached ones) and load nothing more: the server is stopping or
    /// handing over.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let (vm, owner) = {
            let mut current = self.current();
            (current.vm.take(), current.owner.take())
        };
        if let Some(vm) = vm {
            vm.stop();
        }
        drop(owner);
    }

    /// The files whose changes reload the config: the file itself, what
    /// the VM has read (`require`), and what the last failed load read.
    fn files(&self) -> Vec<PathBuf> {
        let Some(path) = &self.path else { return Vec::new() };
        let current = self.current();
        let mut files = vec![path.clone()];
        files.extend(current.vm.iter().flat_map(Vm::files));
        files.extend(current.failed_files.iter().cloned());
        files.sort();
        files.dedup();
        files
    }

    /// Load the config file again (serialized with other loads) and return
    /// the state after it.
    pub async fn reload(&self, server: &Server) -> ConfigState {
        let Some(path) = self.path.clone() else { return ConfigState::default() };
        let _one = self.loads.lock().await;
        if self.stopped.load(Ordering::SeqCst) {
            return self.state();
        }
        let exists = tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file());
        if !exists {
            let unchanged = {
                let current = self.current();
                current.vm.is_none() && !current.state.exists && !current.state.failed && current.state.loaded_at != 0
            };
            if !unchanged {
                tracing::info!(path = %path.display(), "no config file; using an empty config");
                self.apply(server, None, None, &Loaded::default(), false);
            }
            return self.loaded();
        }
        let options = Options { host: server.info.hostname.clone(), ..Options::config(&path) };
        let (vm, loaded) = Vm::load(Source::File(path.clone()), options).await;
        for problem in &loaded.problems {
            tracing::warn!(source = problem.source, level = ?problem.level, "init.lua: {}", problem.message);
        }
        if loaded.failed {
            vm.stop();
            tracing::warn!(path = %path.display(), "config load failed; keeping the last good config");
            let revision = {
                let mut current = self.current();
                current.state.failed = true;
                current.state.problems.clone_from(&loaded.problems);
                current.state.revision += 1;
                current.failed_files.clone_from(&loaded.files);
                current.state.revision
            };
            server.registry.lock().unwrap().emit(Event::ConfigChanged { revision });
            return self.loaded();
        }
        let owner = match handlers_connection(server).await {
            Ok((control, events)) => {
                vm.attach(Api::control(control.clone()), Some(events));
                Some(control)
            }
            Err(error) => {
                tracing::error!("config handlers get no connection: {}", error.message);
                vm.attach(Api::none(), None);
                None
            }
        };
        if self.stopped.load(Ordering::SeqCst) {
            vm.stop();
            return self.state();
        }
        tracing::info!(path = %path.display(), actions = loaded.actions.len(), handlers = loaded.handlers.len(), "config loaded");
        self.apply(server, Some(vm), owner, &loaded, true);
        self.loaded()
    }

    /// The state after a load, waking the watcher.
    fn loaded(&self) -> ConfigState {
        self.loaded.notify_one();
        self.state()
    }

    /// Swap in `vm` (none for a missing file), tell clients, and retire
    /// the VM it replaces.
    fn apply(&self, server: &Server, vm: Option<Vm>, owner: Option<Control>, loaded: &Loaded, exists: bool) {
        let (old, old_owner, revision) = {
            let mut current = self.current();
            if self.stopped.load(Ordering::SeqCst) {
                // `stop` ran meanwhile; it found no VM to stop but this one.
                if let Some(vm) = vm {
                    vm.stop();
                }
                return;
            }
            let revision = current.state.revision + 1;
            current.state = ConfigState {
                revision,
                path: current.state.path.clone(),
                exists,
                loaded_at: crate::block::now_ms(),
                failed: false,
                bindings: loaded.bindings.clone(),
                unbinds: loaded.unbinds.clone(),
                modes: loaded.modes.clone(),
                actions: loaded.actions.clone(),
                handlers: loaded.handlers.clone(),
                timers: loaded.timers,
                problems: loaded.problems.clone(),
            };
            current.failed_files.clear();
            let old = std::mem::replace(&mut current.vm, vm);
            let old_owner = std::mem::replace(&mut current.owner, owner);
            (old, old_owner, revision)
        };
        let mut reg = server.registry.lock().unwrap();
        // The old VM's handlers and timers stop hearing and acting at once
        // (they would run beside the new VM's); its running actions call
        // through their callers' connections and finish.
        if let Some(close) = old_owner.as_ref().and_then(|o| reg.clients.get_mut(&o.client_id())).and_then(|c| c.close.take()) {
            let _ = close.send(());
        }
        reg.emit(Event::ConfigChanged { revision });
        drop(reg);
        if let Some(old) = old {
            tokio::spawn(async move {
                if tokio::time::timeout(RETIRE_WAIT, old.idle()).await.is_err() {
                    tracing::warn!(running = old.running_actions(), "stopping the replaced config with actions still running");
                }
                old.stop();
                drop(old_owner);
            });
        }
    }
}

/// A stand-in connection acting as the owner for the config's handlers and
/// timers, subscribed to events.
async fn handlers_connection(server: &Server) -> Result<(Control, silo_client::Events), RpcError> {
    let stand_in = StandIn { principal: Principal::Owner, role: ClientRole::Cli, pane: None, tty: false, label: LABEL.into() };
    let (control, events) = StandIns::connect(server, stand_in, LABEL).await?;
    control.subscribe().await.map_err(silo_lua::rpc_error)?;
    Ok((control, events))
}

/// Load the config and reload it whenever it changes, until the server
/// stops. Spawned once the server listens.
pub async fn run(server: Arc<Server>) {
    if !server.scripting.enabled() {
        return;
    }
    watch(&server).await;
}

/// What to watch for a set of files: their folders (editors replace files
/// by renaming, so a file is watched through its folder) and the paths
/// whose events mean a change. A folder that does not exist is not watched
/// through an ancestor (that could be the whole home folder): it is looked
/// for every [`POLL`].
#[derive(Default, PartialEq, Eq)]
struct Plan {
    dirs: HashSet<PathBuf>,
    paths: HashSet<PathBuf>,
    /// Whether a file's folder is missing.
    missing: bool,
}

/// `path` with its longest existing ancestor resolved (the paths file
/// events carry: `/private/tmp`, not `/tmp`, on macOS).
fn resolve(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = at.canonicalize() {
            return rest.iter().rev().fold(real, |p, name| p.join(name));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                at = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

fn plan(files: &[PathBuf]) -> Plan {
    let mut plan = Plan::default();
    for file in files {
        let file = resolve(file);
        match file.parent().filter(|dir| dir.is_dir()) {
            Some(dir) => {
                plan.dirs.insert(dir.to_owned());
                plan.paths.insert(file);
            }
            None => plan.missing = true,
        }
    }
    plan
}

/// Load the config, then reload on changes to [`Scripting::files`],
/// debounced.
async fn watch(server: &Arc<Server>) {
    let scripting = &server.scripting;
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let handler = move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if !matches!(event.kind, notify::EventKind::Access(_)) {
            let _ = tx.send(event.paths);
        }
    };
    // Made with the first folder to watch: a server without a config
    // folder has no watcher.
    let mut handler = Some(handler);
    let mut watcher = None;
    let mut current = Plan::default();
    let mut loaded = false;
    loop {
        if scripting.stopped.load(Ordering::SeqCst) {
            return;
        }
        let next = plan(&scripting.files());
        if next != current {
            use notify::Watcher;
            if watcher.is_none()
                && !next.dirs.is_empty()
                && let Some(handler) = handler.take()
            {
                match notify::recommended_watcher(handler) {
                    Ok(made) => watcher = Some(made),
                    Err(error) => tracing::warn!("config watch: {error}; reload with `silo config reload`"),
                }
            }
            let mut added = false;
            if let Some(watcher) = &mut watcher {
                for dir in current.dirs.difference(&next.dirs) {
                    let _ = watcher.unwatch(dir);
                }
                for dir in next.dirs.difference(&current.dirs) {
                    match watcher.watch(dir, notify::RecursiveMode::NonRecursive) {
                        Ok(()) => added = true,
                        Err(error) => tracing::debug!(dir = %dir.display(), "config watch: {error}"),
                    }
                }
            }
            current = next;
            // A folder appeared (the config's, made after the server
            // started; a required file's): the file may have been written
            // before it was watched.
            if added && loaded {
                scripting.reload(server).await;
                continue;
            }
        }
        // The first load, once the config's folder is watched: a change
        // made while it loads is then seen.
        if !loaded {
            loaded = true;
            scripting.reload(server).await;
            continue;
        }
        tokio::select! {
            () = scripting.loaded.notified() => {}
            // Look for the missing folder again.
            () = tokio::time::sleep(POLL), if current.missing => {}
            paths = rx.recv() => {
                let Some(paths) = paths else { return };
                if !paths.iter().any(|p| current.paths.contains(p)) {
                    continue;
                }
                loop {
                    match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                        Ok(Some(_)) => {}
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
                tracing::info!("config changed; reloading");
                scripting.reload(server).await;
            }
        }
    }
}

/// Handle a `config.*` or `action.*` call from `client` (its access is
/// checked: `control` on the server).
///
/// # Errors
/// `action.run`: `not_found` (no such action, or scripting is off), the
/// action's errors (`bad_request` for its args, `timeout`,
/// `action_failed`, `stopped`).
pub async fn handle(server: &Server, client: ClientId, principal: &Principal, method: Method) -> Result<Value, RpcError> {
    let _ = principal;
    let scripting = &server.scripting;
    let value = match method {
        Method::ConfigGet {} => serde_json::to_value(scripting.state()),
        Method::ConfigReload {} => serde_json::to_value(scripting.reload(server).await),
        Method::ActionList {} => serde_json::to_value(ActionList { actions: scripting.state().actions }),
        Method::ActionRun(params) => return run_action(server, client, params).await,
        other => return Err(RpcError::method_not_found(&method_name(&other))),
    };
    value.map_err(RpcError::internal)
}

/// `action.run` for `client`, on its stand-in connection.
async fn run_action(server: &Server, client: ClientId, mut params: ActionRunParams) -> Result<Value, RpcError> {
    let not_found = || RpcError::not_found(format!("action {}", params.name));
    let vm = {
        let current = server.scripting.current();
        let defined = current.state.actions.iter().any(|a| a.name == params.name);
        current.vm.clone().filter(|_| server.scripting.enabled() && defined)
    };
    let vm = vm.ok_or_else(not_found)?;
    params.client_id.get_or_insert(client);
    params.origin.get_or_insert(ActionOrigin::Api);
    {
        // `silo do my_action` typed in a terminal acts on that terminal.
        let reg = server.registry.lock().unwrap();
        if params.block_id.is_none() {
            params.block_id = reg.clients.get(&client).and_then(|c| c.pane.clone());
        }
        if let Some(block) = params.block_id.as_deref()
            && let Some(session) = reg.block_session(block)
            && params.session_id.as_ref().is_none_or(|s| *s == session)
        {
            if params.window_id.is_none() {
                let session = reg.sessions.iter().find(|s| s.id == session);
                params.window_id = session.and_then(|s| s.windows.iter().find(|w| w.contains(block))).map(|w| w.id.clone());
            }
            params.session_id = Some(session);
        }
    }
    let control = StandIns::caller(server, client).await?;
    let call = Invocation { ctx: Ctx::from_run(&params), args: params.args.clone(), api: Api::control(control) };
    let result = vm.run_action(&params.name, call).await?;
    serde_json::to_value(ActionResult { result }).map_err(RpcError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_watched_through_its_folder_once_that_exists() {
        let root = std::env::temp_dir().join(format!("silo-scripting-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let real = root.canonicalize().unwrap();
        let file = root.join("config/init.lua");
        let planned = plan(std::slice::from_ref(&file));
        assert!(planned.missing && planned.dirs.is_empty() && planned.paths.is_empty(), "no ancestor is watched");
        std::fs::create_dir_all(root.join("config")).unwrap();
        let planned = plan(&[file]);
        assert!(!planned.missing);
        assert_eq!(planned.dirs, HashSet::from([real.join("config")]));
        assert_eq!(planned.paths, HashSet::from([real.join("config/init.lua")]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn off_answers_an_empty_state() {
        let scripting = Scripting::new(false, Path::new("/nonexistent"));
        assert!(!scripting.enabled());
        assert_eq!(scripting.state(), ConfigState::default());
        assert!(scripting.files().is_empty());
    }
}
