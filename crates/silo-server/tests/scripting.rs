//! Server scripting (`docs/LUA.md` §8) over real connections: loading
//! `init.lua`, reloads, failed loads, actions run as their caller, config
//! handlers with no client listening, `config.changed`, and `SILO_SESSION`.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};
use silo_client::{Control, Events};
use silo_proto::{
    access::{Rights, Scope},
    config::{ActionList, ActionResult, ActionRunParams, ConfigState, ProblemLevel},
    control::{Capture, Created, Event, GrantCreated, GrantKind, Method, RpcError, SpawnSpec},
    model::{BlockKind, SessionSummary},
    paths,
};
use tokio::{task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(15);

/// Hellos read the process-wide token: one connection set up at a time.
static HELLO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-scripting-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Where the server at `home` reads its config: in `home`, never a real
/// one (`SILO_CONFIG` must be unset).
fn config_file(home: &Path) -> PathBuf {
    let file = paths::config_file(home);
    assert!(file.starts_with(home), "unset SILO_CONFIG to run these tests ({})", file.display());
    file
}

/// Replace the config file atomically, as editors do.
fn write_config(home: &Path, code: &str) {
    let file = config_file(home);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let tmp = file.with_extension("lua.tmp");
    std::fs::write(&tmp, code).unwrap();
    std::fs::rename(&tmp, &file).unwrap();
}

async fn start(home: &Path, scripting: bool) -> JoinHandle<()> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        scripting,
        ..Default::default()
    };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path, token: Option<&str>) -> (Control, Events) {
    let _one = HELLO.lock().await;
    silo_client::set_token(token.map(str::to_owned));
    let connected = Control::connect(&paths::socket(home), "scripting-test").await;
    silo_client::set_token(None);
    connected.unwrap()
}

async fn shutdown(control: Control, task: JoinHandle<()>, home: &Path) {
    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    drop(control);
    timeout(T, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(home);
}

fn code(result: anyhow::Result<Value>) -> String {
    result.unwrap_err().downcast::<RpcError>().map(|e| e.code).unwrap_or_else(|e| format!("{e:#}"))
}

async fn state(control: &Control) -> ConfigState {
    control.call(Method::ConfigGet {}).await.unwrap()
}

/// The config state once `done` holds for it.
async fn wait_state(control: &Control, what: &str, done: impl Fn(&ConfigState) -> bool) -> ConfigState {
    let deadline = tokio::time::Instant::now() + T;
    loop {
        let now = state(control).await;
        if done(&now) {
            return now;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}: {now:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + T;
    while !done().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn names(state: &ConfigState) -> Vec<&str> {
    state.actions.iter().map(|a| a.name.as_str()).collect()
}

fn run(name: &str, args: Value) -> ActionRunParams {
    ActionRunParams { name: name.into(), args, ..ActionRunParams::default() }
}

async fn run_action(control: &Control, params: ActionRunParams) -> anyhow::Result<Value> {
    control.call_value(Method::ActionRun(params)).await
}

async fn result(control: &Control, params: ActionRunParams) -> Value {
    let value = run_action(control, params).await.unwrap();
    serde_json::from_value::<ActionResult>(value).unwrap().result
}

fn shell(command: &str) -> SpawnSpec {
    SpawnSpec { cwd: None, command: vec!["/bin/sh".into(), "-c".into(), command.into()], size: Some(silo_proto::model::TermSize::new(80, 24)) }
}

async fn new_session(control: &Control, spawn: SpawnSpec) -> Created {
    control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap()
}

async fn session_name(control: &Control, id: &str) -> Option<String> {
    let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await.unwrap();
    sessions.into_iter().find(|s| s.id == id).map(|s| s.name)
}

async fn token(owner: &Control, scope: Scope, rights: Rights) -> String {
    let created: GrantCreated =
        owner.call(Method::GrantCreate { scope, rights, kind: GrantKind::Token, ttl_secs: None, label: Some("test".into()) }).await.unwrap();
    created.secret
}

/// Whether a `config.changed` arrives on `events` within `wait`.
async fn hears_config_changed(events: &mut Events, wait: Duration) -> bool {
    timeout(wait, async {
        loop {
            match events.recv().await {
                Some((_, Event::ConfigChanged { .. })) => return true,
                Some(_) => {}
                None => std::future::pending::<()>().await,
            }
        }
    })
    .await
    .unwrap_or(false)
}

const FIRST: &str = r#"silo.bind("cmd-e", "SplitRight")
silo.bind("ctrl-b c", "window.create")
silo.mode("resize", { exclusive = true })
silo.bind("resize/left", "MovePaneEdgeLeft")
silo.bind("cmd-shift-h", function(ctx) end)
silo.action{ name = "hello", title = "Hello", run = function() return "first" end }
silo.action{ name = "hello", title = "Hello Again", run = function() return "again" end }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn config_loads_reloads_and_keeps_the_last_good_one() {
    let home = temp_home("load");
    write_config(&home, FIRST);
    let task = start(&home, true).await;
    let (owner, mut owner_events) = connect(&home, None).await;
    owner.subscribe().await.unwrap();

    let loaded = wait_state(&owner, "the first load", |s| s.exists && s.revision >= 1).await;
    assert!(!loaded.failed && loaded.loaded_at > 0, "{loaded:?}");
    assert_eq!(loaded.path, config_file(&home).display().to_string());
    let keys: Vec<(&str, Option<&str>)> = loaded.bindings.iter().map(|b| (b.keys.as_str(), b.mode.as_deref())).collect();
    assert!(keys.contains(&("cmd-e", None)) && keys.contains(&("ctrl-b c", None)) && keys.contains(&("left", Some("resize"))), "{keys:?}");
    assert_eq!(loaded.modes.len(), 1);
    assert!(loaded.modes[0].exclusive && loaded.modes[0].name == "resize");
    let hidden = loaded.actions.iter().find(|a| a.hidden).expect("the bound function is a hidden action");
    assert!(hidden.name.starts_with("bind_"), "{hidden:?}");
    let hello = loaded.actions.iter().find(|a| a.name == "hello").unwrap();
    assert_eq!(hello.title, "Hello Again", "the later definition wins");
    let warning = loaded.problems.iter().find(|p| p.level == ProblemLevel::Warning).expect("a redefinition warns");
    assert_eq!(warning.source, "init.lua:7", "{warning:?}");
    // Hidden actions are listed too.
    let list: ActionList = owner.call(Method::ActionList {}).await.unwrap();
    assert_eq!(list.actions, loaded.actions);
    assert_eq!(result(&owner, run("hello", Value::Null)).await, json!("again"));
    assert_eq!(code(run_action(&owner, run("nope", Value::Null)).await), "not_found");

    // `config.changed` reaches the owner, not a session-scoped token.
    let created = new_session(&owner, shell("sleep 60")).await;
    let secret = token(&owner, Scope::Session { session_id: created.session_id.clone() }, Rights::Control).await;
    let (scoped, mut scoped_events) = connect(&home, Some(&secret)).await;
    scoped.subscribe().await.unwrap();
    while timeout(Duration::from_millis(200), owner_events.recv()).await.is_ok() {}

    // A change to the file reloads it.
    write_config(&home, "silo.action{ name = 'second', title = 'Second', run = function() return 2 end }\n");
    let second = wait_state(&owner, "the reload on a change", |s| names(s) == ["second"]).await;
    assert!(second.revision > loaded.revision && second.bindings.is_empty());
    assert!(hears_config_changed(&mut owner_events, T).await, "the owner hears config.changed");
    assert!(!hears_config_changed(&mut scoped_events, Duration::from_millis(500)).await, "a session-scoped token does not");
    assert_eq!(code(scoped.call_value(Method::ConfigGet {}).await), RpcError::FORBIDDEN);

    // `config.reload` answers with the state after its load.
    std::fs::write(config_file(&home), "silo.action{ name = 'third', title = 'Third', run = function() return 3 end }\n").unwrap();
    let third: ConfigState = owner.call(Method::ConfigReload {}).await.unwrap();
    assert_eq!(names(&third), ["third"]);
    assert!(!third.failed);

    // A failed load keeps the last good actions and says why.
    write_config(&home, "silo.action{ name = 'broken', title = 'Broken', run = function() end }\nthis is not lua\n");
    let failed: ConfigState = owner.call(Method::ConfigReload {}).await.unwrap();
    assert!(failed.failed, "{failed:?}");
    assert_eq!(names(&failed), ["third"]);
    let error = failed.problems.iter().find(|p| p.level == ProblemLevel::Error).expect("the error is reported");
    assert!(error.source.starts_with("init.lua:2") || error.message.contains("init.lua:2"), "{error:?}");
    assert!(hears_config_changed(&mut owner_events, T).await, "a failed load is announced too");
    assert_eq!(result(&owner, run("third", Value::Null)).await, json!(3), "the last good config still runs");
    assert_eq!(code(run_action(&owner, run("broken", Value::Null)).await), "not_found");

    // Fixing it loads again; removing it leaves an empty config.
    write_config(&home, "silo.action{ name = 'fixed', title = 'Fixed', run = function() end }\n");
    wait_state(&owner, "the fixed config", |s| !s.failed && names(s) == ["fixed"]).await;
    std::fs::remove_file(config_file(&home)).unwrap();
    let empty = wait_state(&owner, "the empty config", |s| !s.exists).await;
    assert!(empty.actions.is_empty() && !empty.failed);
    assert_eq!(code(run_action(&owner, run("fixed", Value::Null)).await), "not_found");

    drop(scoped);
    shutdown(owner, task, &home).await;
}

const ACTIONS: &str = r#"silo.action{ name = "rename", title = "Rename", args = { name = "string" },
  run = function(ctx, a)
    local ok, err, code = silo.session.rename{ name = a.name }
    if not ok then return { error = err, code = code } end
    return { session_id = ctx.session_id, origin = ctx.origin }
  end }
silo.action{ name = "grants", title = "Grants", run = function()
    local list, err, code = silo.call("grant.list", {})
    if list == nil then return { refused = code, message = err } end
    return { ok = true }
  end }
silo.action{ name = "where", title = "Where", run = function(ctx)
    return { session_id = ctx.session_id, window_id = ctx.window_id, block_id = ctx.block_id, client_id = ctx.client_id }
  end }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn actions_run_as_their_caller() {
    let home = temp_home("run");
    write_config(&home, ACTIONS);
    let task = start(&home, true).await;
    let (owner, _) = connect(&home, None).await;
    wait_state(&owner, "the load", |s| s.actions.len() == 3).await;
    let created = new_session(&owner, shell("sleep 60")).await;

    // The owner: the action's value comes back and its calls work.
    let params = ActionRunParams { session_id: Some(created.session_id.clone()), ..run("rename", json!({"name": "renamed"})) };
    let value = result(&owner, params).await;
    assert_eq!(value, json!({"session_id": created.session_id, "origin": "api"}));
    assert_eq!(session_name(&owner, &created.session_id).await.as_deref(), Some("renamed"));
    assert_eq!(result(&owner, run("grants", Value::Null)).await, json!({"ok": true}));
    assert_eq!(code(run_action(&owner, run("rename", json!({"name": 3}))).await), "bad_request", "args are checked");

    // The session and window default from the block; the client from the caller.
    let where_ = result(&owner, ActionRunParams { block_id: Some(created.block_id.clone()), ..run("where", Value::Null) }).await;
    assert_eq!(where_["session_id"], json!(created.session_id));
    assert_eq!(where_["window_id"], json!(created.window_id));
    assert_eq!(where_["block_id"], json!(created.block_id));
    assert_eq!(where_["client_id"], json!(owner.client_id()));

    // A session-scoped token and a server-scoped viewer may not run actions.
    let session_scope = Scope::Session { session_id: created.session_id.clone() };
    let (scoped, _) = connect(&home, Some(&token(&owner, session_scope, Rights::Control).await)).await;
    assert_eq!(code(run_action(&scoped, run("where", Value::Null)).await), RpcError::FORBIDDEN);
    assert_eq!(code(scoped.call_value(Method::ActionList {}).await), RpcError::FORBIDDEN);
    let (viewer, _) = connect(&home, Some(&token(&owner, Scope::Server, Rights::View).await)).await;
    assert_eq!(code(run_action(&viewer, run("where", Value::Null)).await), RpcError::FORBIDDEN);

    // A server controller runs them, and its calls have its rights: it may
    // rename, but not list grants (owner only), and hears the refusal.
    let (controller, _) = connect(&home, Some(&token(&owner, Scope::Server, Rights::Control).await)).await;
    let params = ActionRunParams { session_id: Some(created.session_id.clone()), ..run("rename", json!({"name": "by-token"})) };
    assert_eq!(result(&controller, params).await["session_id"], json!(created.session_id));
    assert_eq!(session_name(&owner, &created.session_id).await.as_deref(), Some("by-token"));
    let refused = result(&controller, run("grants", Value::Null)).await;
    assert_eq!(refused["refused"], json!(RpcError::FORBIDDEN), "{refused}");

    drop((scoped, viewer, controller));
    shutdown(owner, task, &home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn handlers_react_with_no_client_listening() {
    let home = temp_home("handlers");
    write_config(
        &home,
        r#"silo.on("block.exited", function(ctx, ev)
  silo.session.rename{ session_id = ctx.session_id, name = "exited-" .. tostring(ev.exit_code) }
end)
silo.on("block.cwd_changed", function(ctx, ev)
  silo.window.rename{ window_id = ctx.window_id, name = ev.cwd:match("([^/]+)/?$") }
end)
"#,
    );
    let task = start(&home, true).await;
    // Not subscribed: only the config's own connection hears events.
    let (owner, _) = connect(&home, None).await;
    let loaded = wait_state(&owner, "the load", |s| s.handlers.len() == 2).await;
    assert!(loaded.handlers.iter().any(|h| h.event == "block.exited"), "{loaded:?}");
    // Let the handlers' connection seed what it tracks.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let created = new_session(&owner, shell("sleep 1; exit 3")).await;
    wait_for("the handler to rename the session", async || {
        session_name(&owner, &created.session_id).await.is_some_and(|n| n.starts_with("exited-3"))
    })
    .await;
    shutdown(owner, task, &home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scripting_off_loads_nothing() {
    let home = temp_home("off");
    write_config(&home, "silo.action{ name = 'hello', title = 'Hello', run = function() return 1 end }\n");
    let task = start(&home, false).await;
    let (owner, _) = connect(&home, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(state(&owner).await, ConfigState::default());
    let reloaded: ConfigState = owner.call(Method::ConfigReload {}).await.unwrap();
    assert_eq!(reloaded, ConfigState::default());
    let list: ActionList = owner.call(Method::ActionList {}).await.unwrap();
    assert!(list.actions.is_empty());
    assert_eq!(code(run_action(&owner, run("hello", Value::Null)).await), "not_found");
    shutdown(owner, task, &home).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blocks_know_their_session() {
    let home = temp_home("env");
    let task = start(&home, true).await;
    let (owner, _) = connect(&home, None).await;
    // No file: an empty config, not an error.
    let empty = wait_state(&owner, "the empty load", |s| s.revision >= 1).await;
    assert!(!empty.exists && !empty.failed && empty.problems.is_empty(), "{empty:?}");
    let created = new_session(&owner, SpawnSpec { command: vec!["/bin/sh".into()], ..SpawnSpec::default() }).await;
    owner
        .call_value(Method::TerminalSend { block_id: created.block_id.clone(), text: "echo \"S=$SILO_SESSION B=$SILO_BLOCK\"\n".into() })
        .await
        .unwrap();
    let want = format!("S={} B={}", created.session_id, created.block_id);
    wait_for("SILO_SESSION in the shell", async || {
        let capture: Capture = owner.call(Method::TerminalCapture { block_id: created.block_id.clone(), history: true }).await.unwrap();
        capture.text.contains(&want)
    })
    .await;
    shutdown(owner, task, &home).await;
}
