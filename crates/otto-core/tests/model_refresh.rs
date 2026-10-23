//! The hourly model-list refresh: the resolver is asked at start, and a changed list is
//! announced as `auth.changed` so clients reread their pickers.
use otto_core::*;
use otto_llm::*;
use otto_proto::Event;
use otto_store::Store;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

/// `refresh_models` waits for a permit, then reports a change.
struct Resolver(Arc<Semaphore>);
impl ModelResolver for Resolver {
    fn resolve(&self, _: &str, _: &str) -> Result<Arc<dyn Model>> {
        Err(LlmError::new(ErrorClass::Auth, "none").into())
    }
    fn default_model(&self) -> Option<(String, String)> {
        None
    }
    fn list(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    fn refresh_models(&self) -> futures::future::BoxFuture<'_, bool> {
        Box::pin(async {
            self.0.acquire().await.unwrap().forget();
            true
        })
    }
}

#[tokio::test]
async fn a_changed_model_list_is_announced() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let runtime = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(Store::open(dir.path().join("db")).unwrap())),
        models: Arc::new(Resolver(gate.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    let mut events = runtime.events();
    gate.add_permits(1);
    let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("announced").unwrap();
    assert!(matches!(event, Event::AuthChanged {}), "{event:?}");
    assert!(MODEL_REFRESH_EVERY >= Duration::from_secs(3600), "checked hourly, not more");
}
