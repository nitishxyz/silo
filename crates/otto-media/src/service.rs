//! [`Media`]: the providers, their keys (docs/MEDIA.md §2.1), each kind's default (`<state>/media.json`) and the
//! live catalogs (§3.2: memory, then `<state>/media-catalog/<provider>.json`, refreshed after 6 h).
use crate::endpoints::{self, Endpoint, Live};
use crate::models::Model;
use crate::{Call, MediaError, MediaKeySource, MediaKind, MediaProvider};
use otto_llm::auth::{Credential, CredentialStore};
use otto_proto::{MediaChoice, MediaProviderInfo, MediaStatus};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// How long a live catalog is kept.
const CATALOG_TTL: u64 = 6 * 3600;
/// The least time between tries to fetch a catalog that couldn't be read.
const RETRY: Duration = Duration::from_secs(60);

pub struct Media {
    store: CredentialStore,
    state_dir: PathBuf,
    pub(crate) http: reqwest::Client,
    providers: Vec<Arc<dyn MediaProvider>>,
    env: Env,
    /// Live catalogs by provider (loaded from disk on first use).
    live: Mutex<HashMap<String, Live>>,
    /// When each provider's catalog was last tried.
    tried: Mutex<HashMap<String, Instant>>,
    writing: Mutex<()>,
}

impl Media {
    /// Every provider, keys from `store` and the environment, defaults in `state_dir`.
    #[must_use]
    pub fn new(store: CredentialStore, state_dir: PathBuf) -> Arc<Self> {
        Self::with(store, state_dir, crate::providers::all(), Arc::new(|name: &str| std::env::var(name).ok()))
    }

    /// `providers` (tests point them at fake servers) and `env` for environment variables.
    #[must_use]
    pub fn with(store: CredentialStore, state_dir: PathBuf, providers: Vec<Arc<dyn MediaProvider>>, env: Env) -> Arc<Self> {
        let (live, tried) = (Mutex::default(), Mutex::default());
        Arc::new(Self { store, state_dir, http: crate::http::client(), providers, env, live, tried, writing: Mutex::default() })
    }

    /// The media tools: `generate_image`, `generate_video`, `generate_audio`, `list_media_models`,
    /// `media_model_schema` and `generate_media`.
    #[must_use]
    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn otto_tools::Tool>> {
        crate::tools::all(self)
    }

    #[must_use]
    pub fn providers(&self) -> &[Arc<dyn MediaProvider>] {
        &self.providers
    }

    /// A provider by id or name, any case.
    #[must_use]
    pub fn provider(&self, id: &str) -> Option<Arc<dyn MediaProvider>> {
        let id = id.trim();
        self.providers.iter().find(|p| p.id().eq_ignore_ascii_case(id) || p.name().eq_ignore_ascii_case(id)).cloned()
    }

    fn known(&self, id: &str) -> Result<Arc<dyn MediaProvider>, MediaError> {
        self.provider(id).ok_or_else(|| MediaError::invalid(format!("unknown media provider {id}")))
    }

    /// A provider's key and where it comes from: its media key, its chat API key, the environment.
    #[must_use]
    pub fn key(&self, provider: &dyn MediaProvider) -> Option<(String, MediaKeySource)> {
        let api_key = |id: &str| match self.store.get(id) {
            Ok(Some(Credential::ApiKey { key })) if !key.trim().is_empty() => Some(key),
            _ => None,
        };
        api_key(&format!("media:{}", provider.id()))
            .map(|k| (k, MediaKeySource::Media))
            .or_else(|| provider.shares_chat_key().then(|| api_key(provider.id())).flatten().map(|k| (k, MediaKeySource::Chat)))
            .or_else(|| provider.env_key(&*self.env).map(|k| (k, MediaKeySource::Env)))
    }

    /// Every provider's models with their `@` handles (docs/MEDIA.md §3.0.1), live families after the built-in ones.
    pub(crate) fn catalog(&self) -> Vec<Model> {
        crate::models::build(&self.providers, &|id| self.live(id).map(|l| l.endpoints).unwrap_or_default())
    }

    fn info_with(&self, provider: &dyn MediaProvider, catalog: &[Model]) -> MediaProviderInfo {
        let source = self.key(provider).map(|(_, source)| source);
        MediaProviderInfo {
            id: provider.id().into(),
            name: provider.name().into(),
            kinds: provider.kinds().to_vec(),
            connected: source.is_some(),
            source,
            models: catalog.iter().filter(|m| m.provider.id() == provider.id()).map(Model::wire).collect(),
        }
    }

    #[must_use]
    pub fn info(&self, provider: &dyn MediaProvider) -> MediaProviderInfo {
        self.info_with(provider, &self.catalog())
    }

    #[must_use]
    pub fn status(&self) -> MediaStatus {
        let catalog = self.catalog();
        MediaStatus { providers: self.providers.iter().map(|p| self.info_with(p.as_ref(), &catalog)).collect(), defaults: self.defaults() }
    }

    /// The model an `@` handle names (any case): provider id and name, model id and name, kind.
    #[must_use]
    pub fn resolve_handle(&self, handle: &str) -> Option<(String, String, String, String, MediaKind)> {
        let handle = handle.trim();
        let m = self.catalog().into_iter().find(|m| m.handle.eq_ignore_ascii_case(handle))?;
        Some((m.provider.id().into(), m.provider.name().into(), m.id, m.name, m.kind))
    }

    /// Store `provider`'s media key; `None` or empty removes it.
    ///
    /// # Errors
    /// Unknown provider, a malformed key, or the credential store can't be written.
    pub fn set_key(&self, provider: &str, key: Option<String>) -> Result<MediaProviderInfo, MediaError> {
        let provider = self.known(provider)?;
        let id = format!("media:{}", provider.id());
        let stored = match key.map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()) {
            Some(key) => {
                provider.check_key(&key)?;
                self.store.set(&id, Credential::ApiKey { key })
            }
            None => self.store.remove(&id),
        };
        stored.map_err(|e| MediaError::failed(format!("can't save the key: {e}")))?;
        // A new key may open its live catalog.
        if let Ok(mut tried) = self.tried.lock() {
            tried.remove(provider.id());
        }
        Ok(self.info(provider.as_ref()))
    }

    /// Each kind's chosen provider and model.
    #[must_use]
    pub fn defaults(&self) -> BTreeMap<MediaKind, MediaChoice> {
        std::fs::read(self.state_dir.join("media.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| serde_json::from_value(v["defaults"].clone()).ok())
            .unwrap_or_default()
    }

    /// `kind`'s default: `provider` (and `model`), or automatic without one.
    ///
    /// # Errors
    /// Unknown provider, one that can't make `kind`, a model without a provider, or the file can't be written.
    pub fn set_default(&self, kind: MediaKind, provider: Option<String>, model: Option<String>) -> Result<MediaStatus, MediaError> {
        let provider = provider.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty());
        let model = model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty());
        let choice = match provider {
            Some(id) => {
                let provider = self.known(&id)?;
                if !provider.kinds().contains(&kind) {
                    return Err(MediaError::invalid(format!("{} can't make {}", provider.name(), kind_name(kind))));
                }
                MediaChoice { provider: Some(provider.id().into()), model }
            }
            None if model.is_some() => return Err(MediaError::invalid("a default model needs its provider")),
            None => MediaChoice::default(),
        };
        let _writing = self.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut defaults = self.defaults();
        if choice == MediaChoice::default() {
            defaults.remove(&kind);
        } else {
            defaults.insert(kind, choice);
        }
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&self.state_dir)?;
            let tmp = self.state_dir.join(".media.json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&serde_json::json!({"defaults": defaults}))?)?;
            std::fs::rename(tmp, self.state_dir.join("media.json"))
        };
        write().map_err(|e| MediaError::failed(format!("can't save media defaults: {e}")))?;
        Ok(self.status())
    }

    /// `provider`'s cached live catalog, from memory or its file.
    pub(crate) fn live(&self, provider: &str) -> Option<Live> {
        let mut live = self.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !live.contains_key(provider) {
            live.insert(provider.to_owned(), endpoints::load(&self.state_dir, provider)?);
        }
        live.get(provider).cloned()
    }

    fn keep(&self, provider: &str, catalog: Live) {
        if let Err(e) = endpoints::save(&self.state_dir, provider, &catalog) {
            tracing::warn!("can't save the {provider} media catalog: {e}");
        }
        self.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(provider.to_owned(), catalog);
    }

    /// Fetch each live catalog older than 6 h (every one with `force`), all at once; one that can't be read keeps
    /// what was cached and isn't tried again for a minute.
    pub async fn refresh_catalogs(&self, force: bool) {
        self.refresh(None, force).await;
    }

    /// [`Self::refresh_catalogs`] for `only` one provider, or all.
    pub(crate) async fn refresh(&self, only: Option<&str>, force: bool) {
        let stale = |p: &Arc<dyn MediaProvider>| {
            let fresh = self.live(p.id()).is_some_and(|l| endpoints::now().saturating_sub(l.fetched_at) < CATALOG_TTL);
            let tried = self.tried.lock().ok().and_then(|t| t.get(p.id()).copied()).is_some_and(|at| at.elapsed() < RETRY);
            only.is_none_or(|o| o == p.id()) && (force || (!fresh && !tried))
        };
        let due: Vec<Arc<dyn MediaProvider>> = self.providers.iter().filter(|p| stale(p)).cloned().collect();
        let fetched = futures::future::join_all(due.iter().map(|p| async {
            if let Ok(mut tried) = self.tried.lock() {
                tried.insert(p.id().to_owned(), Instant::now());
            }
            let key = self.key(p.as_ref()).map(|(k, _)| k);
            tokio::time::timeout(Duration::from_secs(30), p.endpoints(&self.http, key.as_deref())).await.ok().flatten()
        }))
        .await;
        for (provider, endpoints) in due.iter().zip(fetched) {
            let Some(mut endpoints) = endpoints else { continue };
            // Schemas fetched one at a time (fal) outlive a refresh of the listing.
            let old = self.live(provider.id()).map(|l| l.endpoints).unwrap_or_default();
            for e in endpoints.iter_mut().filter(|e| e.schema.is_none()) {
                e.schema = old.iter().find(|o| o.id == e.id).and_then(|o| o.schema.clone());
            }
            self.keep(provider.id(), Live { fetched_at: endpoints::now(), endpoints });
        }
    }

    /// `endpoint` (fetched on demand) into `provider`'s live catalog, replacing its old entry.
    pub(crate) fn put_endpoint(&self, provider: &str, endpoint: Endpoint) {
        let mut catalog = self.live(provider).unwrap_or_default();
        match catalog.endpoints.iter_mut().find(|e| e.id == endpoint.id) {
            Some(old) => *old = endpoint,
            None => catalog.endpoints.push(endpoint),
        }
        self.keep(provider, catalog);
    }

    pub(crate) fn call(&self, cancel: tokio_util::sync::CancellationToken, progress: Arc<dyn Fn(String) + Send + Sync>) -> Call {
        Call { http: self.http.clone(), cancel, progress }
    }
}

/// `images`, `videos`, `audio`.
fn kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "images",
        MediaKind::Video => "videos",
        MediaKind::Audio => "audio",
    }
}
