//! `api.describe`: JSON Schemas (draft 2020-12) and an example request for
//! every control method.

use schemars::{Schema, schema_for};
use serde_json::{Value, json};

use crate::access::{Rights, Scope};
use crate::agent::*;
use crate::control::{
    ApiMethod, AppResult, BlockView, BusyBlocks, Capture, CommandOutput, CommandRecords, Created, DataTicket, Density, Direction, Dock, Empty,
    GrantCreated, GrantKind, Grants, InvitePurpose, LogText, METHODS, Method, MethodDescription, OwnMode, PairInvite, PairedDevices, Pasted,
    ServerInfo, SizeInfo, SpawnSpec,
};
use crate::data::{ClipKind, ClipboardLocation};
use crate::model::{Axis, BlockKind, FloatRect, HistoryEntry, SessionSummary, SessionView, TermSize};

/// The description of method `name`; `None` for an unknown method.
#[must_use]
pub fn describe(name: &str) -> Option<MethodDescription> {
    let (_, description) = METHODS.iter().find(|(n, _)| *n == name)?;
    let method = examples().into_iter().find(|m| method_name(m) == name)?;
    let example = serde_json::to_value(&method).ok()?;
    Some(MethodDescription {
        description: (*description).to_owned(),
        params_schema: params_schema(name)?,
        result_schema: result_schema(&method).to_value(),
        access: Some(crate::access::access(name, &method)),
        example,
    })
}

/// The `method` tag `m` serializes with.
#[must_use]
pub fn method_name(m: &Method) -> String {
    serde_json::to_value(m).ok().and_then(|v| v["method"].as_str().map(str::to_owned)).unwrap_or_default()
}

/// Every method tag in [`Method`]'s schema.
#[must_use]
pub fn schema_names() -> Vec<String> {
    variants(&schema_for!(Method).to_value()).filter_map(|v| tag(v).map(str::to_owned)).collect()
}

fn variants(root: &Value) -> impl Iterator<Item = &Value> {
    ["oneOf", "anyOf"].into_iter().filter_map(|k| root.get(k)?.as_array()).flatten()
}

fn tag(variant: &Value) -> Option<&str> {
    let method = variant.get("properties")?.get("method")?;
    method.get("const").or_else(|| method.get("enum")?.get(0))?.as_str()
}

/// The `params` subschema of variant `name`, with the `$defs` it reaches.
fn params_schema(name: &str) -> Option<Value> {
    let root = schema_for!(Method).to_value();
    let variant = variants(&root).find(|v| tag(v) == Some(name))?;
    let mut params = variant.get("properties")?.get("params").cloned().unwrap_or_else(|| json!({"type": "object"}));
    if let Some(reference) = params.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix('#')) {
        params = root.pointer(reference)?.clone();
    }
    if let Some(defs) = root.get("$defs").and_then(Value::as_object) {
        let reached = reachable(&params, defs);
        if !reached.is_empty() {
            params.as_object_mut()?.insert("$defs".into(), Value::Object(reached));
        }
    }
    params.as_object_mut()?.insert("$schema".into(), root["$schema"].clone());
    Some(params)
}

/// The definitions `schema` refers to (`#/$defs/…`), transitively.
fn reachable(schema: &Value, defs: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    fn refs<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::Object(map) => {
                if let Some(name) = map.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix("#/$defs/")) {
                    out.push(name);
                }
                map.values().for_each(|v| refs(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }
    let mut reached = serde_json::Map::new();
    let mut todo = Vec::new();
    refs(schema, &mut todo);
    while let Some(name) = todo.pop() {
        if reached.contains_key(name) {
            continue;
        }
        if let Some(def) = defs.get(name) {
            refs(def, &mut todo);
            reached.insert(name.to_owned(), def.clone());
        }
    }
    reached
}

/// The schema of `method`'s result.
fn result_schema(method: &Method) -> Schema {
    use Method as M;
    match method {
        M::BlockReport(_) | M::BlockNotify(_) => schema_for!(Empty),
        M::BrowserSnapshot(_) => schema_for!(BrowserSnapshotResult),
        M::BrowserFind(_) => schema_for!(BrowserFindResult),
        M::BrowserWaitFor(_) => schema_for!(BrowserWaitForResult),
        M::BrowserEval(_) => schema_for!(BrowserEvalResult),
        M::BrowserConsole(_) => schema_for!(BrowserConsoleResult),
        M::BrowserNetwork(_) => schema_for!(BrowserNetworkResult),
        M::BrowserScreenshot(_) => schema_for!(BrowserScreenshotResult),
        M::BrowserClick(_)
        | M::BrowserHover(_)
        | M::BrowserFill(_)
        | M::BrowserFillForm(_)
        | M::BrowserType(_)
        | M::BrowserPress(_)
        | M::BrowserScroll(_)
        | M::BrowserDialog(_) => schema_for!(BrowserAction),
        M::ServerInfo {} => schema_for!(ServerInfo),
        M::ApiList {} => schema_for!(Vec<ApiMethod>),
        M::ApiDescribe { .. } => schema_for!(MethodDescription),
        M::SessionList {} => schema_for!(Vec<SessionSummary>),
        M::SessionCreate { .. } | M::WindowCreate { .. } | M::BlockSplit { .. } => {
            schema_for!(Created)
        }
        M::SessionView { .. } => schema_for!(SessionView),
        M::TerminalCapture { .. } => schema_for!(Capture),
        M::TerminalSize { .. } => schema_for!(SizeInfo),
        M::ClipboardRead { .. } => schema_for!(Pasted),
        M::ClipboardWrite { .. } => schema_for!(Empty),
        M::DataConnect { .. } => schema_for!(DataTicket),
        M::PairCreate { .. } => schema_for!(PairInvite),
        M::PairList {} => schema_for!(PairedDevices),
        M::ServerLog { .. } => schema_for!(LogText),
        M::BlockView { .. } => schema_for!(BlockView),
        M::BlocksBusy { .. } => schema_for!(BusyBlocks),
        M::BlockCommands { .. } => schema_for!(CommandRecords),
        M::TerminalCommandOutput { .. } => schema_for!(CommandOutput),
        M::GrantCreate { .. } => schema_for!(GrantCreated),
        M::GrantList {} => schema_for!(Grants),
        M::ServerShutdown { .. }
        | M::ServerRestart { .. }
        | M::Ping {}
        | M::EventsSubscribe {}
        | M::SessionRename { .. }
        | M::SessionClose { .. } => {
            schema_for!(Empty)
        }
        M::WindowSelect { .. } | M::WindowStep { .. } | M::WindowClose { .. } | M::WindowRename { .. } | M::WindowMove { .. } => schema_for!(Empty),
        M::BlockClose { .. }
        | M::BlockFocus { .. }
        | M::BlockFocusStep { .. }
        | M::BlockFocusDir { .. }
        | M::BlockZoom { .. }
        | M::BlockSwap { .. } => {
            schema_for!(Empty)
        }
        M::BlockFloat { .. }
        | M::BlockMove { .. }
        | M::LayoutRatio { .. }
        | M::LayoutGrow { .. }
        | M::LayoutEdge { .. }
        | M::LayoutEqualize { .. } => {
            schema_for!(Empty)
        }
        M::BlockStatusSeen { .. } | M::GrantRevoke { .. } | M::ApprovalAnswer { .. } | M::AppReply { .. } => schema_for!(Empty),
        M::TerminalSend { .. } | M::TerminalResize { .. } | M::TerminalClear { .. } | M::TerminalReset { .. } => schema_for!(Empty),
        M::OwnBlock { .. } | M::DisownBlock { .. } | M::UiOpenSession { .. } | M::PairRevoke { .. } => schema_for!(Empty),
        M::BrowserNavigate { .. } | M::BrowserBack { .. } | M::BrowserForward { .. } | M::BrowserReload { .. } => schema_for!(Empty),
        M::BrowserZoom { .. } | M::BrowserReport { .. } | M::AgentSet { .. } => schema_for!(Empty),
        M::HostOffer { .. } => schema_for!(Empty),
        M::AppWindowNew { .. }
        | M::AppWindowCycle { .. }
        | M::AppSwitcher { .. }
        | M::AppSessionStep { .. }
        | M::AppHostsAdd { .. }
        | M::AppHostsRename { .. }
        | M::AppHostsRemove { .. }
        | M::AppHostsList { .. }
        | M::AppCopy { .. }
        | M::AppPaste { .. }
        | M::AppSelectAll { .. }
        | M::AppAppearance { .. }
        | M::AppPaneHeaders { .. }
        | M::AppFullScreen { .. }
        | M::AppSettingsOpen { .. }
        | M::AppSettingsReload { .. }
        | M::AppSettingsOpenFile { .. }
        | M::AppSettingsGet { .. }
        | M::AppSettingsSet { .. }
        | M::AppUpdateCheck { .. }
        | M::AppPalette { .. }
        | M::AppHide { .. }
        | M::AppHideOthers { .. }
        | M::AppShowAll { .. }
        | M::AppQuit { .. }
        | M::AppJumpPrompt { .. }
        | M::AppSelectOutput { .. }
        | M::AppCopyOutput { .. }
        | M::AppOpenShare { .. } => schema_for!(AppResult),
    }
}

/// One example request per method, in [`METHODS`] order.
#[must_use]
pub fn examples() -> Vec<Method> {
    use Method as M;
    let (s, w, b) = (|| "s1a2b3c4d".to_owned(), || "w1a2b3c4d".to_owned(), || "b1a2b3c4d".to_owned());
    let shell = || SpawnSpec { cwd: Some("~/src".into()), command: Vec::new(), size: Some(TermSize::new(120, 40)) };
    vec![
        M::BlockReport(BlockReportParams {
            block_id: b(),
            id: String::new(),
            state: crate::model::StatusState::Working,
            kind: None,
            progress: Some(50),
            app: Some("agent".into()),
            title: None,
            message: None,
        }),
        M::BlockNotify(BlockNotifyParams { block_id: b(), title: None, body: "Finished".into() }),
        M::BrowserSnapshot(BrowserSnapshotParams { block_id: b(), within: None, depth: None, boxes: None }),
        M::BrowserFind(BrowserFindParams { block_id: b(), text: Some("Submit".into()), regex: None }),
        M::BrowserClick(BrowserClickParams { block_id: b(), target: "e12".into(), double: None, button: None, include_snapshot: false }),
        M::BrowserHover(BrowserHoverParams { block_id: b(), target: "e12".into(), include_snapshot: false }),
        M::BrowserFill(BrowserFillParams { block_id: b(), target: "e5".into(), value: "hello".into(), include_snapshot: false }),
        M::BrowserFillForm(BrowserFillFormParams {
            block_id: b(),
            fields: vec![BrowserField { target: "e5".into(), value: "hello".into() }],
            include_snapshot: false,
        }),
        M::BrowserType(BrowserTypeParams { block_id: b(), text: "hello".into(), submit: Some("Enter".into()), include_snapshot: false }),
        M::BrowserPress(BrowserPressParams { block_id: b(), key: "Enter".into(), include_snapshot: false }),
        M::BrowserScroll(BrowserScrollParams { block_id: b(), target: None, dx: None, dy: Some(500.0), include_snapshot: false }),
        M::BrowserWaitFor(BrowserWaitForParams { block_id: b(), text: Some(vec!["Ready".into()]), gone: None, timeout_ms: Some(1000) }),
        M::BrowserDialog(BrowserDialogParams { block_id: b(), accept: true, prompt_text: None, include_snapshot: false }),
        M::BrowserEval(BrowserEvalParams { block_id: b(), function: "() => document.title".into(), args: None }),
        M::BrowserConsole(BrowserConsoleParams { block_id: b(), since: None, level: None, limit: Some(100) }),
        M::BrowserNetwork(BrowserNetworkParams { block_id: b(), since: None, limit: Some(100), failed_only: None }),
        M::BrowserScreenshot(BrowserScreenshotParams {
            block_id: b(),
            target: None,
            full_page: None,
            format: Some(ScreenshotFormat::Png),
            quality: None,
        }),
        M::ServerInfo {},
        M::ServerShutdown { force: false },
        M::ServerRestart { force: false },
        M::ServerLog { lines: Some(200) },
        M::ApiList {},
        M::ApiDescribe { method: "block.split".into() },
        M::Ping {},
        M::EventsSubscribe {},
        M::SessionList {},
        M::SessionCreate { name: Some("work".into()), spawn: shell(), kind: BlockKind::Terminal, url: None },
        M::SessionView { session_id: s() },
        M::SessionRename { session_id: s(), name: "play".into() },
        M::SessionClose { session_id: s(), force: false },
        M::WindowCreate { session_id: s(), spawn: SpawnSpec::default(), kind: BlockKind::Browser, url: Some("localhost:3000".into()) },
        M::WindowSelect { session_id: s(), window_id: w() },
        M::WindowStep { session_id: s(), delta: 1 },
        M::WindowClose { window_id: w(), force: false },
        M::WindowRename { window_id: w(), name: Some("logs".into()) },
        M::WindowMove { window_id: w(), index: 0 },
        M::BlockView { block_id: b() },
        M::BlockSplit { block_id: b(), axis: Axis::Horizontal, spawn: shell(), kind: BlockKind::Terminal, url: None },
        M::BlockClose { block_id: b(), force: false },
        M::BlockFocus { block_id: b() },
        M::BlockFocusStep { block_id: b(), delta: -1 },
        M::BlockFocusDir { block_id: b(), direction: Direction::Left },
        M::BlockZoom { block_id: b(), zoomed: true },
        M::BlockSwap { block_id: b(), other_block_id: "b5e6f7a8b".into() },
        M::BlockFloat { block_id: b(), rect: Some(FloatRect::default()) },
        M::BlockMove { block_id: b(), rect: None, dock: Some(Dock { target: "b5e6f7a8b".into(), axis: Axis::Vertical }) },
        M::LayoutRatio { window_id: w(), path: vec![1], ratio: 0.4 },
        M::LayoutGrow { block_id: b(), delta: 0.05 },
        M::LayoutEdge { block_id: b(), direction: Direction::Right },
        M::LayoutEqualize { window_id: w() },
        M::BlocksBusy { session_id: Some(s()), window_id: None, block_ids: None },
        M::BlockStatusSeen { block_id: b(), ids: None },
        M::BlockCommands { block_id: b(), limit: Some(20) },
        M::TerminalSend { block_id: b(), text: "ls\r".into() },
        M::TerminalCapture { block_id: b(), history: false },
        M::TerminalSize { block_id: b() },
        M::TerminalResize { block_id: b(), size: TermSize { columns: 120, rows: 40, cell_width: 16, cell_height: 34 } },
        M::TerminalClear { block_id: b() },
        M::TerminalReset { block_id: b() },
        M::TerminalCommandOutput { block_id: b(), index: None },
        M::ClipboardRead { block_id: b(), kind: ClipKind::Image },
        M::ClipboardWrite { block_id: b(), location: ClipboardLocation::Standard, text: "hello".into() },
        M::OwnBlock { block_ids: vec![b()], mode: OwnMode::Take },
        M::DisownBlock { block_ids: vec![b()] },
        M::DataConnect { block_id: b() },
        M::BrowserNavigate { block_id: b(), url: "example.com".into() },
        M::BrowserBack { block_id: b() },
        M::BrowserForward { block_id: b() },
        M::BrowserReload { block_id: b(), ignore_cache: true },
        M::BrowserZoom { block_id: b(), zoom: 1.0 },
        M::BrowserReport {
            block_id: b(),
            url: "https://example.com/".into(),
            title: "Example Domain".into(),
            favicon: None,
            loading: false,
            history: vec![HistoryEntry { url: "https://example.com/".into(), title: "Example Domain".into() }],
            index: 0,
        },
        M::AgentSet { block_id: b(), session_id: Some("ses_01".into()), title: Some("Fix the lockfile".into()) },
        M::UiOpenSession { session_id: s() },
        M::PairCreate { ttl_secs: 600, purpose: InvitePurpose::Device },
        M::PairList {},
        M::PairRevoke { id: "7c3e5f0d9a1b4c6e8f2a0b1c3d5e7f9a1b3c5d7e9f0a2b4c6d8e0f1a3b5c7d9e".into() },
        M::HostOffer { ticket: "silo1aeaqcaibaeaq".into(), name: "mini".into() },
        M::GrantCreate {
            scope: Scope::Session { session_id: s() },
            rights: Rights::Interact,
            kind: GrantKind::Token,
            ttl_secs: Some(8 * 3600),
            label: Some("claude-code".into()),
        },
        M::GrantList {},
        M::GrantRevoke { id: "3f9a1c0d5e7b2a46".into() },
        M::ApprovalAnswer { id: "a1b2c3d4e5f60718".into(), allow: true },
        M::AppReply { request_id: "r1b2c3d4e5f60718".into(), result: Some(json!({})), error: None },
        M::AppWindowNew { session_id: Some(s()), client_id: None },
        M::AppWindowCycle { client_id: None },
        M::AppSwitcher { client_id: None },
        M::AppSessionStep { delta: 1, client_id: None },
        M::AppHostsAdd { ticket: Some("silo1aeaqcaibaeaq".into()), client_id: None },
        M::AppHostsRename { host: "mini".into(), name: "studio".into(), client_id: None },
        M::AppHostsRemove { host: "mini".into(), client_id: None },
        M::AppHostsList { client_id: None },
        M::AppCopy { block_id: Some(b()), client_id: None },
        M::AppPaste { block_id: None, client_id: None },
        M::AppSelectAll { block_id: None, client_id: None },
        M::AppAppearance { density: Density::Compact, client_id: None },
        M::AppPaneHeaders { visible: false, client_id: None },
        M::AppFullScreen { on: None, client_id: Some(3) },
        M::AppSettingsOpen { section: Some("terminal".into()), client_id: None },
        M::AppSettingsReload { client_id: None },
        M::AppSettingsOpenFile { client_id: None },
        M::AppSettingsGet { id: Some("font_size".into()), client_id: None },
        M::AppSettingsSet { id: "font_size".into(), value: json!(14), client_id: None },
        M::AppUpdateCheck { client_id: None },
        M::AppPalette { query: Some("split".into()), client_id: None },
        M::AppHide { client_id: None },
        M::AppHideOthers { client_id: None },
        M::AppShowAll { client_id: None },
        M::AppQuit { client_id: None },
        M::AppJumpPrompt { delta: -1, block_id: None, client_id: None },
        M::AppSelectOutput { block_id: None, client_id: None },
        M::AppCopyOutput { block_id: None, client_id: None },
        M::AppOpenShare { link: "https://silo.example/#silos1aeaqcaibaeaq".into(), client_id: None },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Request;

    /// Checks the draft 2020-12 keywords schemars emits.
    fn check(schema: &Value, root: &Value, v: &Value, at: &str) -> Result<(), String> {
        let fail = |what: &str| Err(format!("{at}: {what} ({v})"));
        if schema == &Value::Bool(false) {
            return fail("false schema");
        }
        if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
            let def = r.strip_prefix("#/$defs/").and_then(|n| root["$defs"].get(n)).ok_or(format!("{at}: bad $ref {r}"))?;
            check(def, root, v, at)?;
        }
        if let Some(types) = schema.get("type") {
            let types: Vec<&str> = types.as_array().map_or_else(|| vec![types.as_str().unwrap()], |t| t.iter().filter_map(Value::as_str).collect());
            let ok = types.iter().any(|t| match *t {
                "null" => v.is_null(),
                "boolean" => v.is_boolean(),
                "integer" => v.is_i64() || v.is_u64(),
                "number" => v.is_number(),
                "string" => v.is_string(),
                "array" => v.is_array(),
                "object" => v.is_object(),
                _ => false,
            });
            if !ok {
                return fail(&format!("not {types:?}"));
            }
        }
        if schema.get("const").is_some_and(|c| c != v) {
            return fail("const");
        }
        if schema.get("enum").and_then(Value::as_array).is_some_and(|e| !e.contains(v)) {
            return fail("enum");
        }
        let number = v.as_f64();
        if schema.get("minimum").and_then(Value::as_f64).zip(number).is_some_and(|(min, n)| n < min) {
            return fail("minimum");
        }
        if schema.get("maximum").and_then(Value::as_f64).zip(number).is_some_and(|(max, n)| n > max) {
            return fail("maximum");
        }
        if let Some(object) = v.as_object() {
            for key in schema.get("required").and_then(Value::as_array).into_iter().flatten() {
                if !object.contains_key(key.as_str().unwrap()) {
                    return fail(&format!("missing {key}"));
                }
            }
            let props = schema.get("properties").and_then(Value::as_object);
            for (key, value) in object {
                match props.and_then(|p| p.get(key)) {
                    Some(sub) => check(sub, root, value, &format!("{at}.{key}"))?,
                    None => match schema.get("additionalProperties") {
                        Some(Value::Bool(false)) => return fail(&format!("unexpected {key}")),
                        Some(sub @ Value::Object(_)) => check(sub, root, value, &format!("{at}.{key}"))?,
                        _ => {}
                    },
                }
            }
        }
        if let Some(items) = v.as_array() {
            let prefix = schema.get("prefixItems").and_then(Value::as_array);
            for (i, item) in items.iter().enumerate() {
                let sub = prefix.and_then(|p| p.get(i)).or_else(|| schema.get("items").filter(|s| s.is_object()));
                if let Some(sub) = sub {
                    check(sub, root, item, &format!("{at}[{i}]"))?;
                }
            }
        }
        for sub in schema.get("allOf").and_then(Value::as_array).into_iter().flatten() {
            check(sub, root, v, at)?;
        }
        if schema.get("anyOf").and_then(Value::as_array).is_some_and(|any| !any.iter().any(|s| check(s, root, v, at).is_ok())) {
            return fail("anyOf");
        }
        if schema.get("oneOf").and_then(Value::as_array).is_some_and(|one| one.iter().filter(|s| check(s, root, v, at).is_ok()).count() != 1) {
            return fail("oneOf");
        }
        Ok(())
    }

    #[test]
    fn every_method_describes_and_its_example_validates() {
        let names: Vec<_> = METHODS.iter().map(|(n, _)| (*n).to_owned()).collect();
        assert_eq!(examples().iter().map(method_name).collect::<Vec<_>>(), names, "one example per method, in order");
        let mut tags = schema_names();
        let mut sorted = names.clone();
        tags.sort();
        sorted.sort();
        assert_eq!(tags, sorted, "METHODS lists every Method variant");
        for name in &names {
            let d = describe(name).unwrap_or_else(|| panic!("{name} does not describe"));
            assert!(!d.description.is_empty(), "{name}");
            assert_eq!(d.example["method"], name.as_str());
            let params = &d.example["params"];
            check(&d.params_schema, &d.params_schema, params, name).unwrap_or_else(|e| panic!("{name}: {e}\n{}", d.params_schema));
            let request = json!({"id": 1, "method": name, "params": params});
            let parsed: Request = serde_json::from_value(request).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(method_name(&parsed.method), *name);
            assert!(d.result_schema.is_object(), "{name}");
        }
        assert!(describe("nope").is_none());
    }

    /// Every method in [`METHODS`] publishes what it needs, and the
    /// published form matches [`crate::access::need`].
    #[test]
    fn every_method_declares_access() {
        use crate::access::{AccessKind, Need, TargetKind, need};
        for (name, _) in METHODS {
            let d = describe(name).unwrap_or_else(|| panic!("{name} does not describe"));
            let access = d.access.unwrap_or_else(|| panic!("{name} declares no access"));
            let method = examples().into_iter().find(|m| method_name(m) == *name).unwrap();
            match need(&method) {
                Need::Open => assert_eq!(access.kind, AccessKind::Open, "{name}"),
                Need::Offer => assert_eq!(access.kind, AccessKind::Offer, "{name}"),
                Need::AppReply => assert_eq!(access.kind, AccessKind::AppReply, "{name}"),
                Need::Rights { rights, approval, .. } => {
                    assert_eq!((access.kind, access.rights, access.approval), (AccessKind::Rights, Some(rights), approval), "{name}");
                    let target = access.target.unwrap_or_else(|| panic!("{name} has no target"));
                    match &access.param {
                        Some(param) => assert!(d.params_schema["properties"].get(param).is_some(), "{name}: no param {param}"),
                        None => assert_eq!(target, TargetKind::Server, "{name}"),
                    }
                }
            }
            assert_eq!(access.app, name.starts_with("app.") && *name != "app.reply", "{name}");
            if access.rights == Some(Rights::Owner) {
                assert_eq!(access.target, Some(TargetKind::Server), "{name}: owner rights exist only server-wide");
            }
        }
    }

    #[test]
    fn schemas_reject_bad_params_and_accept_results() {
        let d = describe("block.move").unwrap();
        let bad = json!({"rect": {"x": 0.1, "y": 0.1, "w": 0.5, "h": 0.5}});
        assert!(check(&d.params_schema, &d.params_schema, &bad, "p").is_err(), "block_id is required");
        let ok = describe("ping").unwrap().result_schema;
        assert!(check(&ok, &ok, &json!({}), "r").is_ok());
        let created = describe("block.split").unwrap().result_schema;
        assert!(check(&created, &created, &json!({"session_id": "s", "window_id": "w", "block_id": "b"}), "r").is_ok());
        assert!(check(&created, &created, &json!({"session_id": "s"}), "r").is_err());
    }
}
