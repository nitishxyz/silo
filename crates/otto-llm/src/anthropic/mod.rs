//! Anthropic Messages: native wire types and generic model implementation.
pub mod api;
pub mod convert;
#[cfg(feature = "client")]
pub mod stream;
#[cfg(feature = "client")]
pub mod subscription;
use crate::{Caps, ModelInfo, ToolFamily};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Options {
    #[serde(default)]
    pub betas: Vec<api::Beta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<serde_json::Value>,
}

/// Bundled catalog models, without auth-mode overrides.
#[must_use]
pub fn models() -> &'static [ModelInfo] {
    static MODELS: std::sync::LazyLock<Vec<ModelInfo>> =
        std::sync::LazyLock::new(|| crate::catalog::Catalog::global().models("anthropic", None).into_iter().map(|entry| entry.info).collect());
    &MODELS
}
#[must_use]
pub fn model_info(id: &str) -> Option<ModelInfo> {
    crate::catalog::Catalog::global().info("anthropic", id, None)
}
pub(crate) fn base_caps() -> Caps {
    Caps {
        reasoning_replay: true,
        parallel_tools: true,
        strict_tools: true,
        hosted_tools: vec!["web_search".into(), "web_fetch".into(), "code_execution".into()],
        prompt_cache: true,
        tool_family: ToolFamily::Anthropic,
        ..Caps::default()
    }
}
pub(crate) fn supports_mid_system(id: &str) -> bool {
    ["fable-5", "mythos-5", "opus-5", "opus-4-8", "sonnet-5-5", "haiku-5-5"].iter().any(|s| id.contains(s))
}
pub(crate) fn supports_adaptive(id: &str) -> bool {
    supports_mid_system(id) || ["4-6", "4-7", "sonnet-5"].iter().any(|s| id.contains(s))
}
pub(crate) fn always_thinking(id: &str) -> bool {
    ["fable-5", "mythos-5", "opus-5-5", "sonnet-5-5"].iter().any(|s| id.contains(s))
}

#[cfg(feature = "client")]
pub use crate::auth::AuthMode;
#[cfg(feature = "client")]
pub use client::{AnthropicConfig, AnthropicModel};
#[cfg(feature = "client")]
mod client {
    use super::*;
    use crate::{
        CallCx, ErrorClass, EventStream, LlmError, Model, Request,
        auth::{Credential, CredentialSource},
        http::{HttpClient, RetryPolicy, byte_stream_with_idle_timeout, send_with_retry},
    };
    use serde_json::Value;
    use std::time::Duration;
    #[derive(Clone)]
    pub struct AnthropicConfig {
        pub base_url: String,
        pub auth: CredentialSource,
        pub mode: AuthMode,
        pub model: ModelInfo,
        pub betas: Vec<api::Beta>,
        pub http: HttpClient,
        pub retry: RetryPolicy,
        pub idle_timeout: Duration,
    }
    pub struct AnthropicModel {
        config: AnthropicConfig,
    }
    impl AnthropicModel {
        #[must_use]
        pub fn new(config: AnthropicConfig) -> Self {
            Self { config }
        }
        async fn send(&self, path: &str, mut body: Value, betas: &[api::Beta], cx: &CallCx) -> Result<reqwest::Response, LlmError> {
            let base = self.config.base_url.trim_end_matches('/');
            let endpoint = if base.ends_with("/v1") { format!("{base}/{path}") } else { format!("{base}/v1/{path}") };
            let subscription = self.config.mode == AuthMode::Subscription;
            let endpoint = if subscription {
                super::subscription::shape(&mut body);
                super::subscription::url(&endpoint)
            } else {
                endpoint
            };
            let mut credential = self.config.auth.current().await?;
            for attempt in 0..2 {
                let token = match (&credential, subscription) {
                    (Credential::ApiKey { key }, false) => key,
                    (Credential::OAuth { access, .. }, true) => access,
                    _ => return Err(LlmError::new(ErrorClass::Auth, "credential does not match Anthropic auth mode")),
                };
                let result = send_with_retry(
                    || {
                        let request = self.config.http.client().post(&endpoint).header("anthropic-version", "2023-06-01").json(&body);
                        if subscription {
                            super::subscription::headers(request, token, betas)
                        } else {
                            let request = request.header("x-api-key", token);
                            if betas.is_empty() {
                                request
                            } else {
                                request.header("anthropic-beta", betas.iter().map(|b| b.0.as_str()).collect::<Vec<_>>().join(","))
                            }
                        }
                    },
                    &self.config.retry,
                    &cx.cancel,
                )
                .await;
                match result {
                    Err(error) if error.status == Some(401) && attempt == 0 => {
                        credential = self.config.auth.unauthorized(&credential).await?;
                    }
                    other => return other,
                }
            }
            unreachable!()
        }
        fn prepare(&self, req: &Request) -> Result<(Value, Vec<api::Beta>), LlmError> {
            let mut req = req.clone();
            if req.model.is_empty() {
                req.model.clone_from(&self.config.model.id);
            }
            let body = convert::body(&req)?;
            let mut betas = self.config.betas.clone();
            if let Some(options) = &req.provider.anthropic {
                for beta in &options.betas {
                    if !betas.contains(beta) {
                        betas.push(beta.clone());
                    }
                }
            }
            if supports_mid_system(&req.model)
                && req.items.iter().any(|item| item.parts.iter().any(|p| matches!(p, crate::Part::ToolsChanged { .. })))
            {
                let beta = api::Beta::from(api::Beta::INLINE_TOOLS);
                if !betas.contains(&beta) {
                    betas.push(beta);
                }
            }
            Ok((body, betas))
        }
        /// Send a native request through the same auth, retry, and stream transport.
        pub async fn stream_native(&self, request: api::MessagesRequest, cx: CallCx) -> Result<EventStream, LlmError> {
            let model = request.model.clone();
            let mut body = serde_json::to_value(request).map_err(|e| LlmError::new(ErrorClass::InvalidRequest, e.to_string()))?;
            body["stream"] = Value::Bool(true);
            let names = subscription::tool_names(&body);
            let response = self.send("messages", body, &self.config.betas, &cx).await?;
            let stream = stream::decode(byte_stream_with_idle_timeout(response, self.config.idle_timeout, cx.cancel), model);
            Ok(if self.config.mode == AuthMode::Subscription { subscription::normalize(stream, names) } else { stream })
        }
    }
    #[async_trait::async_trait]
    impl Model for AnthropicModel {
        fn info(&self) -> &ModelInfo {
            &self.config.model
        }
        async fn stream(&self, req: Request, cx: CallCx) -> Result<EventStream, LlmError> {
            let (body, betas) = self.prepare(&req)?;
            let model = body["model"].as_str().unwrap_or_default().to_owned();
            let names = subscription::tool_names(&body);
            let response = self.send("messages", body, &betas, &cx).await?;
            let stream = stream::decode(byte_stream_with_idle_timeout(response, self.config.idle_timeout, cx.cancel), model);
            Ok(if self.config.mode == AuthMode::Subscription { subscription::normalize(stream, names) } else { stream })
        }
        async fn count_tokens(&self, req: &Request) -> Result<Option<u64>, LlmError> {
            let (mut body, betas) = self.prepare(req)?;
            body.as_object_mut()
                .expect("request object")
                .retain(|key, _| ["model", "messages", "system", "tools", "tool_choice", "thinking"].contains(&key.as_str()));
            let response = self.send("messages/count_tokens", body, &betas, &CallCx::default()).await?;
            let response: api::CountTokensResponse = tokio::time::timeout(self.config.idle_timeout, response.json())
                .await
                .map_err(|_| LlmError::new(ErrorClass::Timeout, "count_tokens response timed out"))?
                .map_err(crate::http::network)?;
            Ok(Some(response.input_tokens))
        }
    }
}
