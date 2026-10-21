use otto_llm::{
    Model, ModelInfo,
    auth::AuthMode as Mode,
    providers::{LoginFlow, LoginMode, Providers},
};
use otto_proto::{AuthMode, AuthStatus, LoginResult, RpcError};
use otto_server::{AuthFuture, AuthService};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

pub struct Services {
    pub providers: Providers,
    logins: tokio::sync::Mutex<HashMap<String, (String, Instant, LoginFlow)>>,
    script: Option<Arc<crate::script::ScriptModel>>,
}
impl Services {
    pub fn new(providers: Providers) -> anyhow::Result<Self> {
        Ok(Self { providers, logins: Default::default(), script: crate::script::load()? })
    }
    fn statuses(&self) -> Result<Vec<AuthStatus>, RpcError> {
        self.providers
            .status()
            .into_iter()
            .map(|status| {
                if let Some(error) = status.error {
                    return Err(RpcError::internal(error));
                }
                Ok(AuthStatus {
                    provider: status.provider,
                    mode: if status.mode.as_deref() == Some("subscription") { AuthMode::Subscription } else { AuthMode::ApiKey },
                    signed_in: status.signed_in,
                    expires_at: status.expires_at_ms.map(|ms| ms.min(i64::MAX as u64) as i64),
                    account: status.account_id,
                })
            })
            .collect()
    }
    fn status_for(&self, provider: &str) -> Result<AuthStatus, RpcError> {
        self.statuses()?.into_iter().find(|s| s.provider == provider).ok_or_else(|| RpcError::not_found(provider))
    }
    fn models(&self, provider: Option<&str>) -> Vec<ModelInfo> {
        self.providers
            .status()
            .into_iter()
            .filter(|s| s.signed_in && provider.is_none_or(|p| p == s.provider))
            .flat_map(|s| {
                self.providers
                    .list_models(Some(&s.provider), Some(if s.mode.as_deref() == Some("subscription") { Mode::Subscription } else { Mode::ApiKey }))
            })
            .collect()
    }
}
fn rpc(error: otto_llm::LlmError) -> RpcError {
    RpcError::new(if error.class == otto_llm::ErrorClass::Auth { RpcError::AUTH_REQUIRED } else { RpcError::BAD_REQUEST }, error.to_string())
}
impl otto_core::ModelResolver for Services {
    fn resolve(&self, provider: &str, model: &str) -> otto_core::Result<Arc<dyn Model>> {
        if provider.is_empty() || model.is_empty() {
            return Err(otto_llm::LlmError::new(otto_llm::ErrorClass::Auth, "run `otto-silo auth login <provider>`").into());
        }
        if provider == "test"
            && model == "script"
            && let Some(script) = &self.script
        {
            return Ok(script.clone());
        }
        Ok(self.providers.resolve_model(provider, model)?)
    }
    fn default_model(&self) -> Option<(String, String)> {
        if self.script.is_some() { Some(("test".into(), "script".into())) } else { self.providers.default_model() }
    }
    fn list(&self) -> Vec<ModelInfo> {
        self.models(None)
    }
}

impl AuthService for Services {
    fn status(&self) -> AuthFuture<'_, Vec<AuthStatus>> {
        Box::pin(async { self.statuses() })
    }
    fn login(&self, provider: String, mode: AuthMode, api_key: Option<String>, device: bool) -> AuthFuture<'_, LoginResult> {
        Box::pin(async move {
            if device && mode != AuthMode::Subscription {
                return Err(RpcError::bad_request("device login requires subscription mode"));
            }
            let mode = match mode {
                AuthMode::ApiKey => LoginMode::ApiKey,
                AuthMode::Subscription if device => LoginMode::Device,
                AuthMode::Subscription => LoginMode::Subscription,
            };
            let flow = self.providers.begin_login(&provider, mode).await.map_err(rpc)?;
            if mode == LoginMode::ApiKey {
                self.providers.complete_login(flow, api_key.as_deref().unwrap_or_default()).await.map_err(rpc)?;
                return Ok(LoginResult::SignedIn { auth: self.status_for(&provider)? });
            }
            let url = flow.url.clone().unwrap_or_default();
            let user_code = flow.user_code.clone();
            let verification_uri = flow.verification_uri().map(str::to_owned);
            let interval_secs = flow.interval_secs();
            let login_id = uuid::Uuid::new_v4().to_string();
            let mut logins = self.logins.lock().await;
            logins.retain(|_, (_, at, _)| at.elapsed() < Duration::from_secs(900));
            if logins.len() >= 32 {
                return Err(RpcError::new(RpcError::BUSY, "too many pending logins"));
            }
            logins.insert(login_id.clone(), (provider, Instant::now(), flow));
            Ok(LoginResult::Pending { url, login_id, user_code, verification_uri, interval_secs })
        })
    }
    fn login_complete(&self, login_id: String, code: Option<String>) -> AuthFuture<'_, AuthStatus> {
        Box::pin(async move {
            let (provider, at, flow) = self.logins.lock().await.remove(&login_id).ok_or_else(|| RpcError::not_found("login"))?;
            if at.elapsed() >= Duration::from_secs(900) {
                return Err(RpcError::bad_request("login expired"));
            }
            self.providers.complete_login(flow, code.as_deref().unwrap_or_default()).await.map_err(rpc)?;
            self.status_for(&provider)
        })
    }
    fn logout(&self, provider: String) -> AuthFuture<'_, ()> {
        Box::pin(async move {
            self.logins.lock().await.retain(|_, (p, _, _)| p != &provider);
            self.providers.logout(&provider).map_err(rpc)
        })
    }
    fn list_models(&self, provider: Option<String>) -> AuthFuture<'_, Vec<ModelInfo>> {
        Box::pin(async move { Ok(self.models(provider.as_deref())) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn auth_adapter_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let services = Services::new(Providers::new(dir.path().join("auth"), dir.path().into()).unwrap()).unwrap();
        assert!(services.login("openai".into(), AuthMode::ApiKey, None, true).await.is_err());
        assert!(services.login("anthropic".into(), AuthMode::Subscription, None, true).await.is_err());
        assert!(matches!(
            services.login("openai".into(), AuthMode::ApiKey, Some("test-key".into()), false).await.unwrap(),
            LoginResult::SignedIn { .. }
        ));
        assert!(services.status().await.unwrap().iter().any(|status| status.provider == "openai" && status.signed_in));
        assert!(!services.list_models(Some("openai".into())).await.unwrap().is_empty());
        services.logout("openai".into()).await.unwrap();
        let login = services.login("anthropic".into(), AuthMode::Subscription, None, false).await.unwrap();
        let LoginResult::Pending { login_id, url, user_code, verification_uri, interval_secs } = login else {
            panic!("expected OAuth flow");
        };
        assert!(url.starts_with("https://claude.ai/"));
        assert!(user_code.is_none() && verification_uri.is_none() && interval_secs.is_none());
        assert!(services.login_complete(login_id.clone(), Some("code#wrong-state".into())).await.is_err());
        assert!(services.login_complete(login_id, None).await.is_err());
    }
}
