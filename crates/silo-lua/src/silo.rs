//! The global `silo` table (`docs/LUA.md` §4). A call that fails returns
//! `nil, message, code` and never raises.

use std::{collections::HashMap, rc::Rc, time::Duration};

use mlua::{FromLua, Function, IntoLuaMulti, Lua, LuaSerdeExt, MultiValue, Table, UserData, UserDataMethods, Value as LuaValue, Variadic};
use serde_json::{Value, json};
use silo_proto::{
    config::{ActionInfo, ActionOrigin, BindTarget, HandlerInfo, KeyBinding, KeyMode, KeyUnbind, ProblemLevel},
    control::RpcError,
};

use crate::{
    ACTION_FAILED, LogLevel, Mode, NO_CALLER, args,
    convert::{from_lua, to_lua},
    exec::{ExecOptions, ExecResult},
    layout::{self, OpenSpec},
    limits,
    vm::{CURRENT, Current, Filter, State, TimerShared, Waiter, ctx_table, current, message, start_timer, state},
};

/// `nil, message, code`.
fn fail(lua: &Lua, message: impl Into<String>, code: &str) -> mlua::Result<MultiValue> {
    (LuaValue::Nil, message.into(), code.to_owned()).into_lua_multi(lua)
}

/// A call's result: its value (`true` for none), or `nil, message, code`.
fn answer(lua: &Lua, result: Result<Value, RpcError>) -> mlua::Result<MultiValue> {
    match result {
        Ok(Value::Null) => true.into_lua_multi(lua),
        Ok(value) => to_lua(lua, &value)?.into_lua_multi(lua),
        Err(e) => fail(lua, e.message, &e.code),
    }
}

/// Where the Lua code calling into Rust is: `file:line`.
pub(crate) fn caller_source(lua: &Lua) -> String {
    for level in 0..12 {
        let found = lua.inspect_stack(level, |d| {
            let source = d.source();
            // Skip Rust and mlua's own wrapper around async functions.
            let internal = source.what == "C" || source.source.as_deref().is_some_and(|s| s.starts_with("=__mlua"));
            (!internal).then(|| format!("{}:{}", source.short_src.as_deref().unwrap_or("?"), d.current_line().unwrap_or(0)))
        });
        match found {
            None => break,
            Some(None) => {}
            Some(Some(source)) => return source,
        }
    }
    String::new()
}

/// A value as `silo.log` and `print` show it.
pub(crate) fn show(lua: &Lua, value: &LuaValue) -> String {
    match value {
        LuaValue::Nil => "nil".into(),
        LuaValue::Boolean(b) => b.to_string(),
        LuaValue::Integer(i) => i.to_string(),
        LuaValue::Number(n) => n.to_string(),
        LuaValue::String(s) => s.to_string_lossy(),
        LuaValue::Table(_) => from_lua(lua, value).map_or_else(|_| "table".into(), |v| v.to_string()),
        other => other.type_name().to_owned(),
    }
}

/// The running invocation, for a call into Silo; at config load (or with
/// no backend) `Err(nil, message, code)` parts, with a warning kept.
fn caller(lua: &Lua, what: &str) -> Result<Rc<Current>, (String, &'static str)> {
    if let Ok(state) = state(lua)
        && state.loading.get()
    {
        let source = caller_source(lua);
        let message = format!("{what}: calls into Silo do not run while the config loads");
        state.report(ProblemLevel::Warning, &source, message.clone());
        return Err((message, NO_CALLER));
    }
    match current() {
        Some(c) if c.api.is_connected() => Ok(c),
        _ => Err((format!("{what}: nothing to call Silo with here"), NO_CALLER)),
    }
}

fn params_json(lua: &Lua, params: Option<LuaValue>) -> Result<Value, String> {
    match params {
        None | Some(LuaValue::Nil) => Ok(json!({})),
        Some(value) => from_lua(lua, &value).map_err(|e| format!("params: {e}")),
    }
}

/// `silo.<group>.<name>(params)`: `method` with ids filled from `ctx`.
fn method_fn(lua: &Lua, method: String) -> mlua::Result<Function> {
    lua.create_async_function(move |lua, params: Option<LuaValue>| {
        let method = method.clone();
        let caller = caller(&lua, &method);
        async move {
            let cur = match caller {
                Ok(cur) => cur,
                Err((message, code)) => return fail(&lua, message, code),
            };
            let params = match params_json(&lua, params) {
                Ok(p) => p,
                Err(e) => return fail(&lua, format!("{method}: {e}"), "bad_request"),
            };
            let params = cur.api.fill(&method, params, &cur.ctx);
            answer(&lua, cur.api.call(&method, params).await)
        }
    })
}

/// A metatable whose `__index` makes `group.<name>` method functions.
fn group_meta(lua: &Lua, group: &str) -> mlua::Result<Table> {
    let meta = lua.create_table()?;
    let group = group.to_owned();
    meta.set(
        "__index",
        lua.create_function(move |lua, (table, key): (Table, LuaValue)| {
            let LuaValue::String(key) = key else { return Ok(LuaValue::Nil) };
            let method = format!("{group}.{}", key.to_str()?);
            let f = method_fn(lua, method)?;
            table.raw_set(key, f.clone())?;
            Ok(LuaValue::Function(f))
        })?,
    )?;
    Ok(meta)
}

fn group(lua: &Lua, name: &str) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set_metatable(Some(group_meta(lua, name)?))?;
    Ok(table)
}

/// `[a-z][a-z0-9_]*`: action and mode names.
fn valid_name(name: &str) -> bool {
    silo_proto::keys::valid_mode(name)
}

/// What a binding's target means by its spelling (`docs/LUA.md` §4.1).
///
/// # Errors
/// Neither a method, a command id nor an action name.
pub(crate) fn classify(target: &str) -> Result<BindTarget, String> {
    if target.contains('.') {
        return Ok(BindTarget::Method { name: target.to_owned() });
    }
    if target.starts_with(|c: char| c.is_ascii_uppercase()) && target.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Ok(BindTarget::Command { id: target.to_owned() });
    }
    if valid_name(target) {
        return Ok(BindTarget::Action { name: target.to_owned() });
    }
    Err(format!("“{target}” is not a command id, a method or an action name"))
}

/// Report a bad declaration; it is skipped.
fn bad(state: &State, source: &str, message: String) -> mlua::Result<()> {
    state.report(ProblemLevel::Error, source, message);
    Ok(())
}

/// Whether `bind`/`unbind`/`mode` count here: only while a config loads.
fn declaring(state: &State, what: &str, source: &str) -> bool {
    if state.options.mode == Mode::Script {
        state.report(ProblemLevel::Warning, source, format!("{what} does nothing in a script"));
        return false;
    }
    if !state.loading.get() {
        state.report(ProblemLevel::Warning, source, format!("{what} only counts while the config loads"));
        return false;
    }
    true
}

fn keys_of(state: &State, what: &str, source: &str, keys: &LuaValue) -> Option<silo_proto::keys::Keys> {
    let LuaValue::String(text) = keys else {
        state.report(ProblemLevel::Error, source, format!("{what}: keys must be a string"));
        return None;
    };
    let text = text.to_string_lossy();
    match silo_proto::keys::parse(&text) {
        Ok(keys) => Some(keys),
        Err(e) => {
            state.report(ProblemLevel::Error, source, format!("{what}(\"{text}\"): {e}"));
            None
        }
    }
}

fn bind(lua: &Lua, (keys, target, bind_args): (LuaValue, LuaValue, LuaValue)) -> mlua::Result<()> {
    let state = state(lua)?;
    let source = caller_source(lua);
    if !declaring(&state, "silo.bind", &source) {
        return Ok(());
    }
    let Some(keys) = keys_of(&state, "silo.bind", &source, &keys) else { return Ok(()) };
    let bind_args = match &bind_args {
        LuaValue::Nil => json!({}),
        LuaValue::Table(_) => match from_lua(lua, &bind_args) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) if matches!(&bind_args, LuaValue::Table(t) if t.raw_len() == 0) => json!({}),
            Ok(_) => return bad(&state, &source, "silo.bind: args must be a table of named values".into()),
            Err(e) => return bad(&state, &source, format!("silo.bind: args: {e}")),
        },
        _ => return bad(&state, &source, "silo.bind: args must be a table".into()),
    };
    let target = match target {
        LuaValue::String(s) => match classify(&s.to_string_lossy()) {
            Ok(t) => t,
            Err(e) => return bad(&state, &source, format!("silo.bind: {e}")),
        },
        LuaValue::Function(run) => {
            let name = {
                let mut decl = state.decl.borrow_mut();
                decl.bind_fns += 1;
                format!("bind_{}", decl.bind_fns)
            };
            let info = ActionInfo {
                name: name.clone(),
                title: keys.to_string(),
                description: String::new(),
                category: String::new(),
                keywords: Vec::new(),
                args: json!({}),
                repeats: false,
                hidden: true,
                source: source.clone(),
            };
            define(&state, crate::vm::Action { info, run, schema: json!({}), timeout: limits::ACTION });
            BindTarget::Action { name }
        }
        _ => return bad(&state, &source, "silo.bind: the target is a command id, method, action name or function".into()),
    };
    state.decl.borrow_mut().bindings.push(KeyBinding { keys: keys.sequence.to_string(), mode: keys.mode, target, args: bind_args, source });
    Ok(())
}

fn unbind(lua: &Lua, (keys, target): (LuaValue, LuaValue)) -> mlua::Result<()> {
    let state = state(lua)?;
    let source = caller_source(lua);
    if !declaring(&state, "silo.unbind", &source) {
        return Ok(());
    }
    let Some(keys) = keys_of(&state, "silo.unbind", &source, &keys) else { return Ok(()) };
    let target = match target {
        LuaValue::Nil => None,
        LuaValue::String(s) => Some(s.to_string_lossy()),
        _ => return bad(&state, &source, "silo.unbind: the target is a command id, method or action name".into()),
    };
    state.decl.borrow_mut().unbinds.push(KeyUnbind { keys: keys.sequence.to_string(), mode: keys.mode, target, source });
    Ok(())
}

fn mode(lua: &Lua, (name, opts): (LuaValue, Option<Table>)) -> mlua::Result<()> {
    let state = state(lua)?;
    let source = caller_source(lua);
    if !declaring(&state, "silo.mode", &source) {
        return Ok(());
    }
    let name = match name {
        LuaValue::String(s) if valid_name(&s.to_string_lossy()) => s.to_string_lossy(),
        _ => return bad(&state, &source, "silo.mode: a mode name is [a-z][a-z0-9_]*".into()),
    };
    let exclusive = match opts.map(|o| o.get::<Option<bool>>("exclusive")).transpose() {
        Ok(e) => e.flatten().unwrap_or(false),
        Err(_) => return bad(&state, &source, "silo.mode: exclusive is true or false".into()),
    };
    let mode = KeyMode { name: name.clone(), exclusive, source: source.clone() };
    let mut decl = state.decl.borrow_mut();
    if let Some(old) = decl.modes.iter_mut().find(|m| m.name == name) {
        let first = std::mem::replace(old, mode).source;
        drop(decl);
        state.report(ProblemLevel::Warning, &source, format!("silo.mode: {name} is declared again (first at {first}); the later wins"));
    } else {
        decl.modes.push(mode);
    }
    Ok(())
}

/// Add or replace an action (a replacement keeps the first's place).
fn define(state: &State, action: crate::vm::Action) {
    let name = action.info.name.clone();
    let source = action.info.source.clone();
    let earlier = state.actions.borrow_mut().insert(name.clone(), action);
    match earlier {
        Some(old) => state.report(
            ProblemLevel::Warning,
            &source,
            format!("silo.action: {name} is defined again (first at {}); the later wins", old.info.source),
        ),
        None => state.decl.borrow_mut().action_order.push(name),
    }
}

fn field<T: FromLua>(lua: &Lua, spec: &Table, key: &str) -> Result<Option<T>, String> {
    let value: LuaValue = spec.get(key).map_err(|e| e.to_string())?;
    if value.is_nil() {
        return Ok(None);
    }
    let type_name = value.type_name();
    T::from_lua(value, lua).map(Some).map_err(|_| format!("{key} has the wrong type ({type_name})"))
}

fn action(lua: &Lua, (_, spec): (LuaValue, LuaValue)) -> mlua::Result<bool> {
    let state = state(lua)?;
    let source = caller_source(lua);
    let report = |message: String| {
        state.report(ProblemLevel::Error, &source, format!("silo.action: {message}"));
        Ok(false)
    };
    let LuaValue::Table(spec) = spec else { return report("takes a table: silo.action{ name, title, run }".into()) };
    let parsed = (|| -> Result<crate::vm::Action, String> {
        let name: String = field(lua, &spec, "name")?.ok_or("name is required")?;
        if !valid_name(&name) {
            return Err(format!("name “{name}” is not [a-z][a-z0-9_]*"));
        }
        let title: String = field(lua, &spec, "title")?.ok_or_else(|| format!("{name}: title is required"))?;
        let run: Function = field(lua, &spec, "run")?.ok_or_else(|| format!("{name}: run is required"))?;
        let keywords = match spec.get::<LuaValue>("keywords").map_err(|e| e.to_string())? {
            LuaValue::Nil => Vec::new(),
            LuaValue::String(s) => s.to_string_lossy().split_whitespace().map(str::to_owned).collect(),
            LuaValue::Table(t) => {
                t.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>().map_err(|_| format!("{name}: keywords are strings"))?
            }
            _ => return Err(format!("{name}: keywords are a string or a list")),
        };
        let schema = match spec.get::<LuaValue>("args").map_err(|e| e.to_string())? {
            LuaValue::Nil => json!({}),
            value @ LuaValue::Table(_) => {
                let spec = from_lua(lua, &value).map_err(|e| format!("{name}: args: {e}"))?;
                let spec = if spec.as_array().is_some_and(Vec::is_empty) { json!({}) } else { spec };
                args::schema(&spec).map_err(|e| format!("{name}: {e}"))?
            }
            _ => return Err(format!("{name}: args is a table")),
        };
        let timeout = match field::<f64>(lua, &spec, "timeout")? {
            None => limits::ACTION,
            Some(s) if s > 0.0 && s <= limits::ACTION_MAX.as_secs_f64() => Duration::from_secs_f64(s),
            Some(s) => return Err(format!("{name}: timeout {s} is not between 0 and {}", limits::ACTION_MAX.as_secs())),
        };
        let info = ActionInfo {
            name,
            title,
            description: field(lua, &spec, "description")?.unwrap_or_default(),
            category: field(lua, &spec, "category")?.unwrap_or_default(),
            keywords,
            args: schema.clone(),
            repeats: field(lua, &spec, "repeats")?.unwrap_or(false),
            hidden: field(lua, &spec, "hidden")?.unwrap_or(false),
            source: source.clone(),
        };
        Ok(crate::vm::Action { info, run, schema, timeout })
    })();
    match parsed {
        Ok(action) => {
            define(&state, action);
            Ok(true)
        }
        Err(e) => report(e),
    }
}

fn on(lua: &Lua, (event, func): (LuaValue, LuaValue)) -> mlua::Result<bool> {
    let state = state(lua)?;
    let source = caller_source(lua);
    let (LuaValue::String(event), LuaValue::Function(func)) = (event, func) else {
        state.report(ProblemLevel::Error, &source, "silo.on(event, function(ctx, ev))".into());
        return Ok(false);
    };
    let event = event.to_string_lossy();
    state.decl.borrow_mut().handlers.push(HandlerInfo { event: event.clone(), source });
    state.handlers.borrow_mut().push(crate::vm::Handler { event, func });
    Ok(true)
}

/// `silo.invoke(name, args)`: another action, with the same ctx.
fn invoke_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_async_function(|lua, (name, call_args): (String, Option<LuaValue>)| {
        let cur = current();
        async move {
            let state = state(&lua)?;
            let Some(cur) = cur else { return fail(&lua, "silo.invoke: no action is running", NO_CALLER) };
            if cur.depth + 1 >= limits::INVOKE_DEPTH {
                return fail(&lua, format!("silo.invoke: deeper than {}", limits::INVOKE_DEPTH), "too_deep");
            }
            let call_args = match params_json(&lua, call_args) {
                Ok(a) if a.as_array().is_some_and(Vec::is_empty) => json!({}),
                Ok(a) => a,
                Err(e) => return fail(&lua, format!("silo.invoke {name}: {e}"), "bad_request"),
            };
            let local = state.actions.borrow().get(&name).map(|a| (a.run.clone(), a.schema.clone()));
            let Some((run, schema)) = local else {
                if state.options.mode == Mode::Script && cur.api.is_connected() {
                    let mut params = json!({"name": name, "args": call_args, "origin": "script"});
                    for (key, value) in [("session_id", &cur.ctx.session_id), ("window_id", &cur.ctx.window_id), ("block_id", &cur.ctx.block_id)] {
                        if let Some(value) = value {
                            params[key] = json!(value);
                        }
                    }
                    let result = cur.api.call("action.run", params).await.map(|r| r.get("result").cloned().unwrap_or(Value::Null));
                    return answer(&lua, result);
                }
                return fail(&lua, format!("no action {name}"), "not_found");
            };
            if let Err(e) = args::check(&schema, &call_args) {
                return fail(&lua, format!("{name}: {e}"), "bad_request");
            }
            let ctx_t = ctx_table(&lua, &cur.ctx, &state.options.host)?;
            let args_t = to_lua(&lua, &call_args)?;
            let nested = Rc::new(Current {
                api: cur.api.clone(),
                ctx: cur.ctx.clone(),
                deadline: cur.deadline,
                depth: cur.depth + 1,
                guards: Default::default(),
                overdue: Default::default(),
            });
            match CURRENT.scope(nested, run.call_async::<MultiValue>((ctx_t, args_t))).await {
                Ok(values) => match values.into_iter().next().unwrap_or(LuaValue::Nil) {
                    LuaValue::Nil => true.into_lua_multi(&lua),
                    value => value.into_lua_multi(&lua),
                },
                Err(e) => fail(&lua, message(&e), ACTION_FAILED),
            }
        }
    })
}

/// `silo.wait(event[, filter][, timeout])`: the next such event, as
/// `ev, ctx`; `nil, "timeout"`.
fn wait_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_async_function(|lua, (event, a, b): (String, LuaValue, LuaValue)| async move {
        let state = state(&lua)?;
        let (filter, timeout) = match (a, b) {
            (LuaValue::Integer(t), _) => (LuaValue::Nil, Some(t as f64)),
            (LuaValue::Number(t), _) => (LuaValue::Nil, Some(t)),
            (filter, LuaValue::Integer(t)) => (filter, Some(t as f64)),
            (filter, LuaValue::Number(t)) => (filter, Some(t)),
            (filter, _) => (filter, None),
        };
        let filter = match filter {
            LuaValue::Nil => Filter::Any,
            LuaValue::Function(f) => Filter::Func(f),
            value @ LuaValue::Table(_) => match from_lua(&lua, &value) {
                Ok(Value::Object(fields)) => Filter::Fields(fields),
                Ok(_) => Filter::Any,
                Err(e) => return fail(&lua, format!("silo.wait: filter: {e}"), "bad_request"),
            },
            _ => return fail(&lua, "silo.wait: the filter is a table or a function", "bad_request"),
        };
        if timeout.is_some_and(|t| t.is_nan() || t < 0.0) {
            return fail(&lua, "silo.wait: the timeout is seconds", "bad_request");
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.waiters.borrow_mut().push(Waiter { event, filter, tx });
        let heard = match timeout {
            Some(t) => match tokio::time::timeout(Duration::from_secs_f64(t), rx).await {
                Ok(heard) => heard,
                Err(_) => return fail(&lua, "timeout", RpcError::TIMEOUT),
            },
            None => rx.await,
        };
        let Ok(heard) = heard else { return fail(&lua, "stopped", crate::STOPPED) };
        let ctx = crate::Ctx {
            session_id: heard.ctx.session_id.clone(),
            window_id: heard.ctx.window_id.clone(),
            block_id: heard.ctx.block_id.clone(),
            client_id: None,
            origin: Some(ActionOrigin::Event),
        };
        (to_lua(&lua, &heard.ev)?, ctx_table(&lua, &ctx, &state.options.host)?).into_lua_multi(&lua)
    })
}

/// A timer as Lua holds it.
struct Timer(Rc<TimerShared>);

impl UserData for Timer {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancel", |_, this, ()| {
            this.0.cancel();
            Ok(())
        });
        methods.add_method("active", |_, this, ()| Ok(this.0.active.get()));
    }
}

fn timer_fn(lua: &Lua, repeat: bool) -> mlua::Result<Function> {
    let what = if repeat { "silo.every" } else { "silo.after" };
    lua.create_function(move |lua, (seconds, func): (LuaValue, LuaValue)| {
        let state = state(lua)?;
        let seconds = match seconds {
            LuaValue::Integer(i) => i as f64,
            LuaValue::Number(n) => n,
            _ => f64::NAN,
        };
        let LuaValue::Function(func) = func else { return fail(lua, format!("{what}(seconds, function)"), "bad_request") };
        if !(seconds >= 0.0 && seconds.is_finite()) {
            return fail(lua, format!("{what}: seconds must be a number of 0 or more"), "bad_request");
        }
        let ctx = current().map(|c| c.ctx.clone()).unwrap_or_default();
        let ctx = crate::Ctx { origin: Some(ActionOrigin::Timer), client_id: None, ..ctx };
        match start_timer(&state, lua, func, Duration::from_secs_f64(seconds), repeat, ctx) {
            Ok(shared) => lua.create_userdata(Timer(shared))?.into_lua_multi(lua),
            Err(e) => fail(lua, format!("{what}: {e}"), "too_many"),
        }
    })
}

fn exec_options(lua: &Lua, opts: Option<Table>) -> Result<ExecOptions, String> {
    let Some(opts) = opts else { return Ok(ExecOptions::default()) };
    let mut env = HashMap::new();
    if let Some(table) = field::<Table>(lua, &opts, "env")? {
        for pair in table.pairs::<String, LuaValue>() {
            let (key, value) = pair.map_err(|_| "env: names are strings".to_owned())?;
            let value = match value {
                LuaValue::Boolean(false) => None,
                LuaValue::String(s) => Some(s.to_string_lossy()),
                LuaValue::Integer(i) => Some(i.to_string()),
                LuaValue::Number(n) => Some(n.to_string()),
                _ => return Err(format!("env.{key}: a string, or false to remove it")),
            };
            env.insert(key, value);
        }
    }
    let timeout = match field::<f64>(lua, &opts, "timeout")? {
        Some(t) if t > 0.0 && t.is_finite() => Some(Duration::from_secs_f64(t)),
        Some(t) => return Err(format!("timeout {t} is not a number of seconds")),
        None => None,
    };
    Ok(ExecOptions {
        cwd: field(lua, &opts, "cwd")?,
        env,
        stdin: field::<mlua::LuaString>(lua, &opts, "stdin")?.map(|s| s.as_bytes().to_vec()),
        timeout,
        detach: field(lua, &opts, "detach")?.unwrap_or(false),
    })
}

fn exec_result(lua: &Lua, r: &ExecResult) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("ok", r.pid.is_some() || (r.code == Some(0) && !r.timed_out))?;
    t.set("code", r.code)?;
    t.set("signal", r.signal)?;
    t.set("stdout", r.stdout.as_str())?;
    t.set("stderr", r.stderr.as_str())?;
    t.set("timed_out", r.timed_out)?;
    t.set("pid", r.pid)?;
    Ok(t)
}

fn exec_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_async_function(|lua, (argv, opts): (LuaValue, Option<Table>)| {
        let cur = current();
        async move {
            let argv = match &argv {
                LuaValue::Table(t) => t.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>().ok(),
                _ => None,
            };
            let Some(argv) = argv else { return fail(&lua, "silo.exec: argv is a list of strings", "bad_request") };
            let options = match exec_options(&lua, opts) {
                Ok(o) => o,
                Err(e) => return fail(&lua, format!("silo.exec: {e}"), "bad_request"),
            };
            let keep = |guard| match &cur {
                Some(cur) => cur.guards.borrow_mut().push(guard),
                None => std::mem::forget(guard),
            };
            match crate::exec::run(argv, options, keep).await {
                Ok(result) => exec_result(&lua, &result)?.into_lua_multi(&lua),
                Err(e) => fail(&lua, e, "exec_failed"),
            }
        }
    })
}

fn open_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_async_function(|lua, spec: LuaValue| {
        let caller = caller(&lua, "silo.open");
        async move {
            let cur = match caller {
                Ok(cur) => cur,
                Err((message, code)) => return fail(&lua, message, code),
            };
            let spec =
                match from_lua(&lua, &spec).map_err(|e| e.to_string()).and_then(|v| serde_json::from_value::<OpenSpec>(v).map_err(|e| e.to_string()))
                {
                    Ok(spec) => spec,
                    Err(e) => return fail(&lua, format!("silo.open: {e}"), "bad_request"),
                };
            match layout::open(&cur.api, &cur.ctx, spec).await {
                Ok(opened) => answer(&lua, Ok(json!({"window_id": opened.window_id, "block_ids": opened.block_ids}))),
                Err(e) => fail(&lua, e.message, &e.code),
            }
        }
    })
}

fn split_fn(lua: &Lua, axis: &'static str) -> mlua::Result<Function> {
    lua.create_function(move |lua, (ratio, first, second): (LuaValue, LuaValue, LuaValue)| {
        let ratio = match ratio {
            LuaValue::Integer(i) => i as f64,
            LuaValue::Number(n) => n,
            _ => f64::NAN,
        };
        let (LuaValue::Table(first), LuaValue::Table(second)) = (first, second) else {
            return fail(lua, format!("silo.layout: ({axis} ratio, a, b) with two layouts"), "bad_request");
        };
        if !(ratio > 0.0 && ratio < 1.0) {
            return fail(lua, "silo.layout: the ratio is between 0 and 1", "bad_request");
        }
        let t = lua.create_table()?;
        t.set("type", "split")?;
        t.set("axis", axis)?;
        t.set("ratio", ratio)?;
        t.set("first", first)?;
        t.set("second", second)?;
        t.into_lua_multi(lua)
    })
}

fn json_table(lua: &Lua) -> mlua::Result<Table> {
    let json_t = lua.create_table()?;
    json_t.set("null", lua.null())?;
    json_t.set(
        "encode",
        lua.create_function(|lua, (value, opts): (LuaValue, Option<Table>)| {
            let value = match from_lua(lua, &value) {
                Ok(v) => v,
                Err(e) => return fail(lua, format!("silo.json.encode: {e}"), "bad_request"),
            };
            let indent = match opts.map(|o| o.get::<LuaValue>("indent")).transpose()? {
                Some(LuaValue::Integer(n)) if n > 0 => Some(usize::try_from(n.min(16)).unwrap_or(2)),
                Some(LuaValue::Boolean(true)) => Some(2),
                _ => None,
            };
            let text = match indent {
                None => serde_json::to_string(&value),
                Some(n) => {
                    let spaces = " ".repeat(n);
                    let mut out = Vec::new();
                    let mut ser = serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(spaces.as_bytes()));
                    serde::Serialize::serialize(&value, &mut ser).map(|()| String::from_utf8_lossy(&out).into_owned())
                }
            };
            match text {
                Ok(text) => text.into_lua_multi(lua),
                Err(e) => fail(lua, format!("silo.json.encode: {e}"), "bad_request"),
            }
        })?,
    )?;
    json_t.set(
        "decode",
        lua.create_function(|lua, text: mlua::LuaString| match serde_json::from_slice::<Value>(&text.as_bytes()) {
            Ok(Value::Null) => lua.null().into_lua_multi(lua),
            Ok(value) => to_lua(lua, &value)?.into_lua_multi(lua),
            Err(e) => fail(lua, format!("silo.json.decode: {e}"), "bad_request"),
        })?,
    )?;
    Ok(json_t)
}

fn log(lua: &Lua, values: Variadic<LuaValue>) -> mlua::Result<()> {
    let state = state(lua)?;
    let level = match values.first() {
        Some(LuaValue::String(s)) if values.len() > 1 => match &*s.to_string_lossy() {
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" | "warning" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            _ => None,
        },
        _ => None,
    };
    let skip = usize::from(level.is_some());
    let text = values.iter().skip(skip).map(|v| show(lua, v)).collect::<Vec<_>>().join(" ");
    state.log(level.unwrap_or(LogLevel::Info), &text);
    Ok(())
}

/// Put the `silo` table in `lua`'s globals.
pub(crate) fn install(lua: &Lua, state: &Rc<State>) -> mlua::Result<()> {
    let silo = lua.create_table()?;
    let options = &state.options;
    silo.set("version", env!("CARGO_PKG_VERSION"))?;
    silo.set("host", options.host.as_str())?;
    silo.set("config_dir", options.dir.to_string_lossy().as_ref())?;
    let script_args = if options.args.is_null() { json!({}) } else { options.args.clone() };
    silo.set("args", to_lua(lua, &script_args)?)?;
    silo.set("session_id", options.session_id.clone())?;
    silo.set("block_id", options.block_id.clone())?;

    silo.set("bind", lua.create_function(bind)?)?;
    silo.set("unbind", lua.create_function(unbind)?)?;
    silo.set("mode", lua.create_function(mode)?)?;
    silo.set("on", lua.create_function(on)?)?;
    silo.set("invoke", invoke_fn(lua)?)?;
    silo.set("wait", wait_fn(lua)?)?;
    silo.set("after", timer_fn(lua, false)?)?;
    silo.set("every", timer_fn(lua, true)?)?;
    silo.set("exec", exec_fn(lua)?)?;
    silo.set("open", open_fn(lua)?)?;
    silo.set("json", json_table(lua)?)?;
    silo.set("log", lua.create_function(log)?)?;
    silo.set(
        "call",
        lua.create_async_function(|lua, (method, params): (String, Option<LuaValue>)| {
            let caller = caller(&lua, &method);
            async move {
                let cur = match caller {
                    Ok(cur) => cur,
                    Err((message, code)) => return fail(&lua, message, code),
                };
                match params_json(&lua, params) {
                    Ok(params) => answer(&lua, cur.api.call(&method, params).await),
                    Err(e) => fail(&lua, format!("{method}: {e}"), "bad_request"),
                }
            }
        })?,
    )?;
    silo.set(
        "sleep",
        lua.create_async_function(|lua, seconds: f64| async move {
            if !(seconds >= 0.0 && seconds.is_finite()) {
                return fail(&lua, "silo.sleep: seconds must be 0 or more", "bad_request");
            }
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            true.into_lua_multi(&lua)
        })?,
    )?;
    silo.set(
        "stop",
        lua.create_function(|lua, code: Option<i32>| {
            let state = crate::vm::state(lua)?;
            if state.options.mode != Mode::Script {
                return fail(lua, "silo.stop ends scripts only", "bad_request");
            }
            state.finish(crate::Exit { code: code.unwrap_or(0), value: Value::Null, error: None });
            Err(mlua::Error::runtime("silo.stop"))
        })?,
    )?;

    // Builders plus the `layout.*` methods.
    let layout_t = group(lua, "layout")?;
    layout_t.set(
        "block",
        lua.create_function(|lua, spec: Option<Table>| {
            let spec = match spec {
                Some(spec) => spec,
                None => lua.create_table()?,
            };
            spec.raw_set("type", "block")?;
            Ok(spec)
        })?,
    )?;
    layout_t.set("row", split_fn(lua, "horizontal")?)?;
    layout_t.set("column", split_fn(lua, "vertical")?)?;
    silo.set("layout", layout_t)?;

    // `silo.action{…}` defines; `silo.action.run{…}` is the method.
    let action_t = lua.create_table()?;
    let action_meta = group_meta(lua, "action")?;
    action_meta.set("__call", lua.create_function(action)?)?;
    action_t.set_metatable(Some(action_meta))?;
    silo.set("action", action_t)?;
    silo.set("app", group(lua, "app")?)?;

    // Any other `silo.<group>`: a table of method functions.
    let meta = lua.create_table()?;
    meta.set(
        "__index",
        lua.create_function(|lua, (silo, key): (Table, LuaValue)| {
            let LuaValue::String(key) = key else { return Ok(LuaValue::Nil) };
            let name = key.to_str()?.to_owned();
            if !valid_name(&name) {
                return Ok(LuaValue::Nil);
            }
            let table = group(lua, &name)?;
            silo.raw_set(key, table.clone())?;
            Ok(LuaValue::Table(table))
        })?,
    )?;
    silo.set_metatable(Some(meta))?;
    lua.globals().set("silo", silo)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_told_apart_by_spelling() {
        assert_eq!(classify("SplitRight").unwrap(), BindTarget::Command { id: "SplitRight".into() });
        assert_eq!(classify("window.create").unwrap(), BindTarget::Method { name: "window.create".into() });
        assert_eq!(classify("app.mode_enter").unwrap(), BindTarget::Method { name: "app.mode_enter".into() });
        assert_eq!(classify("dev_layout").unwrap(), BindTarget::Action { name: "dev_layout".into() });
        assert!(classify("dev-layout").is_err());
        assert!(classify("").is_err());
    }
}
