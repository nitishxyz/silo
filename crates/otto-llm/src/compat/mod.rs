//! The OpenAI-compatible family: one Chat Completions client and a dialect
//! table for the request shapes that differ (reasoning parameters, reasoning
//! replay, usage options, tool streaming). MiniMax speaks the Anthropic
//! dialect and runs on the Messages client.
pub mod convert;
#[cfg(feature = "client")]
pub mod stream;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    Chat,
    /// Anthropic Messages (`anthropic::AnthropicModel` with this base URL).
    Anthropic,
}

/// How a reasoning effort is requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningParam {
    None,
    /// `reasoning_effort`.
    Effort,
    /// `reasoning: {effort}` (OpenRouter).
    OpenRouter,
    /// `thinking: {type: enabled|disabled}` (Z.ai).
    Thinking,
    /// `thinking: {type}` plus `reasoning_effort` (DeepSeek).
    ThinkingEffort,
}

/// How earlier reasoning goes back to the provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    None,
    /// `reasoning_content` on assistant messages (DeepSeek with tools, Kimi, Z.ai preserved thinking).
    ReasoningContent,
    /// `reasoning_details` on assistant messages (OpenRouter).
    ReasoningDetails,
}

/// Where a local server lists its models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discovery {
    /// Ollama `GET /api/tags`.
    OllamaTags,
    /// `GET /v1/models` (LM Studio, custom servers).
    Models,
}

#[derive(Debug)]
pub struct Dialect {
    pub id: &'static str,
    pub name: &'static str,
    pub wire: Wire,
    pub base_url: &'static str,
    /// Overrides `base_url` (a local server's address).
    pub base_env: Option<&'static str>,
    /// API key variables, first set wins.
    pub env: &'static [&'static str],
    /// Local and custom servers work without a key.
    pub key_required: bool,
    pub reasoning: ReasoningParam,
    pub replay: Replay,
    /// Send `stream_options.include_usage`.
    pub usage_option: bool,
    /// Send `tool_stream: true` to stream tool arguments (Z.ai).
    pub tool_stream: bool,
    /// Automatic prompt caching reported in usage.
    pub cache: bool,
    pub discovery: Option<Discovery>,
}

pub const DIALECTS: &[Dialect] = &[
    Dialect {
        id: "openrouter",
        name: "OpenRouter",
        wire: Wire::Chat,
        base_url: "https://openrouter.ai/api/v1",
        base_env: None,
        env: &["OPENROUTER_API_KEY"],
        key_required: true,
        reasoning: ReasoningParam::OpenRouter,
        replay: Replay::ReasoningDetails,
        usage_option: true,
        tool_stream: false,
        cache: true,
        discovery: None,
    },
    Dialect {
        id: "deepseek",
        name: "DeepSeek",
        wire: Wire::Chat,
        base_url: "https://api.deepseek.com",
        base_env: None,
        env: &["DEEPSEEK_API_KEY"],
        key_required: true,
        reasoning: ReasoningParam::ThinkingEffort,
        replay: Replay::ReasoningContent,
        usage_option: true,
        tool_stream: false,
        cache: true,
        discovery: None,
    },
    Dialect {
        id: "moonshotai",
        name: "Moonshot AI (Kimi)",
        wire: Wire::Chat,
        base_url: "https://api.moonshot.ai/v1",
        base_env: None,
        env: &["MOONSHOT_API_KEY"],
        key_required: true,
        reasoning: ReasoningParam::Effort,
        replay: Replay::ReasoningContent,
        usage_option: true,
        tool_stream: false,
        cache: true,
        discovery: None,
    },
    Dialect {
        id: "zai",
        name: "Z.ai (GLM)",
        wire: Wire::Chat,
        base_url: "https://api.z.ai/api/paas/v4",
        base_env: None,
        env: &["ZHIPU_API_KEY", "ZAI_API_KEY"],
        key_required: true,
        reasoning: ReasoningParam::Thinking,
        replay: Replay::ReasoningContent,
        usage_option: false,
        tool_stream: true,
        cache: true,
        discovery: None,
    },
    Dialect {
        id: "minimax",
        name: "MiniMax",
        wire: Wire::Anthropic,
        base_url: "https://api.minimax.io/anthropic",
        base_env: None,
        env: &["MINIMAX_API_KEY"],
        key_required: true,
        reasoning: ReasoningParam::None,
        replay: Replay::None,
        usage_option: false,
        tool_stream: false,
        cache: true,
        discovery: None,
    },
    Dialect {
        id: "ollama",
        name: "Ollama",
        wire: Wire::Chat,
        base_url: "http://127.0.0.1:11434/v1",
        base_env: Some("OLLAMA_HOST"),
        env: &["OLLAMA_API_KEY"],
        key_required: false,
        reasoning: ReasoningParam::Effort,
        replay: Replay::None,
        usage_option: true,
        tool_stream: false,
        cache: false,
        discovery: Some(Discovery::OllamaTags),
    },
    Dialect {
        id: "lmstudio",
        name: "LM Studio",
        wire: Wire::Chat,
        base_url: "http://127.0.0.1:1234/v1",
        base_env: Some("LMSTUDIO_BASE_URL"),
        env: &["LMSTUDIO_API_KEY"],
        key_required: false,
        reasoning: ReasoningParam::Effort,
        replay: Replay::None,
        usage_option: true,
        tool_stream: false,
        cache: false,
        discovery: Some(Discovery::Models),
    },
    Dialect {
        id: "custom",
        name: "Custom (OpenAI-compatible)",
        wire: Wire::Chat,
        base_url: "",
        base_env: Some("OTTO_SILO_CUSTOM_BASE_URL"),
        env: &["OTTO_SILO_CUSTOM_API_KEY"],
        key_required: false,
        reasoning: ReasoningParam::Effort,
        replay: Replay::None,
        usage_option: true,
        tool_stream: false,
        cache: false,
        discovery: Some(Discovery::Models),
    },
];

#[must_use]
pub fn dialect(id: &str) -> Option<&'static Dialect> {
    DIALECTS.iter().find(|d| d.id == id)
}

impl Dialect {
    /// The base URL: `configured`, else the dialect's env override, else its default.
    #[must_use]
    pub fn base(&self, configured: Option<&str>) -> Option<String> {
        let env = self.base_env.and_then(|name| std::env::var(name).ok()).filter(|v| !v.trim().is_empty());
        let raw = configured.map(str::to_owned).or(env).unwrap_or_else(|| self.base_url.to_owned());
        let raw = raw.trim().trim_end_matches('/');
        if raw.is_empty() {
            return None;
        }
        let url = if raw.contains("://") { raw.to_owned() } else { format!("http://{raw}") };
        // OLLAMA_HOST names the server, not its OpenAI-compatible root.
        Some(if self.discovery == Some(Discovery::OllamaTags) && !url.ends_with("/v1") { format!("{url}/v1") } else { url })
    }
}

pub(crate) fn base_caps(dialect: &Dialect) -> crate::Caps {
    crate::Caps {
        reasoning_replay: dialect.replay != Replay::None,
        parallel_tools: true,
        prompt_cache: dialect.cache,
        tool_family: crate::ToolFamily::Generic,
        ..crate::Caps::default()
    }
}

#[cfg(feature = "client")]
pub use client::{CompatConfig, CompatModel, discover};
#[cfg(feature = "client")]
mod client {
    use super::{Dialect, Discovery, convert, stream};
    use crate::{
        CallCx, EventStream, LlmError, Model, ModelInfo, Request,
        auth::{Credential, CredentialSource},
        http::{HttpClient, RetryPolicy, byte_stream_with_idle_timeout, send_with_retry},
    };
    use std::time::Duration;

    #[derive(Clone)]
    pub struct CompatConfig {
        pub dialect: &'static Dialect,
        /// The OpenAI-compatible root (`…/v1`).
        pub base_url: String,
        /// `None` for keyless local servers.
        pub auth: Option<CredentialSource>,
        pub model: ModelInfo,
        pub http: HttpClient,
        pub retry: RetryPolicy,
        pub idle_timeout: Duration,
    }
    pub struct CompatModel {
        config: CompatConfig,
    }
    impl CompatModel {
        #[must_use]
        pub fn new(config: CompatConfig) -> Self {
            Self { config }
        }
    }
    #[async_trait::async_trait]
    impl Model for CompatModel {
        fn info(&self) -> &ModelInfo {
            &self.config.model
        }
        async fn stream(&self, mut req: Request, cx: CallCx) -> Result<EventStream, LlmError> {
            if req.model.is_empty() {
                req.model.clone_from(&self.config.model.id);
            }
            let dialect = self.config.dialect;
            let body = convert::request(&req, dialect)?;
            let key = match &self.config.auth {
                Some(auth) => match auth.current().await? {
                    Credential::ApiKey { key } => Some(key),
                    Credential::OAuth { access, .. } => Some(access),
                },
                None => None,
            };
            let url = format!("{}/chat/completions", self.config.base_url.trim_end_matches('/'));
            let response = send_with_retry(
                || {
                    let request = self.config.http.client().post(&url).json(&body);
                    match key.as_deref().filter(|k| !k.is_empty()) {
                        Some(key) => request.bearer_auth(key),
                        None => request,
                    }
                },
                &self.config.retry,
                &cx.cancel,
            )
            .await?;
            Ok(stream::stream(byte_stream_with_idle_timeout(response, self.config.idle_timeout, cx.cancel), req.model, dialect.id))
        }
    }

    /// Model ids a local server offers; empty when it is not running.
    pub async fn discover(dialect: &Dialect, base_url: &str, http: &reqwest::Client, key: Option<&str>) -> Vec<String> {
        let Some(kind) = dialect.discovery else { return Vec::new() };
        let base = base_url.trim_end_matches('/');
        let url = match kind {
            Discovery::OllamaTags => format!("{}/api/tags", base.trim_end_matches("/v1")),
            Discovery::Models => format!("{base}/models"),
        };
        let mut request = http.get(url).timeout(Duration::from_millis(1500));
        if let Some(key) = key.filter(|k| !k.is_empty()) {
            request = request.bearer_auth(key);
        }
        let Ok(response) = request.send().await else { return Vec::new() };
        let Ok(value) = response.error_for_status().map(|r| r.json::<serde_json::Value>()) else { return Vec::new() };
        let Ok(value) = value.await else { return Vec::new() };
        let (list, key) = match kind {
            Discovery::OllamaTags => (&value["models"], "name"),
            Discovery::Models => (&value["data"], "id"),
        };
        let mut ids: Vec<String> = list.as_array().into_iter().flatten().filter_map(|m| m[key].as_str().map(String::from)).collect();
        ids.sort();
        ids
    }
}
