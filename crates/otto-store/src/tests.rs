use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use otto_llm::types::{Media, MediaSource, Part, ResultPart, Role};
use serde_json::json;

#[test]
fn connection_durability_settings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("settings.db")).unwrap();
    for (pragma, expected) in [("synchronous", 2), ("foreign_keys", 1), ("busy_timeout", 5000)] {
        let actual: i64 = store.connection.pragma_query_value(None, pragma, |r| r.get(0)).unwrap();
        assert_eq!(actual, expected, "{pragma}");
    }
}

pub(crate) fn session(store: &mut Store) -> String {
    let project = store.create_project("/p").unwrap().id;
    let new = NewSession {
        project_id: project,
        parent_id: None,
        agent: "a".into(),
        model: "m".into(),
        provider: "p".into(),
        title: "t".into(),
        cwd: "/p".into(),
        meta: json!({}),
    };
    store.create_session(new).unwrap().id
}

pub(crate) fn png(seed: u8) -> String {
    STANDARD.encode((0..600_u32).map(|i| u8::try_from(i % 251).unwrap() ^ seed).collect::<Vec<_>>())
}

fn image(data: &str) -> Media {
    Media { mime: "image/png".into(), source: MediaSource::Base64 { data: data.into() }, name: None }
}

fn record(kind: &str, payload: Value) -> EventRecord {
    EventRecord { turn: 1, step: 1, kind: kind.into(), payload }
}

fn raw(store: &Store, sql: &str) -> Vec<String> {
    let mut stmt = store.connection.prepare(sql).unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

#[test]
fn media_and_projected_values_are_stored_once() {
    let mut store = Store::open_in_memory().unwrap();
    let id = session(&mut store);
    let shot = png(1);
    let output = json!({"content":[{"type":"text","text":"shot"},{"type":"image","mime":"image/png","data_base64":shot}],"is_error":false});
    let input = json!({"url":"https://example.com"});
    store
        .append(
            &id,
            record("tool.call", json!({"call_id":"c","name":"shot","input":input})),
            Projection::ToolIntent { call_id: "c".into(), name: "shot".into(), input: input.clone() },
        )
        .unwrap();
    store
        .append(
            &id,
            record("tool.result", json!({"call_id":"c","is_error":false,"output":output,"item_id":"r"})),
            Projection::ToolOutcome { call_id: "c".into(), status: ToolStatus::Done, output: output.clone() },
        )
        .unwrap();
    let mut user = Item::user_text("u", "look");
    user.parts.push(Part::Image(image(&png(2))));
    let result = Item::new(
        "r",
        Role::Tool,
        vec![Part::ToolResult { call_id: "c".into(), content: vec![ResultPart::text("shot"), ResultPart::Image(image(&shot))], is_error: false }],
    );
    for item in [user.clone(), result.clone()] {
        store.append(&id, record("item.done", json!({ "item": item })), Projection::PutItem(item)).unwrap();
    }
    // Two images, each once; nothing stored carries base64 or repeats a projection.
    assert_eq!(raw(&store, "SELECT sha256 FROM blobs").len(), 2);
    let stored =
        [raw(&store, "SELECT payload FROM events"), raw(&store, "SELECT json FROM items"), raw(&store, "SELECT output FROM tool_calls")].concat();
    assert!(stored.iter().all(|s| !s.contains(&shot[..40]) && !s.contains(&png(2)[..40])), "{stored:?}");
    let payloads = raw(&store, "SELECT payload FROM events ORDER BY seq");
    assert!(payloads[0].contains(r#""input":{"$otto_ref":"input"}"#) && payloads[1].contains(r#""output":{"$otto_ref":"output"}"#), "{payloads:?}");
    assert_eq!(payloads[2], r#"{"item":{"$otto_ref":"item"}}"#);
    // Readers get everything back.
    let events = store.events_since(&id, 0, 10).unwrap();
    assert_eq!(events[0].payload["input"], input);
    assert_eq!(events[1].payload["output"], output);
    assert_eq!(events[2].payload["item"], serde_json::to_value(&user).unwrap());
    assert_eq!(events[3].payload["item"], serde_json::to_value(&result).unwrap());
    assert_eq!(store.items(&id).unwrap(), [user.clone(), result.clone()]);
    assert_eq!(store.tool_calls(&id).unwrap()[0].output.as_ref(), Some(&output));
    assert_eq!(store.tool_calls(&id).unwrap()[0].input, input);
    let marks = store.marks(&id, &["item.done", "tool.result"]).unwrap();
    assert_eq!(
        marks.iter().map(|m| (m.2.as_deref(), m.3.as_deref())).collect::<Vec<_>>(),
        [(Some("r"), Some("c")), (Some("u"), None), (Some("r"), None)]
    );
    let mut newest = Vec::new();
    store
        .items_newest(&id, None, |_, item| {
            newest.push(item);
            true
        })
        .unwrap();
    assert_eq!(newest, [result, user]);
}

#[test]
fn an_item_keeps_a_version_only_when_it_changes() {
    let mut store = Store::open_in_memory().unwrap();
    let id = session(&mut store);
    let mut first = Item::user_text("a", "original");
    first.parts.push(Part::Image(image(&png(3))));
    let put = |store: &mut Store, item: Item| store.append(&id, record("item.done", json!({ "item": item })), Projection::PutItem(item)).unwrap();
    put(&mut store, first.clone());
    put(&mut store, Item::user_text("b", "other"));
    assert!(raw(&store, "SELECT json FROM item_versions").is_empty());
    put(&mut store, Item::user_text("a", "edited"));
    assert_eq!(raw(&store, "SELECT item_id FROM item_versions"), ["a"]);
    // The first event still reads the version it put; a fork before the edit gets it.
    assert_eq!(store.events_since(&id, 0, 1).unwrap()[0].payload["item"], serde_json::to_value(&first).unwrap());
    assert_eq!(store.events_since(&id, 2, 1).unwrap()[0].payload["item"]["parts"][0]["text"], "edited");
    let fork = store.fork_session(&id, 2).unwrap();
    assert_eq!(store.items(&fork).unwrap(), [first, Item::user_text("b", "other")]);
}

#[test]
fn a_finished_turn_keeps_the_first_delta_of_each_run() {
    let mut store = Store::open_in_memory().unwrap();
    let id = session(&mut store);
    let delta = |kind: &str, key: &str, text: &str| {
        record(kind, json!({ if kind == "tool.input_delta" { "call_id" } else { "item_id" }: key, "text": text }))
    };
    let mut append = |r: EventRecord, p: Projection| store.append(&id, r, p).unwrap();
    append(record("turn.started", json!({"turn":1})), Projection::TurnStatus(TurnStatus::Running)); // 1
    append(record("item.started", json!({"item_id":"a"})), Projection::None); // 2
    for (kind, text) in [("reasoning.delta", "r1"), ("reasoning.delta", "r2"), ("text.delta", "t1"), ("text.delta", "t2"), ("text.delta", "t3")] {
        append(delta(kind, "a", text), Projection::None); // 3..=7
    }
    append(record("tool.input_start", json!({"call_id":"c","name":"x"})), Projection::None); // 8
    append(delta("tool.input_delta", "c", "{"), Projection::None); // 9
    append(delta("tool.input_delta", "c", "}"), Projection::None); // 10
    append(
        record("tool.call", json!({"call_id":"c","name":"x","input":{}})),
        Projection::ToolIntent { call_id: "c".into(), name: "x".into(), input: json!({}) },
    ); // 11
    let done = Item::new("a", Role::Assistant, vec![Part::text("t1t2t3")]);
    append(record("item.done", json!({ "item": done })), Projection::PutItem(done)); // 12
    append(record("item.started", json!({"item_id":"cut"})), Projection::None); // 13
    append(delta("text.delta", "cut", "kept"), Projection::None); // 14
    append(delta("text.delta", "cut", " too"), Projection::None); // 15
    let seqs = |store: &Store| store.events_since(&id, 0, 100).unwrap().into_iter().map(|e| e.seq).collect::<Vec<_>>();
    assert_eq!(seqs(&store).len(), 15, "a running turn keeps its deltas");
    store.append(&id, record("turn.finished", json!({"turn":1})), Projection::TurnStatus(TurnStatus::Cancelled)).unwrap();
    assert_eq!(seqs(&store), [1, 2, 3, 5, 8, 9, 11, 12, 13, 14, 15, 16], "the unfinished item's deltas stay");
    assert!(store.connection.execute("DELETE FROM events WHERE kind='text.delta'", []).is_ok(), "deltas may be compacted");
    assert!(store.connection.execute("DELETE FROM events WHERE kind='item.done'", []).is_err());
}

/// A database as version `version` wrote it: one session's turn with an image read, streamed deltas, an item edit
/// and a stopped reply.
fn old_database(path: &Path, version: i64) {
    let conn = Connection::open(path).unwrap();
    for &(_, sql) in &schema::MIGRATIONS[..usize::try_from(version).unwrap()] {
        conn.execute_batch(sql).unwrap();
    }
    conn.pragma_update(None, "user_version", version).unwrap();
    conn.execute_batch("INSERT INTO projects VALUES('p','/p',1); INSERT INTO sessions VALUES('s','p',NULL,'a','m','p','t','/p','idle',1,1,0,0,'{}'); INSERT INTO turns(session_id,turn,status,started_at,finished_at) VALUES('s',1,'done',1,2);").unwrap();
    let shot = png(7);
    let output = json!({"content":[{"type":"image","mime":"image/png","data_base64":shot}],"is_error":false});
    let read =
        Item::new("r", Role::Tool, vec![Part::ToolResult { call_id: "c".into(), content: vec![ResultPart::Image(image(&shot))], is_error: false }]);
    let reply = Item::new("a", Role::Assistant, vec![Part::text("hi there")]);
    let events: Vec<(&str, Value, Option<&Item>)> = vec![
        ("turn.started", json!({"turn":1}), None),
        ("item.done", json!({ "item": Item::user_text("u", "first") }), None),
        ("item.started", json!({"item_id":"a","role":"assistant"}), None),
        ("text.delta", json!({"item_id":"a","text":"hi"}), None),
        ("text.delta", json!({"item_id":"a","text":" there"}), None),
        ("tool.input_start", json!({"call_id":"c","name":"read"}), None),
        ("tool.input_delta", json!({"call_id":"c","delta":"{}"}), None),
        ("tool.input_delta", json!({"call_id":"c","delta":" "}), None),
        ("item.done", json!({ "item": reply }), Some(&reply)),
        ("tool.call", json!({"call_id":"c","name":"read","input":{"path":"a.png"}}), None),
        ("tool.result", json!({"call_id":"c","is_error":false,"output":output,"item_id":"r"}), None),
        ("item.done", json!({ "item": read }), Some(&read)),
        ("item.done", json!({ "item": Item::user_text("u", "edited") }), None),
        ("item.started", json!({"item_id":"cut","role":"assistant"}), None),
        ("text.delta", json!({"item_id":"cut","text":"stopped"}), None),
        ("text.delta", json!({"item_id":"cut","text":" here"}), None),
        ("turn.finished", json!({"turn":1,"status":"cancelled"}), None),
    ];
    for (i, (kind, payload, _)) in events.iter().enumerate() {
        let seq = i64::try_from(i).unwrap() + 1;
        conn.execute("INSERT INTO events VALUES('s',?1,1,1,?2,?3,?1)", params![seq, kind, payload.to_string()]).unwrap();
        if *kind == "item.done" {
            let item: Item = serde_json::from_value(payload["item"].clone()).unwrap();
            let (role, json) = (enum_name(&item.role).unwrap(), serde_json::to_string(&item).unwrap());
            conn.execute(
                "INSERT INTO items VALUES('s',?1,?2,?3,?4) ON CONFLICT(session_id,item_id) DO UPDATE SET seq=excluded.seq,json=excluded.json",
                params![item.id, seq, role, json],
            )
            .unwrap();
            conn.execute("INSERT INTO item_versions VALUES('s',?1,?2,?3,?4)", params![item.id, seq, role, json]).unwrap();
        }
    }
    conn.execute("INSERT INTO tool_calls VALUES('s','c','read','{\"path\":\"a.png\"}','done',?1,10,11)", [output.to_string()]).unwrap();
    conn.execute("UPDATE sessions SET last_seq=?1", [events.len()]).unwrap();
}

#[test]
fn the_migration_stores_everything_once_and_reads_back_the_same() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    old_database(&path, 2);
    let before: Vec<(i64, String, Value)> = {
        let conn = Connection::open(&path).unwrap();
        let mut stmt = conn.prepare("SELECT seq,kind,payload FROM events ORDER BY seq").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&r.get::<_, String>(2)?).unwrap())))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let items_before: Vec<Item> = {
        let conn = Connection::open(&path).unwrap();
        let mut stmt = conn.prepare("SELECT json FROM items ORDER BY seq,item_id").unwrap();
        stmt.query_map([], |r| Ok(serde_json::from_str(&r.get::<_, String>(0)?).unwrap())).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    };
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.connection.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0)).unwrap(), 4);
    assert_eq!(store.items("s").unwrap(), items_before);
    let after = store.events_since("s", 0, 100).unwrap();
    let kept: Vec<_> = before.iter().filter(|(seq, ..)| ![5, 8].contains(seq)).cloned().collect();
    assert_eq!(after.into_iter().map(|e| (e.seq, e.kind, e.payload)).collect::<Vec<_>>(), kept, "only the redundant deltas go");
    assert_eq!(raw(&store, "SELECT sha256 FROM blobs").len(), 1);
    assert_eq!(raw(&store, "SELECT item_id FROM item_versions"), ["u"], "only the superseded version stays");
    assert!(raw(&store, "SELECT payload FROM events").iter().all(|p| !p.contains("data_base64\":\"") && !p.contains("hi there")));
    assert_eq!(store.tool_calls("s").unwrap()[0].output.as_ref().unwrap()["content"][0]["data_base64"], png(7));
    assert_eq!(store.fork_session("s", 2).map(|f| store.items(&f).unwrap()).unwrap(), [Item::user_text("u", "first")]);
    // Migration 3 still ran: the turn has its snapshot columns.
    store.set_turn_trees("s", 1, Some("base"), None).unwrap();
    assert_eq!(store.turn_trees("s", 1).unwrap(), (Some("base".into()), None));
    drop(store);
    assert_eq!(Store::open(&path).unwrap().events_since("s", 0, 100).unwrap().len(), kept.len(), "it runs once");
}

#[test]
fn a_long_output_text_is_stored_once() {
    let mut store = Store::open_in_memory().unwrap();
    let id = session(&mut store);
    let long = "line of shell output\n".repeat(200);
    let output = json!({"content":[{"type":"text","text":long}],"is_error":false});
    store
        .append(
            &id,
            record("tool.call", json!({"call_id":"c","name":"shell","input":{}})),
            Projection::ToolIntent { call_id: "c".into(), name: "shell".into(), input: json!({}) },
        )
        .unwrap();
    store
        .append(
            &id,
            record("tool.result", json!({"call_id":"c","output":output})),
            Projection::ToolOutcome { call_id: "c".into(), status: ToolStatus::Done, output: output.clone() },
        )
        .unwrap();
    let result =
        Item::new("r", Role::Tool, vec![Part::ToolResult { call_id: "c".into(), content: vec![ResultPart::text(long.clone())], is_error: false }]);
    store.append(&id, record("item.done", json!({ "item": result })), Projection::PutItem(result.clone())).unwrap();
    assert_eq!(raw(&store, "SELECT sha256 FROM blobs").len(), 1);
    let stored = [raw(&store, "SELECT json FROM items"), raw(&store, "SELECT output FROM tool_calls")].concat();
    assert!(stored.iter().all(|s| s.contains("$otto_text") && !s.contains("line of shell")), "{stored:?}");
    assert_eq!(store.items(&id).unwrap(), std::slice::from_ref(&result));
    assert_eq!(store.tool_calls(&id).unwrap()[0].output.as_ref(), Some(&output));
    assert_eq!(store.events_since(&id, 0, 9).unwrap()[2].payload["item"], serde_json::to_value(&result).unwrap());
    // A short string, or a user's own `blob:` text outside media, is left as it is.
    let own = Item::user_text("u", format!("{}{}", refs::BLOB, "0".repeat(64)));
    store.append(&id, record("item.done", json!({ "item": own })), Projection::PutItem(own.clone())).unwrap();
    assert_eq!(store.items(&id).unwrap()[1], own);
}
