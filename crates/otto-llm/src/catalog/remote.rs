//! Model lists providers report for a signed-in subscription (Kimi Code `GET /models`,
//! Antigravity `fetchAvailableModels`), cached per provider at
//! `<state>/catalog/remote-<provider>.json` and refreshed every 4–6 h.
//! [`Catalog::merge_remote`] lays catalog metadata over them.

use super::{AuthMode, Catalog, CatalogEntry, base_caps};
use crate::{Caps, Effort, ErrorClass, LlmError, ModelInfo};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The shortest refresh interval; each list adds a jitter of up to [`JITTER`].
pub const MIN_AGE: Duration = Duration::from_secs(4 * 60 * 60);
pub const JITTER: Duration = Duration::from_secs(2 * 60 * 60);

/// One model of a provider's own list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RemoteModel {
    pub id: String,
    /// The display name, else the id.
    pub name: String,
    #[serde(default)]
    pub context_window: u64,
    /// The output token limit; 0 when the provider doesn't say.
    #[serde(default)]
    pub max_output: u64,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub image_input: bool,
    /// Empty when the model takes no effort.
    #[serde(default)]
    pub efforts: Vec<Effort>,
}

impl RemoteModel {
    /// The entry of a model only the provider's list knows (subscription-only, no pricing).
    #[must_use]
    pub fn entry(&self, provider: &str) -> CatalogEntry {
        let info = match provider {
            crate::compat::kimi::ID => self.info(),
            #[cfg(feature = "client")]
            crate::google::codeassist::convert::PROVIDER => crate::google::codeassist::info(self),
            _ => ModelInfo {
                id: self.id.clone(),
                provider: provider.into(),
                name: self.name.clone(),
                caps: Caps {
                    reasoning: self.reasoning,
                    vision: self.image_input,
                    context_window: self.context_window,
                    max_output: self.max_output,
                    ..base_caps(provider)
                },
                pricing: None,
                list_pricing: None,
            },
        };
        CatalogEntry {
            info,
            auth: vec![AuthMode::Subscription],
            deprecated: false,
            status: None,
            efforts: self.efforts.clone(),
            release_date: None,
            knowledge: None,
            family: None,
            modalities_in: if self.image_input { vec!["text".into(), "image".into()] } else { vec!["text".into()] },
            list_pricing: None,
            raw: Value::Null,
        }
    }
}

/// A fetched list as saved on disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteList {
    /// Unix seconds.
    pub fetched_at: u64,
    pub models: Vec<RemoteModel>,
}

/// The per-provider lists of one state root, shared by clones.
#[derive(Clone, Debug, Default)]
pub struct RemoteStore {
    dir: PathBuf,
    lists: Arc<RwLock<BTreeMap<String, RemoteList>>>,
}

fn secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// How long a list fetched at `fetched_at` stays fresh: [`MIN_AGE`] plus a jitter
/// stable for that fetch, so providers and machines don't refresh in lockstep.
#[must_use]
pub fn max_age(provider: &str, fetched_at: u64) -> Duration {
    let mut hasher = DefaultHasher::new();
    (provider, fetched_at).hash(&mut hasher);
    MIN_AGE + Duration::from_secs(hasher.finish() % (JITTER.as_secs() + 1))
}

impl RemoteStore {
    /// Read every saved list under `<state_dir>/catalog/`; unreadable files are skipped.
    #[must_use]
    pub fn load(state_dir: &Path) -> Self {
        let dir = state_dir.join("catalog");
        let lists = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|file| {
                let name = file.file_name().into_string().ok()?;
                let provider = name.strip_prefix("remote-")?.strip_suffix(".json")?.to_owned();
                let list = serde_json::from_slice(&std::fs::read(file.path()).ok()?).ok()?;
                Some((provider, list))
            })
            .collect();
        Self { dir, lists: Arc::new(RwLock::new(lists)) }
    }

    #[must_use]
    pub fn get(&self, provider: &str) -> Option<RemoteList> {
        self.lists.read().unwrap_or_else(|e| e.into_inner()).get(provider).cloned()
    }

    /// Whether `provider`'s list was never fetched or is older than its [`max_age`].
    #[must_use]
    pub fn due(&self, provider: &str, now: SystemTime) -> bool {
        self.get(provider).is_none_or(|list| secs(now).saturating_sub(list.fetched_at) >= max_age(provider, list.fetched_at).as_secs())
    }

    /// Write `list` atomically, then serve it.
    ///
    /// # Errors
    /// Filesystem errors; the previous list stays in place.
    pub fn save(&self, provider: &str, list: RemoteList) -> Result<(), LlmError> {
        let io = |e: std::io::Error| LlmError::new(ErrorClass::Other, format!("saving the {provider} model list: {e}"));
        std::fs::create_dir_all(&self.dir).map_err(io)?;
        let temp = self.dir.join(format!(
            ".remote-{provider}-{}-{}.tmp",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
        ));
        let bytes = serde_json::to_vec_pretty(&list).map_err(|e| LlmError::new(ErrorClass::Other, e.to_string()))?;
        let result = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, self.dir.join(format!("remote-{provider}.json"))));
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(io)?;
        self.lists.write().unwrap_or_else(|e| e.into_inner()).insert(provider.into(), list);
        Ok(())
    }

    /// Await `fetch` and save its list as fetched at `now`. An empty list is an error and
    /// keeps the previous one.
    ///
    /// # Errors
    /// The fetch's error, an empty list, or a filesystem error.
    pub async fn refresh(
        &self,
        provider: &str,
        now: SystemTime,
        fetch: impl Future<Output = Result<Vec<RemoteModel>, LlmError>>,
    ) -> Result<(), LlmError> {
        let models = fetch.await?;
        if models.is_empty() {
            return Err(LlmError::new(ErrorClass::Other, format!("{provider} returned no models")));
        }
        self.save(provider, RemoteList { fetched_at: secs(now), models })
    }

    /// [`Catalog::merge_remote`] over `provider`'s saved list.
    #[must_use]
    pub fn models(&self, catalog: &Catalog, provider: &str, mode: Option<AuthMode>) -> Vec<CatalogEntry> {
        let list = self.get(provider);
        catalog.merge_remote(provider, list.as_ref().map(|l| l.models.as_slice()), mode)
    }

    /// One model of [`RemoteStore::models`].
    #[must_use]
    pub fn info(&self, catalog: &Catalog, provider: &str, id: &str, mode: Option<AuthMode>) -> Option<ModelInfo> {
        self.models(catalog, provider, mode).into_iter().find(|e| e.info.id == id).map(|e| e.info)
    }
}

impl Catalog {
    /// A subscription provider's models: its fetched list (`remote`) in its own order, else the
    /// overlay's `subscription_ids`. Catalog metadata wins for ids the catalog knows in `mode`;
    /// the rest come from the list's own fields. Fallback ids the catalog lacks are dropped.
    #[must_use]
    pub fn merge_remote(&self, provider: &str, remote: Option<&[RemoteModel]>, mode: Option<AuthMode>) -> Vec<CatalogEntry> {
        let Some(remote) = remote else {
            let ids = self.overlay.get(provider).map(|o| o.auth.subscription_ids.as_slice()).unwrap_or_default();
            return ids.iter().filter_map(|id| self.entry(provider, id, mode)).collect();
        };
        remote.iter().map(|m| self.entry(provider, &m.id, mode).unwrap_or_else(|| m.entry(provider))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{JITTER, MIN_AGE, RemoteList, RemoteModel, RemoteStore, max_age};
    use crate::catalog::{AuthMode, Catalog};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn model(id: &str) -> RemoteModel {
        RemoteModel {
            id: id.into(),
            name: format!("Remote {id}"),
            context_window: 200_000,
            max_output: 32_000,
            reasoning: true,
            ..RemoteModel::default()
        }
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("otto-remote-{name}-{}", std::process::id()))
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp("roundtrip");
        let store = RemoteStore::load(&dir);
        assert!(store.get("kimi-code").is_none());
        let list = RemoteList { fetched_at: 1_700_000_000, models: vec![model("kimi-k3"), model("kimi-for-coding")] };
        store.save("kimi-code", list.clone()).unwrap();
        assert_eq!(store.get("kimi-code"), Some(list.clone()));
        assert!(dir.join("catalog/remote-kimi-code.json").is_file());
        std::fs::write(dir.join("catalog/remote-broken.json"), "{").unwrap();
        let loaded = RemoteStore::load(&dir);
        assert_eq!(loaded.get("kimi-code"), Some(list));
        assert!(loaded.get("broken").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn due_after_a_jittered_four_to_six_hours() {
        for fetched in [0, 1_700_000_000, 1_700_000_123, 1_800_000_000] {
            for provider in ["kimi-code", "antigravity", "xai"] {
                let age = max_age(provider, fetched);
                assert!(age >= MIN_AGE && age <= MIN_AGE + JITTER, "{provider} {fetched}: {age:?}");
                assert_eq!(age, max_age(provider, fetched));
            }
        }
        let dir = temp("due");
        let store = RemoteStore::load(&dir);
        let fetched = 1_700_000_000;
        assert!(store.due("kimi-code", at(fetched)));
        store.save("kimi-code", RemoteList { fetched_at: fetched, models: vec![model("kimi-k3")] }).unwrap();
        let age = max_age("kimi-code", fetched).as_secs();
        assert!(!store.due("kimi-code", at(fetched)));
        assert!(!store.due("kimi-code", at(fetched + MIN_AGE.as_secs() - 1)));
        assert!(!store.due("kimi-code", at(fetched + age - 1)));
        assert!(store.due("kimi-code", at(fetched + age)));
        assert!(store.due("kimi-code", at(fetched + (MIN_AGE + JITTER).as_secs())));
        assert!(store.due("antigravity", at(fetched)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_saves_and_rejects_empty_lists() {
        let dir = temp("refresh");
        let store = RemoteStore::load(&dir);
        fn run<F: std::future::Future>(fut: F) -> F::Output {
            tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(fut)
        }
        run(store.refresh("kimi-code", at(100), async { Ok(vec![model("kimi-k3")]) })).unwrap();
        assert!(run(store.refresh("kimi-code", at(200), async { Ok(Vec::new()) })).is_err());
        assert_eq!(store.get("kimi-code"), Some(RemoteList { fetched_at: 100, models: vec![model("kimi-k3")] }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_metadata_wins_and_remote_only_ids_are_added() {
        let catalog = Catalog::load(None);
        let mode = Some(AuthMode::Subscription);
        let known = catalog.entry("openai", "gpt-5.6", mode).unwrap();
        let entries = catalog.merge_remote("openai", Some(&[model("gpt-5.6"), model("gpt-remote-only")]), mode);
        let ids: Vec<_> = entries.iter().map(|e| e.info.id.as_str()).collect();
        assert_eq!(ids, ["gpt-5.6", "gpt-remote-only"]);
        assert_eq!(entries[0].info, known.info);
        assert_eq!((entries[0].info.name.as_str(), entries[0].info.caps.context_window), ("GPT-5.6", 1_050_000));
        let added = &entries[1];
        assert_eq!((added.info.name.as_str(), added.info.provider.as_str()), ("Remote gpt-remote-only", "openai"));
        assert!(added.info.caps.reasoning && added.info.caps.context_window == 200_000 && added.info.caps.max_output == 32_000);
        assert_eq!(added.auth, [AuthMode::Subscription]);
        // A remote-only Kimi id gets the generic limits; the seeded kimi-k3 keeps its catalog ones.
        let kimi = catalog.merge_remote("kimi-code", Some(&[model("kimi-k3"), model("kimi-remote")]), mode);
        assert_eq!((kimi[0].info.provider.as_str(), kimi[0].info.caps.max_output), ("kimi-code", 131_072));
        assert_eq!((kimi[1].info.provider.as_str(), kimi[1].info.caps.max_output), ("kimi-code", 32_000));
    }

    #[test]
    fn subscription_ids_are_the_fallback() {
        let catalog = Catalog::load(None);
        let mode = Some(AuthMode::Subscription);
        let ids: Vec<_> = catalog.merge_remote("xai", None, mode).into_iter().map(|e| e.info.id).collect();
        assert_eq!(ids, ["grok-4.5", "grok-4.6", "grok-4.7", "grok-composer-2.5-fast", "grok-build"]);
        let kimi: Vec<_> = catalog.merge_remote("kimi-code", None, mode).into_iter().map(|e| e.info.id).collect();
        assert_eq!(kimi, ["kimi-k3", "kimi-k3.1-pro", "kimi-k2.7-code-highspeed"]);
        let store = RemoteStore::load(&temp("fallback"));
        let from_store: Vec<_> = store.models(&catalog, "xai", mode).into_iter().map(|e| e.info.id).collect();
        assert_eq!(from_store, ids);
        assert_eq!(store.info(&catalog, "xai", "grok-4.6", mode).map(|i| i.id), Some("grok-4.6".into()));
    }
}
