//! Catalog, credential selection, and login orchestration shared by CLI and server.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use crate::{
    ErrorClass, LlmError, Model, ModelInfo, anthropic,
    auth::{self, AuthMode, Credential, CredentialSource, CredentialStore},
    catalog::{Catalog, CatalogEntry, ProviderEntry},
    compat::{self, Wire},
    google,
    http::{HttpClient, RetryPolicy},
    openai,
};

/// Every provider with a transport, in default-model preference order.
pub const IDS: [&str; 12] =
    ["anthropic", "openai", "google", "xai", "openrouter", "deepseek", "moonshotai", "zai", "minimax", "ollama", "lmstudio", "custom"];
/// Providers with a subscription sign-in.
const SUBSCRIPTION: [&str; 2] = ["anthropic", "openai"];
const XAI_URL: &str = "https://api.x.ai/v1";

#[derive(Clone)]
pub struct Providers {
    store: CredentialStore,
    state_dir: PathBuf,
    catalog: Arc<RwLock<Catalog>>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
    /// Models local servers reported at the last [`Providers::discover_local`].
    local: Arc<RwLock<BTreeMap<String, Vec<String>>>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AuthStatus {
    pub provider: String,
    pub mode: Option<String>,
    pub signed_in: bool,
    pub expires_at_ms: Option<u64>,
    pub account_id: Option<String>,
    pub error: Option<String>,
    /// The endpoint for local and custom providers.
    #[serde(default)]
    pub base_url: Option<String>,
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
            local: Arc::default(),
        })
    }

    /// The endpoint configured for a local or custom provider (`settings.json`
    /// in the state dir), else its environment override or default.
    #[must_use]
    pub fn base_url(&self, provider: &str) -> Option<String> {
        let configured = self.settings()[provider]["base_url"].as_str().map(str::to_owned);
        compat::dialect(provider)?.base(configured.as_deref())
    }

    /// Set or clear the base URL of a local or custom provider.
    /// # Errors
    /// Unknown providers, providers with a fixed endpoint, or write failures.
    pub fn set_base_url(&self, provider: &str, url: Option<&str>) -> Result<(), LlmError> {
        if !compat::dialect(provider).is_some_and(|d| d.discovery.is_some()) {
            return Err(LlmError::new(ErrorClass::InvalidRequest, format!("{provider} has a fixed endpoint")));
        }
        let url = url.map(str::trim).filter(|u| !u.is_empty());
        if let Some(url) = url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(LlmError::new(ErrorClass::InvalidRequest, "base URL must start with http:// or https://"));
        }
        let mut settings = self.settings();
        settings[provider]["base_url"] = url.map_or(serde_json::Value::Null, Into::into);
        let io = |e: std::io::Error| LlmError::new(ErrorClass::Other, format!("provider settings: {e}"));
        std::fs::create_dir_all(&self.state_dir).map_err(io)?;
        let temp = self.state_dir.join(format!(".providers-{}.tmp", rand::random::<u64>()));
        std::fs::write(&temp, serde_json::to_vec_pretty(&settings).unwrap_or_default()).map_err(io)?;
        std::fs::rename(&temp, self.state_dir.join("providers.json")).map_err(io)
    }

    fn settings(&self) -> serde_json::Value {
        std::fs::read(self.state_dir.join("providers.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}))
    }

    /// Ask local servers (Ollama, LM Studio, custom) for their models, with a
    /// short timeout each; a server that is not running lists nothing.
    pub async fn discover_local(&self) {
        let Ok(http) = HttpClient::new() else { return };
        let found = futures::future::join_all(compat::DIALECTS.iter().filter(|d| d.discovery.is_some()).map(|dialect| {
            let base = self.base_url(dialect.id);
            let key = self.credential(dialect.id).ok().flatten().and_then(|c| match c {
                Credential::ApiKey { key } => Some(key),
                Credential::OAuth { .. } => None,
            });
            let http = http.clone();
            async move {
                let models = match base {
                    Some(base) => compat::discover(dialect, &base, http.client(), key.as_deref()).await,
                    None => Vec::new(),
                };
                (dialect.id.to_owned(), models)
            }
        }))
        .await;
        *self.local.write().unwrap_or_else(|e| e.into_inner()) = found.into_iter().collect();
    }

    fn local_models(&self, provider: &str) -> Vec<String> {
        self.local.read().unwrap_or_else(|e| e.into_inner()).get(provider).cloned().unwrap_or_default()
    }

    /// A discovered local model that the catalog does not know.
    fn local_entry(&self, provider: &str, id: &str) -> CatalogEntry {
        let catalog = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        let known = catalog.providers().iter().any(|p| p.id == provider);
        let mut info = known.then(|| catalog.info_or_fallback(provider, id, Some(AuthMode::ApiKey))).flatten().unwrap_or_else(|| ModelInfo {
            id: id.into(),
            provider: provider.into(),
            name: id.into(),
            caps: crate::catalog::base_caps(provider),
            pricing: None,
        });
        if info.caps.context_window == 0 {
            info.caps.context_window = 32_768;
        }
        CatalogEntry {
            info,
            auth: vec![AuthMode::ApiKey],
            deprecated: false,
            status: Some("local".into()),
            efforts: Vec::new(),
            release_date: None,
            knowledge: None,
            family: None,
            modalities_in: vec!["text".into()],
            list_pricing: None,
            raw: serde_json::Value::Null,
        }
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
        let mut providers = self.catalog.read().unwrap_or_else(|e| e.into_inner()).providers();
        for dialect in compat::DIALECTS {
            if !providers.iter().any(|p| p.id == dialect.id) {
                providers.push(ProviderEntry {
                    id: dialect.id.into(),
                    name: dialect.name.into(),
                    env: dialect.env.iter().map(|s| (*s).into()).collect(),
                    doc: None,
                    implemented: true,
                });
            }
        }
        providers
    }

    /// Catalog models, plus whatever local servers reported (they replace
    /// the bundled list for those providers when reachable).
    #[must_use]
    pub fn catalog_entries(&self, provider: Option<&str>, mode: Option<AuthMode>) -> Vec<CatalogEntry> {
        let mut entries = Vec::new();
        for entry in self.catalog_providers().into_iter().filter(|entry| provider.is_none_or(|id| id == entry.id)) {
            let local = self.local_models(&entry.id);
            if !local.is_empty() && mode != Some(AuthMode::Subscription) {
                entries.extend(local.iter().map(|id| self.local_entry(&entry.id, id)));
                continue;
            }
            entries.extend(self.catalog.read().unwrap_or_else(|e| e.into_inner()).models(&entry.id, mode));
        }
        entries
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
        if !IDS.contains(&provider) {
            return None;
        }
        if self.local_models(provider).iter().any(|m| m == id) {
            return Some(self.local_entry(provider, id).info);
        }
        let catalog = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        if !catalog.providers().iter().any(|entry| entry.id == provider) {
            drop(catalog);
            return Some(self.local_entry(provider, id).info);
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
            return Err(unknown(provider));
        }
        if model.trim().is_empty() {
            return Err(LlmError::new(ErrorClass::InvalidRequest, "model id must not be empty"));
        }
        let dialect = compat::dialect(provider);
        let credential = match self.credential(provider)? {
            Some(credential) => credential,
            // Local and custom servers need no key.
            None if dialect.is_some_and(|d| !d.key_required) => Credential::ApiKey { key: String::new() },
            None => return Err(login_required(provider)),
        };
        let (mode, auth) = match credential {
            Credential::ApiKey { key } => (AuthMode::ApiKey, CredentialSource::api_key(key)),
            Credential::OAuth { .. } if SUBSCRIPTION.contains(&provider) => {
                let refresh = if provider == "anthropic" { anthropic::subscription::refresh_fn() } else { openai::subscription::refresh_fn() };
                (AuthMode::Subscription, CredentialSource::store(self.store.clone(), provider, refresh))
            }
            Credential::OAuth { .. } => return Err(login_required(provider)),
        };
        let info = if dialect.is_some() {
            self.model_info(provider, model).ok_or_else(|| unknown(provider))?
        } else {
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
        let messages = |base_url: String, auth, mode, model| {
            Arc::new(anthropic::AnthropicModel::new(anthropic::AnthropicConfig {
                base_url,
                auth,
                mode,
                model,
                betas: vec![],
                http: http.clone(),
                retry: retry.clone(),
                idle_timeout,
            })) as Arc<dyn Model>
        };
        let responses = |base_url: &str, auth, mode, model| {
            Arc::new(openai::OpenAiModel::new(openai::OpenAiConfig {
                base_url: base_url.into(),
                auth,
                mode,
                model,
                http: http.clone(),
                retry: retry.clone(),
                idle_timeout,
                organization: None,
                project: None,
            })) as Arc<dyn Model>
        };
        Ok(match (provider, dialect) {
            ("anthropic", _) => messages("https://api.anthropic.com".into(), auth, mode, info),
            ("openai", _) => responses("https://api.openai.com/v1", auth, mode, info),
            ("xai", _) => responses(XAI_URL, auth, mode, info),
            ("google", _) => Arc::new(google::GoogleModel::new(google::GoogleConfig {
                base_url: google::BASE_URL.into(),
                auth,
                model: info,
                http: http.clone(),
                retry: retry.clone(),
                idle_timeout,
            })),
            (_, Some(dialect)) => {
                let base_url = self
                    .base_url(provider)
                    .ok_or_else(|| LlmError::new(ErrorClass::InvalidRequest, format!("set a base URL with `otto-silo auth login {provider}`")))?;
                match dialect.wire {
                    Wire::Anthropic => messages(base_url, auth, mode, info),
                    Wire::Chat => Arc::new(compat::CompatModel::new(compat::CompatConfig {
                        dialect,
                        base_url,
                        auth: Some(auth),
                        model: info,
                        http: http.clone(),
                        retry: retry.clone(),
                        idle_timeout,
                    })),
                }
            }
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
        IDS.into_iter().find_map(|provider| {
            let mode = signed_in(provider)?;
            catalog.default_model(provider, mode).or_else(|| self.local_models(provider).into_iter().next()).map(|model| (provider.into(), model))
        })
    }

    #[must_use]
    pub fn status(&self) -> Vec<AuthStatus> {
        IDS.into_iter()
            .map(|provider| {
                let mut status = AuthStatus {
                    provider: provider.into(),
                    mode: None,
                    signed_in: false,
                    expires_at_ms: None,
                    account_id: None,
                    error: None,
                    base_url: compat::dialect(provider).filter(|d| d.discovery.is_some()).and_then(|_| self.base_url(provider)),
                };
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
                    // A running local server needs no sign-in.
                    Ok(None) if !self.local_models(provider).is_empty() => {
                        status.mode = Some("api_key".into());
                        status.signed_in = true;
                    }
                    Ok(None) => {}
                    Err(error) => status.error = Some(error.to_string()),
                }
                status
            })
            .collect()
    }

    /// Sign-in modes a provider offers: subscription only where a transport exists.
    #[must_use]
    pub fn login_modes(&self, provider: &str) -> Vec<AuthMode> {
        if !IDS.contains(&provider) {
            return Vec::new();
        }
        let mut modes = vec![AuthMode::ApiKey];
        if SUBSCRIPTION.contains(&provider) {
            modes.push(AuthMode::Subscription);
        }
        modes
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
                    // Local and custom servers work without a key.
                    if compat::dialect(&flow.provider).is_some_and(|d| !d.key_required) {
                        return self.store.remove(&flow.provider);
                    }
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
    LlmError::new(ErrorClass::InvalidRequest, format!("unknown provider {provider:?}; choose one of {}", IDS.join(", ")))
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
        if auth::env_api_key("google").is_none() {
            assert_eq!(f.1.model("google", "gemini-3.1-pro-preview").await.err().unwrap().class, ErrorClass::Auth);
        }
        assert!(f.1.model("nope", "x").await.err().unwrap().message.contains("unknown provider"));
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
