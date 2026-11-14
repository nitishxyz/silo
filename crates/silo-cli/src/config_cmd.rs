//! `silo config`, `silo actions` and `silo keymap` (`docs/LUA.md` §5).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::Value;
use silo_client::Control;
use silo_proto::{
    config::{ActionInfo, ActionList, BindTarget, ConfigProblem, ConfigState, ProblemLevel},
    control::{Method, RpcError},
    paths,
};

/// `silo config …`.
#[derive(clap::Subcommand, Debug)]
pub enum ConfigCommand {
    /// Print the config file's path.
    Path,
    /// Load a config file here, with no server; print what it declares and
    /// its problems (exit 1 on an error).
    Check {
        /// The file (default: the config's path).
        file: Option<PathBuf>,
        /// Print what it declares as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Load the server's config again and print the new state.
    Reload {
        /// Print the state as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the server's loaded config.
    Show {
        /// Print the state as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Write a commented starter config if there is none.
    Init,
}

/// What `silo config init` writes (`docs/LUA.md` §4).
pub const TEMPLATE: &str = r#"-- Silo config: keys, actions and reactions, run by this machine's server.
-- Reference: https://github.com/nitishxyz/silo/blob/main/docs/LUA.md
-- `silo config check` checks it; the server reloads it when it changes.

-- Keys. A chord is ctrl-alt-shift-cmd-key; a sequence is chords separated by spaces.
-- silo.bind("cmd-e", "SplitRight")                     -- an app command (Settings › Keyboard lists them)
-- silo.bind("ctrl-b c", "window.create")               -- a control method (`silo api list`)
-- silo.bind("cmd-shift-h", "hello", { name = "you" })  -- the action below

-- An action: in ⌘P, bindable to a key, and `silo do hello Ana` from a terminal.
silo.action{
  name = "hello",
  title = "Say Hello",
  category = "Examples",
  args = { name = "string?" },
  run = function(ctx, args)
    return { greeting = "hello " .. (args.name or "there"), session_id = ctx.session_id }
  end,
}

-- A reaction: runs in the server, app open or not. This one logs failed commands
-- to the server's log; uncomment the next one to name tabs after their folder.
silo.on("block.command_finished", function(ctx, ev)
  if ev.record and ev.record.exit ~= nil and ev.record.exit ~= 0 then
    silo.log("info", "a command failed in block " .. tostring(ctx.block_id))
  end
end)
-- silo.on("block.cwd_changed", function(ctx, ev)
--   silo.window.rename{ window_id = ctx.window_id, name = ev.cwd:match("([^/]+)/?$") }
-- end)
"#;

/// The config's path on this machine's server.
#[must_use]
pub fn path() -> PathBuf {
    paths::config_file(&paths::home())
}

/// A [`ConfigState`] of what `silo_lua::check` loaded from `path`.
#[must_use]
pub fn state_of(path: &Path, exists: bool, loaded: silo_lua::Loaded) -> ConfigState {
    ConfigState {
        path: path.display().to_string(),
        exists,
        failed: loaded.failed,
        bindings: loaded.bindings,
        unbinds: loaded.unbinds,
        modes: loaded.modes,
        actions: loaded.actions,
        handlers: loaded.handlers,
        timers: loaded.timers,
        problems: loaded.problems,
        ..ConfigState::default()
    }
}

fn count(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// `warning init.lua:3: …`.
#[must_use]
pub fn problem_line(problem: &ConfigProblem) -> String {
    let level = match problem.level {
        ProblemLevel::Warning => "warning",
        ProblemLevel::Error => "error",
    };
    if problem.source.is_empty() { format!("{level} {}", problem.message) } else { format!("{level} {}: {}", problem.source, problem.message) }
}

/// Path, whether it exists, what it declares, then each problem.
#[must_use]
pub fn summary(state: &ConfigState) -> String {
    let mut out = format!("path:     {}\nexists:   {}\n", state.path, if state.exists { "yes" } else { "no" });
    if state.revision > 0 {
        let when = if state.loaded_at > 0 { format!(", loaded {}", crate::ago(state.loaded_at / 1000)) } else { String::new() };
        out.push_str(&format!("revision: {}{when}\n", state.revision));
    }
    if state.exists || state.revision > 0 {
        out.push_str(&format!(
            "declares: {}, {}, {}, {}, {}, {}\n",
            count(state.bindings.len(), "binding"),
            count(state.unbinds.len(), "removal"),
            count(state.modes.len(), "mode"),
            count(state.actions.len(), "action"),
            count(state.handlers.len(), "handler"),
            count(state.timers as usize, "timer"),
        ));
    }
    for problem in &state.problems {
        out.push_str(&problem_line(problem));
        out.push('\n');
    }
    out
}

fn has_error(state: &ConfigState) -> bool {
    state.failed || state.problems.iter().any(|p| p.level == ProblemLevel::Error)
}

fn print_state(state: &ConfigState, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(state)?);
    } else {
        print!("{}", summary(state));
    }
    Ok(())
}

/// `silo config check [FILE]`: exit 1 when it failed or has an error.
///
/// # Errors
/// Only output failures.
pub fn check(file: Option<&Path>, json: bool) -> anyhow::Result<i32> {
    let given = file.is_some();
    let path = match file {
        Some(file) => std::path::absolute(file).with_context(|| format!("{}", file.display()))?,
        None => path(),
    };
    if !path.exists() {
        let state = ConfigState { path: path.display().to_string(), ..ConfigState::default() };
        print_state(&state, json)?;
        if given {
            eprintln!("Error: no file {}", path.display());
            return Ok(1);
        }
        if !json {
            println!("no config; `silo config init` writes a starter one");
        }
        return Ok(0);
    }
    let state = state_of(&path, true, silo_lua::check(&path));
    print_state(&state, json)?;
    Ok(i32::from(has_error(&state)))
}

/// Explain `config.*`/`action.*` on a server without them.
fn explained(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<RpcError>().is_some_and(|e| e.code == RpcError::METHOD_NOT_FOUND) {
        return anyhow::anyhow!("this server does not load a config (it is older, or its config is off)");
    }
    error
}

/// `silo config reload`: exit 1 when the load failed.
///
/// # Errors
/// RPC failure.
pub async fn reload(control: &Control, json: bool) -> anyhow::Result<i32> {
    let state: ConfigState = control.call(Method::ConfigReload {}).await.map_err(explained)?;
    print_state(&state, json)?;
    if state.failed && !json {
        println!("the load failed; the previous config stays");
    }
    Ok(i32::from(state.failed))
}

/// `silo config show`.
///
/// # Errors
/// RPC failure.
pub async fn show(control: &Control, json: bool) -> anyhow::Result<i32> {
    let state: ConfigState = control.call(Method::ConfigGet {}).await.map_err(explained)?;
    print_state(&state, json)?;
    if !json {
        if state.failed {
            println!("the last load failed; the previous config stays");
        }
        println!("details: silo keymap, silo actions");
    }
    Ok(0)
}

/// `silo config init`: the template, unless a config exists.
///
/// # Errors
/// A config exists, or it cannot be written.
pub fn init() -> anyhow::Result<i32> {
    let path = path();
    if path.exists() {
        bail!("{} exists; not overwritten (`silo config check` checks it)", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).with_context(|| format!("write {}", path.display()))?;
    std::io::Write::write_all(&mut file, TEMPLATE.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    println!("{}", path.display());
    Ok(0)
}

/// Columns padded to their widest cell; the last one as it is.
fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            if c + 1 < row.len() {
                line.push_str(&format!("{cell:<width$}  ", width = widths[c]));
            } else {
                line.push_str(cell);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// An args schema in short: `file:string ratio:number?`; `any` when it
/// takes anything, empty when it takes nothing named.
#[must_use]
pub fn args_summary(schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return if schema.as_object().is_none_or(serde_json::Map::is_empty) { "any".into() } else { "json".into() };
    };
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    properties
        .iter()
        .map(|(name, sub)| {
            let ty = match sub.get("type") {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("|"),
                _ => "any".into(),
            };
            let optional = if required.contains(&name.as_str()) { "" } else { "?" };
            format!("{name}:{ty}{optional}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `<name> [--times <integer>]` for `silo do <action> --help`: the
/// argument a bare value fills, then the flags, the required first.
#[must_use]
pub fn args_usage(schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object).filter(|p| !p.is_empty()) else {
        return "[--name VALUE]…".into();
    };
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let sole = silo_proto::config::sole_arg(schema);
    let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
    names.sort_by_key(|n| (sole != Some(n), !required.contains(n), *n));
    let part = |name: &&str| {
        let ty = properties[*name].get("type").and_then(Value::as_str).unwrap_or("value");
        let flag = name.replace('_', "-");
        let text = match (sole == Some(name), ty) {
            (true, _) => format!("<{name}>"),
            (false, "boolean") => format!("--{flag}"),
            (false, _) => format!("--{flag} <{ty}>"),
        };
        if required.contains(name) { text } else { format!("[{text}]") }
    };
    names.iter().map(part).collect::<Vec<_>>().join(" ")
}

/// `silo actions`: the server's actions (`all`: the hidden ones too).
///
/// # Errors
/// RPC failure.
pub async fn actions(control: &Control, all: bool, json: bool) -> anyhow::Result<i32> {
    let list: ActionList = control.call(Method::ActionList {}).await.map_err(explained)?;
    let shown: Vec<&ActionInfo> = list.actions.iter().filter(|a| all || !a.hidden).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&shown)?);
        return Ok(0);
    }
    if shown.is_empty() {
        eprintln!("no actions; define one with silo.action{{…}} in {}", path().display());
        return Ok(0);
    }
    let mut rows = vec![["NAME", "TITLE", "CATEGORY", "ARGS", "SOURCE"].map(str::to_owned).to_vec()];
    for a in shown {
        rows.push(vec![a.name.clone(), a.title.clone(), a.category.clone(), args_summary(&a.args), a.source.clone()]);
    }
    print!("{}", table(&rows));
    Ok(0)
}

fn target_text(target: &BindTarget) -> String {
    match target {
        BindTarget::Command { id } => format!("command {id}"),
        BindTarget::Method { name } => format!("method {name}"),
        BindTarget::Action { name } => format!("action {name}"),
        BindTarget::Unknown => "unknown".into(),
    }
}

fn keys_text(mode: Option<&String>, keys: &str) -> String {
    match mode {
        Some(mode) => format!("{mode}/{keys}"),
        None => keys.to_owned(),
    }
}

/// `silo keymap`: the config's bindings, removals, modes and problems.
#[must_use]
pub fn keymap_text(state: &ConfigState) -> String {
    let mut out = String::new();
    if state.bindings.is_empty() {
        out.push_str("no bindings\n");
    } else {
        let mut rows = vec![["KEYS", "TARGET", "ARGS", "SOURCE"].map(str::to_owned).to_vec()];
        for b in &state.bindings {
            let args = match &b.args {
                Value::Null => String::new(),
                Value::Object(map) if map.is_empty() => String::new(),
                other => other.to_string(),
            };
            rows.push(vec![keys_text(b.mode.as_ref(), &b.keys), target_text(&b.target), args, b.source.clone()]);
        }
        out.push_str(&table(&rows));
    }
    if !state.unbinds.is_empty() {
        out.push_str("\nremoved:\n");
        let rows: Vec<Vec<String>> = state
            .unbinds
            .iter()
            .map(|u| {
                vec![
                    format!("  {}", keys_text(u.mode.as_ref(), &u.keys)),
                    u.target.clone().unwrap_or_else(|| "(every binding)".into()),
                    u.source.clone(),
                ]
            })
            .collect();
        out.push_str(&table(&rows));
    }
    if !state.modes.is_empty() {
        out.push_str("\nmodes:\n");
        let rows: Vec<Vec<String>> = state
            .modes
            .iter()
            .map(|m| vec![format!("  {}", m.name), if m.exclusive { "exclusive".into() } else { String::new() }, m.source.clone()])
            .collect();
        out.push_str(&table(&rows));
    }
    if !state.problems.is_empty() {
        out.push('\n');
        for problem in &state.problems {
            out.push_str(&problem_line(problem));
            out.push('\n');
        }
    }
    out
}

/// `silo keymap`.
///
/// # Errors
/// RPC failure.
pub async fn keymap(control: &Control, json: bool) -> anyhow::Result<i32> {
    let state: ConfigState = control.call(Method::ConfigGet {}).await.map_err(explained)?;
    if json {
        let value = serde_json::json!({
            "bindings": state.bindings,
            "unbinds": state.unbinds,
            "modes": state.modes,
            "problems": state.problems,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", keymap_text(&state));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use silo_proto::config::{KeyBinding, KeyMode};

    use super::*;

    #[test]
    fn the_template_loads_without_problems() {
        let dir = std::env::temp_dir().join(format!("silo-cli-template-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("init.lua");
        std::fs::write(&file, TEMPLATE).unwrap();
        let loaded = silo_lua::check(&file);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!loaded.failed && loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.actions.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["hello"]);
        assert_eq!(loaded.handlers.len(), 1);
        assert!(loaded.bindings.is_empty(), "the bindings are commented out");
    }

    #[test]
    fn summaries_count_and_list_problems() {
        let state = ConfigState {
            path: "/c/init.lua".into(),
            exists: true,
            bindings: vec![KeyBinding {
                keys: "left".into(),
                mode: Some("resize".into()),
                target: BindTarget::Command { id: "MovePaneEdgeLeft".into() },
                args: Value::Null,
                source: "init.lua:2".into(),
            }],
            modes: vec![KeyMode { name: "resize".into(), exclusive: true, source: "init.lua:1".into() }],
            problems: vec![ConfigProblem { level: ProblemLevel::Warning, source: "init.lua:3".into(), message: "odd".into() }],
            ..ConfigState::default()
        };
        let text = summary(&state);
        assert!(text.contains("declares: 1 binding, 0 removals, 1 mode, 0 actions, 0 handlers, 0 timers"), "{text}");
        assert!(text.ends_with("warning init.lua:3: odd\n"), "{text}");
        assert!(!has_error(&state));
        let keymap = keymap_text(&state);
        assert!(keymap.contains("resize/left  command MovePaneEdgeLeft") && keymap.contains("resize  exclusive  init.lua:1"), "{keymap}");
    }

    #[test]
    fn args_in_short() {
        let schema = json!({"type": "object", "properties": {"file": {"type": "string"}, "ratio": {"type": "number"}}, "required": ["file"]});
        assert_eq!(args_summary(&schema), "file:string ratio:number?");
        assert_eq!(args_usage(&schema), "<file> [--ratio <number>]", "a bare value is the one required argument");
        let two =
            json!({"properties": {"dry_run": {"type": "boolean"}, "to": {"type": "string"}, "from": {"type": "string"}}, "required": ["to", "from"]});
        assert_eq!(args_usage(&two), "--from <string> --to <string> [--dry-run]");
        assert_eq!(args_usage(&json!({})), "[--name VALUE]…");
        assert_eq!(args_summary(&json!({})), "any");
        assert_eq!(args_summary(&Value::Null), "any");
        assert_eq!(args_summary(&json!({"type": "object", "properties": {}})), "");
    }
}
