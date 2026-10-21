//! Catalog, credential selection, and login orchestration shared by CLI and server.
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use crate::{
    ErrorClass, LlmError, Model, ModelInfo, anthropic,
    auth::{self, AuthMode, Credential, CredentialSource, CredentialStore},
    catalog::{Catalog, CatalogEntry, ProviderEntry},
    http::{HttpClient, RetryPolicy},
    openai,
};

const IDS: [&str; 2] = ["anthropic", "openai"];

#[derive(Clone)]
pub struct Providers {
    store: CredentialStore,
    state_dir: PathBuf,
    catalog: Arc<RwLock<Catalog>>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AuthStatus {
    pub provider: String,
    pub mode: Option<String>,
    pub signed_in: bool,
    pub expires_at_ms: Option<u64>,
    pub account_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMode {
    ApiKey,
    Subscription,
    Max,
    Console,
    Device,
}

/// Opaque, single-use login state. Never serialize PKCE verifiers to clients.
pub struct LoginFlow {
    provider: String,
    pub url: Option<String>,
    pub user_code: Option<String>,
    state: LoginState,
}
enum LoginState {
    ApiKey,
    Anthropic { verifier: String, state: String },
    OpenAi(openai::subscription::Login),
    Device(openai::subscription::DeviceCode),
}
impl LoginFlow {
    #[must_use]
    pub fn verification_uri(&self) -> Option<&str> {
        match &self.state {
            LoginState::Device(code) => Some(&code.verification_uri),
            _ => None,
        }
    }

    #[must_use]
    pub fn interval_secs(&self) -> Option<u64> {
        match &self.state {
            LoginState::Device(code) => Some(code.interval.as_secs()),
            _ => None,
        }
    }

    #[must_use]
    pub fn needs_input(&self) -> bool {
        matches!(self.state, LoginState::ApiKey | LoginState::Anthropic { .. })
    }
}

impl Providers {
    /// # Errors
    /// Returns an auth error if the credential store cannot be opened.
    pub fn new(auth_dir: PathBuf, state_dir: PathBuf) -> Result<Self, LlmError> {
        Ok(Self {
            store: CredentialStore::open(auth_dir)?,
            catalog: Arc::new(RwLock::new(Catalog::load(Some(&state_dir)))),
            state_dir,
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Refresh stale metadata, preserving the loaded catalog on failure.
    /// # Errors
    /// Returns network, validation, or cache-write errors.
    pub async fn refresh_catalog(&self) -> Result<bool, LlmError> {
        self.refresh(false).await
    }

    /// Refresh even when the cache is fresh.
    /// # Errors
    /// Returns network, validation, or cache-write errors.
    pub async fn force_refresh_catalog(&self) -> Result<bool, LlmError> {
        self.refresh(true).await
    }

    async fn refresh(&self, force: bool) -> Result<bool, LlmError> {
        let _guard = self.refresh_lock.lock().await;
        if !force && !self.catalog.read().unwrap_or_else(|e| e.into_inner()).needs_refresh() {
            return Ok(false);
        }
        Catalog::refresh(HttpClient::new()?.client(), &self.state_dir)
            .await
            .map_err(|error| LlmError::new(ErrorClass::Other, format!("catalog refresh: {error}")))?;
        *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = Catalog::load(Some(&self.state_dir));
        Ok(true)
    }

    #[must_use]
    pub fn catalog_providers(&self) -> Vec<ProviderEntry> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner()).providers()
    }

    #[must_use]
    pub fn catalog_entries(&self, provider: Option<&str>, mode: Option<AuthMode>) -> Vec<CatalogEntry> {
        let catalog = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        catalog
            .providers()
            .into_iter()
            .filter(|entry| provider.is_none_or(|id| id == entry.id))
            .flat_map(|entry| catalog.models(&entry.id, mode))
            .collect()
    }

    #[must_use]
    pub fn list_models(&self, provider: Option<&str>, mode: Option<AuthMode>) -> Vec<ModelInfo> {
        self.catalog_entries(provider, mode).into_iter().map(|entry| entry.info).collect()
    }

    /// Supported sign-in modes for a catalog model; empty for unknown ids.
    #[must_use]
    pub fn model_auth_modes(&self, provider: &str, id: &str) -> Vec<AuthMode> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner()).entry(provider, id, None).map(|entry| entry.auth).unwrap_or_default()
    }

    /// Catalog metadata, with estimated fallback capabilities for unknown model ids.
    #[must_use]
    pub fn model_info(&self, provider: &str, id: &str) -> Option<ModelInfo> {
        let catalog = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        if !catalog.providers().iter().any(|entry| entry.id == provider) {
            return None;
        }
        catalog.info_or_fallback(provider, id, None)
    }

    fn credential(&self, provider: &str) -> Result<Option<Credential>, LlmError> {
        self.credential_with_env(provider, auth::env_api_key)
    }

    fn credential_with_env(&self, provider: &str, env: impl FnOnce(&str) -> Option<String>) -> Result<Option<Credential>, LlmError> {
        Ok(self.store.get(provider)?.or_else(|| env(provider).map(|key| Credential::ApiKey { key })))
    }

    /// # Errors
    /// Returns an invalid-request error for unknown providers, or an auth error
    /// with a login hint when credentials are absent.
    pub async fn model(&self, provider: &str, model: &str) -> Result<Arc<dyn Model>, LlmError> {
        self.resolve_model(provider, model)
    }

    /// Construct a model without network I/O; credentials refresh on the first call.
    /// # Errors
    /// Returns provider, model availability, or credential errors.
    pub fn resolve_model(&self, provider: &str, model: &str) -> Result<Arc<dyn Model>, LlmError> {
        if !IDS.contains(&provider) {
            return Err(LlmError::new(ErrorClass::InvalidRequest, format!("provider {provider:?} is not yet supported; choose anthropic or openai")));
        }
        if model.trim().is_empty() {
            return Err(LlmError::new(ErrorClass::InvalidRequest, "model id must not be empty"));
        }
        let credential = self.credential(provider)?.ok_or_else(|| login_required(provider))?;
        let (mode, auth) = match credential {
            Credential::ApiKey { key } => (AuthMode::ApiKey, CredentialSource::api_key(key)),
            Credential::OAuth { .. } => {
                let refresh = if provider == "anthropic" { anthropic::subscription::refresh_fn() } else { openai::subscription::refresh_fn() };
                (AuthMode::Subscription, CredentialSource::store(self.store.clone(), provider, refresh))
            }
        };
        let info = {
            let catalog = self.catalog.read().unwrap_or_else(|e| e.into_inner());
            if mode == AuthMode::Subscription && !catalog.supports(provider, model, mode) {
                let available = catalog.models(provider, Some(mode)).into_iter().map(|entry| entry.info.id).collect::<Vec<_>>().join(", ");
                return Err(LlmError::new(
                    ErrorClass::InvalidRequest,
                    format!("{provider}/{model} is not available with subscription sign-in. Available models: {available}"),
                ));
            }
            catalog
                .info_or_fallback(provider, model, Some(mode))
                .ok_or_else(|| LlmError::new(ErrorClass::InvalidRequest, "model unavailable for current auth mode"))?
        };
        let http = HttpClient::new()?;
        let retry = RetryPolicy::default();
        let idle_timeout = Duration::from_secs(120);
        Ok(match provider {
            "anthropic" => Arc::new(anthropic::AnthropicModel::new(anthropic::AnthropicConfig {
                base_url: "https://api.anthropic.com".into(),
                auth,
                mode,
                model: info,
                betas: vec![],
                http,
                retry,
                idle_timeout,
            })),
            "openai" => Arc::new(openai::OpenAiModel::new(openai::OpenAiConfig {
                base_url: "https://api.openai.com/v1".into(),
                auth,
                mode,
                model: info,
                http,
                retry,
                idle_timeout,
                organization: None,
                project: None,
            })),
            _ => return Err(unknown(provider)),
        })
    }

    #[must_use]
    pub fn default_model(&self) -> Option<(String, String)> {
        self.default_with(std::env::var("OTTO_SILO_MODEL").ok().as_deref(), |provider| {
            self.credential(provider).ok().flatten().map(|credential| match credential {
                Credential::ApiKey { .. } => AuthMode::ApiKey,
                Credential::OAuth { .. } => AuthMode::Subscription,
            })
        })
    }

    fn default_with(&self, override_id: Option<&str>, signed_in: impl Fn(&str) -> Option<AuthMode>) -> Option<(String, String)> {
        if let Some((provider, model)) = override_id.and_then(|value| value.split_once('/'))
            && IDS.contains(&provider)
            && !model.trim().is_empty()
        {
            return Some((provider.into(), model.into()));
        }
        let catalog = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        IDS.into_iter().find_map(|provider| catalog.default_model(provider, signed_in(provider)?).map(|model| (provider.into(), model)))
    }

    #[must_use]
    pub fn status(&self) -> Vec<AuthStatus> {
        IDS.into_iter()
            .map(|provider| {
                let mut status =
                    AuthStatus { provider: provider.into(), mode: None, signed_in: false, expires_at_ms: None, account_id: None, error: None };
                match self.credential(provider) {
                    Ok(Some(Credential::ApiKey { .. })) => {
                        status.mode = Some("api_key".into());
                        status.signed_in = true;
                    }
                    Ok(Some(Credential::OAuth { expires_at_ms, account_id, .. })) => {
                        status.mode = Some("subscription".into());
                        status.signed_in = true;
                        status.expires_at_ms = Some(expires_at_ms);
                        status.account_id = account_id;
                    }
                    Ok(None) => {}
                    Err(error) => status.error = Some(error.to_string()),
                }
                status
            })
            .collect()
    }

    /// # Errors
    /// Returns errors from provider validation, loopback binding, or device authorization.
    pub async fn begin_login(&self, provider: &str, mode: LoginMode) -> Result<LoginFlow, LlmError> {
        if !IDS.contains(&provider) {
            return Err(unknown(provider));
        }
        let mut flow = LoginFlow { provider: provider.into(), url: None, user_code: None, state: LoginState::ApiKey };
        flow.state = match (provider, mode) {
            (_, LoginMode::ApiKey) => LoginState::ApiKey,
            ("anthropic", LoginMode::Subscription | LoginMode::Max | LoginMode::Console) => {
                let mode = if mode == LoginMode::Console { anthropic::subscription::Mode::Console } else { anthropic::subscription::Mode::Max };
                let (url, verifier, state) = anthropic::subscription::authorize(mode);
                flow.url = Some(url);
                LoginState::Anthropic { verifier, state }
            }
            ("openai", LoginMode::Subscription) => match openai::subscription::Login::begin().await {
                Ok(login) => {
                    flow.url = Some(login.url.clone());
                    LoginState::OpenAi(login)
                }
                Err(_) => return self.device_login().await,
            },
            ("openai", LoginMode::Device) => return self.device_login().await,
            _ => return Err(LlmError::new(ErrorClass::InvalidRequest, "login mode is not supported by this provider")),
        };
        Ok(flow)
    }

    async fn device_login(&self) -> Result<LoginFlow, LlmError> {
        let code = openai::subscription::device_code(&reqwest::Client::new()).await?;
        Ok(LoginFlow {
            provider: "openai".into(),
            url: Some(code.verification_uri.clone()),
            user_code: Some(code.user_code.clone()),
            state: LoginState::Device(code),
        })
    }

    /// # Errors
    /// Returns errors for empty keys, mismatched OAuth state, timeout, token exchange,
    /// or credential persistence. Dropping this future cancels pending authorization.
    pub async fn complete_login(&self, flow: LoginFlow, input: &str) -> Result<(), LlmError> {
        let credential = match flow.state {
            LoginState::ApiKey => {
                if input.trim().is_empty() {
                    return Err(LlmError::new(ErrorClass::Auth, "API key must not be empty"));
                }
                Credential::ApiKey { key: input.trim().into() }
            }
            LoginState::Anthropic { verifier, state } => {
                if input.trim().split_once('#').map(|(_, pasted)| pasted) != Some(state.as_str()) {
                    return Err(LlmError::new(ErrorClass::Auth, "OAuth state mismatch; restart login"));
                }
                anthropic::subscription::exchange(input, &verifier).await?
            }
            LoginState::OpenAi(login) => login.finish(&reqwest::Client::new()).await?,
            LoginState::Device(code) => tokio::time::timeout(Duration::from_secs(900), async {
                let client = reqwest::Client::new();
                loop {
                    if let Some(credential) = openai::subscription::poll_device(&client, &code).await? {
                        return Ok(credential);
                    }
                    tokio::time::sleep(code.interval).await;
                }
            })
            .await
            .map_err(|_| LlmError::new(ErrorClass::Timeout, "device login timed out"))??,
        };
        self.store.set(&flow.provider, credential)
    }

    /// Removes stored credentials; environment API keys remain in effect.
    /// # Errors
    /// Returns provider validation or credential-store errors.
    pub fn logout(&self, provider: &str) -> Result<(), LlmError> {
        if !IDS.contains(&provider) {
            return Err(unknown(provider));
        }
        self.store.remove(provider)
    }
}

fn unknown(provider: &str) -> LlmError {
    LlmError::new(ErrorClass::InvalidRequest, format!("unknown provider {provider:?}; choose anthropic or openai"))
}
fn login_required(provider: &str) -> LlmError {
    LlmError::new(ErrorClass::Auth, format!("run `otto-silo auth login {provider}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf, Providers);
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("otto-providers-{}", rand::random::<u64>()));
            let providers = Providers::new(dir.join("auth"), dir.clone()).unwrap();
            Self(dir, providers)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn precedence_login_and_logout() {
        let f = Fixture::new();
        let flow = f.1.begin_login("anthropic", LoginMode::ApiKey).await.unwrap();
        f.1.complete_login(flow, "stored-key").await.unwrap();
        assert_eq!(f.1.credential_with_env("anthropic", |_| Some("env-key".into())).unwrap(), Some(Credential::ApiKey { key: "stored-key".into() }));
        let oauth = Credential::OAuth {
            access: "access".into(),
            refresh: "refresh".into(),
            expires_at_ms: auth::now_ms() + 3_600_000,
            account_id: Some("account".into()),
            extra: serde_json::Value::Null,
        };
        f.1.store.set("anthropic", oauth.clone()).unwrap();
        assert_eq!(f.1.credential_with_env("anthropic", |_| Some("env-key".into())).unwrap(), Some(oauth));
        let model = f.1.model("anthropic", "claude-sonnet-5-5").await.unwrap();
        assert_eq!(model.info().id, "claude-sonnet-5-5");
        let status = f.1.status().into_iter().find(|status| status.provider == "anthropic").unwrap();
        assert_eq!(status.mode.as_deref(), Some("subscription"));
        assert_eq!(status.account_id.as_deref(), Some("account"));
        f.1.logout("anthropic").unwrap();
        assert_eq!(f.1.credential_with_env("anthropic", |_| Some("env-key".into())).unwrap(), Some(Credential::ApiKey { key: "env-key".into() }));
        assert_eq!(f.1.credential_with_env("anthropic", |_| None).unwrap(), None);
    }
    #[test]
    fn defaults_and_unknown_models() {
        let f = Fixture::new();
        assert_eq!(f.1.default_with(None, |_| None), None);
        assert_eq!(f.1.default_with(None, |_| Some(AuthMode::ApiKey)).unwrap(), ("anthropic".into(), "claude-sonnet-5-5".into()));
        assert_eq!(f.1.default_with(None, |p| (p == "openai").then_some(AuthMode::Subscription)).unwrap().1, "gpt-6.1-sol");
        assert_eq!(f.1.default_with(Some("openai/custom"), |_| None).unwrap().1, "custom");
        let info = f.1.model_info("anthropic", "claude-sonnet-5-5-new").unwrap();
        assert_eq!(info.caps, anthropic::model_info("claude-sonnet-5-5").unwrap().caps);
        assert!(f.1.model_info("unknown", "foo").is_none());
    }
    #[tokio::test]
    async fn subscription_enforcement_and_api_fallback() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Providers>();
        let f = Fixture::new();
        f.1.store
            .set(
                "openai",
                Credential::OAuth {
                    access: "access".into(),
                    refresh: "refresh".into(),
                    expires_at_ms: 0,
                    account_id: None,
                    extra: serde_json::Value::Null,
                },
            )
            .unwrap();
        for id in ["gpt-5.5", "gpt-4o", "gpt-unreleased"] {
            let error = f.1.model("openai", id).await.err().unwrap();
            assert_eq!(error.class, ErrorClass::InvalidRequest);
            assert!(error.message.contains("gpt-6.1-sol"));
        }
        assert!(f.1.model("openai", "gpt-6.1-sol").await.is_ok());
        f.1.store.set("openai", Credential::ApiKey { key: "key".into() }).unwrap();
        assert!(f.1.model("openai", "gpt-5.5").await.is_ok());
        assert!(f.1.model("openai", "gpt-unreleased").await.is_ok());
        assert!(f.1.model("google", "gemini-3.1-pro-preview").await.err().unwrap().message.contains("not yet supported"));
        assert!(!f.1.list_models(Some("openai"), Some(AuthMode::Subscription)).iter().any(|info| info.id == "gpt-5.5"));
        assert_eq!(f.1.model_auth_modes("openai", "gpt-5.5"), vec![AuthMode::ApiKey]);
    }

    #[tokio::test]
    async fn fresh_cache_skips_network_and_loads_state_root() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.0.join("catalog")).unwrap();
        let mut feed: serde_json::Value = serde_json::from_str(crate::catalog::SNAPSHOT).unwrap();
        feed["openai"]["models"]["gpt-6.1-sol"]["name"] = serde_json::json!("Cached Sol");
        let cache = serde_json::json!({"refreshed_at": auth::now_ms() / 1000, "providers": feed});
        std::fs::write(f.0.join("catalog/models.json"), serde_json::to_vec(&cache).unwrap()).unwrap();
        let providers = Providers::new(f.0.join("auth"), f.0.clone()).unwrap();
        assert_eq!(providers.model_info("openai", "gpt-6.1-sol").unwrap().name, "Cached Sol");
        assert!(!providers.refresh_catalog().await.unwrap());
    }

    #[tokio::test]
    async fn validates_input_without_network() {
        let f = Fixture::new();
        let flow = f.1.begin_login("anthropic", LoginMode::ApiKey).await.unwrap();
        assert!(f.1.complete_login(flow, " ").await.is_err());
        let flow = f.1.begin_login("anthropic", LoginMode::Max).await.unwrap();
        assert!(f.1.complete_login(flow, "code#wrong-state").await.is_err());
        assert!(f.1.begin_login("anthropic", LoginMode::Device).await.is_err());
    }
}
