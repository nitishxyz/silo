use otto_client::*;
use otto_proto::*;
use serde_json::json;
use tokio::io::DuplexStream;

fn view(seq: i64) -> SessionView {
    serde_json::from_value(json!({"info":{"id":"s","project_id":"p","agent":"a","provider":"p","model":"m","title":"t","cwd":"/","status":"idle","created_at":0,"updated_at":0,"last_seq":seq,"usage":{"usage":{"input":0,"output":0},"cost_usd":0.0}},"items":[],"pending_approvals":[],"pending_questions":[],"queue":[],"running_tool_calls":[],"seq":seq})).unwrap()
}
async fn hello(s: &mut DuplexStream) {
    let f = read_frame(s).await.unwrap().unwrap();
    assert_eq!(f.kind, kind::HELLO);
    assert_eq!(f.parse::<ClientHello>().unwrap().protocol, PROTOCOL);
    write_frame(
        s,
        &Frame::json(
            kind::HELLO,
            &ServerHello {
                protocol: PROTOCOL,
                version: "test".into(),
                server_id: "server".into(),
                pid: 1,
                started_at: 0,
                client_id: "c".into(),
                home: "/home/u".into(),
                silo_home: "/silo".into(),
                build: String::new(),
            },
        )
        .unwrap(),
    )
    .await
    .unwrap();
}
async fn request(s: &mut DuplexStream) -> Request {
    read_frame(s).await.unwrap().unwrap().parse().unwrap()
}
async fn response(s: &mut DuplexStream, id: u64, result: serde_json::Value) {
    write_frame(s, &Frame::json(kind::CONTROL, &ServerMessage::Response { id, result: Some(result), error: None }).unwrap()).await.unwrap();
}
async fn event(s: &mut DuplexStream, seq: i64, event: Event) {
    write_frame(s, &Frame::json(kind::CONTROL, &ServerMessage::Event { seq, session_id: Some("s".into()), event: Box::new(event) }).unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn unknown_and_malformed_live_payloads_do_not_disconnect() {
    let (client, mut server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        hello(&mut server).await;
        let first = request(&mut server).await;
        for payload in [json!({"event":"future.delta","data":{}}), json!({"event":"text.delta","data":{}})] {
            write_frame(
                &mut server,
                &Frame::json(kind::CONTROL, &json!({"live":true,"id":0,"session_id":"s","after_seq":0,"event":payload})).unwrap(),
            )
            .await
            .unwrap();
        }
        event(
            &mut server,
            1,
            Event::ItemDone {
                item: serde_json::from_value(json!({"id":"i","role":"assistant","parts":[{"type":"text","text":"durable"}]})).unwrap(),
            },
        )
        .await;
        response(&mut server, first.id, json!({})).await;
        let second = request(&mut server).await;
        response(&mut server, second.id, json!({})).await;
    });
    let connection = Connection::connect_stream(client).await.unwrap();
    let mut events = connection.events();
    connection.call(Method::Ping {}).await.unwrap();
    let mut mirror = SessionMirror::new(view(0));
    for _ in 0..2 {
        assert!(matches!(events.recv().await.unwrap().unwrap(), ServerMessage::LiveUnknown { .. }));
        assert_eq!(mirror.last_seq, 0);
    }
    let ServerMessage::Event { seq, event, .. } = events.recv().await.unwrap().unwrap() else {
        panic!("expected durable event");
    };
    assert_eq!(mirror.apply(seq, &event), ApplyResult::Applied);
    assert_eq!(mirror.item("i").unwrap().text(), "durable");
    connection.call(Method::Ping {}).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn matching_events_and_loss() {
    let (client, mut server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        hello(&mut server).await;
        let a = request(&mut server).await;
        let b = request(&mut server).await;
        response(&mut server, b.id, json!("b")).await;
        event(&mut server, 1, Event::TurnStarted { turn: 1 }).await;
        response(&mut server, a.id, json!("a")).await;
        let _ = request(&mut server).await;
    });
    let c = Connection::connect_stream(client).await.unwrap();
    let mut events = c.events();
    let (a, b) = tokio::join!(c.call(Method::Ping {}), c.call(Method::ServerInfo {}));
    assert_eq!(a.unwrap(), json!("a"));
    assert_eq!(b.unwrap(), json!("b"));
    assert!(!c.is_closed());
    assert!(matches!(events.recv().await.unwrap().unwrap(), ServerMessage::Event { seq: 1, .. }));
    assert!(c.call(Method::Ping {}).await.is_err());
    assert!(events.recv().await.unwrap().is_err());
    assert!(c.is_closed(), "a dead transport reads as closed");
    assert!(c.call(Method::Ping {}).await.is_err());
    task.await.unwrap();
}
#[tokio::test]
async fn follower_resyncs_gap_and_replacement() {
    let (client, mut server) = tokio::io::duplex(8192);
    let task = tokio::spawn(async move {
        hello(&mut server).await;
        let r = request(&mut server).await;
        assert!(matches!(r.method, Method::SessionView { .. }));
        response(&mut server, r.id, serde_json::to_value(view(0)).unwrap()).await;
        let r = request(&mut server).await;
        assert!(matches!(r.method, Method::SessionSubscribe { after_seq: Some(0), .. }));
        event(&mut server, 2, Event::TurnStarted { turn: 1 }).await;
        response(&mut server, r.id, json!({"session_id":"s","seq":2})).await;
        let r = request(&mut server).await;
        assert!(matches!(r.method, Method::SessionView { .. }));
        response(&mut server, r.id, serde_json::to_value(view(2)).unwrap()).await;
        let r = request(&mut server).await;
        assert!(matches!(r.method, Method::SessionSubscribe { after_seq: Some(2), .. }));
        event(&mut server, 3, Event::TurnStarted { turn: 2 }).await;
        response(&mut server, r.id, json!({"session_id":"s","seq":3})).await;
    });
    let c = Connection::connect_stream(client).await.unwrap();
    let follower = SessionFollower::new(c, "s").await.unwrap();
    let mut changes = follower.changes();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if changes.borrow().mirror.last_seq == 3 {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(changes.borrow().mirror.turn, Some((2, TurnStatus::Running)));
    task.await.unwrap();
    let (client, mut server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        hello(&mut server).await;
        let r = request(&mut server).await;
        assert!(matches!(r.method, Method::SessionSubscribe { after_seq: Some(3), .. }));
        event(&mut server, 4, Event::TurnFinished { turn: 2, status: TurnStatus::Done, usage: Usage::default(), changes: None }).await;
        response(&mut server, r.id, json!({"session_id":"s","seq":4})).await;
    });
    follower.reconnect(Connection::connect_stream(client).await.unwrap()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if changes.borrow().mirror.last_seq == 4 {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    task.await.unwrap();
}
#[tokio::test]
async fn connection_reconnects_through_its_dialer() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = dials.clone();
    let dial: Dial = Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let (client, mut server) = tokio::io::duplex(8192);
            tokio::spawn(async move {
                hello(&mut server).await;
                if n == 0 {
                    let r = request(&mut server).await;
                    assert!(matches!(r.method, Method::SessionView { .. }));
                    response(&mut server, r.id, serde_json::to_value(view(0)).unwrap()).await;
                    let r = request(&mut server).await;
                    assert!(matches!(r.method, Method::SessionSubscribe { after_seq: Some(0), .. }));
                    event(&mut server, 1, Event::TurnStarted { turn: 1 }).await;
                    response(&mut server, r.id, json!({"session_id":"s","seq":1})).await;
                    // Dropping the stream drops the connection.
                } else {
                    let r = request(&mut server).await;
                    assert!(matches!(r.method, Method::SessionSubscribe { after_seq: Some(1), .. }), "resumes from last_seq");
                    event(&mut server, 2, Event::TurnFinished { turn: 1, status: TurnStatus::Done, usage: Usage::default(), changes: None }).await;
                    response(&mut server, r.id, json!({"session_id":"s","seq":2})).await;
                    while read_frame(&mut server).await.is_ok_and(|f| f.is_some()) {}
                }
            });
            Ok(Box::new(client) as Box<dyn Stream>)
        })
    });
    let connection = Connection::connect_dial(dial).await.unwrap();
    let follower = SessionFollower::new(connection, "s").await.unwrap();
    let mut changes = follower.changes();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if changes.borrow().mirror.last_seq == 2 && changes.borrow().connected {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(changes.borrow().mirror.turn, Some((1, TurnStatus::Done)));
    assert_eq!(dials.load(Ordering::SeqCst), 2);
}
#[test]
fn mirror_script_matches_snapshot_and_deduplicates() {
    let mut m = SessionMirror::new(view(0));
    let item: Item = serde_json::from_value(json!({"id":"i","role":"assistant","parts":[{"type":"text","text":"final"}]})).unwrap();
    let start: Event = serde_json::from_value(json!({"event":"item.started","data":{"item_id":"i","role":"assistant"}})).unwrap();
    let script = [
        start,
        Event::TextDelta { item_id: "i".into(), text: "draft".into() },
        Event::ReasoningDelta { item_id: "i".into(), text: "thought".into(), summary: false },
        Event::ItemDone { item: item.clone() },
    ];
    for (i, e) in script.iter().enumerate() {
        assert_eq!(m.apply(i as i64 + 1, e), ApplyResult::Applied);
        assert_eq!(m.apply(i as i64 + 1, e), ApplyResult::Duplicate);
        if i == 1 {
            assert_eq!(m.item("i").unwrap().text(), "draft");
        }
    }
    let mut later = view(4);
    later.items.push(item);
    let expected = SessionMirror::new(later);
    assert_eq!(m.items, expected.items);
    assert_eq!(serde_json::to_value(&m.info).unwrap(), serde_json::to_value(&expected.info).unwrap());
    assert_eq!(m.apply(6, &Event::TurnStarted { turn: 1 }), ApplyResult::Gap { expected: 5, got: 6 });
    assert_eq!(m.last_seq, 4);
}
#[test]
fn approvals_questions_tools_and_usage() {
    let mut m = SessionMirror::new(view(0));
    let script = vec![
        Event::ToolInputDelta { call_id: "c".into(), delta: "{".into() },
        Event::ToolCall { call_id: "c".into(), name: "tool".into(), input: json!({}) },
        Event::ToolProgress { call_id: "c".into(), message: Some("a".into()), output: Some(json!(1)) },
        Event::ToolProgress { call_id: "c".into(), message: Some("b".into()), output: Some(json!(2)) },
        Event::ApprovalRequested {
            approval_id: "a".into(),
            call_id: "c".into(),
            tool: "tool".into(),
            summary: "ok?".into(),
            input: json!({}),
            effects: vec![],
        },
        Event::ApprovalResolved { approval_id: "a".into(), decision: ApprovalDecision::Allow },
        Event::QuestionAsked { question_id: "q".into(), call_id: "c".into(), question: "?".into(), options: vec![] },
        Event::QuestionResolved { question_id: "q".into(), answer: "yes".into() },
        Event::ToolResult { call_id: "c".into(), is_error: false, output: json!(3), item_id: "result".into() },
        Event::Usage {
            turn: 1,
            step: 1,
            usage: Usage { input: 4, output: 2, ..Usage::default() },
            cost_usd: Some(0.1),
            context: otto_proto::ContextUse { used: 4, window: 1000 },
            api_cost_usd: None,
            provider: None,
            model: None,
        },
        Event::TurnFinished { turn: 1, status: TurnStatus::Done, usage: Usage { input: 4, output: 2, ..Usage::default() }, changes: None },
    ];
    for (i, e) in script.iter().enumerate() {
        m.apply(i as i64 + 1, e);
        if i == 4 {
            assert_eq!(m.pending_approvals.len(), 1);
        }
        if i == 6 {
            assert_eq!(m.pending_questions.len(), 1);
        }
    }
    assert!(m.pending_approvals.is_empty());
    assert!(m.pending_questions.is_empty());
    let tool = &m.tools["c"];
    assert_eq!(tool.progress, vec![json!(1), json!(2)]);
    assert_eq!(tool.messages, vec!["a", "b"]);
    assert_eq!(tool.status, ToolStatus::Done);
    assert_eq!(tool.output, Some(json!(3)));
    assert_eq!(m.info.usage.usage.input, 4);
    assert_eq!(m.info.usage.cost_usd, 0.1);
    assert_eq!(m.info.context, otto_proto::ContextUse { used: 4, window: 1000 });
    m.apply(script.len() as i64 + 1, &Event::Compaction { through_seq: 1, summary: None, id: None });
    assert_eq!(m.info.context, otto_proto::ContextUse { used: 0, window: 1000 });
}
#[test]
fn compactions_stream_live_and_give_way_to_their_checkpoint() {
    let mut m = SessionMirror::new(view(0));
    m.apply(1, &Event::ItemDone { item: Item::user_text("u", "hi") });
    m.apply(2, &Event::CompactionStarted { id: "c".into(), trigger: "manual".into() });
    assert_eq!(m.compactions[0].at, 1);
    assert_eq!(m.compactions[0].status, CompactionStatus::Running);
    // Live text adds up, and a durable event in between replays nothing twice.
    assert_eq!(m.apply_live(2, &Event::CompactionDelta { id: "c".into(), text: "Goal: ".into() }), ApplyResult::Applied);
    m.apply(3, &Event::QueueChanged { queued: vec![] });
    assert_eq!(m.apply_live(3, &Event::CompactionDelta { id: "c".into(), text: "ship".into() }), ApplyResult::Applied);
    m.apply(
        4,
        &Event::Usage {
            turn: 0,
            step: 0,
            usage: Usage::default(),
            cost_usd: None,
            context: ContextUse::default(),
            api_cost_usd: None,
            provider: None,
            model: None,
        },
    );
    assert_eq!(m.compactions[0].summary, "Goal: ship");
    assert_eq!(m.apply_live(1, &Event::CompactionDelta { id: "c".into(), text: "stale".into() }), ApplyResult::Duplicate);
    // The checkpoint item takes over; the live entry leaves.
    let mut checkpoint = Item::user_text("c", "");
    checkpoint.parts = vec![Part::Compaction { summary: Some("Goal: ship".into()), opaque: None }];
    m.apply(5, &Event::ItemDone { item: checkpoint });
    m.apply(6, &Event::Compaction { through_seq: 4, summary: Some("Goal: ship".into()), id: Some("c".into()) });
    assert!(m.compactions.is_empty());
    // A failure stays, with its message.
    m.apply(7, &Event::CompactionStarted { id: "d".into(), trigger: "auto".into() });
    m.apply(8, &Event::CompactionFailed { id: "d".into(), message: "empty compaction summary".into(), cancelled: false });
    assert_eq!(m.compactions[0].status, CompactionStatus::Failed { message: "empty compaction summary".into(), cancelled: false });
    // One the turn's end cut off reads as stopped; one that began before the view shows from its next delta.
    m.apply(9, &Event::CompactionStarted { id: "e".into(), trigger: "auto".into() });
    m.apply(10, &Event::TurnFinished { turn: 1, status: TurnStatus::Cancelled, usage: Usage::default(), changes: None });
    assert!(matches!(m.compactions[1].status, CompactionStatus::Failed { cancelled: true, .. }));
    m.apply_live(10, &Event::CompactionDelta { id: "f".into(), text: "late".into() });
    assert_eq!((m.compactions[2].trigger.as_str(), m.compactions[2].summary.as_str()), ("", "late"));
}
#[tokio::test]
async fn rejects_incompatible_hello() {
    let (client, mut server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        read_frame(&mut server).await.unwrap();
        write_frame(
            &mut server,
            &Frame::json(
                kind::HELLO,
                &ServerHello {
                    protocol: PROTOCOL + 1,
                    version: "test".into(),
                    server_id: "s".into(),
                    pid: 1,
                    started_at: 0,
                    client_id: "c".into(),
                    home: String::new(),
                    silo_home: String::new(),
                    build: String::new(),
                },
            )
            .unwrap(),
        )
        .await
        .unwrap();
    });
    assert!(matches!(Connection::connect_stream(client).await, Err(ClientError::Incompatible { .. })));
    task.await.unwrap();
}

#[tokio::test]
async fn frame_limit_and_explicit_close() {
    let (client, mut server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        hello(&mut server).await;
        assert!(read_frame(&mut server).await.unwrap().is_none());
    });
    let c = Connection::connect_stream(client).await.unwrap();
    let mut events = c.events();
    assert!(matches!(
        c.call(Method::ConfigSet { key: "large".into(), value: json!("x".repeat(MAX_FRAME)) }).await,
        Err(ClientError::Io { kind: std::io::ErrorKind::InvalidInput, .. })
    ));
    c.close();
    assert!(matches!(events.recv().await.unwrap(), Err(ClientError::Closed)));
    assert!(matches!(c.call(Method::Ping {}).await, Err(ClientError::Closed)));
    task.await.unwrap();
}

#[tokio::test]
async fn oversized_incoming_frame_terminates_connection() {
    use tokio::io::AsyncWriteExt;
    let (client, mut server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        hello(&mut server).await;
        request(&mut server).await;
        server.write_all(&((MAX_FRAME + 1) as u32).to_le_bytes()).await.unwrap();
    });
    let c = Connection::connect_stream(client).await.unwrap();
    let mut events = c.events();
    assert!(matches!(c.call(Method::Ping {}).await, Err(ClientError::Io { kind: std::io::ErrorKind::InvalidData, .. })));
    assert!(events.recv().await.unwrap().is_err());
    task.await.unwrap();
}

#[test]
fn live_replay_and_mixed_deltas_converge_without_double_counting() {
    let mut replay = SessionMirror::new(view(0));
    let mut live = replay.clone();
    let mut mixed = replay.clone();
    let start = Event::ItemStarted { item_id: "i".into(), role: serde_json::from_value(json!("assistant")).unwrap() };
    for mirror in [&mut replay, &mut live, &mut mixed] {
        mirror.apply(1, &start);
    }
    let delta = |text: &str| Event::TextDelta { item_id: "i".into(), text: text.into() };
    for mirror in [&mut live, &mut mixed] {
        assert_eq!(mirror.apply_live(1, &delta("hel")), ApplyResult::Applied);
        mirror.apply_live(1, &delta("lo"));
        assert_eq!(mirror.last_seq, 1);
        assert_eq!(mirror.item("i").unwrap().text(), "hello");
    }
    replay.apply(2, &delta("hello"));
    mixed.apply(2, &delta("hel"));
    assert_eq!(mixed.item("i").unwrap().text(), "hello");
    mixed.apply(3, &delta("lo"));
    assert_eq!(mixed.item("i").unwrap().text(), "hello");
    assert_eq!(mixed.apply_live(1, &delta("stale")), ApplyResult::Duplicate);
    let done = Event::ItemDone { item: Item::new("i", serde_json::from_value(json!("assistant")).unwrap(), vec![Part::text("hello")]) };
    live.apply(2, &done);
    replay.apply(3, &done);
    mixed.apply(4, &done);
    assert_eq!(json!(live.items), json!(replay.items));
    assert_eq!(json!(live.items), json!(mixed.items));
}

#[test]
fn live_reasoning_tool_input_progress_and_loss_reconcile() {
    let mut mirror = SessionMirror::new(view(0));
    mirror.apply(1, &Event::ItemStarted { item_id: "i".into(), role: serde_json::from_value(json!("assistant")).unwrap() });
    let reasoning = Event::ReasoningDelta { item_id: "i".into(), text: "think".into(), summary: true };
    let input = Event::ToolInputDelta { call_id: "c".into(), delta: "{}".into() };
    let progress = Event::ToolProgress { call_id: "c".into(), message: Some("working".into()), output: Some(json!(1)) };
    for event in [&reasoning, &input, &progress] {
        mirror.apply_live(1, event);
    }
    mirror.apply(2, &reasoning);
    mirror.apply(3, &input);
    mirror.apply(4, &progress);
    assert_eq!(mirror.tools["c"].input_delta, "{}");
    assert_eq!(mirror.tools["c"].messages, ["working"]);
    assert_eq!(mirror.tools["c"].progress, [json!(1)]);
    assert_eq!(mirror.item("i").unwrap().parts.len(), 1);
    mirror.apply_live(4, &Event::TextDelta { item_id: "i".into(), text: "tail".into() });
    mirror.apply(5, &Event::TextDelta { item_id: "i".into(), text: "missing prefix tail".into() });
    assert_eq!(mirror.item("i").unwrap().text(), "missing prefix tail");
}

#[test]
fn an_empty_thinking_delta_is_consumed_and_never_reappears_at_the_end() {
    // A thinking block that streams no text (Claude without `display`), then the reply.
    let mut mirror = SessionMirror::new(view(0));
    mirror.apply(1, &Event::ItemStarted { item_id: "i".into(), role: serde_json::from_value(json!("assistant")).unwrap() });
    let think = Event::ReasoningDelta { item_id: "i".into(), text: String::new(), summary: false };
    let text = |t: &str| Event::TextDelta { item_id: "i".into(), text: t.into() };
    mirror.apply_live(1, &think);
    mirror.apply_live(1, &text("Build the runtime"));
    mirror.apply(2, &think);
    mirror.apply_live(2, &text(" first."));
    mirror.apply(3, &text("Build the runtime first."));
    mirror.apply_live(3, &text(" Then"));
    let kinds = |m: &SessionMirror| m.item("i").unwrap().parts.iter().map(|p| matches!(p, Part::Reasoning { .. })).collect::<Vec<_>>();
    assert_eq!(kinds(&mirror), [true, false], "thinking, then one text part: no empty thinking after it");
    assert_eq!(mirror.item("i").unwrap().text(), "Build the runtime first. Then");
}

#[test]
fn a_snapshot_taken_mid_reply_shows_its_text_and_live_deltas_continue_it() {
    // The view carries the reply being written with the text journaled so far.
    let assistant = || serde_json::from_value(json!("assistant")).unwrap();
    let mut v = view(4);
    v.items.push(Item::new("i", assistant(), vec![Part::text("Hello, ")]));
    let mut mirror = SessionMirror::new(v);
    assert_eq!(mirror.item("i").unwrap().text(), "Hello, ", "at once");
    let delta = |text: &str| Event::TextDelta { item_id: "i".into(), text: text.into() };
    // `wor` was streamed before the subscription (still buffered, not journaled): live starts after it.
    mirror.apply_live(4, &delta("ld"));
    mirror.apply_live(4, &delta("!"));
    // The buffered record lands: what the overlay didn't see fills in, nothing twice.
    mirror.apply(5, &delta("world"));
    assert_eq!(mirror.item("i").unwrap().text(), "Hello, world!");
    mirror.apply_live(5, &delta(" Bye"));
    assert_eq!(mirror.item("i").unwrap().text(), "Hello, world! Bye");
    mirror.apply(6, &delta("! Bye"));
    assert_eq!(mirror.item("i").unwrap().text(), "Hello, world! Bye");
    mirror.apply(7, &Event::ItemDone { item: Item::new("i", assistant(), vec![Part::text("Hello, world! Bye")]) });
    assert_eq!(mirror.items.len(), 1);
    assert_eq!(mirror.item("i").unwrap().text(), "Hello, world! Bye");
}

#[test]
fn a_tool_call_streams_its_input_then_runs_then_finishes() {
    use otto_client::ToolStatus;
    let mut m = SessionMirror::new(view(0));
    m.apply(1, &Event::ToolInputStart { call_id: "c".into(), name: "write".into() });
    assert_eq!((m.tools["c"].name.as_str(), m.tools["c"].status), ("write", ToolStatus::Input), "named before any input");
    assert_eq!(m.apply_live(1, &Event::ToolInputDelta { call_id: "c".into(), delta: "{\"path\":".into() }), ApplyResult::Applied);
    assert_eq!(m.tools["c"].name, "write", "a live delta keeps the name");
    m.apply(2, &Event::ToolInputDelta { call_id: "c".into(), delta: "{\"path\":".into() });
    assert_eq!((m.tools["c"].input_delta.as_str(), m.tools["c"].status), ("{\"path\":", ToolStatus::Input));
    m.apply(3, &Event::ToolCall { call_id: "c".into(), name: "write".into(), input: json!({"path": "a"}) });
    assert_eq!(m.tools["c"].status, ToolStatus::Running);
    m.apply(4, &Event::ToolResult { call_id: "c".into(), is_error: true, output: json!("no"), item_id: "r".into() });
    assert_eq!(m.tools["c"].status, ToolStatus::Error);
    // A no-argument call: named, then called, never a delta.
    m.apply(5, &Event::ToolInputStart { call_id: "d".into(), name: "ls".into() });
    m.apply(6, &Event::ToolCall { call_id: "d".into(), name: "ls".into(), input: json!({}) });
    assert_eq!((m.tools["d"].input_delta.as_str(), m.tools["d"].status), ("", ToolStatus::Running));
}

#[test]
fn tool_input_start_is_a_sequenced_event() {
    let message: ServerMessage =
        serde_json::from_value(json!({"seq": 3, "session_id": "s", "event": "tool.input_start", "data": {"call_id": "c", "name": "ls"}})).unwrap();
    assert!(matches!(message, ServerMessage::Event { seq: 3, .. }), "known to this client");
}

#[test]
fn a_view_taken_mid_call_seeds_the_streaming_input_and_later_deltas_continue_it() {
    let mut v = view(4);
    v.streaming_tool_calls = vec![StreamingToolCall { call_id: "c".into(), name: "write".into(), input_delta: "{\"path\":".into() }];
    let mut m = SessionMirror::new(v);
    assert_eq!((m.tools["c"].name.as_str(), m.tools["c"].status), ("write", otto_client::ToolStatus::Input));
    m.apply(5, &Event::ToolInputDelta { call_id: "c".into(), delta: "\"a\"}".into() });
    assert_eq!(m.tools["c"].input_delta, "{\"path\":\"a\"}");
}
#[test]
fn a_retry_wait_shows_until_the_stream_moves_on() {
    let mut m = SessionMirror::new(view(0));
    let retrying =
        Event::ModelRetrying { class: "rate_limited".into(), message: "slow down".into(), delay_ms: 2000, attempt: 2, max_attempts: 3, at_ms: 1_000 };
    m.apply(1, &Event::TurnStarted { turn: 1 });
    m.apply(2, &retrying);
    let shown = m.retrying.clone().unwrap();
    assert_eq!((shown.class.as_str(), shown.delay_ms, shown.attempt, shown.max_attempts), ("rate_limited", 2000, 2, 3));
    // Bookkeeping events leave it; the model's next event ends it.
    m.apply(3, &Event::QueueChanged { queued: vec![] });
    assert!(m.retrying.is_some());
    m.apply(4, &Event::TextDelta { item_id: "a".into(), text: "hi".into() });
    assert!(m.retrying.is_none());
    // A turn that fails while waiting ends it too, and keeps its error.
    m.apply(5, &retrying);
    m.apply(6, &Event::Error { class: "quota_exhausted".into(), message: "Claude usage limit reached".into(), retryable: false });
    assert!(m.retrying.is_none());
    assert_eq!(m.last_error.as_ref().map(|e| e.1.as_str()), Some("Claude usage limit reached"));
}

fn items(ids: &[&str]) -> Vec<Item> {
    ids.iter().map(|id| Item::user_text(*id, *id)).collect()
}

#[test]
fn mirror_prepends_older_pages_and_keeps_them_through_a_resync() {
    let mut newest = view(10);
    newest.items = items(&["c", "d"]);
    (newest.has_older, newest.oldest_seq) = (true, Some(7));
    let mut m = SessionMirror::new(newest.clone());
    assert!(m.has_older);
    // A compaction seen live after the loaded items, and a live delta overlay.
    m.apply_live(10, &Event::CompactionDelta { id: "k".into(), text: "sum".into() });
    assert_eq!(m.compactions[0].at, 2);
    m.apply(11, &Event::ItemStarted { item_id: "e".into(), role: serde_json::from_value(json!("assistant")).unwrap() });
    m.apply_live(11, &Event::TextDelta { item_id: "e".into(), text: "hi".into() });

    let mut page = view(11);
    page.items = items(&["a", "b", "c"]);
    (page.has_older, page.oldest_seq) = (true, Some(3));
    page.part_times = vec![PartTime { item_id: "b".into(), part: 0, started_at: 1, finished_at: Some(2) }];
    assert_eq!(m.prepend(page), 2);
    assert_eq!(m.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d", "e"]);
    assert_eq!(m.item("b").unwrap().id, "b");
    assert_eq!(m.item("e").unwrap().text(), "hi");
    assert_eq!(m.compactions[0].at, 4);
    assert_eq!(m.part_times.len(), 1);
    assert_eq!(m.oldest_seq, Some(3));
    // The overlay's durable base gained the page too: dropping the overlay keeps it.
    m.clear_live();
    assert_eq!(m.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d", "e"]);
    assert_eq!(m.item("e").unwrap().text(), "");

    // A resync brings the newest window again; the pages loaded before it stay.
    let mut fresh = view(12);
    fresh.items = items(&["d", "e", "f"]);
    (fresh.has_older, fresh.oldest_seq) = (true, Some(8));
    let mut resynced = SessionMirror::new(fresh);
    resynced.keep_older(&m);
    assert_eq!(resynced.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d", "e", "f"]);
    assert_eq!((resynced.has_older, resynced.oldest_seq), (true, Some(3)));
    assert_eq!(resynced.part_times.len(), 1);

    // An older server ignores `before_seq` and sends every item: the missing ones are added, and paging ends.
    let mut everything = view(12);
    everything.items = items(&["z", "a", "b", "c", "d", "e", "f"]);
    assert_eq!(resynced.prepend(everything), 1);
    assert_eq!(resynced.items[0].id, "z");
    assert!(!resynced.has_older);
    // An empty page ends paging too.
    let mut m = SessionMirror::new(newest);
    let mut empty = view(10);
    empty.has_older = true;
    assert_eq!(m.prepend(empty), 0);
    assert!(!m.has_older);
}
#[test]
fn mirror_keeps_the_turns_that_changed_files() {
    let changes = TurnChanges { files: vec!["a.rs".into()], added: 2, ..Default::default() };
    let mut v = view(0);
    v.turns = vec![TurnSummary { turn: 1, item_id: Some("old".into()), changes: changes.clone() }];
    let mut m = SessionMirror::new(v);
    let mut reply = Item::user_text("r2", "done");
    reply.role = Role::Assistant;
    m.apply(1, &Event::ItemDone { item: Item::user_text("u2", "go") });
    m.apply(2, &Event::ItemDone { item: reply });
    m.apply(3, &Event::TurnFinished { turn: 2, status: TurnStatus::Done, usage: Usage::default(), changes: Some(changes.clone()) });
    m.apply(4, &Event::TurnFinished { turn: 3, status: TurnStatus::Done, usage: Usage::default(), changes: None });
    let turns: Vec<_> = m.turns.iter().map(|t| (t.turn, t.item_id.as_deref())).collect();
    assert_eq!(turns, [(1, Some("old")), (2, Some("r2"))]);
    assert_eq!(m.turns[1].changes, changes);
}
#[test]
fn mirror_keeps_each_turns_usage_under_its_reply() {
    let mut v = view(0);
    v.turn_usage = vec![TurnUsage {
        turn: 1,
        item_id: Some("old".into()),
        usage: Usage { input: 9, ..Default::default() },
        cost_usd: 0.1,
        ..Default::default()
    }];
    let mut m = SessionMirror::new(v);
    let step = |input: u64, cache_read: u64, cost: f64| Event::Usage {
        turn: 2,
        step: 1,
        usage: Usage { input, output: 3, cache_read, cache_write: 0, reasoning: 1 },
        cost_usd: Some(cost),
        context: ContextUse::default(),
        api_cost_usd: None,
        provider: None,
        model: None,
    };
    m.apply(1, &Event::ItemDone { item: Item::user_text("u2", "go") });
    m.apply(2, &step(10, 0, 0.25));
    m.apply(3, &step(2, 8, 0.5));
    // Mid-turn: summed, no reply yet.
    assert_eq!((m.turn_usage[1].usage.input, m.turn_usage[1].usage.cache_read, m.turn_usage[1].item_id.as_deref()), (12, 8, None));
    let mut reply = Item::user_text("r2", "done");
    reply.role = Role::Assistant;
    m.apply(4, &Event::ItemDone { item: reply });
    let total = Usage { input: 12, output: 6, cache_read: 8, cache_write: 0, reasoning: 2 };
    m.apply(5, &Event::TurnFinished { turn: 2, status: TurnStatus::Done, usage: total.clone(), changes: None });
    let t = m.turn_usage_of("r2").unwrap();
    assert_eq!((t.turn, &t.usage, t.cost_usd), (2, &total, 0.75));
    assert_eq!(m.turn_usage_of("old").unwrap().usage.input, 9);
    // A turn that ran no step (stopped before the model answered) adds nothing.
    m.apply(6, &Event::TurnFinished { turn: 3, status: TurnStatus::Cancelled, usage: Usage::default(), changes: None });
    assert_eq!(m.turn_usage.len(), 2);
}
#[test]
fn mirror_folds_api_equivalent_costs_by_model() {
    let mut v = view(0);
    v.info.provider = "anthropic".into();
    v.info.model = "claude-a".into();
    // The view's totals: one model, priced.
    v.info.usage.add_step("anthropic", "claude-a", &Usage { input: 100, output: 10, ..Usage::default() }, 0.0, Some(1.0));
    let mut m = SessionMirror::new(v);
    let step = |turn: i64, model: Option<&str>, api: Option<f64>| Event::Usage {
        turn,
        step: 1,
        usage: Usage { input: 10, output: 2, cache_read: 50, ..Usage::default() },
        cost_usd: Some(0.0),
        context: ContextUse::default(),
        api_cost_usd: api,
        provider: model.map(|_| "anthropic".into()),
        model: model.map(Into::into),
    };
    // An older server's step names no model: the session's.
    m.apply(1, &step(2, None, Some(0.5)));
    // A switch mid-session: its own row, its own price; then a model without one.
    m.apply(2, &step(2, Some("claude-b"), Some(0.25)));
    m.apply(3, &step(3, Some("local"), None));
    let totals = &m.info.usage;
    assert_eq!(totals.api_cost_usd, Some(1.75), "unknown adds nothing");
    assert_eq!(totals.cost_usd, 0.0, "billed nothing (a subscription)");
    let models: Vec<_> = totals.by_model.iter().map(|b| (b.model.as_str(), b.steps, b.api_cost_usd)).collect();
    assert_eq!(models, [("claude-a", 2, Some(1.5)), ("claude-b", 1, Some(0.25)), ("local", 1, None)]);
    assert_eq!(totals.usage.input, 130);
    let turn = m.turn_usage.iter().find(|t| t.turn == 2).unwrap();
    assert_eq!((turn.api_cost_usd, turn.by_model.len()), (Some(0.75), 2));
    let turn = m.turn_usage.iter().find(|t| t.turn == 3).unwrap();
    assert_eq!((turn.api_cost_usd, turn.by_model[0].api_cost_usd), (None, None), "never $0");
}
