//! OpenAI Responses and ChatGPT subscription transport.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
pub mod api;
pub mod convert;
#[cfg(feature = "client")]
pub mod stream;
#[cfg(feature = "client")]
pub mod subscription;
#[cfg(feature = "client")]
pub mod ws;
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Options {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Function or custom tools sent with `async: true`: the model keeps
    /// working after calling them and the result may arrive in a later
    /// request (`caps.async_tools`, GPT-6 Astra and later).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub async_tools: Vec<String>,
}
#[cfg(feature = "client")]
pub use crate::auth::AuthMode;
#[cfg(feature = "client")]
pub use client::{OpenAiConfig, OpenAiModel};

/// Bundled catalog models, without auth-mode overrides.
#[must_use]
pub fn models() -> Vec<crate::ModelInfo> {
    crate::catalog::Catalog::global().models("openai", None).into_iter().map(|entry| entry.info).collect()
}
#[must_use]
pub fn model_info(id: &str) -> Option<crate::ModelInfo> {
    crate::catalog::Catalog::global().info("openai", id, None)
}
/// Mid-turn steering (`response.steer`, WebSocket only): the GPT-6 family on
/// the public API. Not documented or used for the ChatGPT/Codex endpoint.
#[must_use]
pub fn supports_steering(id: &str) -> bool {
    id.starts_with("gpt-6")
}
/// Async tool calling: "GPT-6 Astra and later models" (API key).
#[must_use]
pub fn supports_async_tools(id: &str) -> bool {
    id.starts_with("gpt-6")
}
pub(crate) fn base_caps() -> crate::Caps {
    crate::Caps {
        reasoning_replay: true,
        parallel_tools: true,
        strict_tools: true,
        custom_tools: true,
        mid_system: true,
        prompt_cache: true,
        tool_family: crate::ToolFamily::Openai,
        hosted_tools: vec!["web_search".into(), "file_search".into(), "code_interpreter".into()],
        ..crate::Caps::default()
    }
}
/// xAI's Responses API: the same wire, its own tools and no custom tools.
pub(crate) fn xai_caps() -> crate::Caps {
    crate::Caps {
        reasoning_replay: true,
        parallel_tools: true,
        strict_tools: true,
        prompt_cache: true,
        tool_family: crate::ToolFamily::Generic,
        hosted_tools: vec!["web_search".into(), "x_search".into(), "code_execution".into()],
        ..crate::Caps::default()
    }
}

#[cfg(feature = "client")]
mod client {
    use super::{convert, stream, subscription, ws};
    use crate::{
        CallCx, EventStream, LlmError, Model, ModelInfo, ModelSession, Part, Request,
        auth::{AuthMode, Credential, CredentialSource},
        http::{HttpClient, RetryPolicy, byte_stream_with_idle_timeout, send_with_retry},
    };
    use std::{sync::Arc, time::Duration};
    #[derive(Clone)]
    pub struct OpenAiConfig {
        /// API root (normally https://api.openai.com/v1), or a full responses URL.
        pub base_url: String,
        pub auth: CredentialSource,
        pub mode: AuthMode,
        pub model: ModelInfo,
        pub http: HttpClient,
        pub retry: RetryPolicy,
        pub idle_timeout: Duration,
        pub organization: Option<String>,
        pub project: Option<String>,
    }
    pub struct OpenAiModel {
        config: OpenAiConfig,
        ws: Arc<ws::Shared>,
    }
    impl OpenAiModel {
        #[must_use]
        pub fn new(config: OpenAiConfig) -> Self {
            Self { config, ws: Arc::default() }
        }
        /// Steerable models (steering exists only over WebSocket) and the
        /// ChatGPT/Codex endpoint (as the Codex CLI does) use WebSocket mode.
        fn websocket(&self) -> bool {
            self.config.model.caps.steering || (self.config.mode == AuthMode::Subscription && self.provider() == "openai")
        }
        fn provider(&self) -> &str {
            self.config.model.provider.as_str()
        }
        fn url(&self) -> String {
            let base = &self.config.base_url;
            if self.config.mode == AuthMode::Subscription && (base.is_empty() || base.starts_with("https://api.openai.com")) {
                subscription::RESPONSES_URL.into()
            } else {
                endpoint(base)
            }
        }
        fn dial(&self, credential: &Credential, session: Option<&str>) -> Result<ws::Dial, LlmError> {
            let invalid = |_| LlmError::new(crate::ErrorClass::Auth, "invalid header value");
            let url = self.url();
            let url = url.strip_prefix("https://").map_or_else(|| url.replacen("http://", "ws://", 1), |rest| format!("wss://{rest}"));
            if self.config.mode == AuthMode::Subscription {
                let mut headers = subscription::headers(credential, session)?;
                headers.insert("openai-beta", reqwest::header::HeaderValue::from_static(subscription::WEBSOCKET_BETA));
                return Ok(ws::Dial { url, headers: headers.into_iter().filter_map(|(name, value)| Some((name?, value))).collect() });
            }
            let token = match credential {
                Credential::ApiKey { key } => key,
                Credential::OAuth { access, .. } => access,
            };
            let mut headers = vec![(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))];
            if let Some(org) = &self.config.organization {
                headers.push((reqwest::header::HeaderName::from_static("openai-organization"), org.clone()));
            }
            if let Some(project) = &self.config.project {
                headers.push((reqwest::header::HeaderName::from_static("openai-project"), project.clone()));
            }
            let headers =
                headers.into_iter().map(|(name, value)| value.parse().map(|value| (name, value)).map_err(invalid)).collect::<Result<_, _>>()?;
            Ok(ws::Dial { url, headers })
        }
        async fn stream_ws(&self, body: serde_json::Value, model: String, cx: CallCx) -> Result<EventStream, LlmError> {
            let timeout = self.config.idle_timeout;
            let session = cx.session.as_deref();
            let (lane, reused, dial) = if let Some(lane) = self.ws.take_idle() {
                (lane, true, self.dial(&self.config.auth.current().await?, session)?)
            } else {
                let credential = self.config.auth.current().await?;
                let dial = self.dial(&credential, session)?;
                match dial.connect(&cx.cancel, timeout).await {
                    Err(error) if error.status == Some(401) => {
                        let dial = self.dial(&self.config.auth.unauthorized(&credential).await?, session)?;
                        (dial.connect(&cx.cancel, timeout).await?, false, dial)
                    }
                    result => (result?, false, dial),
                }
            };
            let (provider, subscription) = (self.provider().to_owned(), self.config.mode == AuthMode::Subscription);
            let replay = Box::new(move |parts: &[Part]| {
                let item = crate::Item::new("", crate::Role::Assistant, parts.to_vec());
                let mut body = serde_json::json!({"store": false, "input": convert::input(&[item], &provider)});
                if subscription {
                    subscription::shape(&mut body, None);
                } else {
                    convert::stateless_reasoning(&mut body);
                }
                body["input"].as_array().cloned().unwrap_or_default()
            });
            Ok(ws::stream(ws::Call { shared: self.ws.clone(), dial, lane, reused, body, model, idle_timeout: timeout, cancel: cx.cancel, replay }))
        }
        /// `POST /responses/compact`: the compacted window, as one compaction part.
        async fn compact(&self, body: &serde_json::Value, cx: &CallCx) -> Result<EventStream, LlmError> {
            let mut request = serde_json::json!({"model": body["model"], "input": body["input"]});
            if body["instructions"].as_str().is_some_and(|s| !s.is_empty()) {
                request["instructions"] = body["instructions"].clone();
            }
            let url = format!("{}/compact", endpoint(&self.config.base_url));
            let credential = self.config.auth.current().await?;
            let response = match self.send_to(&url, &request, &credential, cx).await {
                Err(error) if error.status == Some(401) => {
                    let fresh = self.config.auth.unauthorized(&credential).await?;
                    self.send_to(&url, &request, &fresh, cx).await?
                }
                result => result?,
            };
            let value: serde_json::Value = tokio::time::timeout(self.config.idle_timeout, response.json())
                .await
                .map_err(|_| LlmError::new(crate::ErrorClass::Timeout, "compaction response timed out"))?
                .map_err(crate::http::network)?;
            Ok(Box::pin(futures::stream::iter(convert::compacted(&value, self.provider()).into_iter().map(Ok))))
        }
        async fn send(&self, body: &serde_json::Value, credential: &Credential, cx: &CallCx) -> Result<reqwest::Response, LlmError> {
            self.send_to(&self.url(), body, credential, cx).await
        }
        async fn send_to(&self, url: &str, body: &serde_json::Value, credential: &Credential, cx: &CallCx) -> Result<reqwest::Response, LlmError> {
            let config = &self.config;
            let headers = if config.mode == AuthMode::Subscription {
                subscription::headers(credential, cx.session.as_deref())?
            } else {
                reqwest::header::HeaderMap::new()
            };
            let token = match credential {
                Credential::ApiKey { key } => key,
                Credential::OAuth { access, .. } => access,
            };
            send_with_retry(
                || {
                    let mut request = config.http.client().post(url).headers(headers.clone());
                    if !token.is_empty() {
                        request = request.bearer_auth(token);
                    }
                    request = request.json(body);
                    if let Some(org) = &config.organization {
                        request = request.header("OpenAI-Organization", org);
                    }
                    if let Some(project) = &config.project {
                        request = request.header("OpenAI-Project", project);
                    }
                    request
                },
                &config.retry,
                &cx.cancel,
            )
            .await
        }
    }
    fn endpoint(base: &str) -> String {
        let base = base.trim_end_matches('/');
        if base.ends_with("/responses") { base.into() } else { format!("{base}/responses") }
    }
    #[async_trait::async_trait]
    impl Model for OpenAiModel {
        fn info(&self) -> &ModelInfo {
            &self.config.model
        }
        async fn stream(&self, mut req: Request, cx: CallCx) -> Result<EventStream, LlmError> {
            if req.model.is_empty() {
                req.model = self.config.model.id.clone();
            }
            let mut body = convert::request_for(&req, self.provider());
            if self.config.mode == AuthMode::Subscription {
                subscription::shape(&mut body, cx.session.as_deref());
            }
            // `/responses/compact` has no custom summarization prompt; `instructions` stays the system prompt.
            if matches!(req.compaction, Some(crate::Compaction::Now { .. })) {
                return self.compact(&body, &cx).await;
            }
            if self.websocket() {
                return self.stream_ws(body, req.model, cx).await;
            }
            let credential = self.config.auth.current().await?;
            let response = match self.send(&body, &credential, &cx).await {
                Err(error) if error.status == Some(401) => {
                    let fresh = self.config.auth.unauthorized(&credential).await?;
                    self.send(&body, &fresh, &cx).await?
                }
                result => result?,
            };
            Ok(stream::stream_for(byte_stream_with_idle_timeout(response, self.config.idle_timeout, cx.cancel), req.model, self.provider()))
        }
        fn session(&self) -> Option<&dyn ModelSession> {
            self.config.model.caps.steering.then_some(self as &dyn ModelSession)
        }
    }
    #[async_trait::async_trait]
    impl ModelSession for OpenAiModel {
        async fn steer(&self, id: &str, input: Vec<Part>) -> Result<bool, LlmError> {
            Ok(self.config.model.caps.steering && self.ws.steer(id, convert::steer_input(&input)))
        }
    }
}
