//! OpenAI Responses and ChatGPT subscription transport.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
pub mod api;
pub mod convert;
#[cfg(feature = "client")]
pub mod stream;
#[cfg(feature = "client")]
pub mod subscription;
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Options {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
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

#[cfg(feature = "client")]
mod client {
    use super::{convert, stream, subscription};
    use crate::{
        CallCx, EventStream, LlmError, Model, ModelInfo, Request,
        auth::{AuthMode, Credential, CredentialSource},
        http::{HttpClient, RetryPolicy, byte_stream_with_idle_timeout, send_with_retry},
    };
    use std::time::Duration;
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
    }
    impl OpenAiModel {
        #[must_use]
        pub fn new(config: OpenAiConfig) -> Self {
            Self { config }
        }
        async fn send(&self, body: &serde_json::Value, credential: &Credential, cx: &CallCx) -> Result<reqwest::Response, LlmError> {
            let config = &self.config;
            let mut headers = reqwest::header::HeaderMap::new();
            let url = if config.mode == AuthMode::Subscription {
                headers = subscription::headers(credential, cx.session.as_deref())?;
                if config.base_url.is_empty() || config.base_url.starts_with("https://api.openai.com") {
                    subscription::RESPONSES_URL.into()
                } else {
                    endpoint(&config.base_url)
                }
            } else {
                endpoint(&config.base_url)
            };
            let token = match credential {
                Credential::ApiKey { key } => key,
                Credential::OAuth { access, .. } => access,
            };
            send_with_retry(
                || {
                    let mut request = config.http.client().post(&url).headers(headers.clone()).bearer_auth(token).json(body);
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
            let mut body = convert::request(&req);
            if self.config.mode == AuthMode::Subscription {
                subscription::shape(&mut body, cx.session.as_deref());
            }
            let credential = self.config.auth.current().await?;
            let response = match self.send(&body, &credential, &cx).await {
                Err(error) if error.status == Some(401) => {
                    let fresh = self.config.auth.unauthorized(&credential).await?;
                    self.send(&body, &fresh, &cx).await?
                }
                result => result?,
            };
            Ok(stream::stream(byte_stream_with_idle_timeout(response, self.config.idle_timeout, cx.cancel), req.model))
        }
    }
}
