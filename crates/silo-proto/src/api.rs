//! `api.describe`: JSON Schemas (draft 2020-12) and an example request for
//! every control method.

use schemars::{Schema, schema_for};
use serde_json::{Value, json};

use crate::control::{
    ApiMethod, Capture, Created, DataTicket, Dock, Empty, METHODS, Method, MethodDescription, OwnMode, PairInvite, PairedDevices, ServerInfo,
    SizeInfo, SpawnSpec,
};
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

/// The `params` subschema of variant `name`, with the shared `$defs`.
fn params_schema(name: &str) -> Option<Value> {
    let root = schema_for!(Method).to_value();
    let variant = variants(&root).find(|v| tag(v) == Some(name))?;
    let mut params = variant.get("properties")?.get("params").cloned().unwrap_or_else(|| json!({"type": "object"}));
    if let (Some(defs), Some(object)) = (root.get("$defs"), params.as_object_mut()) {
        object.insert("$defs".into(), defs.clone());
    }
    params.as_object_mut()?.insert("$schema".into(), root["$schema"].clone());
    Some(params)
}

/// The schema of `method`'s result.
fn result_schema(method: &Method) -> Schema {
    use Method as M;
    match method {
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
        M::DataConnect { .. } => schema_for!(DataTicket),
        M::PairCreate { .. } => schema_for!(PairInvite),
        M::PairList {} => schema_for!(PairedDevices),
        M::ServerShutdown {} | M::Ping {} | M::EventsSubscribe {} | M::SessionRename { .. } | M::SessionClose { .. } => schema_for!(Empty),
        M::WindowSelect { .. } | M::WindowClose { .. } | M::WindowRename { .. } | M::WindowMove { .. } => schema_for!(Empty),
        M::BlockClose { .. } | M::BlockFocus { .. } | M::BlockZoom { .. } | M::BlockSwap { .. } => {
            schema_for!(Empty)
        }
        M::BlockFloat { .. } | M::BlockMove { .. } | M::LayoutRatio { .. } => schema_for!(Empty),
        M::TerminalSend { .. } | M::TerminalResize { .. } | M::TerminalClear { .. } | M::TerminalReset { .. } => schema_for!(Empty),
        M::OwnBlock { .. } | M::DisownBlock { .. } | M::UiOpenSession { .. } | M::PairRevoke { .. } => schema_for!(Empty),
        M::BrowserNavigate { .. } | M::BrowserBack { .. } | M::BrowserForward { .. } | M::BrowserReload { .. } => schema_for!(Empty),
        M::BrowserZoom { .. } | M::BrowserReport { .. } => schema_for!(Empty),
    }
}

/// One example request per method, in [`METHODS`] order.
#[must_use]
pub fn examples() -> Vec<Method> {
    use Method as M;
    let (s, w, b) = (|| "s1a2b3c4d".to_owned(), || "w1a2b3c4d".to_owned(), || "b1a2b3c4d".to_owned());
    let shell = || SpawnSpec { cwd: Some("~/src".into()), command: Vec::new(), size: Some(TermSize::new(120, 40)) };
    vec![
        M::ServerInfo {},
        M::ServerShutdown {},
        M::ApiList {},
        M::ApiDescribe { method: "block.split".into() },
        M::Ping {},
        M::EventsSubscribe {},
        M::SessionList {},
        M::SessionCreate { name: Some("work".into()), spawn: shell(), kind: BlockKind::Terminal, url: None },
        M::SessionView { session_id: s() },
        M::SessionRename { session_id: s(), name: "play".into() },
        M::SessionClose { session_id: s() },
        M::WindowCreate { session_id: s(), spawn: SpawnSpec::default(), kind: BlockKind::Browser, url: Some("localhost:3000".into()) },
        M::WindowSelect { session_id: s(), window_id: w() },
        M::WindowClose { window_id: w() },
        M::WindowRename { window_id: w(), name: Some("logs".into()) },
        M::WindowMove { window_id: w(), index: 0 },
        M::BlockSplit { block_id: b(), axis: Axis::Horizontal, spawn: shell(), kind: BlockKind::Terminal, url: None },
        M::BlockClose { block_id: b() },
        M::BlockFocus { block_id: b() },
        M::BlockZoom { block_id: b(), zoomed: true },
        M::BlockSwap { block_id: b(), other_block_id: "b5e6f7a8b".into() },
        M::BlockFloat { block_id: b(), rect: Some(FloatRect::default()) },
        M::BlockMove { block_id: b(), rect: None, dock: Some(Dock { target: "b5e6f7a8b".into(), axis: Axis::Vertical }) },
        M::LayoutRatio { window_id: w(), path: vec![1], ratio: 0.4 },
        M::TerminalSend { block_id: b(), text: "ls\r".into() },
        M::TerminalCapture { block_id: b(), history: false },
        M::TerminalSize { block_id: b() },
        M::TerminalResize { block_id: b(), size: TermSize { columns: 120, rows: 40, cell_width: 16, cell_height: 34 } },
        M::TerminalClear { block_id: b() },
        M::TerminalReset { block_id: b() },
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
        M::UiOpenSession { session_id: s() },
        M::PairCreate { ttl_secs: 600 },
        M::PairList {},
        M::PairRevoke { id: "7c3e5f0d9a1b4c6e8f2a0b1c3d5e7f9a1b3c5d7e9f0a2b4c6d8e0f1a3b5c7d9e".into() },
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
