use otto_proto::{ClientHello, ClientRole, EVENTS, Event, Method, Request, ServerMessage, paths};
use serde_json::json;

#[test]
fn optional_params_and_forward_compatibility() {
    let request: Request = serde_json::from_value(json!({"id": 7, "method": "ping"})).unwrap();
    assert!(matches!(request.method, Method::Ping {}));
    let request: Request = serde_json::from_value(json!({"id": 7, "method": "session.list"})).unwrap();
    assert!(matches!(request.method, Method::SessionList { include_archived: false, project_id: None }));
    assert!(serde_json::from_value::<Request>(json!({"id": 1, "method": "session.view"})).is_err());
    let mut hello = serde_json::to_value(ClientHello::ours("test", ClientRole::Viewer)).unwrap();
    hello["role"] = json!("future-role");
    assert_eq!(serde_json::from_value::<ClientHello>(hello).unwrap().role, ClientRole::Unknown);
    let event: ServerMessage = serde_json::from_value(json!({"seq": 8, "session_id": "s", "event": "future.event", "data": {}})).unwrap();
    assert!(matches!(event, ServerMessage::UnknownEvent { seq: 8, session_id: Some(id) } if id == "s"));
    let event: ServerMessage = serde_json::from_value(json!({"seq": 9, "session_id": "s", "event": "text.delta", "data": {}})).unwrap();
    assert!(matches!(event, ServerMessage::MalformedEvent { seq: 9, session_id: Some(id), .. } if id == "s"));
    let event =
        ServerMessage::Event { seq: 10, session_id: Some("s".into()), event: Box::new(Event::TextDelta { item_id: "i".into(), text: "hi".into() }) };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire, json!({"seq": 10, "session_id": "s", "event": "text.delta", "data": {"item_id": "i", "text": "hi"}}));
    assert!(matches!(serde_json::from_value::<ServerMessage>(wire).unwrap(), ServerMessage::Event { seq: 10, .. }));
    let global: ServerMessage = serde_json::from_value(json!({"seq": 11, "event": "sessions.changed", "data": {}})).unwrap();
    assert!(matches!(global, ServerMessage::Event { session_id: None, .. }));
    assert!(serde_json::from_value::<ServerMessage>(json!({"seq": "bad", "event": "sessions.changed"})).is_err());
    let response: ServerMessage = serde_json::from_value(json!({"id": 1, "result": {}})).unwrap();
    assert!(matches!(response, ServerMessage::Response { id: 1, result: Some(_), error: None }));
}

#[test]
fn event_table_is_exhaustive() {
    let schema = schemars::schema_for!(Event).to_value();
    let variants = schema.get("oneOf").or_else(|| schema.get("anyOf")).unwrap().as_array().unwrap();
    let mut tags: Vec<_> = variants.iter().map(|v| v["properties"]["event"]["const"].as_str().unwrap()).collect();
    let mut names = EVENTS.to_vec();
    tags.sort();
    names.sort();
    assert_eq!(tags, names);
}

#[test]
fn state_paths() {
    let home = std::path::Path::new("/state");
    assert_eq!(paths::socket(home), home.join("server.sock"));
    assert_eq!(paths::db(home), home.join("otto.db"));
    assert_eq!(paths::auth_dir(home), home.join("auth"));
    assert_eq!(paths::log(home), home.join("server.log"));
    assert_eq!(paths::runtime(home), home.join("runtime.json"));
    assert_eq!(paths::start_lock(home), home.join("start.lock"));
    assert_eq!(paths::server_lock(home), home.join("server.lock"));
}

#[test]
fn live_wire_is_unsequenced_restricted_and_ignored_by_old_clients() {
    let message = ServerMessage::Live {
        live: true,
        id: 0,
        session_id: "s".into(),
        after_seq: 4,
        event: Box::new(Event::TextDelta { item_id: "i".into(), text: "hi".into() }),
    };
    let wire = serde_json::to_value(message).unwrap();
    assert_eq!(wire, json!({"live":true,"id":0,"session_id":"s","after_seq":4,"event":{"event":"text.delta","data":{"item_id":"i","text":"hi"}}}));
    assert!(wire.get("seq").is_none());
    assert!(matches!(serde_json::from_value::<ServerMessage>(wire.clone()).unwrap(), ServerMessage::Live { after_seq: 4, .. }));
    // The original v1 decoder's no-seq branch deserializes this response shape.
    #[derive(serde::Deserialize)]
    struct OldResponse {
        id: u64,
        result: Option<serde_json::Value>,
        error: Option<otto_proto::RpcError>,
    }
    let old: OldResponse = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(old.id, 0);
    assert!(old.result.is_none() && old.error.is_none());
    let mut invalid = wire;
    invalid["event"] = json!({"event":"turn.started","data":{"turn":1}});
    assert!(matches!(serde_json::from_value::<ServerMessage>(invalid.clone()).unwrap(), ServerMessage::LiveUnknown { .. }));
    invalid["after_seq"] = json!(-1);
    assert!(serde_json::from_value::<ServerMessage>(invalid).is_err());
}
