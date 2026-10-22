//! Request-scoped credentials and refresh coordination.
pub mod oauth;
mod store;
pub use store::CredentialStore;

use crate::{ErrorClass, LlmError};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey { key: String },
    OAuth { access: String, refresh: String, expires_at_ms: u64, account_id: Option<String>, extra: serde_json::Value },
}
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { .. } => f.debug_struct("ApiKey").field("key", &"[REDACTED]").finish(),
            Self::OAuth { expires_at_ms, .. } => f
                .debug_struct("OAuth")
                .field("access", &"[REDACTED]")
                .field("refresh", &"[REDACTED]")
                .field("expires_at_ms", expires_at_ms)
                .finish_non_exhaustive(),
        }
    }
}
impl Credential {
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        matches!(self, Self::OAuth { expires_at_ms, .. } if *expires_at_ms > now_ms().saturating_add(300_000))
    }
}
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
}
fn auth_error(message: impl Into<String>) -> LlmError {
    LlmError::new(ErrorClass::Auth, message)
}

#[must_use]
pub fn env_api_key(provider: &str) -> Option<String> {
    let names: &[&str] = match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "google" | "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_GENERATIVE_AI_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        id => crate::compat::dialect(id).map(|d| d.env).unwrap_or_default(),
    };
    names.iter().find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

pub async fn fresh_oauth<F, Fut>(store: &CredentialStore, provider: &str, refresh_fn: F) -> Result<Credential, LlmError>
where
    F: FnOnce(Credential) -> Fut,
    Fut: Future<Output = Result<Credential, LlmError>>,
{
    coordinated(store, provider, None, refresh_fn).await
}

pub async fn on_unauthorized<F, Fut>(store: &CredentialStore, provider: &str, used: &Credential, refresh_fn: F) -> Result<Credential, LlmError>
where
    F: FnOnce(Credential) -> Fut,
    Fut: Future<Output = Result<Credential, LlmError>>,
{
    coordinated(store, provider, Some(used), refresh_fn).await
}

async fn coordinated<F, Fut>(store: &CredentialStore, provider: &str, used: Option<&Credential>, refresh_fn: F) -> Result<Credential, LlmError>
where
    F: FnOnce(Credential) -> Fut,
    Fut: Future<Output = Result<Credential, LlmError>>,
{
    let mutex = store.provider_mutex(provider);
    let _single_flight = mutex.lock().await;
    let credential = store.get(provider)?.ok_or_else(|| auth_error("missing OAuth credential"))?;
    if credential.is_fresh() && used != Some(&credential) {
        return Ok(credential);
    }
    let owned = store.clone();
    let _lock = tokio::task::spawn_blocking(move || owned.lock()).await.map_err(|e| auth_error(e.to_string()))??;
    let credential = store.get(provider)?.ok_or_else(|| auth_error("missing OAuth credential"))?;
    if credential.is_fresh() && used != Some(&credential) {
        return Ok(credential);
    }
    if !matches!(credential, Credential::OAuth { .. }) {
        return Err(auth_error("expected OAuth credential"));
    }
    let credential = refresh_fn(credential).await?;
    if !matches!(credential, Credential::OAuth { .. }) {
        return Err(auth_error("refresh returned a non-OAuth credential"));
    }
    store.set_locked(provider, credential.clone())?;
    Ok(credential)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    Subscription,
}

pub type RefreshFuture = Pin<Box<dyn Future<Output = Result<Credential, LlmError>> + Send>>;
pub type RefreshFn = Arc<dyn Fn(Credential) -> RefreshFuture + Send + Sync>;

#[derive(Clone)]
pub enum CredentialSource {
    Static(Credential),
    Store { store: CredentialStore, provider: String, refresh: RefreshFn },
}
/// Provider auth handle; subscription shaping remains provider-owned.
pub type Auth = CredentialSource;
impl CredentialSource {
    #[must_use]
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::Static(Credential::ApiKey { key: key.into() })
    }
    #[must_use]
    pub fn store(store: CredentialStore, provider: impl Into<String>, refresh: RefreshFn) -> Self {
        Self::Store { store, provider: provider.into(), refresh }
    }
    pub async fn current(&self) -> Result<Credential, LlmError> {
        match self {
            Self::Static(credential) => Ok(credential.clone()),
            Self::Store { store, provider, refresh } => {
                if let Some(credential @ Credential::ApiKey { .. }) = store.get(provider)? {
                    return Ok(credential);
                }
                fresh_oauth(store, provider, |credential| refresh(credential)).await
            }
        }
    }
    pub async fn unauthorized(&self, used: &Credential) -> Result<Credential, LlmError> {
        match self {
            Self::Static(_) => Err(auth_error("static credential rejected")),
            Self::Store { store, provider, refresh } => on_unauthorized(store, provider, used, |credential| refresh(credential)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn token(fresh: bool) -> Credential {
        Credential::OAuth {
            access: if fresh { "new" } else { "old" }.into(),
            refresh: "secret".into(),
            expires_at_ms: if fresh { now_ms() + 3_600_000 } else { 0 },
            account_id: None,
            extra: serde_json::Value::Null,
        }
    }
    #[tokio::test]
    async fn independent_stores_refresh_once_and_401_reloads() {
        let dir = std::env::temp_dir().join(format!("otto-auth-{}", rand::random::<u64>()));
        let a = CredentialStore::open(&dir).unwrap();
        let b = CredentialStore::open(&dir).unwrap();
        a.set("test", token(false)).unwrap();
        let calls = AtomicUsize::new(0);
        let refresh = |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(token(true))
        };
        let (one, two) = tokio::join!(fresh_oauth(&a, "test", refresh), fresh_oauth(&b, "test", refresh));
        assert_eq!(one.unwrap(), two.unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        on_unauthorized(&a, "test", &token(false), refresh).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let used = a.get("test").unwrap().unwrap();
        on_unauthorized(&a, "test", &used, refresh).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!format!("{:?}", token(true)).contains("secret"));
        a.remove("test").unwrap();
        assert!(a.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
