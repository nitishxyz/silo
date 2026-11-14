//! The VM against a fake backend: loading, actions, calls, layouts,
//! events, timers, limits, commands and scripts.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use silo_lua::{Api, Backend, BoxFuture, Ctx, Exit, Invocation, Loaded, LogLevel, Mode, Options, Source, Vm, check};
use silo_proto::{
    config::{ActionOrigin, BindTarget, ProblemLevel},
    control::{Event, RpcError},
};
use tokio::sync::mpsc;

type Answer = Box<dyn FnMut(&str, &Value) -> Result<Value, RpcError> + Send>;

/// Records every call; answers with `answer` (else `{}`).
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, Value)>>,
    answer: Mutex<Option<Answer>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn answering(answer: impl FnMut(&str, &Value) -> Result<Value, RpcError> + Send + 'static) -> Arc<Self> {
        Arc::new(Self { calls: Mutex::default(), answer: Mutex::new(Some(Box::new(answer))) })
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn api(self: &Arc<Self>) -> Api {
        Api::new(self.clone())
    }

    /// The params of the first call to `method`, waiting up to 3 s.
    async fn wait_for(&self, method: &str) -> Value {
        let start = Instant::now();
        loop {
            if let Some((_, params)) = self.calls().into_iter().find(|(m, _)| m == method) {
                return params;
            }
            assert!(start.elapsed() < Duration::from_secs(3), "no {method} call: {:?}", self.calls());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Backend for Fake {
    fn call(&self, method: String, params: Value) -> BoxFuture<Result<Value, RpcError>> {
        self.calls.lock().unwrap().push((method.clone(), params.clone()));
        let answer = match self.answer.lock().unwrap().as_mut() {
            Some(answer) => answer(&method, &params),
            None => Ok(json!({})),
        };
        Box::pin(async move { answer })
    }
}

async fn config(code: &str) -> (Vm, Loaded) {
    Vm::load(Source::Text { code: code.into() }, Options::default()).await
}

fn call(api: Api, args: Value) -> Invocation {
    Invocation { ctx: Ctx { origin: Some(ActionOrigin::Cli), ..Ctx::default() }, args, api }
}

fn script_options() -> Options {
    Options { mode: Mode::Script, source_name: "test.lua".into(), ..Options::default() }
}

async fn script_with(code: &str, options: Options, api: Api, events: Option<silo_lua::Events>) -> Exit {
    let (vm, loaded) = Vm::load(Source::Text { code: code.into() }, options).await;
    assert!(!loaded.failed, "{:?}", loaded.problems);
    vm.attach(api, events);
    tokio::time::timeout(Duration::from_secs(10), vm.finished()).await.expect("the script ends")
}

async fn script(code: &str) -> Exit {
    script_with(code, script_options(), Fake::new().api(), None).await
}

fn keys(text: &str) -> String {
    silo_proto::keys::parse(text).unwrap().sequence.to_string()
}

#[tokio::test]
async fn a_config_declares_bindings_modes_unbinds_actions_and_handlers() {
    let (_vm, loaded) = config(
        r#"
silo.bind("cmd-e", "SplitRight")
silo.bind("cmd-shift-l", "dev_layout", { file = "app.log" })
silo.bind("ctrl-b c", "window.create")
silo.bind("ctrl-b v", function(ctx) silo.block.split{ axis = "horizontal" } end)
silo.unbind("cmd-k")
silo.unbind("cmd-k", "SwitchSession")
silo.mode("resize", { exclusive = true })
silo.bind("resize/left", "MovePaneEdgeLeft")
silo.on("block.cwd_changed", function() end)
silo.action{ name = "dev_layout", title = "Dev Layout", category = "Project", keywords = "start work",
  args = { file = "string", ratio = "number?" }, run = function() end }
"#,
    )
    .await;
    assert!(!loaded.failed);
    assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    let b = &loaded.bindings;
    assert_eq!(b.len(), 5);
    assert_eq!((b[0].keys.as_str(), &b[0].target), ("cmd-e", &BindTarget::Command { id: "SplitRight".into() }));
    assert_eq!(b[0].source, "init.lua:2");
    assert_eq!(b[1].keys, keys("cmd-shift-l"));
    assert_eq!(b[1].target, BindTarget::Action { name: "dev_layout".into() });
    assert_eq!(b[1].args, json!({"file": "app.log"}));
    assert_eq!(b[2].keys, "ctrl-b c", "a sequence");
    assert_eq!(b[2].target, BindTarget::Method { name: "window.create".into() });
    assert_eq!(b[2].args, json!({}));
    assert_eq!(b[3].target, BindTarget::Action { name: "bind_1".into() }, "a function is a hidden action");
    assert_eq!((b[4].mode.as_deref(), b[4].keys.as_str()), (Some("resize"), "left"));

    assert_eq!(loaded.unbinds.len(), 2);
    assert_eq!(loaded.unbinds[0].target, None);
    assert_eq!(loaded.unbinds[1].target.as_deref(), Some("SwitchSession"));
    assert_eq!(loaded.modes.len(), 1);
    assert!(loaded.modes[0].exclusive);

    let names: Vec<&str> = loaded.actions.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["bind_1", "dev_layout"]);
    assert!(loaded.actions[0].hidden);
    let dev = &loaded.actions[1];
    assert_eq!((dev.title.as_str(), dev.category.as_str()), ("Dev Layout", "Project"));
    assert_eq!(dev.keywords, ["start", "work"]);
    assert_eq!(dev.args["required"], json!(["file"]));
    assert_eq!(dev.source, "init.lua:11");
    assert_eq!(loaded.handlers.len(), 1);
    assert_eq!((loaded.handlers[0].event.as_str(), loaded.handlers[0].source.as_str()), ("block.cwd_changed", "init.lua:10"));
}

#[tokio::test]
async fn bad_declarations_are_problems_and_the_rest_loads() {
    let (_vm, loaded) = config(
        r#"
silo.bind("hyper-x", "SplitRight")
silo.bind("cmd-j", "dev-layout")
silo.action{ name = "nameless_title", run = function() end }
silo.action{ name = "bad_args", title = "B", args = { x = "strng" }, run = function() end }
silo.mode("Resize")
local view, err, code = silo.session.view{}
silo.bind("cmd-j", "SplitDown")
silo.action{ name = "twice", title = "One", run = function() end }
silo.action{ name = "twice", title = "Two", run = function() end }
"#,
    )
    .await;
    assert!(!loaded.failed);
    let problems: Vec<(ProblemLevel, &str)> = loaded.problems.iter().map(|p| (p.level, p.source.as_str())).collect();
    assert_eq!(
        problems,
        [
            (ProblemLevel::Error, "init.lua:2"),
            (ProblemLevel::Error, "init.lua:3"),
            (ProblemLevel::Error, "init.lua:4"),
            (ProblemLevel::Error, "init.lua:5"),
            (ProblemLevel::Error, "init.lua:6"),
            (ProblemLevel::Warning, "init.lua:7"),
            (ProblemLevel::Warning, "init.lua:10"),
        ],
        "{:#?}",
        loaded.problems
    );
    assert!(loaded.problems[2].message.contains("title"), "{:?}", loaded.problems[2]);
    assert_eq!(loaded.bindings.len(), 1);
    assert_eq!(loaded.bindings[0].target, BindTarget::Command { id: "SplitDown".into() });
    let twice = loaded.actions.iter().find(|a| a.name == "twice").unwrap();
    assert_eq!(twice.title, "Two", "the later definition wins");
}

#[tokio::test]
async fn a_syntax_or_runtime_error_fails_the_load() {
    let (_vm, loaded) = config("silo.bind('cmd-e', 'SplitRight')\nlocal x = = 1\n").await;
    assert!(loaded.failed);
    assert_eq!(loaded.problems.len(), 1);
    assert_eq!(loaded.problems[0].source, "init.lua:2", "{:?}", loaded.problems);

    let (_vm, loaded) = config("silo.bind('cmd-e', 'SplitRight')\n\nerror('boom')\n").await;
    assert!(loaded.failed);
    let last = loaded.problems.last().unwrap();
    assert_eq!((last.level, last.source.as_str()), (ProblemLevel::Error, "init.lua:3"));
    assert!(last.message.contains("boom"));
}

#[tokio::test]
async fn require_loads_beside_the_config_and_the_files_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let init = dir.path().join("init.lua");
    std::fs::write(&init, "require('keys')\nrequire('plugin')\n").unwrap();
    std::fs::write(dir.path().join("keys.lua"), "silo.bind('cmd-k', 'SwitchSession')\n").unwrap();
    std::fs::create_dir(dir.path().join("plugin")).unwrap();
    std::fs::write(dir.path().join("plugin/init.lua"), "\nsilo.mode('plug')\n").unwrap();
    let (vm, loaded) = Vm::load(Source::File(init.clone()), Options::config(&init)).await;
    assert!(!loaded.failed, "{:?}", loaded.problems);
    assert_eq!(loaded.bindings[0].source, "keys.lua:1");
    assert_eq!(loaded.modes[0].source, "plugin/init.lua:2");
    assert_eq!(loaded.files, [init.clone(), dir.path().join("keys.lua"), dir.path().join("plugin/init.lua")]);
    assert_eq!(vm.files(), loaded.files);

    let checked = check(&init);
    assert_eq!(checked.bindings, loaded.bindings);
    assert!(!checked.failed);
    let missing = check(&dir.path().join("nope.lua"));
    assert!(missing.failed);
}

#[tokio::test]
async fn removed_stdlib_functions_are_gone() {
    let (_vm, loaded) = config(
        "assert(io.popen == nil and os.execute == nil and os.exit == nil and package.loadlib == nil)\n\
         assert(package.cpath == '' and io.open ~= nil and os.time ~= nil)\n",
    )
    .await;
    assert!(!loaded.failed, "{:?}", loaded.problems);
}

#[tokio::test]
async fn actions_check_their_args_and_return_json() {
    let (vm, _) = config(
        r#"
silo.action{ name = "echo", title = "Echo", args = { file = "string", n = "integer?" }, run = function(ctx, a) return { a = a, origin = ctx.origin } end }
silo.action{ name = "decline", title = "No", run = function() return false end }
silo.action{ name = "nothing", title = "Nothing", run = function() end }
silo.action{ name = "fails", title = "Fails", run = function() error("nope", 0) end }
"#,
    )
    .await;
    let api = Fake::new().api();
    let ok = vm.run_action("echo", call(api.clone(), json!({"file": "a", "n": 2}))).await.unwrap();
    assert_eq!(ok, json!({"a": {"file": "a", "n": 2}, "origin": "cli"}));
    for bad in [json!({}), json!({"file": 1}), json!({"file": "a", "n": 1.5})] {
        let error = vm.run_action("echo", call(api.clone(), bad.clone())).await.unwrap_err();
        assert_eq!(error.code, "bad_request", "{bad}: {error:?}");
    }
    assert_eq!(vm.run_action("decline", call(api.clone(), Value::Null)).await.unwrap(), json!(false));
    assert_eq!(vm.run_action("nothing", call(api.clone(), Value::Null)).await.unwrap(), Value::Null);
    let failed = vm.run_action("fails", call(api.clone(), Value::Null)).await.unwrap_err();
    assert_eq!((failed.code.as_str(), failed.message.as_str()), (silo_lua::ACTION_FAILED, "nope"));
    assert_eq!(vm.run_action("missing", call(api, Value::Null)).await.unwrap_err().code, "not_found");
}

#[tokio::test]
async fn calls_fill_ids_from_the_context() {
    let (vm, _) = config(
        r#"
silo.action{ name = "go", title = "Go", run = function(ctx)
  silo.block.zoom{ zoomed = true }
  silo.session.view{}
  silo.window.rename{ name = "x" }
  silo.call("block.zoom", { zoomed = true })
  silo.app.palette{}
  silo.foo.bar{ a = 1 }
  silo.block.focus{ block_id = "b9" }
  local r, err, code = silo.block.view{}
  return { r = r, err = err, code = code }
end }
"#,
    )
    .await;
    let fake = Fake::answering(|method, _| match method {
        "block.view" => Err(RpcError::not_found("block b1")),
        _ => Ok(json!({})),
    });
    let ctx = Ctx {
        session_id: Some("s1".into()),
        window_id: Some("w1".into()),
        block_id: Some("b1".into()),
        client_id: Some(7),
        origin: Some(ActionOrigin::Key),
    };
    let result = vm.run_action("go", Invocation { ctx, args: Value::Null, api: fake.api() }).await.unwrap();
    assert_eq!(result, json!({"err": "block b1 not found", "code": "not_found"}), "failures are nil, message, code");
    assert_eq!(
        fake.calls(),
        [
            ("block.zoom".into(), json!({"zoomed": true, "block_id": "b1"})),
            ("session.view".into(), json!({"session_id": "s1"})),
            ("window.rename".into(), json!({"name": "x", "window_id": "w1"})),
            ("block.zoom".into(), json!({"zoomed": true})),
            ("app.palette".into(), json!({"client_id": 7})),
            ("foo.bar".into(), json!({"a": 1})),
            ("block.focus".into(), json!({"block_id": "b9"})),
            ("block.view".into(), json!({"block_id": "b1"})),
        ]
    );
}

#[tokio::test]
async fn invoke_runs_another_action_until_too_deep() {
    let (vm, _) = config(
        r#"
silo.action{ name = "inner", title = "I", args = { x = "integer" }, run = function(ctx, a) return { x = a.x * 2, session = ctx.session_id } end }
silo.action{ name = "outer", title = "O", run = function() return silo.invoke("inner", { x = 21 }) end }
silo.action{ name = "rec", title = "R", run = function(ctx, a)
  local n = (a.n or 0) + 1
  local r, err, code = silo.invoke("rec", { n = n })
  if r == nil then return { depth = n, code = code } end
  return r
end }
"#,
    )
    .await;
    let api = Fake::new().api();
    let ctx = Ctx { session_id: Some("s1".into()), ..Ctx::default() };
    let outer = vm.run_action("outer", Invocation { ctx, args: Value::Null, api: api.clone() }).await.unwrap();
    assert_eq!(outer, json!({"x": 42, "session": "s1"}));
    let deep = vm.run_action("rec", call(api, Value::Null)).await.unwrap();
    assert_eq!(deep, json!({"depth": 16, "code": "too_deep"}));
}

#[tokio::test]
async fn a_busy_loop_or_a_long_wait_is_stopped_at_the_deadline() {
    let (vm, _) = config(
        r#"
silo.action{ name = "spin", title = "Spin", timeout = 0.3, run = function() while true do end end }
silo.action{ name = "caught", title = "Caught", timeout = 0.3, run = function() while true do pcall(function() while true do end end) end end }
silo.action{ name = "sleepy", title = "Sleepy", timeout = 0.3, run = function() silo.sleep(30) end }
silo.action{ name = "fine", title = "Fine", run = function() return 1 end }
"#,
    )
    .await;
    let api = Fake::new().api();
    for name in ["spin", "caught", "sleepy"] {
        let start = Instant::now();
        let error = vm.run_action(name, call(api.clone(), Value::Null)).await.unwrap_err();
        assert_eq!(error.code, RpcError::TIMEOUT, "{name}: {error:?}");
        assert!(start.elapsed() < Duration::from_secs(5), "{name}");
    }
    assert_eq!(vm.run_action("fine", call(api, Value::Null)).await.unwrap(), json!(1), "the VM goes on");
}

#[tokio::test]
async fn actions_interleave_at_waits() {
    let (vm, _) = config(
        r#"
silo.action{ name = "slow", title = "S", run = function() silo.sleep(0.5) return "slow" end }
silo.action{ name = "quick", title = "Q", run = function() return "quick" end }
"#,
    )
    .await;
    let api = Fake::new().api();
    let slow = {
        let (vm, api) = (vm.clone(), api.clone());
        tokio::spawn(async move { vm.run_action("slow", call(api, Value::Null)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(vm.running_actions(), 1);
    let start = Instant::now();
    assert_eq!(vm.run_action("quick", call(api, Value::Null)).await.unwrap(), json!("quick"));
    assert!(start.elapsed() < Duration::from_millis(400));
    assert_eq!(slow.await.unwrap().unwrap(), json!("slow"));
    vm.idle().await;
}

#[tokio::test]
async fn exec_runs_commands_with_options() {
    let exit = script(
        r#"
local r = silo.exec({ "sh", "-c", "echo hi; echo err >&2; exit 3" })
local c = silo.exec({ "cat" }, { stdin = "abc" })
local t = silo.exec({ "sleep", "5" }, { timeout = 0.2 })
local e = silo.exec({ "sh", "-c", "echo $SILO_LUA_X" }, { env = { SILO_LUA_X = "y" } })
local d = silo.exec({ "pwd" }, { cwd = "/" })
local missing, err, code = silo.exec({ "/nonexistent/program" })
local empty, err2 = silo.exec({})
return { r.code, r.stdout, r.stderr, r.ok, c.stdout, t.timed_out, t.ok, e.stdout, d.stdout, missing == nil, code, empty == nil }
"#,
    )
    .await;
    assert_eq!(exit.error, None);
    assert_eq!(exit.value, json!([3, "hi\n", "err\n", false, "abc", true, false, "y\n", "/\n", true, "exec_failed", true]));
}

#[tokio::test]
async fn a_command_is_killed_when_its_action_ends() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let (vm, _) = config(&format!(
        r#"silo.action{{ name = "run", title = "R", timeout = 0.5, run = function()
  silo.exec({{ "sh", "-c", "echo $$ > {}; sleep 30" }})
end }}"#,
        pidfile.display()
    ))
    .await;
    let error = vm.run_action("run", call(Fake::new().api(), Value::Null)).await.unwrap_err();
    assert_eq!(error.code, RpcError::TIMEOUT);
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_owned();
    let start = Instant::now();
    loop {
        let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid]).output().unwrap();
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if stat.is_empty() || stat.starts_with('Z') {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(3), "{pid} still runs ({stat})");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// window.create gives `b1` in `w1`; each split the next block; the
/// `fail_split`th split (from 1) fails.
fn layout_fake(fail_split: usize) -> Arc<Fake> {
    let mut splits = 0;
    Fake::answering(move |method, params| match method {
        "window.create" => Ok(json!({"session_id": "s1", "window_id": "w1", "block_id": "b1"})),
        "block.split" => {
            splits += 1;
            if splits == fail_split {
                return Err(RpcError::bad_request("no room"));
            }
            let window = if params["block_id"] == "b0" { "w0" } else { "w1" };
            Ok(json!({"session_id": "s1", "window_id": window, "block_id": format!("b{}", splits + 1)}))
        }
        "session.view" => Ok(json!({"session": {"id": "s1", "name": "work", "active_window": "w0", "revision": 1, "created_at": 0,
            "windows": [{"id": "w0", "focused": "b0", "floats": [], "layout": {"type": "split", "axis": "horizontal", "ratio": 0.5,
                "first": {"type": "block", "id": "b0"}, "second": {"type": "block", "id": "b2"}}}]}, "blocks": []})),
        _ => Ok(json!({})),
    })
}

const OPEN: &str = r#"
silo.action{ name = "dev", title = "Dev", run = function(ctx, a)
  local w, err, code = silo.open{ name = a.name, beside = a.beside, layout = silo.layout.row(0.6,
    silo.layout.block{ command = { "nvim", "app.log" } },
    silo.layout.column(0.5,
      silo.layout.block{ command = { "npm", "run", "dev" }, cwd = "~/src" },
      silo.layout.block{ kind = "browser", url = "http://localhost:3000" })) }
  if not w then return { err = err, code = code } end
  return w
end }
"#;

#[tokio::test]
async fn open_builds_a_layout_in_a_new_tab() {
    let (vm, _) = config(OPEN).await;
    let fake = layout_fake(0);
    let ctx = Ctx { session_id: Some("s1".into()), ..Ctx::default() };
    let opened = vm.run_action("dev", Invocation { ctx, args: json!({"name": "dev"}), api: fake.api() }).await.unwrap();
    assert_eq!(opened, json!({"window_id": "w1", "block_ids": ["b1", "b2", "b3"]}));
    assert_eq!(
        fake.calls(),
        [
            ("window.create".into(), json!({"session_id": "s1", "kind": "terminal", "spawn": {"command": ["nvim", "app.log"]}})),
            (
                "block.split".into(),
                json!({"block_id": "b1", "axis": "horizontal", "kind": "terminal", "spawn": {"command": ["npm", "run", "dev"], "cwd": "~/src"}})
            ),
            ("layout.ratio".into(), json!({"window_id": "w1", "path": [], "ratio": 0.6})),
            ("block.split".into(), json!({"block_id": "b2", "axis": "vertical", "kind": "browser", "spawn": {}, "url": "http://localhost:3000"})),
            ("layout.ratio".into(), json!({"window_id": "w1", "path": [1], "ratio": 0.5})),
            ("window.rename".into(), json!({"window_id": "w1", "name": "dev"})),
        ]
    );
}

#[tokio::test]
async fn open_closes_what_it_made_when_a_step_fails() {
    let (vm, _) = config(OPEN).await;
    let fake = layout_fake(2);
    let ctx = Ctx { session_id: Some("s1".into()), ..Ctx::default() };
    let result = vm.run_action("dev", Invocation { ctx, args: json!({}), api: fake.api() }).await.unwrap();
    assert_eq!(result, json!({"err": "no room", "code": "bad_request"}));
    let last = fake.calls().last().cloned().unwrap();
    assert_eq!(last, ("window.close".into(), json!({"window_id": "w1", "force": true})));

    // Beside a block: what was split off is closed one by one.
    let fake = layout_fake(3);
    let result = vm.run_action("dev", call(fake.api(), json!({"beside": "b0"}))).await.unwrap();
    assert_eq!(result["code"], "bad_request");
    let calls = fake.calls();
    let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(methods, ["block.split", "session.view", "block.split", "layout.ratio", "block.split", "block.close", "block.close"]);
    assert_eq!(calls[3].1, json!({"window_id": "w0", "path": [1], "ratio": 0.6}), "paths start where the first block landed");
    assert_eq!(calls[5].1, json!({"block_id": "b3", "force": true}));
    assert_eq!(calls[6].1, json!({"block_id": "b2", "force": true}));

    let (vm, _) = config(
        "silo.action{ name = 'x', title = 'X', run = function() local w, err, code = silo.open{ layout = silo.layout.block{} } return { w == nil, code } end }",
    )
    .await;
    let no_session = vm.run_action("x", call(Fake::new().api(), Value::Null)).await.unwrap();
    assert_eq!(no_session, json!([true, "bad_request"]));
}

#[tokio::test]
async fn scripts_return_values_and_exit_codes() {
    let exit = script("return { a = 1, list = { 'x' } }").await;
    assert_eq!((exit.code, exit.value), (0, json!({"a": 1, "list": ["x"]})));
    let exit = script("error('bad', 0)").await;
    assert_eq!((exit.code, exit.error.as_deref()), (1, Some("bad")));
    let exit = script("silo.stop(4) return 1").await;
    assert_eq!((exit.code, exit.value), (4, Value::Null));
    let exit = script("silo.after(0.05, function() silo.stop(3) end) return 'later'").await;
    assert_eq!(exit.code, 3, "a pending timer keeps it alive");
    let exit = script("return function() end").await;
    assert_eq!(exit.code, 1);

    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    let options = Options {
        args: json!({"name": "web"}),
        session_id: Some("s1".into()),
        log: Some(Arc::new(move |level: LogLevel, text: &str| sink.lock().unwrap().push((level, text.to_owned())))),
        ..script_options()
    };
    let exit = script_with(
        "silo.bind('cmd-e', 'SplitRight')\nsilo.log('warn', 'hey')\nreturn { silo.args.name, silo.session_id }",
        options,
        Fake::new().api(),
        None,
    )
    .await;
    assert_eq!(exit.value, json!(["web", "s1"]));
    let logs = logs.lock().unwrap().clone();
    assert!(logs.iter().any(|(l, t)| *l == LogLevel::Warn && t.contains("silo.bind does nothing in a script")), "{logs:?}");
    assert!(logs.contains(&(LogLevel::Warn, "hey".into())));

    let (vm, loaded) = Vm::load(Source::Text { code: "return (".into() }, script_options()).await;
    assert!(loaded.failed);
    assert_eq!(loaded.problems[0].source, "test.lua:1");
    vm.attach(Fake::new().api(), None);
    assert_eq!(vm.finished().await.code, 1);
}

#[tokio::test]
async fn a_failing_timer_ends_a_fail_fast_script() {
    let options = Options { fail_fast: true, ..script_options() };
    let exit = script_with("silo.after(0.01, function() error('tick', 0) end)", options, Fake::new().api(), None).await;
    assert_eq!(exit.code, 1);
    assert!(exit.error.unwrap().contains("tick"));
    let exit = script("silo.after(0.01, function() error('tick', 0) end) return 2").await;
    assert_eq!((exit.code, exit.value), (0, json!(2)), "without fail-fast the error is reported");
}

#[tokio::test]
async fn timers_tick_cancel_and_count() {
    let exit = script(
        r#"
local n = 0
local t
t = silo.every(0.02, function() n = n + 1 if n == 3 then t:cancel() end end)
local once = silo.after(0.01, function() end)
silo.sleep(0.3)
return { n = n, active = t:active(), once = once:active() }
"#,
    )
    .await;
    assert_eq!(exit.value, json!({"n": 3, "active": false, "once": false}));

    // A config's timers start when it is attached.
    let (vm, loaded) = config("silo.every(0.01, function(ctx) silo.call('tick', { origin = ctx.origin }) end)").await;
    assert_eq!(loaded.timers, 1);
    let fake = Fake::new();
    tokio::time::sleep(Duration::from_millis(80)).await;
    vm.attach(fake.api(), None);
    assert!(fake.calls().is_empty() || fake.calls().iter().all(|(m, _)| m == "tick"));
    assert_eq!(fake.wait_for("tick").await, json!({"origin": "timer"}));
    vm.stop();
    assert_eq!(vm.finished().await.code, 0);
}

fn block(id: &str, cwd: &str) -> Value {
    json!({"id": id, "session_id": "s1", "kind": "terminal", "title": "zsh", "cwd": cwd, "lifecycle": "running",
        "size": {"columns": 80, "rows": 24}, "owner_name": null})
}

fn view(blocks: &[Value]) -> Value {
    json!({"session": {"id": "s1", "name": "work", "active_window": "w1", "revision": 1, "created_at": 0,
        "windows": [{"id": "w1", "layout": {"type": "block", "id": blocks[0]["id"]}, "focused": blocks[0]["id"], "floats": []}]},
        "blocks": blocks})
}

fn event(name: &str, data: Value) -> (u64, Event) {
    (0, serde_json::from_value(json!({"event": name, "data": data})).unwrap())
}

#[tokio::test]
async fn handlers_hear_derived_events_with_their_context() {
    let (vm, loaded) = config(
        r#"
silo.on("block.cwd_changed", function(ctx, ev)
  silo.call("seen", { cwd = ev.cwd, block = ctx.block_id, window = ctx.window_id, session = ctx.session_id, origin = ctx.origin })
end)
silo.on("block.bell", function(ctx, ev) silo.call("bell", { block = ev.block_id }) end)
"#,
    )
    .await;
    assert_eq!(loaded.handlers.len(), 2);
    let fake = Fake::answering(|method, _| match method {
        "session.list" => Ok(json!([{"id": "s1", "name": "work", "windows": 1, "blocks": 1, "attached_clients": 0, "created_at": 0}])),
        "session.view" => Ok(view(&[block("b1", "/a")])),
        _ => Ok(json!({})),
    });
    let (tx, rx) = mpsc::unbounded_channel();
    vm.attach(fake.api(), Some(rx));
    fake.wait_for("session.view").await;
    tx.send(event("block.changed", json!({"block": block("b1", "/a")}))).unwrap();
    tx.send(event("block.changed", json!({"block": block("b1", "/b")}))).unwrap();
    tx.send(event("block.bell", json!({"block_id": "b1"}))).unwrap();
    assert_eq!(fake.wait_for("seen").await, json!({"cwd": "/b", "block": "b1", "window": "w1", "session": "s1", "origin": "event"}));
    assert_eq!(fake.wait_for("bell").await, json!({"block": "b1"}));
    let seen = fake.calls().iter().filter(|(m, _)| m == "seen").count();
    assert_eq!(seen, 1, "the same cwd is not a change");
}

#[tokio::test]
async fn a_script_hears_only_its_session_and_waits_with_filters() {
    let (tx, rx) = mpsc::unbounded_channel();
    let sender = tokio::spawn(async move {
        loop {
            for (block, session) in [("b1", "s1"), ("b9", "s9"), ("b2", "s1")] {
                let data = json!({"block_id": block, "session_id": session, "session_name": "w", "block_title": "t", "reason": "done", "at": 0});
                if tx.send(event("block.attention", data)).is_err() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let options = Options { session_id: Some("s1".into()), ..script_options() };
    let exit = script_with(
        r#"
local ev, ctx = silo.wait("block.attention", { block_id = "b2" }, 5)
local other = silo.wait("block.attention", function(ev) return ev.session_id == "s9" end, 0.2)
local none, err = silo.wait("never.sent", 0.05)
return { ctx.block_id, ev.reason, other == nil, err }
"#,
        options,
        Fake::new().api(),
        Some(rx),
    )
    .await;
    sender.abort();
    assert_eq!(exit.error, None);
    assert_eq!(exit.value, json!(["b2", "done", true, "timeout"]));
}
