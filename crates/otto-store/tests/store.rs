use otto_store::*;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

fn new_session(store: &mut Store) -> Session {
    let project = store.create_project("/project").unwrap();
    store
        .create_session(NewSession {
            project_id: project.id,
            parent_id: None,
            agent: "build".into(),
            model: "model".into(),
            provider: "test".into(),
            title: "session".into(),
            cwd: "/project".into(),
            meta: json!({"cache":"must not inherit"}),
        })
        .unwrap()
}
#[test]
fn atomic_metadata_and_enumeration() {
    let mut store = Store::open_in_memory().unwrap();
    let session = new_session(&mut store);
    assert_eq!(store.all_sessions().unwrap().len(), 1);
    assert_eq!(store.list_projects().unwrap()[0].id, session.project_id);
    assert_eq!(store.project(&session.project_id).unwrap().unwrap().root, "/project");
    let update = SessionUpdate { title: Some("renamed".into()), status: Some("busy".into()), ..Default::default() };
    store.append_with_session(&session.id, event("session.changed"), Projection::None, update.clone()).unwrap();
    let row = store.session(&session.id).unwrap().unwrap();
    assert_eq!(row.title, "renamed");
    assert_eq!(row.status, "busy");
    assert_eq!(row.last_seq, 1);
    assert!(
        store
            .append_with_session(&session.id, event("bad"), outcome("missing"), SessionUpdate { title: Some("must rollback".into()), ..update })
            .is_err()
    );
    let row = store.session(&session.id).unwrap().unwrap();
    assert_eq!(row.title, "renamed");
    assert_eq!(row.last_seq, 1);
}
fn event(kind: &str) -> EventRecord {
    EventRecord { turn: 1, step: 1, kind: kind.into(), payload: json!({}) }
}
fn intent(call: &str) -> Projection {
    Projection::ToolIntent { call_id: call.into(), name: "write".into(), input: json!({"path":"a"}) }
}
fn outcome(call: &str) -> Projection {
    Projection::ToolOutcome { call_id: call.into(), status: ToolStatus::Done, output: json!("ok") }
}

#[test]
fn projects_sessions_and_settings() {
    fn assert_send<T: Send>() {}
    assert_send::<Store>();
    assert_send::<Arc<Mutex<Store>>>();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let mut store = Store::open(&path).unwrap();
    let first = new_session(&mut store);
    assert_eq!(store.create_project("/project").unwrap().id, first.project_id);
    assert_eq!(store.project_by_root("/missing").unwrap(), None);
    assert!(store.session("missing").unwrap().is_none());
    store
        .update_session(
            &first.id,
            SessionUpdate { title: Some("renamed".into()), archived: Some(true), meta: Some(json!(null)), ..Default::default() },
        )
        .unwrap();
    assert!(store.list_sessions(&first.project_id, false).unwrap().is_empty());
    let list = store.list_sessions(&first.project_id, true).unwrap();
    assert_eq!(list[0].title, "renamed");
    assert_eq!(list[0].meta, json!(null));
    assert!(store.update_session("missing", SessionUpdate::default()).is_err());
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).unwrap(), "wal");
    assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), 3);
    drop(store);
    assert!(Store::open(&path).unwrap().session(&first.id).unwrap().is_some());
}

#[test]
fn atomic_append_order_and_usage_replacement() {
    let mut store = Store::open_in_memory().unwrap();
    let id = new_session(&mut store).id;
    assert!(store.append("missing", event("bad"), Projection::None).is_err());
    assert_eq!(store.append(&id, event("intent"), intent("a")).unwrap(), 1);
    assert!(store.append(&id, event("duplicate"), intent("a")).is_err());
    assert!(store.append(&id, event("missing outcome"), outcome("b")).is_err());
    assert_eq!(store.append(&id, event("outcome"), outcome("a")).unwrap(), 2);
    assert!(store.append(&id, event("repeat outcome"), outcome("a")).is_err());
    for (item_id, text) in [("z", "first"), ("a", "second")] {
        store.append(&id, event("item"), Projection::PutItem(Item::user_text(item_id, text))).unwrap();
    }
    assert_eq!(store.items(&id).unwrap().iter().map(Item::text).collect::<Vec<_>>(), ["first", "second"]);
    for count in [10, 20] {
        store
            .append(
                &id,
                event("usage"),
                Projection::Usage(UsageRecord {
                    provider: "test".into(),
                    model: "model".into(),
                    usage: Usage { input: count, output: 5, cache_read: 3, cache_write: 2, reasoning: 1 },
                    cost_usd: 0.5,
                }),
            )
            .unwrap();
    }
    let totals = store.usage_totals(&id).unwrap();
    assert_eq!(totals.usage.input, 20);
    assert_eq!(totals.usage.total_input(), 25);
    assert_eq!(totals.cost_usd, 0.5);
    let mut next = event("usage");
    next.step = 2;
    store
        .append(
            &id,
            next,
            Projection::Usage(UsageRecord {
                provider: "test".into(),
                model: "model".into(),
                usage: Usage { input: 1, ..Default::default() },
                cost_usd: 0.1,
            }),
        )
        .unwrap();
    assert_eq!(store.usage_totals(&id).unwrap().usage.input, 21);
    let events = store.events_since(&id, 0, 100).unwrap();
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), (1..=7).collect::<Vec<_>>());
    assert_eq!(store.events_since(&id, 2, 2).unwrap().iter().map(|e| e.seq).collect::<Vec<_>>(), [3, 4]);
    assert!(store.events_since(&id, 0, 0).unwrap().is_empty());
    assert_eq!(store.session(&id).unwrap().unwrap().last_seq, 7);
    assert_eq!(store.tool_calls(&id).unwrap()[0].status, ToolStatus::Done);
}

#[test]
fn crash_recovery_is_durable_journaled_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.db");
    let mut store = Store::open(&path).unwrap();
    let id = new_session(&mut store).id;
    let second = new_session(&mut store).id;
    store.append(&id, event("turn.started"), Projection::TurnStatus(TurnStatus::Running)).unwrap();
    for call in ["intent", "running", "done"] {
        store.append(&id, event("tool.call"), intent(call)).unwrap();
    }
    store.append(&id, event("tool.result"), outcome("done")).unwrap();
    store.append(&second, event("tool.call"), intent("other")).unwrap();
    // Model a process that died after marking a call running, before its outcome.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("UPDATE tool_calls SET status='running' WHERE call_id='running'", []).unwrap();
    drop(conn);
    drop(store);
    let mut store = Store::open(&path).unwrap();
    let recovery = store.recover(&id).unwrap();
    assert_eq!(recovery.tool_calls.len(), 2);
    assert_eq!(recovery.interrupted_turns, vec![(id.clone(), 1)]);
    assert!(recovery.tool_calls.iter().all(|c| c.status == ToolStatus::Unknown && c.finished_at.is_some()));
    assert!(recovery.tool_calls.iter().all(|c| c.output.as_ref().unwrap()["text"] == "outcome unknown"));
    assert_eq!(store.tool_calls(&second).unwrap()[0].status, ToolStatus::Intent);
    assert!(store.recover(&id).unwrap().tool_calls.is_empty());
    assert_eq!(store.recover_all().unwrap().tool_calls.len(), 1);
    assert!(store.recover_all().unwrap().interrupted_turns.is_empty());
    assert_eq!(
        store.events_since(&id, 5, 100).unwrap().iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        ["tool.recovered", "tool.recovered", "turn.interrupted"]
    );
    assert!(store.append(&id, event("retry"), intent("intent")).is_err());
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(conn.query_row("SELECT status FROM turns WHERE session_id=?", [&id], |r| r.get::<_, String>(0)).unwrap(), "interrupted");
    assert!(conn.execute("DELETE FROM events", []).is_err());
    assert!(conn.execute("UPDATE events SET kind='tampered'", []).is_err());
    assert!(Store::open(&path).unwrap().tool_calls(&id).unwrap().iter().all(|c| matches!(c.status, ToolStatus::Done | ToolStatus::Unknown)));
}

#[test]
fn outbox_and_historical_fork() {
    let mut store = Store::open_in_memory().unwrap();
    let id = new_session(&mut store).id;
    store.append(&id, event("item"), Projection::PutItem(Item::user_text("a", "original"))).unwrap();
    store.append(&id, event("item"), Projection::PutItem(Item::user_text("b", "later"))).unwrap();
    store.append(&id, event("item"), Projection::PutItem(Item::user_text("a", "edited"))).unwrap();
    let fork = store.fork_session(&id, 1).unwrap();
    assert_eq!(store.items(&fork).unwrap(), [Item::user_text("a", "original")]);
    let session = store.session(&fork).unwrap().unwrap();
    assert_eq!(session.parent_id, Some(id.clone()));
    assert_eq!(session.last_seq, 1);
    assert!(session.meta.get("cache").is_none());
    assert!(store.tool_calls(&fork).unwrap().is_empty());
    assert_eq!(store.usage_totals(&fork).unwrap(), UsageTotals::default());
    let empty = store.fork_session(&id, 0).unwrap();
    assert!(store.items(&empty).unwrap().is_empty());
    assert!(store.fork_session(&id, 4).is_err());
    assert!(store.fork_session(&id, -1).is_err());
    let entry = store.outbox_push(&id, &fork, 1, json!({"result":"done"})).unwrap();
    assert_eq!(store.outbox_pending(&id).unwrap()[0].id, entry);
    store.outbox_mark_delivered(&entry).unwrap();
    store.outbox_mark_delivered(&entry).unwrap();
    assert!(store.outbox_pending(&id).unwrap().is_empty());
    assert!(store.outbox_mark_delivered("missing").is_err());
    assert!(store.outbox_push(&id, "missing", 1, json!(null)).is_err());
}

#[test]
fn outbox_versions_push_once_and_deliver_once() {
    let mut store = Store::open_in_memory().unwrap();
    let parent = new_session(&mut store);
    let child = store
        .create_session(NewSession {
            project_id: parent.project_id.clone(),
            parent_id: Some(parent.id.clone()),
            agent: "general".into(),
            model: "model".into(),
            provider: "test".into(),
            title: "child".into(),
            cwd: "/project".into(),
            meta: json!({}),
        })
        .unwrap();
    assert_eq!(store.children(&parent.id).unwrap().iter().map(|c| c.id.clone()).collect::<Vec<_>>(), std::slice::from_ref(&child.id));
    let first = store.outbox_push(&parent.id, &child.id, 3, json!({"status":"done"})).unwrap();
    assert_eq!(store.outbox_push(&parent.id, &child.id, 3, json!({"status":"again"})).unwrap(), first);
    let second = store.outbox_push(&parent.id, &child.id, 4, json!({"status":"done"})).unwrap();
    assert_eq!(store.outbox_pending(&parent.id).unwrap().iter().map(|e| e.id.clone()).collect::<Vec<_>>(), [first.clone(), second.clone()]);
    assert_eq!(store.outbox_pending_sessions().unwrap(), std::slice::from_ref(&parent.id));
    let item = |id: &str| Projection::PutItem(Item::user_text(format!("outbox-{id}"), "delivered"));
    let seq = store.append_delivering(&parent.id, event("item.done"), item(&first), SessionUpdate::default(), &first).unwrap();
    assert!(matches!(
        store.append_delivering(&parent.id, event("item.done"), item(&first), SessionUpdate::default(), &first),
        Err(Error::InvalidTransition(_))
    ));
    assert_eq!(store.session(&parent.id).unwrap().unwrap().last_seq, seq, "a refused delivery appends nothing");
    assert!(store.append_delivering(&child.id, event("item.done"), item(&second), SessionUpdate::default(), &second).is_err());
    store.append_delivering(&parent.id, event("item.done"), item(&second), SessionUpdate::default(), &second).unwrap();
    assert!(store.outbox_pending_sessions().unwrap().is_empty());
}

#[test]
fn independent_connections_assign_gap_free_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.db");
    let mut store = Store::open(&path).unwrap();
    let id = new_session(&mut store).id;
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).unwrap();
                for _ in 0..25 {
                    store.append(&id, event("tick"), Projection::None).unwrap();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(store.events_since(&id, 0, 200).unwrap().iter().map(|e| e.seq).collect::<Vec<_>>(), (1..=100).collect::<Vec<_>>());
}

#[test]
fn rejects_future_schema() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let conn = rusqlite::Connection::open(file.path()).unwrap();
    conn.pragma_update(None, "user_version", 999).unwrap();
    assert!(matches!(Store::open(file.path()), Err(Error::SchemaVersion(999))));
}

fn delta(kind: &str, text: &str) -> EventRecord {
    EventRecord { turn: 1, step: 1, kind: kind.into(), payload: json!({"text":text}) }
}
#[test]
fn coalescing_time_size_boundaries_and_metadata() {
    let mut c = Coalescer::new("session");
    assert_eq!(c.session_id(), "session");
    assert!(c.push(delta("text.delta", "a"), Duration::ZERO).is_empty());
    assert!(c.push(delta("text.delta", "b"), Duration::from_millis(49)).is_empty());
    assert_eq!(c.flush_due(Duration::from_millis(50)).unwrap().payload["text"], "ab");
    assert!(c.flush().is_none());
    assert_eq!(c.push(delta("text.delta", &"x".repeat(4096)), Duration::ZERO).len(), 1);
    assert!(c.push(delta("reasoning.delta", "a"), Duration::ZERO).is_empty());
    let mut summary = delta("reasoning.delta", "b");
    summary.payload["summary"] = json!(true);
    assert_eq!(c.push(summary, Duration::ZERO)[0].payload["text"], "a");
    assert_eq!(c.push(event("finish"), Duration::ZERO).len(), 2);
    assert!(c.flush().is_none());
    assert!(c.push(delta("text.delta", "a"), Duration::ZERO).is_empty());
    let mut next = delta("text.delta", "b");
    next.step = 2;
    assert_eq!(c.push(next, Duration::ZERO).len(), 1);
    assert_eq!(c.push(delta("reasoning.delta", "c"), Duration::ZERO).len(), 1);
    assert_eq!(c.push(delta("reasoning.delta", "d"), Duration::from_millis(51))[0].payload["text"], "c");
    assert_eq!(c.flush().unwrap().payload["text"], "d");
}
#[test]
fn recent_projects_order_by_latest_session() {
    let mut store = Store::open_in_memory().unwrap();
    let idle = store.create_project("/idle").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let busy = new_session(&mut store);
    std::thread::sleep(Duration::from_millis(5));
    let late = store.create_project("/late").unwrap();
    let recent = store.recent_projects().unwrap();
    assert_eq!(recent.iter().map(|(p, _)| p.root.as_str()).collect::<Vec<_>>(), ["/late", "/project", "/idle"]);
    assert_eq!(recent[0].1, late.created_at);
    assert_eq!(recent[2].1, idle.created_at);
    std::thread::sleep(Duration::from_millis(5));
    store.update_session(&busy.id, SessionUpdate { title: Some("touched".into()), ..Default::default() }).unwrap();
    let recent = store.recent_projects().unwrap();
    assert_eq!(recent[0].0.id, busy.project_id);
    assert_eq!(recent[0].1, store.session(&busy.id).unwrap().unwrap().updated_at);
}

#[test]
fn turn_trees_and_changes_survive_the_migration() {
    // A version 2 database with a turn: migration 3 adds the snapshot columns, empty.
    let file = tempfile::NamedTempFile::new().unwrap();
    {
        let conn = rusqlite::Connection::open(file.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE turns (session_id TEXT NOT NULL, turn INTEGER NOT NULL, status TEXT NOT NULL, started_at INTEGER NOT NULL, finished_at INTEGER, PRIMARY KEY(session_id,turn));
             INSERT INTO turns VALUES('old',1,'done',1,2);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
    }
    let mut store = Store::open(file.path()).unwrap();
    assert_eq!(store.turn_trees("old", 1).unwrap(), (None, None));
    store.set_turn_trees("old", 1, Some("base"), None).unwrap();
    store.set_turn_trees("old", 1, None, Some("end")).unwrap();
    assert_eq!(store.turn_trees("old", 1).unwrap(), (Some("base".into()), Some("end".into())));
    assert!(store.set_turn_trees("old", 2, Some("x"), None).is_err());
    assert_eq!(store.session_base_tree("old").unwrap().as_deref(), Some("base"), "the session's first snapshot");
    assert_eq!(store.session_base_tree("none").unwrap(), None);

    let mut store = Store::open_in_memory().unwrap();
    let id = new_session(&mut store).id;
    store.append(&id, event("turn.started"), Projection::TurnStatus(TurnStatus::Running)).unwrap();
    store.set_turn_trees(&id, 1, Some("aaa"), None).unwrap();
    let mut reply = Item::user_text("reply", "done");
    reply.role = serde_json::from_value(json!("assistant")).unwrap();
    store.append(&id, event("item.done"), Projection::PutItem(Item::user_text("prompt", "go"))).unwrap();
    store.append(&id, event("item.done"), Projection::PutItem(reply)).unwrap();
    let changes = json!({"base_tree":"aaa","end_tree":"bbb","files":["a.rs"],"added":1,"removed":0});
    let finished = EventRecord { turn: 1, step: 0, kind: "turn.finished".into(), payload: json!({"turn":1,"status":"done","changes":changes}) };
    store.append(&id, finished, Projection::TurnStatus(TurnStatus::Done)).unwrap();
    assert_eq!(store.turn_trees(&id, 1).unwrap(), (Some("aaa".into()), None));
    assert_eq!(store.turn_changes(&id).unwrap(), [(1, Some("reply".into()), changes)]);
    // A turn without changes is not listed.
    let plain = EventRecord { turn: 2, step: 0, kind: "turn.finished".into(), payload: json!({"turn":2,"status":"done"}) };
    store.append(&id, plain, Projection::TurnStatus(TurnStatus::Done)).unwrap();
    assert_eq!(store.turn_changes(&id).unwrap().len(), 1);
}

#[test]
fn turn_usage_sums_each_turns_steps_under_its_reply() {
    let mut store = Store::open_in_memory().unwrap();
    let id = new_session(&mut store).id;
    let at = |turn: i64, step: i64, kind: &str| EventRecord { turn, step, kind: kind.into(), payload: json!({}) };
    let usage = |input: u64, cache_read: u64, cost_usd: f64| {
        Projection::Usage(UsageRecord {
            provider: "test".into(),
            model: "model".into(),
            usage: Usage { input, output: 10, cache_read, cache_write: 1, reasoning: 2 },
            cost_usd,
        })
    };
    let mut reply = Item::user_text("r1", "done");
    reply.role = serde_json::from_value(json!("assistant")).unwrap();
    let mut call = reply.clone();
    call.id = "c1".into();
    store.append(&id, at(1, 0, "item.done"), Projection::PutItem(Item::user_text("u1", "go"))).unwrap();
    store.append(&id, at(1, 1, "item.done"), Projection::PutItem(call)).unwrap();
    store.append(&id, at(1, 1, "usage"), usage(100, 0, 0.25)).unwrap();
    store.append(&id, at(1, 2, "item.done"), Projection::PutItem(reply)).unwrap();
    store.append(&id, at(1, 2, "usage"), usage(5, 95, 0.5)).unwrap();
    // A turn whose reply isn't written yet.
    store.append(&id, at(2, 1, "usage"), usage(7, 0, 0.0)).unwrap();
    let turns = store.turn_usage(&id).unwrap();
    let first = UsageTotals { usage: Usage { input: 105, output: 20, cache_read: 95, cache_write: 2, reasoning: 4 }, cost_usd: 0.75 };
    assert_eq!(turns[0], (1, Some("r1".into()), first));
    assert_eq!((turns[1].0, turns[1].1.as_deref(), turns[1].2.usage.input), (2, None, 7));
    assert_eq!(turns.len(), 2);
    assert!(store.turn_usage("none").unwrap().is_empty());
}

#[test]
fn usage_rows_keep_each_step_and_its_model_dated_by_its_turn() {
    let mut store = Store::open_in_memory().unwrap();
    let id = new_session(&mut store).id;
    let at = |turn: i64, step: i64, kind: &str| EventRecord { turn, step, kind: kind.into(), payload: json!({}) };
    let usage = |model: &str, input: u64| {
        Projection::Usage(UsageRecord {
            provider: "anthropic".into(),
            model: model.into(),
            usage: Usage { input, output: 1, ..Usage::default() },
            cost_usd: 0.0,
        })
    };
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    store.append(&id, at(1, 0, "turn.started"), Projection::TurnStatus(TurnStatus::Running)).unwrap();
    store.append(&id, at(1, 1, "usage"), usage("a", 10)).unwrap();
    // A switch mid-turn: a row of its own.
    store.append(&id, at(1, 2, "usage"), usage("b", 20)).unwrap();
    // A step the turns table doesn't know (no turn status): dated by its events.
    store.append(&id, at(2, 1, "usage"), usage("a", 30)).unwrap();
    let rows = store.usage_rows(&id).unwrap();
    let seen: Vec<_> = rows.iter().map(|r| (r.turn, r.step, r.record.model.as_str(), r.record.usage.input)).collect();
    assert_eq!(seen, [(1, 1, "a", 10), (1, 2, "b", 20), (2, 1, "a", 30)]);
    assert!(rows.iter().all(|r| r.at >= before && r.session_id == id));
    assert_eq!(store.usage_since(before).unwrap().len(), 3);
    assert!(store.usage_since(i64::MAX).unwrap().is_empty());
    assert!(store.usage_rows("none").unwrap().is_empty());
}
