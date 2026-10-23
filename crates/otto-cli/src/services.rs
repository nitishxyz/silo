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
use tokio::sync::Notify;

/// A started subscription login. `flow` is taken while `login_complete` runs; `cancel` aborts it.
struct PendingLogin {
    provider: String,
    at: Instant,
    flow: Option<LoginFlow>,
    cancel: Arc<Notify>,
}
impl PendingLogin {
    fn abort(self) {
        self.cancel.notify_one();
    }
}

pub struct Services {
    pub providers: Providers,
    logins: tokio::sync::Mutex<HashMap<String, PendingLogin>>,
    script: Option<Arc<crate::script::ScriptModel>>,
}

fn proto_mode(mode: Mode) -> AuthMode {
    match mode {
        Mode::ApiKey => AuthMode::ApiKey,
        Mode::Subscription => AuthMode::Subscription,
    }
}

/// Sign-in modes offered for a provider: every mode at least one of its catalog models accepts.
fn supported_modes(model_modes: impl IntoIterator<Item = Mode>) -> Vec<AuthMode> {
    let mut modes: Vec<AuthMode> = Vec::new();
    for mode in model_modes.into_iter().map(proto_mode) {
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    modes.sort_by_key(|mode| *mode == AuthMode::Subscription);
    modes
}

impl Services {
    pub fn new(providers: Providers) -> anyhow::Result<Self> {
        Ok(Self { providers, logins: Default::default(), script: crate::script::load()? })
    }
    /// Modes the provider has a sign-in for that at least one of its models accepts
    /// (local servers list models only once reachable, so they keep API key).
    fn modes(&self, provider: &str) -> Vec<AuthMode> {
        let offered = self.providers.login_modes(provider);
        let models: Vec<Mode> = self.providers.catalog_entries(Some(provider), None).into_iter().flat_map(|entry| entry.auth).collect();
        supported_modes(offered.into_iter().filter(|mode| models.is_empty() || models.contains(mode)))
    }
    fn statuses(&self) -> Result<Vec<AuthStatus>, RpcError> {
        let mut statuses = self
            .providers
            .status()
            .into_iter()
            .map(|status| {
                if let Some(error) = status.error {
                    return Err(RpcError::internal(error));
                }
                Ok(AuthStatus {
                    modes: self.modes(&status.provider),
                    provider: status.provider,
                    mode: if status.mode.as_deref() == Some("subscription") { AuthMode::Subscription } else { AuthMode::ApiKey },
                    signed_in: status.signed_in,
                    expires_at: status.expires_at_ms.map(|ms| ms.min(i64::MAX as u64) as i64),
                    account: status.account_id,
                    default_model: None,
                    base_url: status.base_url,
                    plan: status.plan,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        // The test script model counts as a signed-in provider.
        if self.script.is_some() {
            statuses.push(AuthStatus {
                provider: "test".into(),
                mode: AuthMode::ApiKey,
                signed_in: true,
                expires_at: None,
                account: None,
                modes: Vec::new(),
                default_model: None,
                base_url: None,
                plan: None,
            });
        }
        if let Some((provider, model)) = otto_core::ModelResolver::default_model(self)
            && let Some(status) = statuses.iter_mut().find(|s| s.provider == provider)
        {
            status.default_model = Some(model);
        }
        Ok(statuses)
    }
    fn status_for(&self, provider: &str) -> Result<AuthStatus, RpcError> {
        self.statuses()?.into_iter().find(|s| s.provider == provider).ok_or_else(|| RpcError::not_found(provider))
    }
    fn models(&self, provider: Option<&str>) -> Vec<ModelInfo> {
        let script = self.script.as_ref().filter(|_| provider.is_none_or(|p| p == "test")).map(|s| s.info().clone());
        self.providers
            .status()
            .into_iter()
            .filter(|s| s.signed_in && provider.is_none_or(|p| p == s.provider))
            .flat_map(|s| {
                self.providers
                    .list_models(Some(&s.provider), Some(if s.mode.as_deref() == Some("subscription") { Mode::Subscription } else { Mode::ApiKey }))
            })
            .chain(script)
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
    fn refresh_models(&self) -> futures::future::BoxFuture<'_, bool> {
        Box::pin(self.providers.refresh_remote(false))
    }
}

impl AuthService for Services {
    fn status(&self) -> AuthFuture<'_, Vec<AuthStatus>> {
        Box::pin(async {
            self.providers.discover_local().await;
            self.statuses()
        })
    }
    fn login(
        &self,
        provider: String,
        mode: AuthMode,
        api_key: Option<String>,
        device: bool,
        base_url: Option<String>,
        paste: bool,
    ) -> AuthFuture<'_, LoginResult> {
        Box::pin(async move {
            if (device || paste) && mode != AuthMode::Subscription {
                return Err(RpcError::bad_request("device and paste logins require subscription mode"));
            }
            // Written to <state>/providers.json, where provider resolution reads it.
            if let Some(url) = &base_url {
                self.providers.set_base_url(&provider, Some(url)).map_err(rpc)?;
            }
            let mode = match mode {
                AuthMode::ApiKey => LoginMode::ApiKey,
                AuthMode::Subscription if device => LoginMode::Device,
                AuthMode::Subscription if paste => LoginMode::Paste,
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
            let needs_code = flow.needs_input();
            let accepts_code = flow.accepts_paste();
            let callback = flow.callback().map(|(port, path)| otto_proto::LoginCallback { port, path });
            let login_id = uuid::Uuid::new_v4().to_string();
            let mut logins = self.logins.lock().await;
            // A new login supersedes older ones for the same provider (and frees their loopback port).
            let stale: Vec<String> = logins
                .iter()
                .filter(|(_, l)| l.provider == provider || (l.flow.is_some() && l.at.elapsed() >= Duration::from_secs(900)))
                .map(|(id, _)| id.clone())
                .collect();
            for id in stale {
                if let Some(login) = logins.remove(&id) {
                    login.abort();
                }
            }
            if logins.len() >= 32 {
                return Err(RpcError::new(RpcError::BUSY, "too many pending logins"));
            }
            logins.insert(login_id.clone(), PendingLogin { provider, at: Instant::now(), flow: Some(flow), cancel: Arc::new(Notify::new()) });
            Ok(LoginResult::Pending { url, login_id, user_code, verification_uri, interval_secs, needs_code, callback, accepts_code })
        })
    }
    fn login_complete(&self, login_id: String, code: Option<String>) -> AuthFuture<'_, AuthStatus> {
        Box::pin(async move {
            let (provider, flow, cancel) = {
                let mut logins = self.logins.lock().await;
                let login = logins.get_mut(&login_id).ok_or_else(|| RpcError::not_found("login"))?;
                if login.at.elapsed() >= Duration::from_secs(900) {
                    logins.remove(&login_id);
                    return Err(RpcError::bad_request("login expired"));
                }
                let flow = login.flow.take().ok_or_else(|| RpcError::new(RpcError::BUSY, "login already in progress"))?;
                (login.provider.clone(), flow, login.cancel.clone())
            };
            let result = tokio::select! {
                result = self.providers.complete_login(flow, code.as_deref().unwrap_or_default()) => result.map_err(rpc),
                () = cancel.notified() => Err(RpcError::bad_request("login cancelled")),
            };
            let mut logins = self.logins.lock().await;
            if logins.get(&login_id).is_some_and(|l| Arc::ptr_eq(&l.cancel, &cancel)) {
                logins.remove(&login_id);
            }
            drop(logins);
            result?;
            self.status_for(&provider)
        })
    }
    fn login_cancel(&self, login_id: String) -> AuthFuture<'_, ()> {
        Box::pin(async move {
            if let Some(login) = self.logins.lock().await.remove(&login_id) {
                login.abort();
            }
            Ok(())
        })
    }
    fn logout(&self, provider: String) -> AuthFuture<'_, ()> {
        Box::pin(async move {
            let mut logins = self.logins.lock().await;
            let ids: Vec<String> = logins.iter().filter(|(_, l)| l.provider == provider).map(|(id, _)| id.clone()).collect();
            for id in ids {
                if let Some(login) = logins.remove(&id) {
                    login.abort();
                }
            }
            drop(logins);
            self.providers.logout(&provider).map_err(rpc)
        })
    }
    fn list_models(&self, provider: Option<String>) -> AuthFuture<'_, Vec<ModelInfo>> {
        Box::pin(async move {
            self.providers.discover_local().await;
            Ok(self.models(provider.as_deref()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn login_base_url_persists_for_local_providers_only() {
        let dir = tempfile::tempdir().unwrap();
        let services = Services::new(Providers::new(dir.path().join("auth"), dir.path().into()).unwrap()).unwrap();
        let url = "http://127.0.0.1:9";
        services.login("custom".into(), AuthMode::ApiKey, Some("k".into()), false, Some(url.into()), false).await.unwrap();
        assert!(std::fs::read_to_string(dir.path().join("providers.json")).unwrap().contains(url));
        assert!(services.statuses().unwrap().iter().any(|s| s.provider == "custom" && s.base_url.as_deref() == Some(url)));
        assert!(services.login("openai".into(), AuthMode::ApiKey, Some("k".into()), false, Some(url.into()), false).await.is_err());
        // A local server needs no key; with none answering it stays signed out.
        let LoginResult::SignedIn { auth } = services.login("ollama".into(), AuthMode::ApiKey, None, false, Some(url.into()), false).await.unwrap()
        else {
            panic!("expected a saved endpoint");
        };
        assert!(!auth.signed_in && auth.base_url.is_some());
    }
    #[tokio::test]
    async fn auth_adapter_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let services = Services::new(Providers::new(dir.path().join("auth"), dir.path().into()).unwrap()).unwrap();
        assert!(services.login("openai".into(), AuthMode::ApiKey, None, true, None, false).await.is_err());
        assert!(services.login("anthropic".into(), AuthMode::Subscription, None, true, None, false).await.is_err());
        assert!(matches!(
            services.login("openai".into(), AuthMode::ApiKey, Some("test-key".into()), false, None, false).await.unwrap(),
            LoginResult::SignedIn { .. }
        ));
        assert!(
            services.status().await.unwrap().iter().any(|status| status.provider == "openai" && status.signed_in && status.default_model.is_some())
        );
        assert!(!services.list_models(Some("openai".into())).await.unwrap().is_empty());
        services.logout("openai".into()).await.unwrap();
        let login = services.login("anthropic".into(), AuthMode::Subscription, None, false, None, false).await.unwrap();
        let LoginResult::Pending { login_id, url, user_code, verification_uri, interval_secs, needs_code, callback, accepts_code } = login else {
            panic!("expected OAuth flow");
        };
        assert!(url.starts_with("https://claude.ai/") && needs_code && accepts_code && callback.is_none());
        assert!(user_code.is_none() && verification_uri.is_none() && interval_secs.is_none());
        assert!(services.login_complete(login_id.clone(), Some("code#wrong-state".into())).await.is_err());
        assert!(services.login_complete(login_id, None).await.is_err());
    }

    #[tokio::test]
    async fn a_paste_login_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let services = Services::new(Providers::new(dir.path().join("auth"), dir.path().into()).unwrap()).unwrap();
        assert!(services.login("antigravity".into(), AuthMode::ApiKey, None, false, None, true).await.is_err());
        assert!(services.login("kimi-code".into(), AuthMode::Subscription, None, false, None, true).await.is_err(), "no paste redirect");
        let login = services.login("antigravity".into(), AuthMode::Subscription, None, false, None, true).await.unwrap();
        let LoginResult::Pending { login_id, url, needs_code, accepts_code, callback, user_code, .. } = login else {
            panic!("expected a paste login")
        };
        assert!(url.contains("redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback") && needs_code && accepts_code);
        assert!(callback.is_none() && user_code.is_none());
        // A callback URL whose state is not this login's is refused before any exchange.
        let pasted = "https://antigravity.google/oauth-callback?code=c&state=other";
        assert!(services.login_complete(login_id.clone(), Some(pasted.into())).await.is_err());
        assert!(services.login_complete(login_id, Some("c".into())).await.is_err(), "the login is spent");
    }

    #[test]
    fn supported_modes_dedupes_and_orders_api_key_first() {
        assert_eq!(supported_modes([Mode::Subscription, Mode::ApiKey, Mode::Subscription]), vec![AuthMode::ApiKey, AuthMode::Subscription]);
        assert_eq!(supported_modes([Mode::ApiKey]), vec![AuthMode::ApiKey]);
        assert!(supported_modes([]).is_empty());
    }

    #[tokio::test]
    async fn status_reports_modes_per_provider_even_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let services = Services::new(Providers::new(dir.path().join("auth"), dir.path().into()).unwrap()).unwrap();
        let statuses = services.status().await.unwrap();
        for provider in ["anthropic", "openai", "xai"] {
            let status = statuses.iter().find(|s| s.provider == provider).unwrap();
            assert!(!status.signed_in);
            assert_eq!(status.modes, vec![AuthMode::ApiKey, AuthMode::Subscription], "{provider}");
        }
        for provider in ["kimi-code", "antigravity"] {
            assert_eq!(
                statuses.iter().find(|s| s.provider == provider).unwrap().modes,
                vec![AuthMode::Subscription],
                "{provider}: subscription only"
            );
        }
        // Every other provider signs in with an API key (optional for local servers).
        for provider in ["google", "openrouter", "deepseek", "moonshotai", "zai", "minimax", "ollama", "lmstudio", "custom"] {
            let status = statuses.iter().find(|s| s.provider == provider).unwrap_or_else(|| panic!("{provider} listed"));
            assert_eq!(status.modes, vec![AuthMode::ApiKey], "{provider}");
        }
    }

    #[tokio::test]
    async fn cancel_and_supersede_drop_pending_logins() {
        let dir = tempfile::tempdir().unwrap();
        let services = Arc::new(Services::new(Providers::new(dir.path().join("auth"), dir.path().into()).unwrap()).unwrap());
        let LoginResult::Pending { login_id, .. } =
            services.login("anthropic".into(), AuthMode::Subscription, None, false, None, false).await.unwrap()
        else {
            panic!("expected OAuth flow");
        };
        services.login_cancel(login_id.clone()).await.unwrap();
        assert!(services.login_complete(login_id, Some("x#y".into())).await.is_err());
        // A second login for the same provider supersedes the first.
        let LoginResult::Pending { login_id: first, .. } =
            services.login("anthropic".into(), AuthMode::Subscription, None, false, None, false).await.unwrap()
        else {
            panic!("expected OAuth flow");
        };
        let LoginResult::Pending { login_id: second, .. } =
            services.login("anthropic".into(), AuthMode::Subscription, None, false, None, false).await.unwrap()
        else {
            panic!("expected OAuth flow");
        };
        assert!(services.login_complete(first, Some("x#y".into())).await.is_err());
        services.login_cancel(second).await.unwrap();
        assert!(services.logins.lock().await.is_empty());
    }
}
