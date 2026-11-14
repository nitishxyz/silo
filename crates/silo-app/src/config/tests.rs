use serde_json::json;
use silo_proto::keys::Sequence;

use super::*;

fn config(value: Value) -> ConfigState {
    serde_json::from_value(value).unwrap()
}

fn with_actions(names: &[&str]) -> ConfigState {
    ConfigState { actions: names.iter().map(|n| action(n, "", false)).collect(), ..ConfigState::default() }
}

fn action(name: &str, category: &str, hidden: bool) -> ActionInfo {
    serde_json::from_value(json!({ "name": name, "title": name.to_uppercase(), "category": category, "hidden": hidden })).unwrap()
}

fn bound(target: Target, args: Value) -> Bound {
    Bound { mode: None, keys: Sequence::parse("cmd-e").unwrap(), target, args }
}

fn focus() -> Focus {
    Focus { session_id: Some("s1".into()), window_id: Some("w1".into()), block_id: Some("b1".into()) }
}

#[test]
fn commands_and_app_methods_run_here() {
    assert_eq!(resolve(&bound(Target::Command(Command::NewTab), Value::Null), &focus(), None, None), Run::Command(Command::NewTab));
    assert_eq!(resolve(&bound(Target::Blocked, Value::Null), &focus(), None, None), Run::Nothing);
    let enter = resolve(&bound(Target::Method("app.mode_enter".into()), json!({ "name": "resize" })), &focus(), None, None);
    assert_eq!(enter, Run::App(json!({ "method": "app.mode_enter", "params": { "name": "resize" } })));
    let Run::App(call) = enter else { unreachable!() };
    assert!(matches!(serde_json::from_value::<Method>(call), Ok(Method::AppModeEnter { name, once: false, .. }) if name == "resize"));
    let exit = resolve(&bound(Target::Method("app.mode_exit".into()), Value::Null), &focus(), None, None);
    let Run::App(call) = exit else { panic!("{exit:?}") };
    assert!(matches!(serde_json::from_value::<Method>(call), Ok(Method::AppModeExit { all: false, .. })), "no args is {{}}");
}

#[test]
fn methods_get_the_focus_ids_they_take_and_leave_out() {
    let call = |name: &str, args: Value| resolve(&bound(Target::Method(name.into()), args), &focus(), None, None);
    assert_eq!(
        call("block.zoom", json!({ "zoomed": true })),
        Run::Call { method: "block.zoom".into(), params: json!({ "zoomed": true, "block_id": "b1" }) }
    );
    assert_eq!(
        call("block.zoom", json!({ "block_id": "b9" })),
        Run::Call { method: "block.zoom".into(), params: json!({ "block_id": "b9" }) },
        "given ids are kept"
    );
    assert_eq!(call("session.view", Value::Null), Run::Call { method: "session.view".into(), params: json!({ "session_id": "s1" }) });
    assert_eq!(
        call("later.method", json!({ "x": 1 })),
        Run::Call { method: "later.method".into(), params: json!({ "x": 1 }) },
        "a method this build doesn't know goes as given"
    );
    assert_eq!(fill_focus("session.view", json!({}), &Focus::default()), json!({}), "no focus, nothing filled");
}

#[test]
fn actions_run_on_the_focused_host_else_the_home_server() {
    let here = with_actions(&["build"]);
    let home = with_actions(&["build", "dev_layout"]);
    let run = |name: &str, here: Option<&ConfigState>, home: Option<&ConfigState>| {
        resolve(&bound(Target::Action(name.into()), json!({ "file": "a" })), &focus(), here, home)
    };
    let Run::Action { on, params } = run("build", Some(&here), Some(&home)) else { panic!() };
    assert_eq!(on, On::Here);
    assert_eq!((params.session_id.as_deref(), params.block_id.as_deref()), (Some("s1"), Some("b1")));
    assert_eq!((params.args, params.origin), (json!({ "file": "a" }), Some(ActionOrigin::Key)));
    let Run::Action { on, params } = run("dev_layout", Some(&here), Some(&home)) else { panic!() };
    assert_eq!(on, On::Home);
    assert_eq!((params.session_id, params.window_id, params.block_id), (None, None, None), "the focus is on another host");
    assert_eq!(run("nope", Some(&here), Some(&home)), Run::Missing("No action named “nope”".into()));
    assert_eq!(run("build", None, None), Run::Missing("No action named “build”".into()), "no configs: a share link, an older server");
    assert_eq!(action_on("dev_layout", None, Some(&home)), Some(On::Home));
}

#[test]
fn the_args_prompt_reads_key_value_words() {
    let schema = json!({
        "type": "object",
        "properties": { "ratio": { "type": "number" }, "file": { "type": "string" }, "on": { "type": "boolean" } },
        "required": ["file", "ratio"],
    });
    assert_eq!(required_args(&schema), ["file", "ratio"]);
    assert_eq!(args_placeholder(&schema), "file=… ratio=… on=…", "the required first, then by name");
    assert_eq!(args_placeholder(&json!({})), "key=value …");
    assert_eq!(parse_args("file=app.log ratio=0.4 on=true", &schema), Ok(json!({ "file": "app.log", "ratio": 0.4, "on": true })));
    assert_eq!(parse_args("file=42 ratio=1", &schema), Ok(json!({ "file": "42", "ratio": 1 })), "a string where the schema says so");
    assert_eq!(parse_args(r#"file="my notes.md" ratio=2 x=null"#, &schema), Ok(json!({ "file": "my notes.md", "ratio": 2, "x": null })));
    assert_eq!(parse_args("ratio=0.4", &schema), Err("Missing file".into()));
    assert_eq!(parse_args("file", &schema), Err("“file” isn't key=value".into()));
    assert_eq!(parse_args("", &json!({})), Ok(json!({})));
    assert_eq!(parse_args("v= 1", &json!({})), Err("“1” isn't key=value".into()));
}

#[test]
fn one_required_arg_is_typed_alone() {
    let schema = json!({
        "type": "object",
        "properties": { "name": { "type": "string" }, "times": { "type": "integer" } },
        "required": ["name"],
    });
    assert_eq!(args_placeholder(&schema), "name (optional: times=…)");
    assert_eq!(args_placeholder(&json!({ "properties": { "name": { "type": "string" } }, "required": ["name"] })), "name");
    assert_eq!(parse_args("work", &schema), Ok(json!({ "name": "work" })));
    assert_eq!(parse_args("  my new tab ", &schema), Ok(json!({ "name": "my new tab" })), "the words are one value");
    assert_eq!(parse_args("42", &schema), Ok(json!({ "name": "42" })), "a string where the schema says so");
    assert_eq!(parse_args("a=b", &schema), Ok(json!({ "name": "a=b" })), "not a declared name: part of the value");
    assert_eq!(parse_args("work times=2", &schema), Ok(json!({ "name": "work", "times": 2 })));
    assert_eq!(parse_args("name=work times=2", &schema), Ok(json!({ "name": "work", "times": 2 })), "key=value still works");
    assert_eq!(parse_args("name=work again", &schema), Err("“again” isn't key=value (name is already given)".into()));
    assert_eq!(parse_args("times=2", &schema), Err("Missing name".into()));
    assert_eq!(parse_args("", &schema), Err("Missing name".into()));
    let number = json!({ "properties": { "n": { "type": "integer" } }, "required": ["n"] });
    assert_eq!(parse_args("3", &number), Ok(json!({ "n": 3 })));
}

#[test]
fn palette_actions_are_deduplicated_visible_and_grouped() {
    let mut here = with_actions(&[]);
    here.actions = vec![action("deploy", "Project", false), action("bind_1", "", true), action("notes", "", false)];
    let mut home = with_actions(&[]);
    home.actions = vec![action("deploy", "Home", false), action("dev_layout", "Project", false), action("backup", "Home", false)];
    let listed: Vec<(usize, &str, &str)> =
        palette_actions(&[(2, &here), (0, &home)]).into_iter().map(|(h, a)| (h, a.name.as_str(), a.category.as_str())).collect();
    assert_eq!(listed, [(0, "backup", "Home"), (2, "deploy", "Project"), (0, "dev_layout", "Project"), (2, "notes", "")]);
    let item = action_item(2, &here.actions[0], Some("mini"), Some("⌘K".into()));
    assert_eq!((item.text.as_str(), item.key.as_str(), item.detail.as_deref()), ("DEPLOY", "action:deploy", Some("mini")));
    assert_eq!(item.group, finder::Group::Action(category_id("Project")));
    assert_eq!(item.group.label(), "Project");
    assert_eq!(finder::Group::Action(category_id("  ")).label(), "Actions");
    assert_eq!(item.right, Some(finder::Right::Shortcut("⌘K".into())));
    assert!(item.keywords.contains("deploy"));
}

#[test]
fn a_config_parses_from_config_get() {
    let state = config(json!({ "revision": 3, "actions": [{ "name": "open_file", "title": "Open in Zed" }] }));
    assert_eq!(action_on("open_file", Some(&state), None), Some(On::Here));
    assert_eq!(action_on("open_url", Some(&state), None), None, "no round trip for an undefined reserved action");
}
