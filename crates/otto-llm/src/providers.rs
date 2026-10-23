//! Catalog, credential selection, and login orchestration shared by CLI and server.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use crate::{
    ErrorClass, LlmError, Model, ModelInfo, anthropic,
    auth::{
        self, AuthMode, Credential, CredentialSource, CredentialStore,
        flows::{self, Callback, SubscriptionSpec},
    },
    catalog::{
        Catalog, CatalogEntry, ProviderEntry,
        remote::{RemoteModel, RemoteStore},
    },
    compat::{self, Wire},
    google,
    http::{HttpClient, RetryPolicy},
    openai,
};

/// Every provider with a transport, in default-model preference order.
pub const IDS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "xai",
    "openrouter",
    "deepseek",
    "moonshotai",
    compat::kimi::ID,
    google::codeassist::convert::PROVIDER,
    "zai",
    "minimax",
    "ollama",
    "lmstudio",
    "custom",
];
const XAI_URL: &str = "https://api.x.ai/v1";
/// Subscription providers whose model list comes from the provider (`catalog::remote`).
pub const REMOTE: &[&str] = &[compat::kimi::ID, google::codeassist::convert::PROVIDER];

/// Fetches `provider`'s model list with a signed-in credential (tests replace it).
type RemoteFetch = Arc<dyn Fn(&'static str, Credential) -> futures::future::BoxFuture<'static, Result<Vec<RemoteModel>, LlmError>> + Send + Sync>;
fn fetch_remote(provider: &'static str, credential: Credential) -> futures::future::BoxFuture<'static, Result<Vec<RemoteModel>, LlmError>> {
    Box::pin(async move {
        let Credential::OAuth { access, .. } = &credential else { return Err(LlmError::new(ErrorClass::Auth, "expected a sign-in")) };
        let http = HttpClient::new()?;
        if provider == compat::kimi::ID {
            return compat::kimi::fetch_kimi_models(http.client(), access).await;
        }
        let project = match google::codeassist::stored_project(&credential) {
            Some(project) => project,
            None => google::codeassist::project::Project::new(http.clone(), None).get(access).await?,
        };
        google::codeassist::fetch_antigravity_models(http.client(), access, &project).await
    })
}

#[derive(Clone)]
pub struct Providers {
    store: CredentialStore,
    state_dir: PathBuf,
    catalog: Arc<RwLock<Catalog>>,
    /// The model lists [`REMOTE`] providers reported.
    remote: RemoteStore,
    remote_fetch: RemoteFetch,
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
    /// Subscription plan label (`Max`, `Pro`, `Plus`, …) when the sign-in reports it.
    #[serde(default)]
    pub plan: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMode {
    ApiKey,
    Subscription,
    Max,
    Console,
    Device,
    /// Browser sign-in where the user pastes the code or callback URL.
    Paste,
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
    Pkce(flows::Login),
    Device(&'static SubscriptionSpec, flows::DeviceCode),
}
impl LoginFlow {
    #[must_use]
    pub fn verification_uri(&self) -> Option<&str> {
        match &self.state {
            LoginState::Device(_, code) => Some(&code.verification_uri),
            _ => None,
        }
    }

    #[must_use]
    pub fn interval_secs(&self) -> Option<u64> {
        match &self.state {
            LoginState::Device(_, code) => Some(code.interval.as_secs()),
            _ => None,
        }
    }

    /// The user has to supply something to `complete_login`: an API key, or the code the provider shows.
    #[must_use]
    pub fn needs_input(&self) -> bool {
        match &self.state {
            LoginState::ApiKey => true,
            LoginState::Pkce(login) => login.needs_paste(),
            LoginState::Device(..) => false,
        }
    }

    /// `complete_login` also takes a pasted code or full callback URL instead of waiting for the browser.
    #[must_use]
    pub fn accepts_paste(&self) -> bool {
        matches!(self.state, LoginState::Pkce(_))
    }

    /// The loopback `(port, path)` the browser is redirected to, for flows that listen on one.
    #[must_use]
    pub fn callback(&self) -> Option<(u16, String)> {
        match &self.state {
            LoginState::Pkce(login) => login.callback().map(|c| (c.port(), c.path().to_owned())),
            _ => None,
        }
    }
}

impl Providers {
    /// # Errors
    /// Returns an auth error if the credential store cannot be opened.
    pub fn new(auth_dir: PathBuf, state_dir: PathBuf) -> Result<Self, LlmError> {
        Ok(Self {
            store: CredentialStore::open(auth_dir)?,
            catalog: Arc::new(RwLock::new(Catalog::load(Some(&state_dir)))),
            remote: RemoteStore::load(&state_dir),
            remote_fetch: Arc::new(fetch_remote),
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

    /// Fetch the model list of every [`REMOTE`] provider signed in with a subscription whose list
    /// is due (`force`: regardless). Returns whether a list was saved; failures keep the old one.
    pub async fn refresh_remote(&self, force: bool) -> bool {
        let mut changed = false;
        for provider in REMOTE.iter().copied() {
            if force || self.remote.due(provider, SystemTime::now()) {
                changed |= self.refresh_remote_one(provider).await;
            }
        }
        changed
    }
    async fn refresh_remote_one(&self, provider: &'static str) -> bool {
        let (Ok(Some(Credential::OAuth { .. })), Some(spec)) = (self.store.get(provider), flows::spec(provider)) else { return false };
        let source = CredentialSource::store(self.store.clone(), provider, flows::refresh_fn(spec, self.state_dir.clone()));
        let result = match source.current().await {
            Ok(credential) => self.remote.refresh(provider, SystemTime::now(), (self.remote_fetch)(provider, credential)).await,
            Err(error) => Err(error),
        };
        result.inspect_err(|error| tracing::debug!(%error, provider, "model list refresh")).is_ok()
    }

    /// The entries of a [`REMOTE`] provider: its fetched list over the catalog, else the overlay's.
    fn remote_entries(&self, provider: &str, mode: Option<AuthMode>) -> Vec<CatalogEntry> {
        let catalog = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        self.remote.models(&catalog, provider, mode).into_iter().filter(|e| mode.is_none_or(|m| e.auth.contains(&m))).collect()
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
            if REMOTE.contains(&entry.id.as_str()) {
                entries.extend(self.remote_entries(&entry.id, mode));
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
        if REMOTE.contains(&provider)
            && let Some(entry) = self.remote_entries(provider, None).into_iter().find(|e| e.info.id == id)
        {
            return Some(entry.info);
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
        let project = google::codeassist::stored_project(&credential);
        let (mode, auth) = match credential {
            Credential::ApiKey { key } => (AuthMode::ApiKey, CredentialSource::api_key(key)),
            Credential::OAuth { .. } => match flows::spec(provider) {
                Some(spec) => {
                    (AuthMode::Subscription, CredentialSource::store(self.store.clone(), provider, flows::refresh_fn(spec, self.state_dir.clone())))
                }
                None => return Err(login_required(provider)),
            },
        };
        let info = if dialect.is_some() || REMOTE.contains(&provider) {
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
            ("xai", _) if mode == AuthMode::Subscription => openai::xai::model(info, auth, http.clone(), retry.clone(), idle_timeout),
            ("xai", _) => responses(XAI_URL, auth, mode, info),
            ("google", _) => Arc::new(google::GoogleModel::new(google::GoogleConfig {
                base_url: google::BASE_URL.into(),
                auth,
                model: info,
                http: http.clone(),
                retry: retry.clone(),
                idle_timeout,
            })),
            (compat::kimi::ID, _) => Arc::new(compat::kimi::KimiCodeModel::new(compat::kimi::KimiCodeConfig {
                base_url: compat::kimi::BASE_URL.into(),
                auth,
                efforts: self.remote_entries(provider, None).into_iter().find(|e| e.info.id == model).map(|e| e.efforts).unwrap_or_default(),
                model: info,
                headers: compat::kimi::kimi_headers(&self.state_dir)?,
                http: http.clone(),
                retry: retry.clone(),
                idle_timeout,
            })),
            (google::codeassist::convert::PROVIDER, _) => {
                Arc::new(google::codeassist::AntigravityModel::new(google::codeassist::AntigravityConfig::new(
                    info,
                    auth,
                    http.clone(),
                    google::codeassist::project::Project::new(http.clone(), project),
                )))
            }
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
        IDS.iter().copied().find_map(|provider| {
            let mode = signed_in(provider)?;
            catalog.default_model(provider, mode).or_else(|| self.local_models(provider).into_iter().next()).map(|model| (provider.into(), model))
        })
    }

    #[must_use]
    pub fn status(&self) -> Vec<AuthStatus> {
        IDS.iter()
            .copied()
            .map(|provider| {
                let mut status = AuthStatus {
                    provider: provider.into(),
                    mode: None,
                    signed_in: false,
                    expires_at_ms: None,
                    account_id: None,
                    error: None,
                    base_url: compat::dialect(provider).filter(|d| d.discovery.is_some()).and_then(|_| self.base_url(provider)),
                    plan: None,
                };
                match self.credential(provider) {
                    Ok(Some(Credential::ApiKey { .. })) => {
                        status.mode = Some("api_key".into());
                        status.signed_in = true;
                    }
                    Ok(Some(Credential::OAuth { expires_at_ms, account_id, extra, .. })) => {
                        status.mode = Some("subscription".into());
                        status.signed_in = true;
                        status.expires_at_ms = Some(expires_at_ms);
                        status.account_id = account_id;
                        status.plan = flows::spec(provider).and_then(|spec| (spec.plan)(&extra));
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
        match flows::spec(provider) {
            Some(spec) if spec.api_key => vec![AuthMode::ApiKey, AuthMode::Subscription],
            Some(_) => vec![AuthMode::Subscription],
            None => vec![AuthMode::ApiKey],
        }
    }

    /// The provider's sign-in has a device flow, the way out when no browser opens.
    #[must_use]
    pub fn has_device_login(&self, provider: &str) -> bool {
        flows::spec(provider).is_some_and(|spec| spec.device.is_some())
    }

    /// The provider's sign-in can show a code to paste, the way out when the browser cannot reach this machine.
    #[must_use]
    pub fn has_paste_login(&self, provider: &str) -> bool {
        flows::spec(provider).is_some_and(|spec| spec.callbacks.iter().any(|c| matches!(c, Callback::Paste { .. })))
    }

    /// # Errors
    /// Returns errors from provider validation, loopback binding, or device authorization.
    pub async fn begin_login(&self, provider: &str, mode: LoginMode) -> Result<LoginFlow, LlmError> {
        if !IDS.contains(&provider) {
            return Err(unknown(provider));
        }
        let mut flow = LoginFlow { provider: provider.into(), url: None, user_code: None, state: LoginState::ApiKey };
        let unsupported = || LlmError::new(ErrorClass::InvalidRequest, "login mode is not supported by this provider");
        let spec = flows::spec(provider);
        if mode == LoginMode::ApiKey {
            return if spec.is_none_or(|spec| spec.api_key) { Ok(flow) } else { Err(unsupported()) };
        }
        let spec = spec.ok_or_else(unsupported)?;
        let callback = match mode {
            LoginMode::Device => None,
            LoginMode::Paste => Some(spec.callbacks.iter().find(|c| matches!(c, Callback::Paste { .. })).ok_or_else(unsupported)?),
            LoginMode::Max | LoginMode::Console if spec.console.is_none() => return Err(unsupported()),
            _ => spec.callbacks.first(),
        };
        if let Some(callback) = callback {
            match flows::Login::begin(spec, callback, mode == LoginMode::Console).await {
                Ok(login) => {
                    flow.url = Some(login.url.clone());
                    flow.state = LoginState::Pkce(login);
                    return Ok(flow);
                }
                // A busy loopback port falls back to the device flow where there is one.
                Err(error) if spec.device.is_none() || mode == LoginMode::Paste => return Err(error),
                Err(_) => {}
            }
        }
        let code = flows::begin_device(spec, flows::Cx { client: &reqwest::Client::new(), dir: &self.state_dir }).await?;
        flow.url = Some(code.url().to_owned());
        flow.user_code = Some(code.user_code.clone());
        flow.state = LoginState::Device(spec, code);
        Ok(flow)
    }

    /// `input` is the API key, or for a sign-in the pasted code, `code#state` or full callback URL;
    /// empty waits for the browser's loopback callback or the device approval.
    /// # Errors
    /// Returns errors for empty keys, mismatched OAuth state, timeout, token exchange,
    /// or credential persistence. Dropping this future cancels pending authorization.
    pub async fn complete_login(&self, flow: LoginFlow, input: &str) -> Result<(), LlmError> {
        let client = reqwest::Client::new();
        let cx = flows::Cx { client: &client, dir: &self.state_dir };
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
            LoginState::Pkce(login) => login.finish(cx, input).await?,
            LoginState::Device(spec, code) => flows::poll_device(spec, cx, &code).await?,
        };
        let subscription = matches!(credential, Credential::OAuth { .. });
        self.store.set(&flow.provider, credential)?;
        // The new account's own model list; a failure keeps the previous or fallback list.
        if let Some(provider) = REMOTE.iter().copied().find(|p| *p == flow.provider).filter(|_| subscription) {
            self.refresh_remote_one(provider).await;
        }
        Ok(())
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
        assert!(f.1.begin_login("openai", LoginMode::Console).await.is_err(), "no console sign-in, and no device fallback for it");
    }

    #[tokio::test]
    async fn kimi_code_is_subscription_only() {
        let f = Fixture::new();
        assert_eq!(f.1.login_modes("kimi-code"), vec![AuthMode::Subscription]);
        assert!(f.1.has_device_login("kimi-code"));
        assert!(f.1.begin_login("kimi-code", LoginMode::ApiKey).await.is_err());
        assert!(f.1.begin_login("kimi-code", LoginMode::Paste).await.is_err());
        assert_eq!(f.1.resolve_model("kimi-code", "kimi-k3").err().unwrap().class, ErrorClass::Auth);
        let oauth = Credential::OAuth { access: "a".into(), refresh: "r".into(), expires_at_ms: 0, account_id: None, extra: serde_json::Value::Null };
        f.1.store.set("kimi-code", oauth).unwrap();
        let model = f.1.resolve_model("kimi-code", "kimi-k3").unwrap();
        assert_eq!((model.info().provider.as_str(), model.info().caps.context_window), ("kimi-code", 1_048_576));
        assert!(f.0.join("kimi-device-id").exists(), "the device id lives in the state folder");
        assert!(f.1.list_models(Some("kimi-code"), Some(AuthMode::Subscription)).iter().any(|m| m.id == "kimi-k3.1-pro"));
        assert_eq!(f.1.status().iter().find(|s| s.provider == "kimi-code").unwrap().mode.as_deref(), Some("subscription"));
    }

    #[tokio::test]
    async fn antigravity_is_subscription_only_with_a_paste_login() {
        let f = Fixture::new();
        assert_eq!(IDS.len(), 14);
        assert_eq!(IDS.iter().position(|p| *p == "antigravity"), IDS.iter().position(|p| *p == "kimi-code").map(|i| i + 1));
        assert_eq!(f.1.login_modes("antigravity"), vec![AuthMode::Subscription]);
        assert!(!f.1.has_device_login("antigravity") && f.1.has_paste_login("antigravity") && !f.1.has_paste_login("kimi-code"));
        assert!(f.1.begin_login("antigravity", LoginMode::ApiKey).await.is_err());
        let flow = f.1.begin_login("antigravity", LoginMode::Paste).await.unwrap();
        let url = flow.url.clone().unwrap();
        assert!(url.starts_with("https://accounts.google.com/") && url.contains("access_type=offline") && url.contains("prompt=consent"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback"));
        assert!(flow.needs_input() && flow.accepts_paste() && flow.callback().is_none());
        assert_eq!(f.1.resolve_model("antigravity", "gemini-3.1-pro-low").err().unwrap().class, ErrorClass::Auth);
        let extra = serde_json::json!({"project":"proj"});
        f.1.store
            .set("antigravity", Credential::OAuth { access: "a".into(), refresh: "r".into(), expires_at_ms: 0, account_id: None, extra })
            .unwrap();
        let model = f.1.resolve_model("antigravity", "gemini-3.1-pro-low").unwrap();
        assert_eq!((model.info().provider.as_str(), model.info().caps.tool_family), ("antigravity", crate::ToolFamily::Gemini));
        assert!(f.1.list_models(Some("antigravity"), Some(AuthMode::Subscription)).iter().any(|m| m.id == "gemini-pro-agent"));
        assert!(f.1.list_models(Some("antigravity"), Some(AuthMode::ApiKey)).is_empty());
        let default = f.1.default_with(None, |p| (p == "antigravity").then_some(AuthMode::Subscription));
        assert_eq!(default, Some(("antigravity".into(), "gemini-3.1-pro-low".into())));
    }

    #[tokio::test]
    async fn xai_offers_both_sign_ins_and_resolves_subscription_models() {
        let f = Fixture::new();
        assert_eq!(f.1.login_modes("xai"), vec![AuthMode::ApiKey, AuthMode::Subscription]);
        assert!(f.1.has_device_login("xai") && std::ptr::eq(flows::spec("xai").unwrap(), &openai::xai::SPEC));
        let oauth = Credential::OAuth { access: "a".into(), refresh: "r".into(), expires_at_ms: 0, account_id: None, extra: serde_json::Value::Null };
        f.1.store.set("xai", oauth).unwrap();
        for id in openai::xai::SUBSCRIPTION_MODELS.iter().chain(&["grok-build"]) {
            let model = f.1.resolve_model("xai", id).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!((model.info().provider.as_str(), model.info().id.as_str()), ("xai", *id));
        }
        let listed: Vec<_> = f.1.list_models(Some("xai"), Some(AuthMode::Subscription)).into_iter().map(|m| m.id).collect();
        assert!(listed.iter().any(|id| id == "grok-build") && listed.iter().any(|id| id == "grok-4.6"), "{listed:?}");
        f.1.store.set("xai", Credential::ApiKey { key: "k".into() }).unwrap();
        assert!(f.1.resolve_model("xai", "grok-build").is_err(), "the proxy models need the sign-in");
    }

    /// A token endpoint answering every request with `body`.
    async fn token_server(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let mut buffer = [0; 8192];
                let _ = tcp.read(&mut buffer).await;
                let reply =
                    format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = tcp.write_all(reply.as_bytes()).await;
            }
        });
        url
    }

    #[tokio::test]
    async fn remote_lists_refresh_after_sign_in_and_merge_over_the_catalog() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut f = Fixture::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        f.1.remote_fetch = Arc::new(move |provider: &'static str, credential: Credential| -> futures::future::BoxFuture<'static, _> {
            counter.fetch_add(1, Ordering::SeqCst);
            assert!(provider == "kimi-code" && matches!(&credential, Credential::OAuth { access, .. } if access == "fresh"));
            let model = |id: &str| RemoteModel { id: id.into(), name: id.into(), context_window: 1000, ..RemoteModel::default() };
            Box::pin(async move { Ok(vec![model("kimi-k3"), model("kimi-new")]) })
        });
        // Before any list: the overlay's fallback ids.
        assert!(f.1.list_models(Some("kimi-code"), Some(AuthMode::Subscription)).iter().any(|m| m.id == "kimi-k3.1-pro"));
        let token = token_server(r#"{"access_token":"fresh","refresh_token":"r","expires_in":3600}"#).await;
        let spec: &'static SubscriptionSpec = Box::leak(Box::new(SubscriptionSpec {
            token: Box::leak(token.into_boxed_str()),
            headers: None,
            finish: None,
            callbacks: &[],
            ..clone_spec(&compat::kimi::SPEC)
        }));
        let login = flows::Login::begin(spec, &Callback::Paste { redirect: "https://example.invalid/cb", bare_code: true }, false).await.unwrap();
        let flow = LoginFlow { provider: "kimi-code".into(), url: None, user_code: None, state: LoginState::Pkce(login) };
        f.1.complete_login(flow, "the-code").await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "signing in fetches the account's list");
        let ids: Vec<_> = f.1.list_models(Some("kimi-code"), Some(AuthMode::Subscription)).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["kimi-k3", "kimi-new"], "the fetched list, in its order");
        assert_eq!(f.1.model_info("kimi-code", "kimi-k3").unwrap().caps.context_window, 1_048_576, "catalog metadata wins");
        assert_eq!(f.1.model_info("kimi-code", "kimi-new").unwrap().caps.context_window, 1000);
        assert!(f.0.join("catalog/remote-kimi-code.json").exists());
        assert!(!f.1.refresh_remote(false).await, "a fresh list is not due");
        assert!(Providers::new(f.0.join("auth"), f.0.clone()).unwrap().list_models(Some("kimi-code"), None).iter().any(|m| m.id == "kimi-new"));
        assert!(f.1.refresh_remote(true).await);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "only signed-in providers are fetched");
    }

    /// The fields a test spec keeps from `spec` (specs are not `Clone`).
    fn clone_spec(spec: &SubscriptionSpec) -> SubscriptionSpec {
        SubscriptionSpec {
            provider: spec.provider,
            api_key: spec.api_key,
            client_id: spec.client_id,
            client_secret: spec.client_secret,
            authorize: "https://example.invalid/authorize",
            console: None,
            token: spec.token,
            scopes: spec.scopes,
            params: spec.params,
            nonce: spec.nonce,
            callbacks: spec.callbacks,
            device: None,
            headers: spec.headers,
            exchange_encoding: spec.exchange_encoding,
            exchange_state: spec.exchange_state,
            refresh: spec.refresh,
            credential: spec.credential,
            finish: spec.finish,
            plan: spec.plan,
        }
    }
}
