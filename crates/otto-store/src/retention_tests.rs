use crate::{tests::session, *};
use serde_json::json;

fn count(store: &Store, sql: &str) -> i64 {
    store.connection.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn undo_history_is_capped_and_kept_across_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("undo.db");
    let mut store = Store::open(&path).unwrap();
    let id = session(&mut store);
    let text = |i: usize| format!("version {i}\n");
    store.undo_push(&id, "/p/new.txt", None, Some("created\n")).unwrap();
    for i in 1..=UNDO_PER_FILE + 10 {
        store.undo_push(&id, "/p/a.txt", Some(&text(i - 1)), Some(&text(i))).unwrap();
    }
    let rows = |s: &Store, p: &str| count(s, &format!("SELECT count(*) FROM undo_edits WHERE path='{p}'"));
    assert_eq!(rows(&store, "/p/a.txt"), i64::try_from(UNDO_PER_FILE).unwrap(), "the newest per file");
    drop(store);
    let mut store = Store::open(&path).unwrap();
    let last = store.undo_last(&id, "/p/a.txt").unwrap().unwrap();
    assert_eq!((last.before, last.after), (Some(text(UNDO_PER_FILE + 9)), Some(text(UNDO_PER_FILE + 10))));
    store.undo_remove(&id, last.id).unwrap();
    assert_eq!(store.undo_last(&id, "/p/a.txt").unwrap().unwrap().after, Some(text(UNDO_PER_FILE + 9)));
    assert_eq!(store.undo_last(&id, "/p/new.txt").unwrap().unwrap().before, None, "absent before");
    assert_eq!(store.undo_last(&id, "/p/other.txt").unwrap(), None);
    // Per session: the newest edits within the byte cap.
    let big = |i: usize| format!("{i:08}{}", "x".repeat(512 << 10));
    for i in 0..20 {
        store.undo_push(&id, &format!("/p/big{}.txt", i % 3), Some(&big(i)), Some(&big(i + 100))).unwrap();
    }
    let total = count(&store, &format!("SELECT sum(bytes) FROM undo_edits WHERE session_id='{id}'"));
    assert!(total <= UNDO_SESSION_BYTES && total > UNDO_SESSION_BYTES - (2 << 20), "{total}");
    assert_eq!(store.undo_last(&id, "/p/big1.txt").unwrap().unwrap().after, Some(big(119)), "the newest stay");
    assert_eq!(rows(&store, "/p/a.txt"), 0, "the oldest went first");
    // Their content is stored once and goes when nothing keeps it.
    store.maintain().unwrap();
    let kept = count(
        &store,
        "SELECT count(*) FROM (SELECT before FROM undo_edits WHERE before IS NOT NULL UNION SELECT after FROM undo_edits WHERE after IS NOT NULL)",
    );
    assert_eq!(count(&store, "SELECT count(*) FROM blobs"), kept);
    // An archived session's history goes.
    store.update_session(&id, SessionUpdate { archived: Some(true), ..Default::default() }).unwrap();
    assert!(store.maintain().unwrap().undo_dropped > 0);
    assert_eq!((count(&store, "SELECT count(*) FROM undo_edits"), count(&store, "SELECT count(*) FROM blobs")), (0, 0));
}

#[test]
fn a_blob_still_referenced_elsewhere_is_kept() {
    let mut store = Store::open_in_memory().unwrap();
    let id = session(&mut store);
    let long = "shared file content\n".repeat(200);
    let item = Item::user_text("u", long.clone());
    store
        .append(&id, EventRecord { turn: 1, step: 1, kind: "item.done".into(), payload: json!({ "item": item }) }, Projection::PutItem(item.clone()))
        .unwrap();
    let edit = store.undo_push(&id, "/p/f", Some(&long), Some("short")).unwrap();
    store.undo_remove(&id, edit).unwrap();
    assert_eq!(store.maintain().unwrap().blobs_deleted, 1, "only the undo-only content");
    assert_eq!(store.items(&id).unwrap(), [item]);
}

#[test]
fn a_long_session_stays_bounded_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("long.db")).unwrap();
    assert_eq!(count(&store, "PRAGMA auto_vacuum"), 2, "incremental");
    store.connection.pragma_update(None, "synchronous", "OFF").unwrap();
    let id = session(&mut store);
    let (turns, deltas) = (60, 80);
    let mut items = Vec::new();
    let mut outputs = Vec::new();
    for turn in 1..=turns {
        let rec = |kind: &str, payload| EventRecord { turn, step: 1, kind: kind.into(), payload };
        store.append(&id, rec("turn.started", json!({"turn": turn})), Projection::TurnStatus(TurnStatus::Running)).unwrap();
        let item_id = format!("a{turn}");
        store.append(&id, rec("item.started", json!({"item_id": item_id, "role": "assistant"})), Projection::None).unwrap();
        for d in 0..deltas {
            // Each delta is as long as a coalesced one (4 KiB at most): long enough for a blob.
            let text = format!("{turn}:{d}:{}", "streamed ".repeat(330));
            store.append(&id, rec("text.delta", json!({"item_id": item_id, "text": text})), Projection::None).unwrap();
        }
        let item = Item::new(
            &item_id,
            otto_llm::types::Role::Assistant,
            vec![otto_llm::types::Part::Text { text: format!("reply {turn}"), citations: vec![] }],
        );
        store.append(&id, rec("item.done", json!({ "item": item })), Projection::PutItem(item.clone())).unwrap();
        items.push(item);
        let call = format!("c{turn}");
        let input = json!({"command": "cargo test"});
        store
            .append(
                &id,
                rec("tool.call", json!({"call_id": call, "input": input})),
                Projection::ToolIntent { call_id: call.clone(), name: "shell".into(), input },
            )
            .unwrap();
        let output = json!({"content": [{"type": "text", "text": format!("{turn} {}", "ok\n".repeat(3000))}]});
        let outcome = Projection::ToolOutcome { call_id: call.clone(), status: ToolStatus::Done, output: output.clone() };
        store.append(&id, rec("tool.result", json!({"call_id": call, "output": output})), outcome).unwrap();
        outputs.push(output);
        let file = |i: i64| format!("{i} {}", "line\n".repeat(2000));
        store.undo_push(&id, "/p/main.rs", Some(&file(turn - 1)), Some(&file(turn))).unwrap();
        store.append(&id, rec("turn.finished", json!({"turn": turn})), Projection::TurnStatus(TurnStatus::Done)).unwrap();
    }
    let streamed = turns * deltas * 3000;
    store.maintain().unwrap();
    let size = count(&store, "PRAGMA page_count") * count(&store, "PRAGMA page_size");
    assert!(size < streamed / 3, "{size} bytes after streaming {streamed}");
    assert_eq!(count(&store, "PRAGMA freelist_count"), 0, "free pages given back");
    assert_eq!(count(&store, "SELECT count(*) FROM events WHERE kind='text.delta'"), turns, "the first of each run stays");
    assert_eq!(count(&store, "SELECT count(*) FROM undo_edits"), i64::try_from(UNDO_PER_FILE).unwrap());
    // Replay: the view's items and every event read back whole.
    assert_eq!(store.items(&id).unwrap(), items);
    let calls: Vec<_> = store.tool_calls(&id).unwrap().into_iter().map(|c| c.output.unwrap()).collect();
    assert_eq!(calls, outputs);
    let events = store.events_since(&id, 0, usize::MAX).unwrap();
    let text = serde_json::to_string(&events).unwrap();
    assert!(!text.contains("$otto_text") && !text.contains("$otto_ref") && !text.contains(refs::BLOB), "every ref resolves");
    let results: Vec<_> = events.iter().filter(|e| e.kind == "tool.result").map(|e| e.payload["output"].clone()).collect();
    assert_eq!(results, outputs);
    assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(store.marks(&id, &["text.delta"]).unwrap().len(), usize::try_from(turns).unwrap(), "part timing keeps its runs");
}
