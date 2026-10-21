//! Pins the wire schema. Additive changes update the fixture; breaking changes bump PROTOCOL.

use otto_proto::{ClientHello, Event, Frame, METHODS, PROTOCOL, RpcError, ServerHello, ServerMessage, api, kind};
use schemars::schema_for;
use serde_json::{Map, Value, json};

const FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/protocol.json");

fn plain(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().filter(|(k, v)| !(k == "description" && v.is_string())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k.clone(), if k == "properties" { properties(v) } else { plain(v) })).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(plain).collect()),
        other => other,
    }
}

fn properties(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, plain(v))).collect())
        }
        other => plain(other),
    }
}

fn frame_kinds() -> Map<String, Value> {
    let source = include_str!("../src/lib.rs");
    let body = &source[source.find("pub mod kind {").unwrap()..];
    body.lines()
        .take_while(|line| *line != "}")
        .filter_map(|line| {
            let (name, value) = line.trim().strip_prefix("pub const ")?.split_once(": u8 = ")?;
            Some((name.to_owned(), json!(value.trim_end_matches(';'))))
        })
        .collect()
}

fn schema() -> Value {
    let methods: Map<String, Value> = METHODS
        .iter()
        .map(|(name, _)| {
            let d = api::describe(name).unwrap();
            ((*name).to_owned(), json!({"params": plain(d.params_schema), "result": plain(d.result_schema)}))
        })
        .collect();
    plain(json!({
        "protocol": PROTOCOL,
        "hellos": {"ClientHello": schema_for!(ClientHello), "ServerHello": schema_for!(ServerHello)},
        "events": schema_for!(Event),
        "server_message": schema_for!(ServerMessage),
        "rpc_error": schema_for!(RpcError),
        "methods": methods,
        "frame_kinds": frame_kinds(),
        "frames": {
            "hello": Frame::json(kind::HELLO, &json!({})).unwrap().encode(),
            "control": Frame::json(kind::CONTROL, &json!({"id": 1})).unwrap().encode(),
        },
    }))
}

#[test]
fn wire_schema_matches_protocol_json() {
    let current = serde_json::to_string_pretty(&schema()).unwrap() + "\n";
    if std::env::var_os("OTTO_UPDATE_PROTOCOL").is_some() {
        std::fs::write(FILE, &current).unwrap();
        return;
    }
    let pinned = std::fs::read_to_string(FILE).unwrap_or_default();
    if pinned != current {
        let line =
            pinned.lines().zip(current.lines()).position(|(a, b)| a != b).unwrap_or_else(|| pinned.lines().count().min(current.lines().count()));
        panic!(
            "wire schema changed at protocol.json line {}. Additive? OTTO_UPDATE_PROTOCOL=1 cargo test -p otto-proto --test protocol. Breaking? Bump PROTOCOL first.",
            line + 1
        );
    }
}
