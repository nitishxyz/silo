//! A model call that fails or waits to retry: a spent quota ends the turn with
//! an error in words, and a retry wait is announced while the turn runs. Offline.
use otto_core::*;
use otto_llm::*;
use otto_proto::{Delivery, Event, InputPart, TurnStatus};
use otto_store::Store;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// What each call does: fail at once, or report a retry wait and then answer.
enum Call {
    Fail(LlmError),
    RetryThenAnswer(RetryNotice),
}
struct Flaky {
    info: ModelInfo,
    calls: Mutex<VecDeque<Call>>,
}
#[async_trait::async_trait]
impl Model for Flaky {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, _: Request, cx: CallCx) -> std::result::Result<EventStream, LlmError> {
        let call = self.calls.lock().unwrap().pop_front();
        match call {
            Some(Call::Fail(error)) => Err(error),
            Some(Call::RetryThenAnswer(notice)) => {
                cx.retry.as_ref().expect("the step passes a retry hook").notify(&notice);
                tokio::time::sleep(notice.delay).await;
                let events =
                    vec![StreamEvent::PartDone { part: Part::text("ok") }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }];
                Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
            }
            None => Ok(Box::pin(futures::stream::pending())),
        }
    }
}
struct Resolver(Arc<Flaky>);
impl ModelResolver for Resolver {
    fn resolve(&self, _: &str, _: &str) -> Result<Arc<dyn Model>> {
        Ok(self.0.clone())
    }
    fn default_model(&self) -> Option<(String, String)> {
        Some(("anthropic".into(), "test".into()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        vec![self.0.info.clone()]
    }
}

/// One turn against `calls`; every event it journaled.
async fn turn(dir: &Path, calls: Vec<Call>) -> Vec<Event> {
    let model = Arc::new(Flaky {
        info: ModelInfo { id: "test".into(), provider: "anthropic".into(), name: "test".into(), caps: Caps::default(), pricing: None },
        calls: Mutex::new(calls.into()),
    });
    let config = RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        models: Arc::new(Resolver(model)),
        state_dir: dir.join("state"),
        user_config_dir: dir.join("user"),
    };
    let runtime = Runtime::new(config).await.unwrap();
    let project = runtime.open_project(dir).await.unwrap();
    let s = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.into(), ..Default::default() }).await.unwrap();
    let (_, mut live) = runtime.subscribe(&s.id, 0).await.unwrap();
    s.prompt(vec![InputPart::Text { text: "go".into() }], Delivery::Queue).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async { while !matches!(live.recv().await.unwrap().1, Event::TurnFinished { .. }) {} })
        .await
        .unwrap();
    let (events, _) = runtime.subscribe(&s.id, 0).await.unwrap();
    runtime.shutdown().await.unwrap();
    events.into_iter().map(|(_, e)| e).collect()
}

#[tokio::test]
async fn a_spent_quota_ends_the_turn_with_an_error_in_words() {
    let dir = tempfile::tempdir().unwrap();
    let quota = LlmError::new(ErrorClass::QuotaExhausted, "You've hit your limit")
        .with_status(429)
        .with_retry_after(Some(Duration::from_secs(3 * 3600 + 12 * 60 + 30)));
    let events = turn(dir.path(), vec![Call::Fail(quota)]).await;
    let error =
        events.iter().find_map(|e| if let Event::Error { class, message, retryable } = e { Some((class, message, *retryable)) } else { None });
    let (class, message, retryable) = error.expect("the turn reports its error");
    assert_eq!(class, "quota_exhausted");
    assert_eq!(message, "Claude usage limit reached — resets in 3h 12m\nYou've hit your limit");
    assert!(!retryable, "resending now cannot help");
    assert!(events.iter().any(|e| matches!(e, Event::TurnFinished { status: TurnStatus::Error, .. })));
    assert!(!events.iter().any(|e| matches!(e, Event::ModelRetrying { .. })));
}

#[tokio::test]
async fn a_retry_wait_is_announced_then_the_turn_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let notice =
        RetryNotice { class: ErrorClass::RateLimited, message: "slow down".into(), delay: Duration::from_millis(20), attempt: 2, max_attempts: 3 };
    let events = turn(dir.path(), vec![Call::RetryThenAnswer(notice)]).await;
    let at = events.iter().position(|e| matches!(e, Event::ModelRetrying { .. })).expect("the wait is announced");
    let Event::ModelRetrying { class, message, delay_ms, attempt, max_attempts, at_ms } = &events[at] else { unreachable!() };
    assert_eq!((class.as_str(), message.as_str(), *delay_ms, *attempt, *max_attempts), ("rate_limited", "slow down", 20, 2, 3));
    assert!(*at_ms > 0);
    // Before the reply it held up, and the turn ends well.
    let started = events.iter().position(|e| matches!(e, Event::ItemStarted { role: Role::Assistant, .. })).unwrap();
    assert!(at < started);
    assert!(events.iter().any(|e| matches!(e, Event::TurnFinished { status: TurnStatus::Done, .. })));
    assert!(!events.iter().any(|e| matches!(e, Event::Error { .. })));
}
