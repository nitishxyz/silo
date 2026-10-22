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
    assert_eq!(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
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
