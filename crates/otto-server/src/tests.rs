#[path = "api_tests.rs"]
mod api_tests;
#[path = "limits_tests.rs"]
mod limits_tests;

use super::*;
use futures::StreamExt;
use otto_client::{ApplyResult, Connection, SessionCreateOptions, SessionFollower, SessionMirror};
use otto_core::RuntimeConfig;
use otto_llm::{CallCx, CallKind, Caps, EventStream, FinishReason, Item, LlmError, Model, Part, StreamEvent};
use otto_proto::*;
use serde_json::json;
use std::{collections::VecDeque, sync::Mutex as StdMutex};

struct ScriptedModel {
    info: ModelInfo,
    scripts: StdMutex<VecDeque<Vec<StreamEvent>>>,
}
#[async_trait::async_trait]
impl Model for ScriptedModel {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, _: otto_llm::Request, _: CallCx) -> Result<EventStream, LlmError> {
        let script = self.scripts.lock().unwrap().pop_front();
        Ok(match script {
            Some(events) => Box::pin(futures::stream::iter(events).then(|event| async {
                let delay = if matches!(event, StreamEvent::PartDone { part: Part::Text { .. } }) { 100 } else { 5 };
                tokio::time::sleep(Duration::from_millis(delay)).await;
                Ok(event)
            })),
            None => Box::pin(futures::stream::pending()),
        })
    }
}
struct Resolver(Arc<ScriptedModel>);
impl ModelResolver for Resolver {
    fn resolve(&self, _: &str, _: &str) -> otto_core::Result<Arc<dyn Model>> {
        Ok(self.0.clone())
    }
    fn default_model(&self) -> Option<(String, String)> {
        Some(("test".into(), "test".into()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        vec![self.0.info.clone()]
    }
}
struct NoAuth;
impl AuthService for NoAuth {
    fn status(&self) -> AuthFuture<'_, Vec<AuthStatus>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn login(&self, _: String, _: AuthMode, _: Option<String>, device: bool) -> AuthFuture<'_, LoginResult> {
        Box::pin(async move {
            if !device {
                return Err(RpcError::auth_required("test"));
            }
            Ok(LoginResult::Pending {
                url: "https://example.test/device".into(),
                login_id: "login".into(),
                user_code: Some("ABCD".into()),
                verification_uri: Some("https://example.test/device".into()),
                interval_secs: Some(5),
            })
        })
    }
    fn login_complete(&self, _: String, _: Option<String>) -> AuthFuture<'_, AuthStatus> {
        Box::pin(async { Err(RpcError::auth_required("test")) })
    }
    fn logout(&self, _: String) -> AuthFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn list_models(&self, _: Option<String>) -> AuthFuture<'_, Vec<ModelInfo>> {
        Box::pin(async { Ok(vec![]) })
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    runtime: Arc<otto_core::Runtime>,
    models: Arc<dyn ModelResolver>,
}
impl Fixture {
    async fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
        let dir = tempfile::Builder::new().prefix("otto-server-").tempdir_in("/tmp").unwrap();
        let model = Arc::new(ScriptedModel {
            info: ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None },
            scripts: StdMutex::new(scripts.into()),
        });
        let models: Arc<dyn ModelResolver> = Arc::new(Resolver(model));
        let runtime = Arc::new(
            otto_core::Runtime::new(RuntimeConfig {
                store: Arc::new(StdMutex::new(otto_store::Store::open_in_memory().unwrap())),
                models: models.clone(),
                state_dir: dir.path().to_owned(),
                user_config_dir: dir.path().to_owned(),
            })
            .await
            .unwrap(),
        );
        Self { dir, runtime, models }
    }
    fn config(&self) -> ServerConfig {
        ServerConfig {
            home: self.dir.path().into(),
            runtime: self.runtime.clone(),
            models: self.models.clone(),
            auth: Arc::new(NoAuth),
            idle_exit: None,
        }
    }
    async fn connect(&self) -> Connection {
        Connection::connect(self.dir.path()).await.unwrap()
    }
    async fn session(&self, client: &Connection) -> SessionInfo {
        client
            .session_create(SessionCreateOptions { project_root: Some(self.dir.path().to_string_lossy().into_owned()), ..Default::default() })
            .await
            .unwrap()
    }
}
fn text(value: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta { text: value.into() },
        StreamEvent::PartDone { part: Part::text(value) },
        StreamEvent::Finish { reason: FinishReason::Stop, response_id: None },
    ]
}
fn call(name: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![StreamEvent::PartDone { part: Part::ToolCall { call_id: "call1".into(), name: name.into(), input, kind: CallKind::Function, opaque: None } }]
}
async fn prompt(client: &Connection, id: &str) {
    client.prompt(id, vec![InputPart::Text { text: "hello".into() }], Delivery::Queue).await.unwrap();
}
async fn finished(follower: &SessionFollower) -> SessionMirror {
    let mut changes = follower.changes();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = changes.borrow_and_update().clone();
            assert!(state.error.is_none(), "{:?}", state.error);
            if state.mirror.turn.is_some_and(|(_, status)| status == TurnStatus::Done) {
                return state.mirror;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn hello_api_validation_lock_and_shutdown() {
    let f = Fixture::new(vec![]).await;
    let server = Server::start(f.config()).await.unwrap();
    assert!(matches!(Server::start(f.config()).await, Err(Error::AlreadyRunning)));
    let client = f.connect().await;
    assert_eq!(client.hello().protocol, PROTOCOL);
    assert_eq!(client.hello().server_id, server.info.server_id);
    assert_eq!(std::fs::metadata(f.dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(paths::socket(f.dir.path())).unwrap().permissions().mode() & 0o777, 0o600);
    for (method, _) in METHODS {
        let result = client.call(Method::ApiDescribe { method: (*method).into() }).await.unwrap();
        assert_eq!(result["example"]["method"], *method);
        assert!(result["result_schema"].is_object());
    }
    let mut hello = ClientHello::ours("bad", ClientRole::Owner);
    hello.protocol += 1;
    let stream = tokio::net::UnixStream::connect(paths::socket(f.dir.path())).await.unwrap();
    assert!(matches!(Connection::connect_stream_with_hello(stream, hello).await, Err(otto_client::ClientError::Incompatible { .. })));
    let error = client.session_create(SessionCreateOptions::default()).await.unwrap_err();
    assert!(matches!(error, otto_client::ClientError::Rpc(RpcError { code, .. }) if code == RpcError::BAD_REQUEST));
    client.call(Method::ConfigSet { key: "model".into(), value: json!("test") }).await.unwrap();
    assert_eq!(client.call(Method::ConfigGet { key: Some("model".into()) }).await.unwrap()["value"], "test");
    client.call(Method::ServerShutdown {}).await.unwrap();
    server.serve_forever().await.unwrap();
    assert!(!paths::socket(f.dir.path()).exists());
    assert!(!paths::runtime(f.dir.path()).exists());
}

#[tokio::test]
async fn two_clients_follow_tool_round_trip_identically() {
    let f = Fixture::new(vec![call("read", json!({"path":"hello.txt"})), text("answer")]).await;
    std::fs::write(f.dir.path().join("hello.txt"), "world").unwrap();
    let server = Server::start(f.config()).await.unwrap();
    let a = f.connect().await;
    let b = f.connect().await;
    let session = f.session(&a).await;
    let fa = SessionFollower::new(a.clone(), &session.id).await.unwrap();
    let fb = SessionFollower::new(b, &session.id).await.unwrap();
    let mut live_events = a.events();
    prompt(&a, &session.id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::Live { event, .. } = live_events.recv().await.unwrap().unwrap()
                && matches!(*event, Event::TextDelta { .. })
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    for follower in [&fa, &fb] {
        let mut changes = follower.changes();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = changes.borrow_and_update().clone();
                if state.mirror.items.last().is_some_and(|item| item.text() == "answer") {
                    assert!(!state.mirror.turn.is_some_and(|(_, status)| status == TurnStatus::Done));
                    break;
                }
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }
    let ma = finished(&fa).await;
    let mb = finished(&fb).await;
    assert_eq!(ma.last_seq, mb.last_seq);
    assert_eq!(json!(ma.items), json!(mb.items));
    assert_eq!(json!(ma.info), json!(mb.info));
    assert_eq!(ma.items.last().map(Item::text).as_deref(), Some("answer"));
    assert!(ma.items.iter().any(|i| i.parts.iter().any(|p| matches!(p, Part::ToolResult { is_error: false, .. }))));
    let (events, _) = f.runtime.subscribe(&session.id, 0).await.unwrap();
    assert!(events.iter().enumerate().all(|(i, e)| e.0 == i as i64 + 1));
    assert_eq!(ma.last_seq, events.last().unwrap().0);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconnect_mid_turn_replays_exact_contiguous_tail() {
    let f = Fixture::new(vec![text("resumed")]).await;
    let server = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    let session = f.session(&client).await;
    let mut mirror = SessionMirror::new(client.session_view(&session.id).await.unwrap());
    let mut events = client.events();
    client.session_subscribe(&session.id, Some(mirror.last_seq)).await.unwrap();
    prompt(&client, &session.id).await;
    loop {
        if let ServerMessage::Event { seq, session_id: Some(id), event } = events.recv().await.unwrap().unwrap()
            && id == session.id
        {
            assert_eq!(mirror.apply(seq, &event), ApplyResult::Applied);
            if matches!(*event, Event::TurnStarted { .. }) {
                break;
            }
        }
    }
    let after = mirror.last_seq;
    client.close();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let resumed = f.connect().await;
    let mut events = resumed.events();
    resumed.session_subscribe(&session.id, Some(after)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::Event { seq, session_id: Some(id), event } = events.recv().await.unwrap().unwrap()
                && id == session.id
            {
                assert_eq!(mirror.apply(seq, &event), ApplyResult::Applied);
                if matches!(*event, Event::TurnFinished { .. }) {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    let view = resumed.session_view(&session.id).await.unwrap();
    assert_eq!(mirror.last_seq, view.seq);
    assert_eq!(json!(mirror.items), json!(view.items));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_answer_over_socket() {
    let f = Fixture::new(vec![call("shell", json!({"command":"printf approved"})), text("done")]).await;
    let server = Server::start(f.config()).await.unwrap();
    let client = f.connect().await;
    client.call(Method::ConfigSet { key: "approval_mode".into(), value: json!("ask_on_effects") }).await.unwrap();
    let session = f.session(&client).await;
    let follower = SessionFollower::new(client.clone(), &session.id).await.unwrap();
    let mut events = client.events();
    prompt(&client, &session.id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::Event { event, .. } = events.recv().await.unwrap().unwrap()
                && let Event::ApprovalRequested { approval_id, .. } = *event
            {
                client.approval_answer(&session.id, &approval_id, ApprovalDecision::Allow, Some("wire approval note".into())).await.unwrap();
                break;
            }
        }
    })
    .await
    .unwrap();
    let mirror = finished(&follower).await;
    assert!(mirror.pending_approvals.is_empty());
    assert!(
        mirror.items.iter().any(|item| item.parts.iter().any(|part| matches!(part, Part::System { text } if text.contains("wire approval note"))))
    );
    assert!(mirror.items.iter().any(|i| i.parts.iter().any(|p| matches!(p, Part::ToolResult { is_error: false, .. }))));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_exit_waits_for_clients_and_busy_turns() {
    let f = Fixture::new(vec![]).await;
    let mut config = f.config();
    config.idle_exit = Some(Duration::from_millis(150));
    let server = Server::start(config).await.unwrap();
    let client = f.connect().await;
    let session = f.session(&client).await;
    prompt(&client, &session.id).await;
    client.close();
    drop(client);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(paths::socket(f.dir.path()).exists());
    f.runtime.cancel(&session.id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), server.serve_forever()).await.unwrap().unwrap();
    assert!(!paths::socket(f.dir.path()).exists());
}

#[test]
fn uid_predicate_and_logging() {
    assert!(same_uid(42, 42));
    assert!(!same_uid(42, 43));
    let dir = tempfile::tempdir().unwrap();
    let file = init_logging(dir.path()).unwrap();
    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn bounded_queue_overflow_is_client_local() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let (close, closed) = watch::channel(false);
    let slow = crate::connection::Sink::test_sink(tx, close);
    let (tx, _rx2) = tokio::sync::mpsc::channel(1);
    let (close, healthy_closed) = watch::channel(false);
    let healthy = crate::connection::Sink::test_sink(tx, close);
    let event = Event::SessionsChanged {};
    assert!(slow.event(None, 1, event.clone()));
    assert!(!slow.event(None, 2, event.clone()));
    assert!(*closed.borrow());
    assert!(healthy.event(None, 1, event));
    assert!(!*healthy_closed.borrow());
}

#[tokio::test]
async fn nonreading_socket_is_closed_without_harming_other_clients() {
    let f = Fixture::new(vec![]).await;
    let server = Server::start(f.config()).await.unwrap();
    let healthy = f.connect().await;
    healthy.call(Method::ConfigSet { key: "large".into(), value: json!("x".repeat(65536)) }).await.unwrap();
    let mut slow = tokio::net::UnixStream::connect(paths::socket(f.dir.path())).await.unwrap();
    write_frame(&mut slow, &Frame::json(kind::HELLO, &ClientHello::ours("slow", ClientRole::Owner)).unwrap()).await.unwrap();
    assert_eq!(read_frame(&mut slow).await.unwrap().unwrap().kind, kind::HELLO);
    tokio::time::timeout(Duration::from_secs(5), async {
        for id in 1..1000 {
            let request = Request { id, method: Method::ConfigGet { key: Some("large".into()) } };
            if write_frame(&mut slow, &Frame::json(kind::CONTROL, &request).unwrap()).await.is_err() {
                break;
            }
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    healthy.call(Method::Ping {}).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async { while let Ok(Some(_)) = read_frame(&mut slow).await {} }).await.unwrap();
    healthy.call(Method::Ping {}).await.unwrap();
    server.shutdown().await.unwrap();
}
