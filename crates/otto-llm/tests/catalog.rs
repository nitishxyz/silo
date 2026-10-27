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
    // Plus Kimi Code and Google Antigravity, which only the overlay lists.
    assert_eq!(providers.iter().filter(|p| p.implemented).count(), otto_llm::catalog::PROVIDERS.len() + 2);
    for (id, default) in [("kimi-code", "kimi-k3"), ("antigravity", "gemini-3.1-pro-low")] {
        assert!(!catalog.models(id, Some(otto_llm::catalog::AuthMode::Subscription)).is_empty());
        assert!(catalog.models(id, Some(otto_llm::catalog::AuthMode::ApiKey)).is_empty());
        assert_eq!(catalog.default_model(id, otto_llm::catalog::AuthMode::Subscription).as_deref(), Some(default));
        assert_eq!(catalog.merge_remote(id, None, None).first().map(|e| e.info.id.as_str()), Some(default), "subscription_ids fall back");
    }
    assert_eq!(providers.iter().find(|p| p.id == "antigravity").map(|p| p.name.as_str()), Some("Google Antigravity"));
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
    for id in ["gpt-6-astra", "gpt-6-sol", "gpt-6-luna", "gpt-6.1-sol", "gpt-6.1-terra"] {
        assert!(catalog.supports("openai", id, AuthMode::Subscription));
    }
    assert!(catalog.models("google", Some(AuthMode::Subscription)).is_empty());
    assert_eq!(catalog.models("xai", Some(AuthMode::Subscription)).len(), 5);
    assert!(catalog.supports("xai", "grok-4.7", AuthMode::Subscription));
    assert!(catalog.supports("xai", "grok-composer-2.5-fast", AuthMode::Subscription));
    assert!(!catalog.supports("xai", "grok-composer-2.5-fast", AuthMode::ApiKey));
    assert!(catalog.supports("xai", "grok-build", AuthMode::Subscription) && !catalog.supports("xai", "grok-build", AuthMode::ApiKey));
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
    assert_eq!(catalog.entry("xai", "grok-build", None).unwrap().auth, vec![AuthMode::Subscription], "the overlay's policy, not the cache's");
    assert!(!catalog.supports("xai", "grok-build", AuthMode::ApiKey));
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

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> otto_llm::Usage {
    otto_llm::Usage { input, output, cache_read, cache_write, reasoning: 0 }
}
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn tiered_list_prices_price_the_whole_request_by_its_prompt() {
    let dir = TempDir::new();
    let mut feed = snapshot();
    // Terra-like: `tiers` at 272k, and a stale `context_over_200k` beside it that `tiers` supersedes.
    let terra = json!({"input": 1, "output": 5, "cache_read": 0.05, "cache_write": 1.25,
        "context_over_200k": {"input": 2, "output": 7.5, "cache_read": 0.1, "cache_write": 2.5},
        "tiers": [{"input": 2, "output": 7.5, "cache_read": 0.1, "cache_write": 2.5, "tier": {"size": 272_000, "type": "context"}}]});
    let mut model = feed["openai"]["models"]["gpt-5.5"].clone();
    model["id"] = json!("gpt-terra");
    model["cost"] = terra;
    feed["openai"]["models"]["gpt-terra"] = model;
    // Claude-like: only the older `context_over_200k`; a tier omitting a price keeps the base's.
    let mut model = feed["anthropic"]["models"]["claude-sonnet-5-5"].clone();
    model["id"] = json!("claude-long");
    model["cost"] = json!({"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75,
        "context_over_200k": {"input": 6, "output": 22.5, "cache_read": 0.6}});
    feed["anthropic"]["models"]["claude-long"] = model;
    dir.cache(feed, now());
    let catalog = Catalog::load(Some(&dir.0));

    let terra = catalog.list_pricing("openai", "gpt-terra").unwrap();
    assert_eq!(terra.tiers.len(), 1, "the stale field adds no tier");
    assert_eq!(terra.tiers[0].above_prompt_tokens, 272_000);
    // 250k prompt: base prices (above the old 200k mark, below the tier).
    assert!(close(terra.cost(&usage(50_000, 10_000, 200_000, 0)), (50_000.0 * 1.0 + 10_000.0 * 5.0 + 200_000.0 * 0.05) / 1e6));
    // Exactly at the threshold: still base. One token over: all of it, output too, at the tier.
    assert!(close(terra.cost(&usage(272_000, 0, 0, 0)), 0.272));
    assert!(close(terra.cost(&usage(72_001, 1_000, 200_000, 0)), (72_001.0 * 2.0 + 1_000.0 * 7.5 + 200_000.0 * 0.1) / 1e6));

    let claude = catalog.list_pricing("anthropic", "claude-long").unwrap();
    assert_eq!(claude.tiers.len(), 1);
    let tier = &claude.tiers[0];
    assert_eq!((tier.above_prompt_tokens, tier.input, tier.output, tier.cache_read, tier.cache_write), (200_000, 6.0, 22.5, 0.6, 3.75));
    assert!(close(claude.cost(&usage(1_000, 1_000, 100_000, 10_000)), (1_000.0 * 3.0 + 1_000.0 * 15.0 + 100_000.0 * 0.3 + 10_000.0 * 3.75) / 1e6));
    // The prompt counts cached and written tokens: 150k read + 60k written + 1k is above 200k.
    assert!(close(claude.cost(&usage(1_000, 1_000, 150_000, 60_000)), (1_000.0 * 6.0 + 1_000.0 * 22.5 + 150_000.0 * 0.6 + 60_000.0 * 3.75) / 1e6));
    // A dated snapshot takes its base model's price; an unknown model has none.
    assert_eq!(catalog.list_pricing("anthropic", "claude-long-20270101"), Some(claude));
    assert_eq!(catalog.list_pricing("anthropic", "claude-nothing"), None);
    assert_eq!(catalog.list_pricing("ollama", "llama"), None);
}

#[test]
fn subscriptions_bill_nothing_but_keep_the_list_price() {
    let catalog = Catalog::global();
    let sub = catalog.info("openai", "gpt-6.1-sol", Some(AuthMode::Subscription)).unwrap();
    let api = catalog.info("openai", "gpt-6.1-sol", Some(AuthMode::ApiKey)).unwrap();
    let tokens = usage(10_000, 2_000, 50_000, 0);
    assert_eq!(sub.pricing.unwrap().cost(&tokens), 0.0);
    let list = sub.list_pricing.clone().unwrap();
    assert!(list.cost(&tokens) > 0.0);
    assert_eq!(Some(list), api.list_pricing);
    assert_eq!(api.pricing, api.list_pricing, "an API key is billed the list price");
    assert_eq!(catalog.list_pricing("openai", "gpt-6.1-sol"), api.list_pricing);
    // A tier in the bundled feed (Haiku's above 100k) is parsed.
    let haiku = catalog.list_pricing("anthropic", "claude-haiku-5-5").unwrap();
    assert_eq!(haiku.tiers.iter().map(|t| t.above_prompt_tokens).collect::<Vec<_>>(), [100_000]);
    // An unknown id priced from a template is an estimate; its list price is only a snapshot's base's.
    let fallback = catalog.info_or_fallback("openai", "gpt-unknown-thing", Some(AuthMode::ApiKey)).unwrap();
    assert_eq!(fallback.list_pricing, None);
}
