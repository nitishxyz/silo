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

/// The compaction events are additive: a client built before them skips the
/// journaled ones as unknown and the live delta as `LiveUnknown`, and the
/// `compaction` event keeps its old shape, its `id` optional both ways.
#[test]
fn compaction_events_are_additive() {
    let started = json!({"seq": 3, "session_id": "s", "event": "compaction.started", "data": {"id": "c1", "trigger": "auto"}});
    assert!(matches!(
        serde_json::from_value::<ServerMessage>(started).unwrap(),
        ServerMessage::Event { event, .. } if matches!(&*event, Event::CompactionStarted { id, trigger } if id == "c1" && trigger == "auto")
    ));
    let old = json!({"seq": 4, "session_id": "s", "event": "compaction", "data": {"through_seq": 2, "summary": "so far"}});
    assert!(matches!(
        serde_json::from_value::<ServerMessage>(old).unwrap(),
        ServerMessage::Event { event, .. } if matches!(&*event, Event::Compaction { through_seq: 2, id: None, .. })
    ));
    let done = Event::Compaction { through_seq: 2, summary: None, id: Some("c1".into()) };
    assert_eq!(serde_json::to_value(&done).unwrap()["data"]["id"], "c1");
    let failed: Event = serde_json::from_value(json!({"event": "compaction.failed", "data": {"id": "c1", "message": "empty"}})).unwrap();
    assert!(matches!(failed, Event::CompactionFailed { cancelled: false, .. }));
    let delta = Event::CompactionDelta { id: "c1".into(), text: "Goal".into() };
    assert!(delta.is_live_delta());
    let wire = serde_json::to_value(ServerMessage::Live { live: true, id: 0, session_id: "s".into(), after_seq: 3, event: Box::new(delta) }).unwrap();
    assert!(matches!(serde_json::from_value::<ServerMessage>(wire.clone()).unwrap(), ServerMessage::Live { after_seq: 3, .. }));
    // An older client's event enum has no `compaction.delta`: its decoder takes the live frame as unknown.
    #[derive(serde::Deserialize)]
    #[serde(tag = "event", content = "data")]
    #[allow(dead_code)]
    enum Old {
        #[serde(rename = "text.delta")]
        TextDelta { item_id: String, text: String },
    }
    assert!(serde_json::from_value::<Old>(wire["event"].clone()).is_err());
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

#[test]
fn agent_shapes_and_no_plan_mode() {
    use otto_proto::{AgentDef, AgentInfo, AgentScope};
    let info: AgentInfo = serde_json::from_value(json!({"name": "build", "description": "", "model": null, "provider": null})).unwrap();
    assert_eq!(info.scope, AgentScope::Builtin);
    let request: Request =
        serde_json::from_value(json!({"id": 1, "method": "agents.delete", "params": {"name": "x", "scope": "project", "cwd": "/p"}})).unwrap();
    assert!(matches!(request.method, Method::AgentsDelete { scope: AgentScope::Project, .. }));
    let def: AgentDef = serde_json::from_value(json!({
        "name": "x", "description": "", "model": null, "effort": null, "approval_mode": "ask_all", "prompt": "", "scope": "user"
    }))
    .unwrap();
    assert!(def.tools.is_empty() && def.loadable.is_empty() && !def.overrides_builtin);
    let wire = serde_json::to_value(&def).unwrap();
    for key in ["tools", "loadable", "disallowed", "overrides_builtin", "approval_mode", "scope"] {
        assert!(wire.get(key).is_some(), "{key}");
    }
    let plan = json!({"id": 1, "method": "session.set_mode", "params": {"session_id": "s", "approval_mode": "plan"}});
    assert!(serde_json::from_value::<Request>(plan).is_err());
}

#[test]
fn mention_and_file_complete_shapes() {
    use otto_proto::{FileMatch, InputPart, Item, Mention, MentionKind};
    let input = vec![
        InputPart::Text { text: "ask ".into() },
        InputPart::Mention { kind: MentionKind::Agent, target: "build".into() },
        InputPart::Mention { kind: MentionKind::File, target: "docs/".into() },
    ];
    let wire = serde_json::to_value(&input).unwrap();
    assert_eq!(
        wire,
        json!([{"type": "text", "text": "ask "}, {"type": "mention", "kind": "agent", "target": "build"}, {"type": "mention", "kind": "file", "target": "docs/"}])
    );
    assert_eq!(serde_json::from_value::<Vec<InputPart>>(wire).unwrap(), input);
    let request: Request = serde_json::from_value(json!({"id": 1, "method": "files.complete", "params": {"cwd": "/p"}})).unwrap();
    assert!(matches!(request.method, Method::FilesComplete { ref query, limit: 50, .. } if query.is_empty()));
    assert_eq!(serde_json::to_value(FileMatch { path: "src/".into(), dir: true }).unwrap(), json!({"path": "src/", "dir": true}));
    // Items without mentions keep their old shape; a missing target says so.
    let mut item = Item::user_text("u1", "ask @build");
    assert!(serde_json::to_value(&item).unwrap().get("mentions").is_none());
    item.mentions.push(Mention { kind: MentionKind::Agent, target: "build".into(), range: [4, 10], missing: false });
    item.mentions.push(Mention { kind: MentionKind::File, target: "gone.rs".into(), range: [0, 0], missing: true });
    let wire = serde_json::to_value(&item).unwrap();
    assert_eq!(
        wire["mentions"],
        json!([{"kind": "agent", "target": "build", "range": [4, 10]}, {"kind": "file", "target": "gone.rs", "range": [0, 0], "missing": true}])
    );
    assert_eq!(serde_json::from_value::<Item>(wire).unwrap(), item);
}

#[test]
fn model_retrying_is_additive_on_the_wire() {
    let event = Event::ModelRetrying { class: "rate_limited".into(), message: "slow".into(), delay_ms: 2000, attempt: 2, max_attempts: 3, at_ms: 5 };
    let wire = serde_json::to_value(ServerMessage::Event { seq: 3, session_id: Some("s".into()), event: Box::new(event) }).unwrap();
    assert_eq!(
        wire,
        json!({"seq":3,"session_id":"s","event":"model.retrying","data":{"class":"rate_limited","message":"slow","delay_ms":2000,"attempt":2,"max_attempts":3,"at_ms":5}})
    );
    assert!(
        matches!(serde_json::from_value::<ServerMessage>(wire).unwrap(), ServerMessage::Event { event, .. } if matches!(*event, Event::ModelRetrying { attempt: 2, .. }))
    );
}

/// `turn.finished.changes` and `SessionView.turns` are additive: an older server's event and view read as none,
/// a turn without changes keeps the old shape, and an older client ignores the new field.
#[test]
fn turn_changes_are_additive() {
    use otto_proto::{SessionView, TurnChanges, TurnStatus};
    let old = json!({"event": "turn.finished", "data": {"turn": 2, "status": "done", "usage": {"input": 1, "output": 2}}});
    let event: Event = serde_json::from_value(old.clone()).unwrap();
    assert!(matches!(event, Event::TurnFinished { turn: 2, status: TurnStatus::Done, changes: None, .. }));
    assert_eq!(serde_json::to_value(&event).unwrap()["data"].get("changes"), None);
    let changes = TurnChanges {
        base_tree: Some("4b825dc".into()),
        end_tree: Some("9f1e2aa".into()),
        files: vec!["src/a.rs".into(), "b.txt".into()],
        more: 0,
        added: 12,
        removed: 3,
    };
    let event = Event::TurnFinished { turn: 3, status: TurnStatus::Done, usage: Default::default(), changes: Some(changes.clone()) };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(
        wire["data"]["changes"],
        json!({"base_tree": "4b825dc", "end_tree": "9f1e2aa", "files": ["src/a.rs", "b.txt"], "added": 12, "removed": 3})
    );
    assert!(matches!(serde_json::from_value::<Event>(wire.clone()).unwrap(), Event::TurnFinished { changes: Some(c), .. } if c == changes));
    // Outside git: no trees.
    let fallback: TurnChanges = serde_json::from_value(json!({"files": ["notes.md"], "added": 0, "removed": 0})).unwrap();
    assert_eq!((fallback.base_tree.as_deref(), fallback.file_count()), (None, 1));
    // An older client's TurnFinished has no `changes`: serde skips the unknown field.
    #[derive(serde::Deserialize)]
    #[serde(tag = "event", content = "data")]
    #[allow(dead_code)]
    enum Old {
        #[serde(rename = "turn.finished")]
        TurnFinished { turn: i64, status: TurnStatus, usage: otto_proto::Usage },
    }
    assert!(serde_json::from_value::<Old>(wire).is_ok());
    // A view from an older server has no `turns`.
    let mut view = serde_json::to_value(sample_view()).unwrap();
    view.as_object_mut().unwrap().remove("turns");
    assert!(serde_json::from_value::<SessionView>(view).unwrap().turns.is_empty());
    // Nor `turn_usage`.
    assert!(sample_view().turn_usage.is_empty());
    let mut with = sample_view();
    with.turn_usage = vec![otto_proto::TurnUsage { turn: 1, item_id: None, usage: Default::default(), cost_usd: 0.0 }];
    let wire = serde_json::to_value(&with).unwrap();
    assert!(wire["turn_usage"][0].get("item_id").is_none(), "no reply yet: omitted");
    assert_eq!(serde_json::from_value::<SessionView>(wire).unwrap().turn_usage, with.turn_usage);
}
fn sample_view() -> otto_proto::SessionView {
    serde_json::from_value(json!({
        "info": {"id": "s", "project_id": "p", "parent_id": null, "agent": "build", "model": "m", "provider": "x", "title": "t", "cwd": "/",
                 "status": "idle", "created_at": 0, "updated_at": 0, "last_seq": 0, "usage": {"usage": {"input": 0, "output": 0}, "cost_usd": 0.0}},
        "items": [], "pending_approvals": [], "pending_questions": [], "queue": [], "running_tool_calls": [], "seq": 0
    }))
    .unwrap()
}
