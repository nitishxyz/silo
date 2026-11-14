//! `silo do` for scripts and actions (`docs/LUA.md` §5): what a target is,
//! the flags and `key=value` arguments, running a script with `silo-lua`
//! and calling `action.run`. Methods stay with `generated.rs`.

use std::{
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use serde_json::{Map, Value};
use silo_client::{Control, Events};
use silo_lua::{Api, Ctx, Invocation, LogLevel, LogSink, Mode, Options, Source, Vm};
use silo_proto::{
    config::{ActionInfo, ActionList, ActionOrigin, ActionResult, ActionRunParams, ProblemLevel},
    control::{BlockView, Method, RpcError},
    model::SessionSummary,
};

/// `silo do --help`.
pub const USAGE: &str = "\
Usage: silo do <action> [VALUE | --name VALUE | name=VALUE]… [-s SESSION] [-b BLOCK] [--args JSON|@FILE] [--json]
       silo do <file.lua | -> [key=value]… [-s SESSION] [-b BLOCK] [--all] [--for 10m] [--fail-fast] [--args JSON|@FILE] [--json]
       silo do -e CODE [key=value]…
       silo do <file.lua> --action NAME [VALUE | --name VALUE | name=VALUE]…
       silo do <method> [--param value]… [--json PARAMS]

A target ending in .lua, with a slash, or `-` (stdin) is a script; a name with a dot is a
method (`silo api list`, `silo do <method> --help`); any other name is an action (`silo actions`).

An action's arguments are --name VALUE or name=VALUE; a bare value is the action's one
required argument (`silo do name_tab work`). A script's are key=value. 2 is a number, true
a boolean, null null, anything else a string (a string wherever the action declares one).
--args gives exact JSON and wins. An argument named like a flag below is written name=VALUE.

  -s, --session <ID|NAME>  Run in this session (default: $SILO_SESSION, else the block's)
  -b, --block <ID>         Run from this block (default: $SILO_BLOCK)
      --all                Script handlers hear every session, not only the script's
      --for <DURATION>     Stop the script after this long (`30s`, `10m`) and exit 0
      --fail-fast          A failing handler or timer ends the script (exit 1)
      --args <JSON|@FILE>  Arguments as a JSON object
      --action <NAME>      Run this action of the file, loaded here as a config
      --json               Print the result indented
  -e <CODE>                Run CODE as the script
";

/// What `silo do` was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Script(Script),
    /// A name matching `[a-z][a-z0-9_]*`.
    Action(String),
    /// A name with a dot: `generated.rs`.
    Method(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Script {
    File(PathBuf),
    /// `-`.
    Stdin,
    /// `-e CODE`.
    Eval(String),
}

/// An action name: `[a-z][a-z0-9_]*`.
#[must_use]
pub fn is_action_name(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase()) && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A script ends in `.lua`, contains a `/` or is `-`; a method has a dot;
/// an action is `[a-z][a-z0-9_]*`.
///
/// # Errors
/// None of these.
pub fn classify(word: &str) -> anyhow::Result<Target> {
    if word == "-" {
        return Ok(Target::Script(Script::Stdin));
    }
    if word.ends_with(".lua") || word.contains('/') {
        return Ok(Target::Script(Script::File(word.into())));
    }
    if word.contains('.') {
        return Ok(Target::Method(word.to_owned()));
    }
    if is_action_name(word) {
        return Ok(Target::Action(word.to_owned()));
    }
    bail!("{word:?} is not a script (ends in .lua, has a /, or is -), an action ([a-z][a-z0-9_]*) or a method (has a dot); see `silo do --help`")
}

/// `silo do`, parsed.
#[derive(Debug)]
pub enum Parsed {
    /// `silo do --help` with no target.
    Help,
    /// A method: `generated.rs` takes the words as they are.
    Method,
    Run(Box<Run>),
}

/// A script or an action to run, and how.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub target: Target,
    pub session: Option<String>,
    pub block: Option<String>,
    pub all: bool,
    pub run_for: Option<Duration>,
    pub fail_fast: bool,
    /// `key=value` words, in order, as given.
    pub pairs: Vec<(String, String)>,
    /// An action's other words, in order: `--name value` flags and bare
    /// values, read with the action's declared args ([`Run::args`]).
    pub extras: Vec<String>,
    /// `--args`.
    pub args_json: Option<Map<String, Value>>,
    pub json: bool,
    /// `--action NAME` of a file.
    pub action: Option<String>,
    pub help: bool,
}

/// Parse `silo do <first> <rest>…`.
///
/// # Errors
/// An unknown flag, a missing value, a stray word, or flags that do not go
/// with the target.
pub fn parse(first: &str, rest: &[String]) -> anyhow::Result<Parsed> {
    let flags_first = first.starts_with('-') && first != "-";
    let (mut target, words): (Option<Target>, Vec<String>) = if flags_first {
        (None, std::iter::once(first.to_owned()).chain(rest.iter().cloned()).collect())
    } else {
        match classify(first)? {
            Target::Method(_) => return Ok(Parsed::Method),
            target => (Some(target), rest.to_vec()),
        }
    };
    let (mut session, mut block, mut all, mut run_for, mut fail_fast, mut json, mut help) = (None, None, false, None, false, false, false);
    let (mut pairs, mut args_json, mut action) = (Vec::new(), None, None);
    let mut extras: Vec<String> = Vec::new();
    let mut only_args = false;
    let mut i = 0;
    while i < words.len() {
        let word = words[i].clone();
        i += 1;
        if !only_args && word == "--" {
            only_args = true;
            continue;
        }
        if !only_args && word.starts_with('-') && word != "-" {
            let (flag, inline) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
                _ => (word.clone(), None),
            };
            let mut value = || -> anyhow::Result<String> {
                if let Some(value) = &inline {
                    return Ok(value.clone());
                }
                let value = words.get(i).cloned().with_context(|| format!("{flag} needs a value"))?;
                i += 1;
                Ok(value)
            };
            let switch = match flag.as_str() {
                "-h" | "--help" => Some(&mut help),
                "--all" => Some(&mut all),
                "--fail-fast" => Some(&mut fail_fast),
                "--json" => Some(&mut json),
                _ => None,
            };
            if let Some(switch) = switch {
                if inline.is_some() {
                    bail!("{flag} takes no value");
                }
                *switch = true;
                continue;
            }
            match flag.as_str() {
                "-e" => {
                    let code = value()?;
                    if target.is_some() {
                        bail!("-e takes the place of a script; give one or the other");
                    }
                    target = Some(Target::Script(Script::Eval(code)));
                }
                "-s" | "--session" => session = Some(value()?),
                "-b" | "--block" => block = Some(value()?),
                "--for" => run_for = Some(crate::parse_duration(&value()?)?),
                "--args" => args_json = Some(read_args(&value()?)?),
                "--action" => action = Some(value()?),
                // An action's own argument (`--name work`): read once the
                // action's args are known.
                _ if flag.starts_with("--") && flag.len() > 2 => extras.push(word),
                _ => bail!("unknown flag {flag}; see `silo do --help`"),
            }
            continue;
        }
        if let Some((key, raw)) = word.split_once('=')
            && !key.is_empty()
            && !key.contains('/')
        {
            pairs.push((key.to_owned(), raw.to_owned()));
            continue;
        }
        if target.is_none() && !only_args {
            match classify(&word)? {
                Target::Method(method) => bail!("put the method first: silo do {method} [--param value]…"),
                found => target = Some(found),
            }
            continue;
        }
        extras.push(word);
    }
    let Some(target) = target else {
        if help {
            return Ok(Parsed::Help);
        }
        bail!("name a script, an action or a method; see `silo do --help`");
    };
    let scripted = all || run_for.is_some() || fail_fast;
    match (&target, &action) {
        (Target::Action(_), _) if scripted => bail!("--all, --for and --fail-fast are for scripts"),
        (Target::Script(Script::File(_)), Some(_)) if scripted => bail!("--all, --for and --fail-fast are for scripts, not --action"),
        (Target::Script(Script::File(_)), Some(name)) if !is_action_name(name) => bail!("--action {name:?}: an action name is [a-z][a-z0-9_]*"),
        (Target::Script(Script::File(_)), Some(_)) | (_, None) => {}
        (_, Some(_)) => bail!("--action runs an action of a .lua file: silo do <file.lua> --action NAME"),
    }
    // Only an action declares arguments to read flags and bare values by.
    if matches!(target, Target::Script(_))
        && action.is_none()
        && let Some(word) = extras.first()
    {
        if word.starts_with("--") {
            bail!("unknown flag {}; see `silo do --help`", word.split('=').next().unwrap_or(word));
        }
        bail!("unexpected argument {word:?}: a script's arguments are key=value; see `silo do --help`");
    }
    Ok(Parsed::Run(Box::new(Run { target, session, block, all, run_for, fail_fast, pairs, extras, args_json, json, action, help })))
}

/// `--args`: a JSON object, or `@file` holding one.
fn read_args(text: &str) -> anyhow::Result<Map<String, Value>> {
    let (json, from) = match text.strip_prefix('@') {
        Some(file) => (std::fs::read_to_string(file).with_context(|| format!("--args: cannot read {file}"))?, file),
        None => (text.to_owned(), "--args"),
    };
    match serde_json::from_str(&json).with_context(|| format!("{from}: invalid JSON"))? {
        Value::Object(map) => Ok(map),
        _ => bail!("{from}: --args must be a JSON object"),
    }
}

/// A `key=value` word's value: `true`, `false`, `null`, a JSON number, else
/// the text as a string.
#[must_use]
pub fn typed(text: &str) -> Value {
    match text {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => match serde_json::from_str::<Value>(text) {
            Ok(number @ Value::Number(_)) if !text.starts_with(char::is_whitespace) && !text.ends_with(char::is_whitespace) => number,
            _ => Value::String(text.to_owned()),
        },
    }
}

impl Run {
    /// The args object: the `key=value` words, an action's `--name value`
    /// flags and bare values (read with `schema`, the action's declared
    /// args), all typed as strings where `schema` declares a string, then
    /// `--args` over them.
    ///
    /// # Errors
    /// A flag the action does not declare or without its value, or a bare
    /// value when it is not clear which argument it is.
    pub fn args(&self, schema: Option<&Value>) -> anyhow::Result<Value> {
        let property = |key: &str| schema.and_then(|s| s.get("properties")?.get(key));
        let kind = |key: &str| property(key).and_then(|p| p.get("type")?.as_str());
        let value = |key: &str, raw: &str| if kind(key) == Some("string") { Value::String(raw.to_owned()) } else { typed(raw) };
        let declared: Vec<&str> =
            schema.and_then(|s| s.get("properties")?.as_object()).map(|p| p.keys().map(String::as_str).collect()).unwrap_or_default();
        let mut out = Map::new();
        for (key, raw) in &self.pairs {
            out.insert(key.clone(), value(key, raw));
        }
        let mut bare: Vec<&str> = Vec::new();
        let mut words = self.extras.iter().peekable();
        while let Some(word) = words.next() {
            let Some(flag) = word.strip_prefix("--") else {
                bare.push(word);
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((name, raw)) => (name, Some(raw)),
                None => (flag, None),
            };
            let key = name.replace('-', "_");
            if !declared.is_empty() && !declared.contains(&key.as_str()) {
                bail!("unknown flag --{name}; it takes: {}", crate::config_cmd::args_usage(schema.unwrap_or(&Value::Null)));
            }
            let given = match inline {
                Some(raw) => value(&key, raw),
                // `--on` alone switches a boolean on.
                None if kind(&key) == Some("boolean") && !words.peek().is_some_and(|w| matches!(w.as_str(), "true" | "false")) => Value::Bool(true),
                None => value(&key, words.next().with_context(|| format!("--{name} needs a value"))?),
            };
            out.insert(key, given);
        }
        if !bare.is_empty() {
            let Some(key) = schema.and_then(silo_proto::config::sole_arg) else {
                let usage = schema.filter(|_| !declared.is_empty()).map(crate::config_cmd::args_usage);
                bail!("unexpected argument {:?}: name the arguments ({})", bare[0], usage.as_deref().unwrap_or("key=value"));
            };
            if out.contains_key(key) {
                bail!("unexpected argument {:?}: {key} is already given", bare[0]);
            }
            // Several words are one string: `silo do name_tab my tab`.
            if bare.len() > 1 && kind(key) != Some("string") {
                bail!("unexpected argument {:?}: {key} takes one value", bare[1]);
            }
            out.insert(key.to_owned(), value(key, &bare.join(" ")));
        }
        if let Some(given) = &self.args_json {
            out.extend(given.clone());
        }
        Ok(Value::Object(out))
    }
}

/// Print `value` as JSON: indented on a terminal or with `--json`, else
/// compact.
pub fn print_json(value: &Value, indent: bool) {
    let text = if indent || std::io::stdout().is_terminal() { serde_json::to_string_pretty(value) } else { serde_json::to_string(value) };
    println!("{}", text.unwrap_or_default());
}

/// A session id, by id or name.
///
/// # Errors
/// No such session, or RPC failure.
pub async fn session_id(control: &Control, target: &str) -> anyhow::Result<String> {
    let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await?;
    match sessions.iter().find(|s| s.id == target).or_else(|| sessions.iter().find(|s| s.name == target)) {
        Some(session) => Ok(session.id.clone()),
        None => bail!("no session {target:?}; `silo ls` lists them"),
    }
}

/// The session block `block` is in.
///
/// # Errors
/// No such block, or RPC failure.
pub async fn session_of_block(control: &Control, block: &str) -> anyhow::Result<String> {
    let view: BlockView = control.call(Method::BlockView { block_id: block.to_owned() }).await.with_context(|| format!("block {block}"))?;
    Ok(view.block.session_id)
}

/// `$name`, set and not empty, when `local` (on another host it names
/// nothing there).
fn env(name: &str, local: bool) -> Option<String> {
    local.then(|| std::env::var(name).ok()).flatten().filter(|v| !v.is_empty())
}

/// Where it runs: `-s` and `-b`; else, on this machine, `$SILO_BLOCK` (not
/// when `-s` names a session) and, with `infer`, `$SILO_SESSION` or the
/// block's session.
async fn place(control: &Control, run: &Run, local: bool, infer: bool) -> anyhow::Result<(Option<String>, Option<String>)> {
    let session = match &run.session {
        Some(target) => Some(session_id(control, target).await?),
        None => None,
    };
    let block = match (&run.block, &session) {
        (Some(block), _) => Some(block.clone()),
        (None, None) => env("SILO_BLOCK", local),
        (None, Some(_)) => None,
    };
    if session.is_some() || !infer {
        return Ok((session, block));
    }
    let explicit = run.block.is_some();
    if !explicit && let Some(session) = env("SILO_SESSION", local) {
        return Ok((Some(session), block));
    }
    let Some(id) = block.clone() else { return Ok((None, None)) };
    match session_of_block(control, &id).await {
        Ok(session) => Ok((Some(session), block)),
        Err(error) if explicit => Err(error),
        // A stale `$SILO_BLOCK`: run from nowhere in particular.
        Err(_) => Ok((None, None)),
    }
}

/// Run a parsed `silo do` script or action over `control`. `local`: the
/// server is this machine's (`$SILO_BLOCK` and `$SILO_SESSION` apply).
///
/// # Errors
/// Connection failures and bad input; a script's or action's own failure
/// is printed and is exit 1.
pub async fn run(control: Control, events: Events, run: Run, local: bool) -> anyhow::Result<i32> {
    match run.target.clone() {
        Target::Action(name) if run.help => action_help(&control, &name).await,
        Target::Action(name) => action(&control, &name, &run, local).await,
        Target::Script(_) if run.help => {
            print!("{USAGE}");
            Ok(0)
        }
        Target::Script(Script::File(path)) if run.action.is_some() => {
            let name = run.action.clone().unwrap_or_default();
            file_action(&control, &path, &name, &run, local).await
        }
        Target::Script(script) => script_run(control, events, script, &run, local).await,
        Target::Method(method) => bail!("{method} is a method"),
    }
}

/// An action error, as `Error: …` (exit 1), with what to do for the usual
/// ones; anything else (the connection) is returned.
fn action_failed(name: &str, error: anyhow::Error) -> anyhow::Result<i32> {
    let Some(rpc) = error.downcast_ref::<RpcError>() else { return Err(error) };
    let message = match rpc.code.as_str() {
        RpcError::METHOD_NOT_FOUND => "this server does not run actions (it is older, or its config is off)".to_owned(),
        "not_found" if silo_proto::control::METHODS.iter().any(|(m, _)| *m == name) => {
            format!("no action {name}; for the method, `silo api call {name}`")
        }
        "not_found" if rpc.message.contains(name) => format!("no action {name}; `silo actions` lists them"),
        _ => without_traceback(&rpc.message).to_owned(),
    };
    eprintln!("Error: {message}");
    Ok(1)
}

async fn action_list(control: &Control) -> anyhow::Result<Vec<ActionInfo>> {
    let list: ActionList = control.call(Method::ActionList {}).await?;
    Ok(list.actions)
}

/// `silo do <action> --help`: what `silo actions` knows of it.
async fn action_help(control: &Control, name: &str) -> anyhow::Result<i32> {
    let actions = match action_list(control).await {
        Ok(actions) => actions,
        Err(error) => return action_failed(name, error),
    };
    let Some(info) = actions.iter().find(|a| a.name == name) else {
        eprintln!("Error: no action {name}; `silo actions` lists them");
        return Ok(1);
    };
    println!("{}{}", info.title, if info.description.is_empty() { String::new() } else { format!("\n{}", info.description) });
    println!("\nUsage: silo do {name} {}", crate::config_cmd::args_usage(&info.args));
    if !info.source.is_empty() {
        println!("Defined at {}", info.source);
    }
    Ok(0)
}

/// `action.run` on the server.
async fn action(control: &Control, name: &str, run: &Run, local: bool) -> anyhow::Result<i32> {
    let (session_id, block_id) = place(control, run, local, false).await?;
    // Typed by the action's own declaration where it has one.
    let typed = !run.pairs.is_empty() || !run.extras.is_empty();
    let schema = if typed { action_list(control).await.ok().and_then(|l| l.into_iter().find(|a| a.name == name)) } else { None };
    let params = ActionRunParams {
        name: name.to_owned(),
        args: run.args(schema.as_ref().map(|a| &a.args))?,
        session_id,
        block_id,
        origin: Some(ActionOrigin::Cli),
        ..ActionRunParams::default()
    };
    match control.call::<ActionResult>(Method::ActionRun(params)).await {
        Ok(result) => {
            if !result.result.is_null() {
                print_json(&result.result, run.json);
            }
            Ok(0)
        }
        Err(error) => action_failed(name, error),
    }
}

/// Where `silo.log` and the VM's warnings go: stderr. A line starting with
/// [`TOP`] goes to `top` instead.
fn sink(top: Option<tokio::sync::mpsc::UnboundedSender<u32>>) -> LogSink {
    Arc::new(move |level, message| {
        if let Some(count) = message.strip_prefix(TOP) {
            if let Some(top) = &top {
                let _ = top.send(count.trim().parse().unwrap_or(0));
            }
            return;
        }
        match level {
            LogLevel::Debug => {}
            LogLevel::Info => eprintln!("{message}"),
            LogLevel::Warn => eprintln!("warning: {message}"),
            LogLevel::Error => eprintln!("error: {message}"),
        }
    })
}

/// `--action NAME` of a file: loaded here as a config (none of the running
/// config changes), the action run with this connection.
async fn file_action(control: &Control, path: &Path, name: &str, run: &Run, local: bool) -> anyhow::Result<i32> {
    let path = std::path::absolute(path).with_context(|| format!("cannot read {}", path.display()))?;
    if !path.is_file() {
        bail!("cannot read {}: no such file", path.display());
    }
    let (session_id, block_id) = place(control, run, local, true).await?;
    let options = Options { host: control.hello().server.hostname.clone(), log: Some(sink(None)), ..Options::config(&path) };
    let (vm, loaded) = Vm::load(Source::File(path.clone()), options).await;
    if loaded.failed {
        for problem in loaded.problems.iter().filter(|p| p.level == ProblemLevel::Error) {
            eprintln!("Error: {}", shown(&problem.source, &problem.message));
        }
        vm.stop();
        return Ok(1);
    }
    let Some(info) = loaded.actions.iter().find(|a| a.name == name) else {
        let names: Vec<&str> = loaded.actions.iter().filter(|a| !a.hidden).map(|a| a.name.as_str()).collect();
        eprintln!("Error: {} defines no action {name} (it has: {})", path.display(), if names.is_empty() { "none".into() } else { names.join(", ") });
        vm.stop();
        return Ok(1);
    };
    let ctx = Ctx { session_id: session_id.clone(), block_id: block_id.clone(), origin: Some(ActionOrigin::Cli), ..Ctx::default() };
    let api = Api::control(control.clone()).with_session(session_id).with_block(block_id);
    let args = match run.args(Some(&info.args)) {
        Ok(args) => args,
        Err(error) => {
            vm.stop();
            return Err(error);
        }
    };
    let result = vm.run_action(name, Invocation { ctx, args, api }).await;
    vm.stop();
    match result {
        Ok(value) => {
            if !value.is_null() {
                print_json(&value, run.json);
            }
            Ok(0)
        }
        Err(error) => {
            eprintln!("Error: {}", without_traceback(&error.message));
            Ok(1)
        }
    }
}

fn shown(source: &str, message: &str) -> String {
    if source.is_empty() { message.to_owned() } else { format!("{source}: {message}") }
}

/// A Lua error's message without the traceback Lua adds to it.
fn without_traceback(message: &str) -> &str {
    message.split_once("\nstack traceback:").map_or(message, |(head, _)| head)
}

/// What the wrapped script logs once its top level has returned, with the
/// number of handlers it registered.
const TOP: &str = "\u{1}silo-do-top ";

/// The script, wrapped (on its first line, so line numbers hold) to count
/// `silo.on` handlers and say when its top level returns: `silo-lua` does
/// not tell, and a script still running then is one listening. A `#!`
/// first line becomes a comment.
fn wrap(code: &str) -> String {
    const PRELUDE: &str = "local __silo_log, __silo_on, __silo_pack, __silo_unpack, __silo_n = silo.log, silo.on, table.pack, table.unpack, 0 \
        silo.on = function(...) local ok = __silo_on(...) if ok then __silo_n = __silo_n + 1 end return ok end \
        return (function(r) __silo_log(\"debug\", \"\\1silo-do-top \" .. __silo_n) return __silo_unpack(r, 1, r.n) end)(__silo_pack((function(...) ";
    let code = if code.starts_with('#') { format!("--{code}") } else { code.to_owned() };
    format!("{PRELUDE}{code}\nend)(...)))")
}

/// Run a script: print its returned value, its errors, and exit with its
/// code; 130 on Ctrl-C; 0 when `--for` runs out.
async fn script_run(control: Control, events: Events, script: Script, run: &Run, local: bool) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("current directory")?;
    let (code, name, dir) = match script {
        Script::File(path) => {
            let code = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
            let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).map_or_else(|| cwd.clone(), |p| cwd.join(p));
            (code, path.display().to_string(), dir)
        }
        Script::Stdin => {
            let code = tokio::task::spawn_blocking(|| {
                let mut code = String::new();
                std::io::stdin().read_to_string(&mut code).map(|_| code)
            })
            .await?
            .context("cannot read the script from stdin")?;
            (code, "stdin".to_owned(), cwd)
        }
        Script::Eval(code) => (code, "-e".to_owned(), cwd),
    };
    let (session_id, block_id) = place(&control, run, local, true).await?;
    let (top_tx, mut top_rx) = tokio::sync::mpsc::unbounded_channel();
    let options = Options {
        mode: Mode::Script,
        dir,
        source_name: name,
        host: control.hello().server.hostname.clone(),
        args: run.args(None)?,
        all_sessions: run.all,
        session_id: session_id.clone(),
        block_id: block_id.clone(),
        fail_fast: run.fail_fast,
        log: Some(sink(Some(top_tx))),
    };
    let (vm, loaded) = Vm::load(Source::Text { code: wrap(&code) }, options).await;
    if loaded.failed {
        for problem in loaded.problems.iter().filter(|p| p.level == ProblemLevel::Error) {
            eprintln!("Error: {}", shown(&problem.source, &problem.message));
        }
        vm.stop();
        return Ok(1);
    }
    control.subscribe().await?;
    vm.attach(Api::control(control.clone()).with_session(session_id).with_block(block_id), Some(events));

    let finished = vm.finished();
    tokio::pin!(finished);
    let stop_at = run.run_for.map(|d| tokio::time::Instant::now() + d);
    // After the top level returns, a moment to end on its own; still
    // running then, it is listening.
    let mut listening: Option<(tokio::time::Instant, u32)> = None;
    let mut announced = false;
    let far = || tokio::time::Instant::now() + Duration::from_secs(86_400 * 365);
    loop {
        tokio::select! {
            exit = &mut finished => {
                if let Some(error) = &exit.error {
                    eprintln!("Error: {}", without_traceback(error));
                }
                if !exit.value.is_null() {
                    print_json(&exit.value, run.json);
                }
                return Ok(exit.code);
            }
            Some(handlers) = top_rx.recv(), if listening.is_none() => {
                listening = Some((tokio::time::Instant::now() + Duration::from_millis(100), handlers));
            }
            () = tokio::time::sleep_until(listening.map_or_else(far, |(at, _)| at)), if listening.is_some() && !announced => {
                announced = true;
                let what = match listening.map_or(0, |(_, n)| n) {
                    0 => "timers".to_owned(),
                    1 => "1 handler".to_owned(),
                    n => format!("{n} handlers"),
                };
                eprintln!("listening: {what}; Ctrl-C to stop");
            }
            () = tokio::time::sleep_until(stop_at.unwrap_or_else(far)), if stop_at.is_some() => {
                vm.stop();
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut finished).await;
                return Ok(0);
            }
            _ = tokio::signal::ctrl_c() => {
                vm.stop();
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut finished).await;
                return Ok(130);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    fn parsed(text: &str) -> anyhow::Result<Parsed> {
        let words = words(text);
        parse(&words[0], &words[1..])
    }

    fn run_of(text: &str) -> Run {
        match parsed(text).unwrap() {
            Parsed::Run(run) => *run,
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn targets_are_scripts_actions_or_methods() {
        assert_eq!(classify("layout.lua").unwrap(), Target::Script(Script::File("layout.lua".into())));
        assert_eq!(classify("./dev").unwrap(), Target::Script(Script::File("./dev".into())));
        assert_eq!(classify("scripts/x.lua").unwrap(), Target::Script(Script::File("scripts/x.lua".into())));
        assert_eq!(classify("-").unwrap(), Target::Script(Script::Stdin));
        assert_eq!(classify("dev_layout").unwrap(), Target::Action("dev_layout".into()));
        assert_eq!(classify("a1").unwrap(), Target::Action("a1".into()));
        assert_eq!(classify("window.step").unwrap(), Target::Method("window.step".into()));
        assert_eq!(classify("app.full_screen").unwrap(), Target::Method("app.full_screen".into()));
        for bad in ["Dev", "1abc", "dev-layout", "_x", ""] {
            assert!(classify(bad).is_err(), "{bad:?}");
        }
        assert!(is_action_name("x") && !is_action_name("x.y") && !is_action_name("X"));
    }

    #[test]
    fn values_are_typed() {
        assert_eq!(typed("2"), json!(2));
        assert_eq!(typed("-1.5"), json!(-1.5));
        assert_eq!(typed("true"), json!(true));
        assert_eq!(typed("false"), json!(false));
        assert_eq!(typed("null"), Value::Null);
        for text in ["app.log", "007", "+1", ".5", "inf", "NaN", "1e999", "", " 2", "True", "[1]", "{}"] {
            assert_eq!(typed(text), json!(text), "{text:?}");
        }
    }

    #[test]
    fn args_merge_with_json_winning() {
        let run = run_of("dev_layout file=app.log n=2 on=true x=null eq=a=b n=3");
        assert_eq!(run.args(None).unwrap(), json!({"file": "app.log", "n": 3, "on": true, "x": null, "eq": "a=b"}));
        let mut run = run_of("dev_layout n=2 name=7");
        run.args_json = Some(json!({"n": 5}).as_object().cloned().unwrap());
        let schema = json!({"type": "object", "properties": {"name": {"type": "string"}, "n": {"type": "integer"}}});
        assert_eq!(run.args(Some(&schema)).unwrap(), json!({"n": 5, "name": "7"}), "a declared string stays one; --args wins");
        let Parsed::Run(run) = parse("x", &["--args".into(), "{\"a\": [1]}".into(), "a=2".into()]).unwrap() else { panic!() };
        assert_eq!(run.args(None).unwrap(), json!({"a": [1]}));
        assert!(parse("x", &["--args".into(), "[1]".into()]).is_err());
        assert!(parse("x", &["--args".into(), "nope".into()]).is_err());
    }

    #[test]
    fn an_action_takes_bare_values_and_flags() {
        let one = json!({"type": "object", "properties": {"name": {"type": "string"}, "times": {"type": "integer"}, "loud": {"type": "boolean"}}, "required": ["name"]});
        let args = |text: &str, schema: &Value| run_of(text).args(Some(schema)).map_err(|e| e.to_string());
        // A bare value is the one required argument; several words are one string.
        assert_eq!(args("name_tab work", &one), Ok(json!({"name": "work"})));
        assert_eq!(args("name_tab 42", &one), Ok(json!({"name": "42"})), "a declared string stays one");
        assert_eq!(args("name_tab my new tab", &one), Ok(json!({"name": "my new tab"})));
        assert_eq!(args("name_tab work times=2", &one), Ok(json!({"name": "work", "times": 2})));
        // Flags: `--name VALUE`, `--name=VALUE`, a boolean alone, dashes for underscores.
        assert_eq!(args("name_tab --name work --times 2 --loud", &one), Ok(json!({"name": "work", "times": 2, "loud": true})));
        assert_eq!(args("name_tab --name=a=b --loud false", &one), Ok(json!({"name": "a=b", "loud": false})));
        assert_eq!(args("name_tab work --loud", &one), Ok(json!({"name": "work", "loud": true})));
        let dashed = json!({"properties": {"dry_run": {"type": "boolean"}}});
        assert_eq!(args("deploy --dry-run", &dashed), Ok(json!({"dry_run": true})));
        assert_eq!(args("deploy true", &dashed), Ok(json!({"dry_run": true})), "the only argument, optional");
        // What is not clear is refused, saying how to write it.
        assert_eq!(args("name_tab --nope 1", &one).unwrap_err(), "unknown flag --nope; it takes: <name> [--loud] [--times <integer>]");
        assert_eq!(args("name_tab --name", &one).unwrap_err(), "--name needs a value");
        assert_eq!(args("name_tab work name=other", &one).unwrap_err(), "unexpected argument \"work\": name is already given");
        let two = json!({"properties": {"file": {"type": "string"}, "ratio": {"type": "number"}}, "required": ["file", "ratio"]});
        assert_eq!(
            args("split app.log", &two).unwrap_err(),
            "unexpected argument \"app.log\": name the arguments (--file <string> --ratio <number>)"
        );
        assert_eq!(args("split --file app.log --ratio 0.4", &two), Ok(json!({"file": "app.log", "ratio": 0.4})));
        let number = json!({"properties": {"n": {"type": "integer"}}, "required": ["n"]});
        assert_eq!(args("step 2", &number), Ok(json!({"n": 2})));
        assert_eq!(args("step 2 3", &number).unwrap_err(), "unexpected argument \"3\": n takes one value");
        // An action that declares nothing takes any flag, and no bare value.
        assert_eq!(args("anything --x 1 --y=z", &json!({})), Ok(json!({"x": 1, "y": "z"})));
        assert_eq!(args("anything word", &json!({})).unwrap_err(), "unexpected argument \"word\": name the arguments (key=value)");
        assert_eq!(run_of("anything word").args(None).unwrap_err().to_string(), "unexpected argument \"word\": name the arguments (key=value)");
        // A script has no declaration to read them by.
        assert_eq!(parsed("x.lua --name work").unwrap_err().to_string(), "unknown flag --name; see `silo do --help`");
        assert!(parsed("x.lua word").unwrap_err().to_string().contains("a script's arguments are key=value"));
        assert_eq!(args("x.lua --action name_tab work --loud", &one), Ok(json!({"name": "work", "loud": true})));
    }

    #[test]
    fn flags_parse() {
        assert!(matches!(parsed("window.step --session-id s1 --delta 1").unwrap(), Parsed::Method));
        assert!(matches!(parsed("--help").unwrap(), Parsed::Help));
        assert!(matches!(parsed("-h").unwrap(), Parsed::Help));
        let run = run_of("./x.lua -s work -b b1 --all --for 10m --fail-fast --json n=1");
        assert_eq!(run.target, Target::Script(Script::File("./x.lua".into())));
        assert_eq!((run.session.as_deref(), run.block.as_deref()), (Some("work"), Some("b1")));
        assert!(run.all && run.fail_fast && run.json);
        assert_eq!(run.run_for, Some(Duration::from_secs(600)));
        assert_eq!(run.pairs, [("n".to_owned(), "1".to_owned())]);
        let run = run_of("x.lua --session=work --for=30s");
        assert_eq!((run.session.as_deref(), run.run_for), (Some("work"), Some(Duration::from_secs(30))));
        // Flags before the target, and -e.
        let run = run_of("-s work layout.lua name=web");
        assert_eq!((run.target, run.session.as_deref()), (Target::Script(Script::File("layout.lua".into())), Some("work")));
        let Parsed::Run(run) = parse("-e", &["return silo.args".into(), "n=2".into()]).unwrap() else { panic!() };
        assert_eq!(run.target, Target::Script(Script::Eval("return silo.args".into())));
        assert_eq!(run.args(None).unwrap(), json!({"n": 2}));
        let run = run_of("- n=1");
        assert_eq!(run.target, Target::Script(Script::Stdin));
        let run = run_of("dev.lua --action dev_layout file=a");
        assert_eq!(run.action.as_deref(), Some("dev_layout"));
        let run = run_of("greet --help");
        assert!(run.help);
        // Everything after `--` is an argument.
        let run = run_of("x.lua -- -n=1");
        assert_eq!(run.pairs, [("-n".to_owned(), "1".to_owned())]);
    }

    #[test]
    fn bad_flags_are_refused() {
        for bad in [
            "greet --for 1m",
            "greet --all",
            "greet --fail-fast",
            "greet --action x",
            "x.lua --action Bad",
            "x.lua --action x --for 1m",
            "x.lua --nope",
            "x.lua --json=yes",
            "x.lua -s",
            "x.lua stray",
            "x.lua --for soon",
            "-s work",
            "-s work window.step",
            "Greet",
        ] {
            assert!(parsed(bad).is_err(), "{bad}");
        }
        assert!(parse("x.lua", &["-e".into(), "return 1".into()]).is_err(), "-e and a file");
    }

    #[test]
    fn wrapping_keeps_line_one_and_comments_a_shebang() {
        let wrapped = wrap("#!/usr/bin/env silo do\nreturn 1");
        assert_eq!(wrapped.lines().count(), 3);
        assert!(wrapped.lines().next().unwrap().ends_with("--#!/usr/bin/env silo do"));
        assert!(wrap("return 1").lines().next().unwrap().ends_with("return 1"));
    }

    /// The wrapped script runs as it would bare: its value, its errors'
    /// line numbers, and the handler count it reports.
    #[tokio::test]
    async fn a_wrapped_script_runs_as_written() {
        async fn go(code: &str) -> (silo_lua::Exit, Vec<u32>) {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let options = Options { mode: Mode::Script, args: json!({"n": 2}), log: Some(sink(Some(tx))), ..Options::default() };
            let (vm, loaded) = Vm::load(Source::Text { code: wrap(code) }, options).await;
            assert!(!loaded.failed, "{:?}", loaded.problems);
            vm.attach(Api::none(), None);
            let exit = tokio::time::timeout(Duration::from_secs(5), vm.finished()).await.unwrap();
            let mut tops = Vec::new();
            while let Ok(n) = rx.try_recv() {
                tops.push(n);
            }
            (exit, tops)
        }
        let (exit, tops) = go("local x = silo.args.n\nreturn { x = x * 2, args = ... == nil }").await;
        assert_eq!((exit.code, exit.value, tops), (0, json!({"x": 4, "args": true}), vec![0]));
        let (exit, _) = go("local a = 1\nerror('boom')").await;
        assert_eq!(exit.code, 1);
        assert!(exit.error.as_deref().is_some_and(|e| e.contains(":2:") && e.contains("boom")), "{exit:?}");
        let (exit, _) = go("silo.stop(3)").await;
        assert_eq!(exit.code, 3);
        let (exit, tops) = go("#!/bin/silo\nsilo.on('block.bell', function() end)\nsilo.after(0.05, function() silo.stop(4) end)").await;
        assert_eq!((exit.code, tops), (4, vec![1]));
    }
}
