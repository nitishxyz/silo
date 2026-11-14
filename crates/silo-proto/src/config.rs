//! Lua config and actions (`docs/LUA.md` §7, feature `config`): what a
//! server's `init.lua` declares, and running its actions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{BlockId, ClientId, SessionId, WindowId};

/// The config file's name (see [`crate::paths::config_file`]).
pub const CONFIG_FILE: &str = "init.lua";

/// Result of `config.get` and `config.reload`: what the loaded config
/// declares.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigState {
    /// Bumps on every applied load.
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub exists: bool,
    /// Unix ms; 0: never.
    #[serde(default)]
    pub loaded_at: u64,
    /// The last load failed: this is the previous good state plus its
    /// problems.
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub bindings: Vec<KeyBinding>,
    #[serde(default)]
    pub unbinds: Vec<KeyUnbind>,
    #[serde(default)]
    pub modes: Vec<KeyMode>,
    #[serde(default)]
    pub actions: Vec<ActionInfo>,
    #[serde(default)]
    pub handlers: Vec<HandlerInfo>,
    #[serde(default)]
    pub timers: u32,
    #[serde(default)]
    pub problems: Vec<ConfigProblem>,
}

/// `silo.bind(keys, target, args)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct KeyBinding {
    /// A sequence of chords (`ctrl-b c`), without the mode.
    pub keys: String,
    #[serde(default)]
    pub mode: Option<String>,
    pub target: BindTarget,
    #[serde(default)]
    pub args: Value,
    /// Where it was declared (`init.lua:12`).
    #[serde(default)]
    pub source: String,
}

/// What a binding runs, decided by its spelling (`docs/LUA.md` §4.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BindTarget {
    /// An app command, by id (`SplitRight`).
    Command { id: String },
    /// A control method (`window.create`; `app.*` runs in the client).
    Method { name: String },
    /// An action of the config.
    Action { name: String },
    /// A newer target; the binding is skipped.
    #[serde(other)]
    Unknown,
}

/// `silo.unbind(keys[, target])`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KeyUnbind {
    pub keys: String,
    #[serde(default)]
    pub mode: Option<String>,
    /// Only this binding (a command id, method or action); `None`: every
    /// binding of the keys, the app's defaults too.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub source: String,
}

/// `silo.mode(name, {exclusive})`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KeyMode {
    pub name: String,
    /// Keys the mode does not bind do nothing while it is active.
    #[serde(default)]
    pub exclusive: bool,
    #[serde(default)]
    pub source: String,
}

/// `silo.action{…}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ActionInfo {
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// JSON Schema of the args; `{}` for any.
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub repeats: bool,
    /// Not listed in ⌘P (a function bound to a key).
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub source: String,
}

/// `silo.on(event, fn)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HandlerInfo {
    pub event: String,
    #[serde(default)]
    pub source: String,
}

/// Something wrong with the config, found while loading it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigProblem {
    pub level: ProblemLevel,
    #[serde(default)]
    pub source: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProblemLevel {
    Warning,
    Error,
}

/// Params of `action.run`. The ids are where it runs from (the focus);
/// absent when unknown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ActionRunParams {
    pub name: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<WindowId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<BlockId>,
    /// The client whose `app.*` calls the action makes (`silo.app.*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<ClientId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ActionOrigin>,
}

/// What started an action (`ctx.origin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActionOrigin {
    Key,
    Palette,
    Cli,
    Api,
    Script,
    Event,
    Timer,
    #[serde(other)]
    Unknown,
}

/// Result of `action.run`: what `run` returned, as JSON (`null`: nothing).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ActionResult {
    #[serde(default)]
    pub result: Value,
}

/// Result of `action.list`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ActionList {
    #[serde(default)]
    pub actions: Vec<ActionInfo>,
}

/// The argument a bare value fills (`silo do name_tab work`, a value typed
/// alone in the palette's prompt): the only one `schema` requires, else its
/// only one. None when that is ambiguous or the schema declares none.
#[must_use]
pub fn sole_arg(schema: &Value) -> Option<&str> {
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    match required.as_slice() {
        [one] => Some(one),
        [] => match schema.get("properties").and_then(Value::as_object)?.keys().collect::<Vec<_>>().as_slice() {
            [one] => Some(one.as_str()),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_bare_value_fills_the_one_argument_there_is() {
        let props = |required: Value| json!({"type": "object", "properties": {"name": {"type": "string"}, "times": {"type": "integer"}}, "required": required});
        assert_eq!(sole_arg(&props(json!(["name"]))), Some("name"), "the only required one");
        assert_eq!(sole_arg(&props(json!(["name", "times"]))), None, "two required");
        assert_eq!(sole_arg(&props(json!([]))), None, "two optional");
        assert_eq!(sole_arg(&json!({"properties": {"name": {"type": "string"}}})), Some("name"), "the only one, optional");
        assert_eq!(sole_arg(&json!({})), None, "anything goes");
    }

    #[test]
    fn targets_and_origins_read_newer_variants_as_unknown() {
        let target: BindTarget = serde_json::from_value(json!({"type": "command", "id": "SplitRight"})).unwrap();
        assert_eq!(target, BindTarget::Command { id: "SplitRight".into() });
        let target: BindTarget = serde_json::from_value(json!({"type": "macro", "steps": []})).unwrap();
        assert_eq!(target, BindTarget::Unknown);
        assert_eq!(serde_json::to_value(ActionOrigin::Palette).unwrap(), "palette");
        assert_eq!(serde_json::from_value::<ActionOrigin>(json!("voice")).unwrap(), ActionOrigin::Unknown);
        assert_eq!(serde_json::to_value(ProblemLevel::Warning).unwrap(), "warning");
    }

    #[test]
    fn a_sparse_state_parses() {
        let state: ConfigState = serde_json::from_value(json!({"revision": 2, "later": true})).unwrap();
        assert_eq!(state, ConfigState { revision: 2, ..ConfigState::default() });
        let run: ActionRunParams = serde_json::from_value(json!({"name": "dev_layout"})).unwrap();
        assert_eq!(run.args, Value::Null);
        assert_eq!(serde_json::to_value(&run).unwrap(), json!({"name": "dev_layout", "args": null}));
    }
}
