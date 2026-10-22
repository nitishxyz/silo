use otto_llm::{
    ToolFamily,
    catalog::{AuthMode, Catalog, ModelsDevProvider, SNAPSHOT, trim_feed},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "otto-catalog-{}-{}-{}",
            std::process::id(),
            now(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("catalog")).unwrap();
        Self(path)
    }
    fn cache(&self, providers: Value, refreshed_at: u64) {
        std::fs::write(
            self.0.join("catalog/models.json"),
            serde_json::to_vec(&json!({"refreshed_at": refreshed_at, "providers": providers})).unwrap(),
        )
        .unwrap();
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}
fn snapshot() -> Value {
    serde_json::from_str(SNAPSHOT).unwrap()
}

#[test]
fn snapshot_and_wrappers() {
    let parsed: std::collections::BTreeMap<String, ModelsDevProvider> = serde_json::from_str(SNAPSHOT).unwrap();
    assert_eq!(parsed.len(), otto_llm::catalog::PROVIDERS.len());
    let catalog = Catalog::global();
    let providers = catalog.providers();
    assert_eq!(providers.iter().filter(|p| p.implemented).count(), otto_llm::catalog::PROVIDERS.len());
    for id in otto_llm::catalog::PROVIDERS {
        assert!(!catalog.models(id, None).is_empty());
    }
    assert!(catalog.info("anthropic", "claude-sonnet-5-5", None).is_some());
    assert!(catalog.info("openai", "gpt-6.1-sol", None).is_some());
    assert!(!otto_llm::anthropic::models().is_empty());
    assert!(!otto_llm::openai::models().is_empty());
    assert!(otto_llm::anthropic::models().iter().all(|m| m.caps.tool_family == ToolFamily::Anthropic));
    assert!(otto_llm::openai::models().iter().all(|m| m.caps.tool_family == ToolFamily::Openai));
    assert!(otto_llm::google::models().iter().all(|m| m.caps.tool_family == ToolFamily::Gemini && m.caps.reasoning_replay == m.caps.reasoning));
    assert!(catalog.models("deepseek", None).iter().all(|e| e.info.caps.tool_family == ToolFamily::Generic && e.info.caps.prompt_cache));
    assert_eq!(otto_llm::anthropic::model_info("claude-opus-4-7"), catalog.info("anthropic", "claude-opus-4-7", None));
    assert_eq!(otto_llm::openai::model_info("gpt-4o"), catalog.info("openai", "gpt-4o", None));
}

#[test]
fn native_client_tools_follow_model_and_auth() {
    let catalog = Catalog::global();
    let native = |id: &str, mode| catalog.info("openai", id, Some(mode)).unwrap_or_else(|| panic!("{id}")).caps.native_tools;
    assert_eq!(native("gpt-5.5", AuthMode::ApiKey), ["apply_patch", "shell"]);
    assert_eq!(native("gpt-6-astra", AuthMode::ApiKey), ["apply_patch", "shell"]);
    assert_eq!(native("gpt-5.1", AuthMode::ApiKey), ["apply_patch"]);
    assert_eq!(native("gpt-5.3-codex", AuthMode::ApiKey), ["shell"]);
    assert!(native("gpt-5", AuthMode::ApiKey).is_empty());
    assert!(native("gpt-4.1", AuthMode::ApiKey).is_empty());
    assert!(native("o3", AuthMode::ApiKey).is_empty());
    // The ChatGPT/Codex endpoint gets the built-ins.
    assert!(native("gpt-6-astra", AuthMode::Subscription).is_empty());
    assert_eq!(otto_llm::openai::native_tools("gpt-5.1-2025-11-13"), ["apply_patch"]);
    assert!(otto_llm::anthropic::models().iter().all(|m| m.caps.native_tools == ["text_editor_20250728", "bash_20250124"]));
    assert!(catalog.models("deepseek", None).iter().all(|e| e.info.caps.native_tools.is_empty()));
}

#[test]
fn trims_non_chat_but_keeps_unknown_fields_and_deprecated() {
    let mut feed = snapshot();
    let template = feed["openai"]["models"]["gpt-5.5"].clone();
    for id in ["image", "audio", "video", "embedding", "realtime", "zero", "no-tools", "deprecated"] {
        let mut model = template.clone();
        model["id"] = json!(id);
        match id {
            "image" | "audio" | "video" => model["modalities"]["output"] = json!(["text", id]),
            "zero" => model["limit"]["context"] = json!(0),
            "no-tools" => model["tool_call"] = json!(false),
            "deprecated" => {
                model["status"] = json!("deprecated");
                model["future_field"] = json!({"kept": true});
            }
            _ => {}
        }
        feed["openai"]["models"][id] = model;
    }
    let trimmed = trim_feed(feed).unwrap();
    let models = &trimmed["openai"].models;
    for id in ["image", "audio", "video", "embedding", "realtime", "zero", "no-tools"] {
        assert!(!models.contains_key(id), "{id}");
    }
    assert_eq!(models["deprecated"].extra["future_field"]["kept"], true);
    assert_eq!(models["deprecated"].extra["status"], "deprecated");
    assert!(models["gpt-5.5"].extra["cost"]["tiers"].is_array());
    let value = serde_json::to_value(&trimmed).unwrap();
    assert_eq!(value["openai"]["models"]["gpt-5.5"], template);
}

#[test]
fn auth_and_subscription_limits() {
    let catalog = Catalog::global();
    for mode in [AuthMode::ApiKey, AuthMode::Subscription] {
        assert!(catalog.supports("anthropic", "claude-opus-4-7", mode));
        for id in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"] {
            assert!(catalog.supports("anthropic", id, mode));
        }
    }
    assert!(catalog.supports("openai", "gpt-4o", AuthMode::ApiKey));
    assert!(!catalog.supports("openai", "gpt-4o", AuthMode::Subscription));
    assert!(!catalog.supports("openai", "gpt-5.3-codex-spark", AuthMode::Subscription));
    for id in ["gpt-6-astra", "gpt-6-sol", "gpt-6-luna", "gpt-6.1-sol"] {
        assert!(catalog.supports("openai", id, AuthMode::Subscription));
    }
    assert!(catalog.models("google", Some(AuthMode::Subscription)).is_empty());
    assert_eq!(catalog.models("xai", Some(AuthMode::Subscription)).len(), 3);
    assert!(catalog.supports("xai", "grok-composer-2.5-fast", AuthMode::Subscription));
    assert!(!catalog.supports("xai", "grok-composer-2.5-fast", AuthMode::ApiKey));
    assert!(catalog.entry("xai", "grok-build", None).is_none());
    for id in ["gpt-5.2", "gpt-5.3-codex", "gpt-5.4", "gpt-5.4-mini", "gpt-5.5"] {
        assert!(!catalog.supports("openai", id, AuthMode::Subscription));
        assert!(catalog.supports("openai", id, AuthMode::ApiKey));
    }
    let entry = catalog.entry("openai", "gpt-6.1-sol", Some(AuthMode::Subscription)).unwrap();
    assert_eq!(entry.info.pricing.unwrap().input, 0.0);
    assert!(entry.list_pricing.unwrap().input > 0.0);
    assert_eq!(entry.raw["limit"]["context"], entry.info.caps.context_window);
    assert_eq!(catalog.info("openai", "gpt-5.5", Some(AuthMode::ApiKey)).unwrap().caps.context_window, 1_050_000);
}

#[test]
fn defaults_are_available() {
    let catalog = Catalog::global();
    for provider in ["anthropic", "openai"] {
        for mode in [AuthMode::ApiKey, AuthMode::Subscription] {
            let id = catalog.default_model(provider, mode).unwrap();
            assert!(catalog.supports(provider, &id, mode));
        }
    }
    for provider in ["google", "xai", "openrouter", "deepseek", "moonshotai", "zai", "minimax"] {
        let id = catalog.default_model(provider, AuthMode::ApiKey).unwrap_or_else(|| panic!("{provider} default"));
        assert!(catalog.supports(provider, &id, AuthMode::ApiKey), "{provider}/{id}");
    }
    assert!(catalog.default_model("lmstudio", AuthMode::ApiKey).is_none());
}

#[test]
fn provider_specific_caps() {
    let catalog = Catalog::global();
    let api = catalog.info("openai", "gpt-6.1-sol", Some(AuthMode::ApiKey)).unwrap().caps;
    assert!(api.steering && api.async_tools && api.native_compaction);
    let sub = catalog.info("openai", "gpt-6.1-sol", Some(AuthMode::Subscription)).unwrap().caps;
    assert!(!sub.steering && !sub.async_tools && !sub.native_compaction);
    assert!(!catalog.info("openai", "gpt-5.5", Some(AuthMode::ApiKey)).unwrap().caps.async_tools);
    assert!(catalog.info("anthropic", "claude-sonnet-5-5", None).unwrap().caps.native_compaction);
    assert!(!catalog.info("anthropic", "claude-haiku-4-5", None).is_some_and(|i| i.caps.native_compaction));
    let xai = catalog.info("xai", "grok-4.6", Some(AuthMode::ApiKey)).unwrap().caps;
    assert!(xai.reasoning_replay && !xai.custom_tools && !xai.steering && xai.hosted_tools.contains(&"x_search".to_owned()));
    // models.dev marks DeepSeek's interleaved reasoning field; it must be replayed.
    assert!(catalog.info("deepseek", "deepseek-v4-pro", None).unwrap().caps.reasoning_replay);
}

#[test]
fn cache_precedence_overlay_and_staleness() {
    let dir = TempDir::new();
    assert!(Catalog::load(Some(&dir.0)).needs_refresh());
    let mut feed = snapshot();
    feed["openai"]["models"]["gpt-5.5"]["cost"]["input"] = json!(123.0);
    feed["openai"]["models"]["gpt-5.5"]["limit"]["context"] = json!(999_999);
    let mut model = feed["openai"]["models"]["gpt-5.5"].clone();
    model["id"] = json!("gpt-future");
    feed["openai"]["models"]["gpt-future"] = model;
    dir.cache(feed.clone(), now());
    let catalog = Catalog::load(Some(&dir.0));
    assert!(!catalog.needs_refresh());
    assert!(catalog.info("openai", "gpt-future", None).is_some());
    assert!(!catalog.supports("openai", "gpt-future", AuthMode::Subscription));
    assert_eq!(catalog.info("openai", "gpt-5.5", Some(AuthMode::ApiKey)).unwrap().pricing.unwrap().input, 123.0);
    assert!(catalog.entry("openai", "gpt-5.5", Some(AuthMode::Subscription)).is_none());
    assert_eq!(catalog.info("openai", "gpt-5.5", Some(AuthMode::ApiKey)).unwrap().caps.context_window, 999_999);
    dir.cache(feed.clone(), now() - 86_401);
    assert!(Catalog::load(Some(&dir.0)).needs_refresh());
    dir.cache(feed, now() + 86_400);
    assert!(Catalog::load(Some(&dir.0)).needs_refresh());
}

#[test]
fn cache_merges_per_id_and_corrupt_cache_falls_back() {
    let dir = TempDir::new();
    let mut feed = snapshot();
    feed["openai"]["models"].as_object_mut().unwrap().retain(|id, _| id == "gpt-5.5");
    let mut hidden = feed["xai"]["models"]["grok-4.6"].clone();
    hidden["id"] = json!("grok-build");
    hidden["auth"] = json!(["api_key", "subscription"]);
    feed["xai"]["models"]["grok-build"] = hidden.clone();
    hidden["id"] = json!("grok-composer-2.5-fast");
    hidden["limit"]["context"] = json!(12345);
    feed["xai"]["models"]["grok-composer-2.5-fast"] = hidden;
    dir.cache(feed, now());
    let catalog = Catalog::load(Some(&dir.0));
    assert!(catalog.info("openai", "gpt-4o", None).is_some());
    assert!(catalog.entry("xai", "grok-build", None).is_none());
    assert!(!catalog.supports("xai", "grok-build", AuthMode::Subscription));
    assert_eq!(catalog.info("xai", "grok-composer-2.5-fast", Some(AuthMode::Subscription)).unwrap().caps.context_window, 200_000);
    std::fs::write(dir.0.join("catalog/models.json"), b"bad json").unwrap();
    let catalog = Catalog::load(Some(&dir.0));
    assert!(catalog.needs_refresh());
    assert_eq!(catalog.models("openai", None).len(), Catalog::global().models("openai", None).len());
}

#[test]
fn fallback_does_not_grant_auth_for_known_models() {
    let catalog = Catalog::global();
    let info = catalog.info_or_fallback("anthropic", "claude-sonnet-5-5-future", Some(AuthMode::Subscription)).unwrap();
    assert_eq!(info.id, "claude-sonnet-5-5-future");
    assert_eq!(info.caps, catalog.info("anthropic", "claude-sonnet-5-5", Some(AuthMode::Subscription)).unwrap().caps);
    assert!(catalog.info_or_fallback("openai", "gpt-4o", Some(AuthMode::Subscription)).is_none());
    assert!(!catalog.supports("anthropic", "claude-sonnet-5-5-future", AuthMode::Subscription));
    let unknown = catalog.info_or_fallback("custom", "local-model", None).unwrap();
    assert_eq!(unknown.caps.tool_family, ToolFamily::Generic);
}

#[test]
fn entries_sort_newest_first_and_deprecated_last() {
    let dir = TempDir::new();
    let mut feed = snapshot();
    feed["openai"]["models"]["gpt-5.5"]["status"] = json!("deprecated");
    feed["openai"]["models"]["gpt-5.5"]["release_date"] = json!("9999-01-01");
    dir.cache(feed, now());
    let entries = Catalog::load(Some(&dir.0)).models("openai", None);
    let mut saw_deprecated = false;
    let mut previous_date = None;
    for entry in entries {
        if entry.deprecated {
            saw_deprecated = true;
        } else {
            assert!(!saw_deprecated);
            if let Some(previous) = previous_date {
                assert!(previous >= entry.release_date);
            }
            previous_date = Some(entry.release_date);
        }
    }
    assert!(saw_deprecated);
}

#[test]
fn capabilities_and_efforts_come_from_source() {
    let entry = Catalog::global().entry("openai", "gpt-5.5", None).unwrap();
    assert!(entry.info.caps.vision);
    assert!(entry.info.caps.pdf);
    assert!(entry.info.caps.reasoning);
    assert!(entry.info.caps.strict_tools);
    assert!(entry.efforts.contains(&otto_llm::Effort::Xhigh));
    let nonreasoning = Catalog::global().entry("openai", "gpt-4o", None).unwrap();
    assert!(!nonreasoning.info.caps.reasoning);
    assert!(!nonreasoning.info.caps.reasoning_replay);
}
