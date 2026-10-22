//! Bundled models.dev metadata, locally cached refreshes, and auth policy.

use crate::{Caps, Effort, ModelInfo, Pricing};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "client")]
pub use crate::auth::AuthMode;
/// Auth metadata remains available in transcript-only builds.
#[cfg(not(feature = "client"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    ApiKey,
    Subscription,
}

pub const SOURCE: &str = "https://models.dev/api.json";
pub const SNAPSHOT: &str = include_str!("../../catalog/snapshot.json");
const OVERLAY: &str = include_str!("../../catalog/overlay.json");
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// models.dev providers otto has a transport for. Ollama and custom servers
/// are not in models.dev; their models come from the server itself.
pub const PROVIDERS: [&str; 10] = ["anthropic", "openai", "google", "xai", "openrouter", "deepseek", "moonshotai", "zai", "minimax", "lmstudio"];
/// A refreshed feed must contain at least these.
const REQUIRED: [&str; 4] = ["anthropic", "openai", "google", "xai"];
type Providers = BTreeMap<String, ModelsDevProvider>;

/// Source model fields are retained verbatim, including future fields and pricing tiers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelsDevModel {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelsDevProvider {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub doc: Option<String>,
    pub models: BTreeMap<String, ModelsDevModel>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
pub struct ProviderEntry {
    pub id: String,
    pub name: String,
    pub env: Vec<String>,
    pub doc: Option<String>,
    pub implemented: bool,
}

#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub info: ModelInfo,
    pub auth: Vec<AuthMode>,
    pub deprecated: bool,
    pub status: Option<String>,
    pub efforts: Vec<Effort>,
    pub release_date: Option<String>,
    pub knowledge: Option<String>,
    pub family: Option<String>,
    pub modalities_in: Vec<String>,
    pub list_pricing: Option<Pricing>,
    pub raw: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AuthPolicy {
    #[serde(default = "api_default")]
    default: Vec<String>,
    #[serde(default)]
    subscription_ids: Vec<String>,
    #[serde(default)]
    subscription_prefixes: Vec<String>,
    #[serde(default)]
    overrides: BTreeMap<String, Vec<String>>,
}
fn api_default() -> Vec<String> {
    vec!["api_key".into()]
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Overlay {
    auth: AuthPolicy,
    #[serde(default)]
    subscription_overrides: BTreeMap<String, Value>,
    #[serde(default)]
    defaults: BTreeMap<String, String>,
    #[serde(default)]
    manual: BTreeMap<String, ModelsDevModel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Cache {
    refreshed_at: u64,
    providers: Providers,
}

#[derive(Clone, Debug)]
pub struct Catalog {
    providers: Providers,
    overlay: BTreeMap<String, Overlay>,
    refreshed_at: Option<SystemTime>,
}

/// Trim a models.dev feed to supported providers and tool-using text chat models.
///
/// # Errors
/// Returns an error for malformed provider or model metadata.
pub fn trim_feed(feed: Value) -> Result<BTreeMap<String, ModelsDevProvider>, serde_json::Error> {
    let mut providers = BTreeMap::new();
    for id in PROVIDERS {
        let Some(value) = feed.get(id) else { continue };
        let mut provider: ModelsDevProvider = serde_json::from_value(value.clone())?;
        provider.id = id.into();
        provider.models.retain(|id, model| {
            model.id.clone_from(id);
            is_chat(model)
        });
        providers.insert(id.into(), provider);
    }
    Ok(providers)
}

fn is_chat(model: &ModelsDevModel) -> bool {
    let fields = &model.extra;
    let outputs = fields.get("modalities").and_then(|m| m.get("output")).and_then(Value::as_array);
    fields.get("tool_call").and_then(Value::as_bool) == Some(true)
        && fields.get("limit").and_then(|l| l.get("context")).and_then(Value::as_u64).unwrap_or_default() > 0
        && outputs.is_some_and(|items| items.len() == 1 && items[0] == "text")
        && !["embedding", "realtime", "-audio", "-tts", "-live-"].iter().any(|part| model.id.contains(part))
}

impl Catalog {
    /// Load without network access. `cache_dir` is the state root; the cache is `catalog/models.json` beneath it.
    /// Corrupt or unreadable caches silently fall back to the bundled snapshot.
    #[must_use]
    pub fn load(cache_dir: Option<&Path>) -> Self {
        let mut catalog = Self {
            providers: serde_json::from_str(SNAPSHOT).expect("valid bundled models.dev snapshot"),
            overlay: serde_json::from_str(OVERLAY).expect("valid bundled catalog policy"),
            refreshed_at: None,
        };
        if let Some(cache) = cache_dir
            .and_then(|dir| std::fs::read(dir.join("catalog/models.json")).ok())
            .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        {
            // Validate cached data with the same filter used by the updater.
            if let Ok(providers) = serde_json::to_value(cache.providers).and_then(trim_feed) {
                for (id, mut provider) in providers {
                    if let Some(mut existing) = catalog.providers.remove(&id) {
                        existing.models.extend(provider.models);
                        provider.models = existing.models;
                    }
                    catalog.providers.insert(id, provider);
                }
                catalog.refreshed_at = UNIX_EPOCH.checked_add(Duration::from_secs(cache.refreshed_at));
            }
        }
        for (id, overlay) in &catalog.overlay {
            if let Some(provider) = catalog.providers.get_mut(id) {
                provider.models.extend(overlay.manual.clone());
            }
        }
        catalog
    }

    #[must_use]
    pub fn global() -> &'static Self {
        static CATALOG: LazyLock<Catalog> = LazyLock::new(|| Catalog::load(None));
        &CATALOG
    }

    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        self.refreshed_at.and_then(|at| at.elapsed().ok()).is_none_or(|age| age >= MAX_AGE)
    }

    /// Fetch and atomically cache a trimmed feed. Does not mutate existing catalog instances.
    /// Reload with `load` after success; failures leave the previous cache intact.
    ///
    /// # Errors
    /// Returns network, validation, or filesystem errors; callers may ignore them and retain the loaded catalog.
    #[cfg(feature = "client")]
    pub async fn refresh(http: &reqwest::Client, cache_dir: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use std::io::Write;
        let feed = http.get(SOURCE).timeout(Duration::from_secs(60)).send().await?.error_for_status()?.json().await?;
        let providers = trim_feed(feed)?;
        if REQUIRED.iter().any(|id| providers.get(*id).is_none_or(|p| p.models.is_empty())) {
            return Err("models.dev returned an incomplete catalog".into());
        }
        let cache = Cache { refreshed_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(), providers };
        let bytes = serde_json::to_vec_pretty(&cache)?;
        let dir = cache_dir.join("catalog");
        std::fs::create_dir_all(&dir)?;
        let temp = dir.join(format!(".models-{}-{}.tmp", std::process::id(), rand::random::<u64>()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, dir.join("models.json"))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result?;
        Ok(())
    }

    #[must_use]
    pub fn providers(&self) -> Vec<ProviderEntry> {
        self.providers
            .values()
            .map(|p| ProviderEntry { id: p.id.clone(), name: p.name.clone(), env: p.env.clone(), doc: p.doc.clone(), implemented: true })
            .collect()
    }

    #[must_use]
    pub fn models(&self, provider: &str, mode: Option<AuthMode>) -> Vec<CatalogEntry> {
        let mut entries: Vec<_> =
            self.providers.get(provider).into_iter().flat_map(|p| p.models.keys()).filter_map(|id| self.entry(provider, id, mode)).collect();
        entries.sort_by(|a, b| {
            a.deprecated.cmp(&b.deprecated).then_with(|| b.release_date.cmp(&a.release_date)).then_with(|| a.info.id.cmp(&b.info.id))
        });
        entries
    }

    #[must_use]
    pub fn entry(&self, provider: &str, id: &str, mode: Option<AuthMode>) -> Option<CatalogEntry> {
        let model = self.providers.get(provider)?.models.get(id)?;
        let overlay = self.overlay.get(provider)?;
        let auth = model_auth(&overlay.auth, id);
        if auth.is_empty() || mode.is_some_and(|m| !auth.contains(&m)) {
            return None;
        }
        let raw = serde_json::to_value(model).ok()?;
        let list_pricing = pricing(&raw["cost"]);
        let mut effective = raw.clone();
        if mode == Some(AuthMode::Subscription) {
            effective["cost"] = serde_json::json!({"input": 0, "output": 0, "cache_read": 0, "cache_write": 0});
        }
        if mode == Some(AuthMode::Subscription)
            && let Some(overrides) = overlay.subscription_overrides.get(id)
        {
            merge(&mut effective, overrides);
        }
        let mut caps = base_caps(provider);
        let modalities_in = strings(&effective["modalities"]["input"]);
        caps.reasoning = effective["reasoning"].as_bool().unwrap_or_default();
        caps.reasoning_replay &= caps.reasoning;
        caps.vision = modalities_in.iter().any(|m| m == "image");
        caps.pdf = modalities_in.iter().any(|m| m == "pdf");
        caps.audio_in = modalities_in.iter().any(|m| m == "audio");
        caps.strict_tools &= effective["structured_output"].as_bool().unwrap_or_default();
        caps.context_window = effective["limit"]["context"].as_u64().unwrap_or_default();
        caps.max_output = effective["limit"]["output"].as_u64().unwrap_or_default();
        if provider == "anthropic" {
            caps.mid_system = crate::anthropic::supports_mid_system(id);
            caps.inline_tool_changes = caps.mid_system;
            caps.deferred_tools = !id.contains("haiku-4");
            caps.native_compaction = crate::anthropic::supports_compaction(id);
        }
        if provider == "openai" {
            openai_caps(&mut caps, id, mode);
        }
        if let Some(field) = raw["interleaved"]["field"].as_str() {
            // models.dev names the field reasoning must be replayed in.
            caps.reasoning_replay |= caps.reasoning && field == "reasoning_content";
        }
        let status = text(&raw["status"]);
        let efforts = raw["reasoning_options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|option| option["type"] == "effort")
            .flat_map(|option| strings(&option["values"]))
            .filter_map(|effort| serde_json::from_value(Value::String(effort)).ok())
            .collect();
        Some(CatalogEntry {
            info: ModelInfo { id: id.into(), name: model.name.clone(), provider: provider.into(), caps, pricing: pricing(&effective["cost"]) },
            auth,
            deprecated: status.as_deref() == Some("deprecated"),
            status,
            efforts,
            release_date: text(&raw["release_date"]),
            knowledge: text(&raw["knowledge"]),
            family: text(&raw["family"]),
            modalities_in,
            list_pricing,
            raw,
        })
    }

    #[must_use]
    pub fn info(&self, provider: &str, id: &str, mode: Option<AuthMode>) -> Option<ModelInfo> {
        self.entry(provider, id, mode).map(|entry| entry.info)
    }

    #[must_use]
    pub fn default_model(&self, provider: &str, mode: AuthMode) -> Option<String> {
        let id = self.overlay.get(provider)?.defaults.get(mode_name(mode))?;
        self.supports(provider, id, mode).then(|| id.clone())
    }

    #[must_use]
    pub fn supports(&self, provider: &str, id: &str, mode: AuthMode) -> bool {
        self.entry(provider, id, Some(mode)).is_some()
    }

    /// Unknown ids inherit the longest hyphen-delimited prefix match among models available in the mode.
    /// With no prefix match, use that mode's default, then its newest model; unknown providers get generic caps.
    /// This is an estimate, not an auth grant: known but disallowed ids return `None`.
    #[must_use]
    pub fn info_or_fallback(&self, provider: &str, id: &str, mode: Option<AuthMode>) -> Option<ModelInfo> {
        if self.providers.get(provider).is_some_and(|p| p.models.contains_key(id)) {
            return self.info(provider, id, mode);
        }
        let entries = self.models(provider, mode);
        let closest = entries
            .iter()
            .map(|e| {
                let score = e.info.id.split('-').zip(id.split('-')).take_while(|(a, b)| a == b).map(|(part, _)| part.len() + 1).sum::<usize>();
                (score, e)
            })
            .filter(|(score, _)| *score > 0)
            .max_by_key(|(score, _)| *score)
            .map(|(_, e)| e);
        let default = mode.and_then(|mode| self.default_model(provider, mode));
        let template = closest.or_else(|| entries.iter().find(|e| Some(&e.info.id) == default.as_ref())).or_else(|| entries.first());
        if template.is_none() && self.providers.contains_key(provider) {
            return None;
        }
        let mut info = template.map_or_else(
            || ModelInfo { id: id.into(), name: id.into(), provider: provider.into(), caps: Caps::default(), pricing: None },
            |entry| entry.info.clone(),
        );
        info.id = id.into();
        info.name = id.into();
        if provider == "openai" {
            openai_caps(&mut info.caps, id, mode);
        }
        Some(info)
    }
}

/// Provider-level capabilities before the per-model fields.
#[must_use]
pub fn base_caps(provider: &str) -> Caps {
    match provider {
        "anthropic" => crate::anthropic::base_caps(),
        "openai" => crate::openai::base_caps(),
        "xai" => crate::openai::xai_caps(),
        "google" => crate::google::base_caps(),
        id => crate::compat::dialect(id).map(crate::compat::base_caps).unwrap_or_default(),
    }
}
/// OpenAI features that depend on the model and the auth mode: steering and
/// async tools (GPT-6, API key), compaction (API key) and native tools.
fn openai_caps(caps: &mut Caps, id: &str, mode: Option<AuthMode>) {
    let api = mode != Some(AuthMode::Subscription);
    caps.steering = api && crate::openai::supports_steering(id);
    caps.async_tools = api && crate::openai::supports_async_tools(id);
    caps.native_compaction = api;
    caps.native_tools = if api { crate::openai::native_tools(id) } else { Vec::new() };
}

fn mode_name(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::ApiKey => "api_key",
        AuthMode::Subscription => "subscription",
    }
}
fn model_auth(policy: &AuthPolicy, id: &str) -> Vec<AuthMode> {
    let mut modes = policy.default.clone();
    if policy.subscription_ids.iter().any(|s| s == id) || policy.subscription_prefixes.iter().any(|p| id.starts_with(p)) {
        modes.push("subscription".into());
    }
    let modes = policy.overrides.get(id).unwrap_or(&modes);
    [AuthMode::ApiKey, AuthMode::Subscription].into_iter().filter(|mode| modes.iter().any(|s| s == mode_name(*mode))).collect()
}
fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}
fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(text).collect()
}
fn pricing(value: &Value) -> Option<Pricing> {
    Some(Pricing {
        input: value.get("input")?.as_f64()?,
        output: value.get("output")?.as_f64()?,
        cache_read: value["cache_read"].as_f64().unwrap_or_default(),
        cache_write: value["cache_write"].as_f64().unwrap_or_default(),
    })
}
fn merge(target: &mut Value, source: &Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            merge(target.entry(key).or_insert(Value::Null), value);
        }
    } else {
        *target = source.clone();
    }
}
