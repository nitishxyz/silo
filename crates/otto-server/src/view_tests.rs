//! Sessions larger than a frame open, page back, and never cost the connection.
use crate::tests::*;
use otto_llm::{CallKind, Media, MediaSource, ResultPart, Role};
use otto_proto::clip::{ITEM_BUDGET, VIEW_BUDGET, json_len};
use otto_store::{EventRecord, Projection, Store};

/// A server whose store the test fills directly.
async fn with_store(f: &mut Fixture) -> Arc<StdMutex<Store>> {
    let store = Arc::new(StdMutex::new(Store::open_in_memory().unwrap()));
    f.runtime = Arc::new(
        otto_core::Runtime::new(otto_core::RuntimeConfig {
            store: store.clone(),
            models: f.models.clone(),
            state_dir: f.dir.path().to_owned(),
            user_config_dir: f.dir.path().to_owned(),
        })
        .await
        .unwrap(),
    );
    store
}

fn put(store: &Arc<StdMutex<Store>>, session: &str, item: Item) {
    let record = EventRecord { turn: 1, step: 1, kind: "item.done".into(), payload: json!({ "item": item }) };
    store.lock().unwrap().append(session, record, Projection::PutItem(item)).unwrap();
}

fn screenshot(bytes: usize) -> ResultPart {
    ResultPart::Image(Media { mime: "image/png".into(), source: MediaSource::Base64 { data: "Q".repeat(bytes * 4 / 3) }, name: None })
}

/// 26 MB of items: an early todo list, 120 replies of 200 KB, and two 2 MB tool results.
fn fill(store: &Arc<StdMutex<Store>>, session: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut add = |item: Item| {
        ids.push(item.id.clone());
        put(store, session, item);
    };
    let todos = json!({"todos": [{"step": "Page back", "status": "in_progress"}]});
    add(Item::new(
        "todo-call",
        Role::Assistant,
        vec![Part::ToolCall { call_id: "todo".into(), name: "update_todos".into(), input: todos, kind: CallKind::Function, opaque: None }],
    ));
    for i in 0..120 {
        add(Item::new(format!("reply-{i:03}"), Role::Assistant, vec![Part::text(format!("{i}:{}", "w".repeat(200 * 1024)))]));
        if i == 60 || i == 119 {
            add(Item::new(
                format!("shot-call-{i}"),
                Role::Assistant,
                vec![Part::ToolCall {
                    call_id: format!("shot-{i}"),
                    name: "browser_screenshot".into(),
                    input: json!({}),
                    kind: CallKind::Function,
                    opaque: None,
                }],
            ));
            add(Item::new(
                format!("shot-{i}"),
                Role::Tool,
                vec![Part::ToolResult {
                    call_id: format!("shot-{i}"),
                    content: vec![ResultPart::text(format!("page {}", "p".repeat(1024 * 1024))), screenshot(2 * 1024 * 1024)],
                    is_error: false,
                }],
            ));
        }
    }
    ids
}

#[tokio::test]
async fn a_session_larger_than_a_frame_opens_and_pages_back_to_its_start() {
    let mut f = Fixture::new(vec![]).await;
    let store = with_store(&mut f).await;
    let server = f.start().await;
    let client = f.connect().await;
    let session = f.session(&client).await;
    let ids = fill(&store, &session.id);
    let stored: usize = store.lock().unwrap().items(&session.id).unwrap().iter().map(json_len).sum();
    assert!(stored > MAX_FRAME, "{stored}");

    let follower = SessionFollower::new(f.connect().await, &session.id).await.unwrap();
    let first = follower.current().mirror;
    assert!(first.has_older);
    assert!(first.items.len() < ids.len());
    assert!(first.items.iter().map(json_len).sum::<usize>() <= VIEW_BUDGET);
    // The newest window, in order, ending with the newest item.
    let tail = &ids[ids.len() - first.items.len()..];
    assert_eq!(first.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), tail);
    assert_eq!(first.todos.as_ref().unwrap()["todos"][0]["step"], "Page back");
    let shot = first.item("shot-119").unwrap();
    assert!(shot.clipped.unwrap() > 3 * 1024 * 1024);
    assert!(json_len(shot) <= ITEM_BUDGET);
    let Part::ToolResult { content, .. } = &shot.parts[0] else { panic!("{shot:?}") };
    assert!(matches!(&content[1], ResultPart::Text { text } if text.starts_with("[image/png, 2.0 MB")));

    let mut pages = 0;
    while follower.current().mirror.has_older {
        let before = follower.current().mirror.items.len();
        let added = follower.load_older().await.unwrap();
        assert!(added > 0 && added <= otto_proto::clip::PAGE_ITEMS as usize);
        assert_eq!(follower.current().mirror.items.len(), before + added);
        pages += 1;
    }
    assert!(pages >= 1);
    let all = follower.current().mirror;
    assert_eq!(all.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ids);
    assert!(all.item("shot-60").unwrap().clipped.is_some());
    assert!(all.item("reply-000").unwrap().clipped.is_none());
    assert_eq!(follower.load_older().await.unwrap(), 0);
    // Still following: a rename arrives.
    client.call(Method::SessionRename { session_id: session.id.clone(), title: "Big".into() }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut changes = follower.changes();
        while changes.borrow_and_update().mirror.info.title != "Big" {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(follower.current().mirror.items.len(), ids.len());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_old_style_view_without_params_gets_the_newest_window_and_limits_hold() {
    let mut f = Fixture::new(vec![]).await;
    let store = with_store(&mut f).await;
    let server = f.start().await;
    let client = f.connect().await;
    let session = f.session(&client).await;
    let ids = fill(&store, &session.id);
    // The request an older client sends: `{"session_id"}` alone.
    let request = serde_json::to_value(Method::SessionView { session_id: session.id.clone(), limit: None, before_seq: None }).unwrap();
    assert_eq!(request["params"], json!({ "session_id": session.id }));
    let raw = client.call(Method::SessionView { session_id: session.id.clone(), limit: None, before_seq: None }).await.unwrap();
    assert!(serde_json::to_vec(&raw).unwrap().len() < VIEW_BUDGET + 1024 * 1024);
    assert_eq!(raw["has_older"], true);
    let view: SessionView = serde_json::from_value(raw).unwrap();
    assert_eq!(view.items.last().unwrap().id, *ids.last().unwrap());

    let five =
        client.call_typed::<SessionView>(Method::SessionView { session_id: session.id.clone(), limit: Some(5), before_seq: None }).await.unwrap();
    assert_eq!(five.items.iter().map(|i| i.id.clone()).collect::<Vec<_>>(), ids[ids.len() - 5..]);
    let older = client.session_view_before(&session.id, five.oldest_seq.unwrap(), Some(3)).await.unwrap();
    assert_eq!(older.items.iter().map(|i| i.id.clone()).collect::<Vec<_>>(), ids[ids.len() - 8..ids.len() - 5]);
    assert!(older.has_older);
    let first = client.session_view_before(&session.id, 2, None).await.unwrap();
    assert!(first.items.is_empty() || first.items[0].id == ids[0]);
    assert!(!first.has_older);
    // otto's own view still holds every item, unclipped.
    let full = f.runtime.view(&session.id).await.unwrap();
    assert_eq!(full.items.len(), ids.len());
    assert!(full.items.iter().all(|i| i.clipped.is_none()));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reply_too_large_for_a_frame_is_an_error_and_the_connection_lives() {
    let f = Fixture::new(vec![]).await;
    let server = f.start().await;
    let client = f.connect().await;
    let big = "x".repeat(9 * 1024 * 1024);
    client.call(Method::ConfigSet { key: "first".into(), value: json!(big) }).await.unwrap();
    client.call(Method::ConfigSet { key: "second".into(), value: json!(big) }).await.unwrap();
    let error = client.call(Method::ConfigGet { key: None }).await.unwrap_err();
    assert!(
        matches!(&error, otto_client::ClientError::Rpc(RpcError { code, data: Some(data), .. }) if code == RpcError::TOO_LARGE && data["bytes"].as_u64().unwrap() > MAX_FRAME as u64),
        "{error:?}"
    );
    // The same connection still answers.
    client.call(Method::Ping {}).await.unwrap();
    assert_eq!(client.call(Method::ConfigGet { key: Some("first".into()) }).await.unwrap()["value"].as_str().unwrap().len(), big.len());
    server.shutdown().await.unwrap();
}
