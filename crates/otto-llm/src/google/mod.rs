//! Google Gemini through the Interactions API (`POST /v1beta/interactions`,
//! SSE), stateless (`store=false`) with full-history replay. [`codeassist`] is Antigravity's
//! Cloud Code Assist wire.
#[cfg(feature = "client")]
pub mod codeassist;
pub mod convert;
#[cfg(feature = "client")]
pub mod stream;

pub const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Bundled catalog models.
#[must_use]
pub fn models() -> Vec<crate::ModelInfo> {
    crate::catalog::Catalog::global().models("google", None).into_iter().map(|entry| entry.info).collect()
}
pub(crate) fn base_caps() -> crate::Caps {
    crate::Caps {
        reasoning_replay: true,
        parallel_tools: true,
        prompt_cache: true,
        tool_family: crate::ToolFamily::Gemini,
        hosted_tools: vec!["google_search".into(), "code_execution".into(), "url_context".into()],
        ..crate::Caps::default()
    }
}

#[cfg(feature = "client")]
pub use client::{GoogleConfig, GoogleModel};
#[cfg(feature = "client")]
mod client {
    use super::{convert, stream};
    use crate::{
        CallCx, EventStream, LlmError, Model, ModelInfo, Request,
        auth::{Credential, CredentialSource},
        http::{HttpClient, RetryPolicy, byte_stream_with_idle_timeout, send_with_retry},
    };
    use std::time::Duration;

    #[derive(Clone)]
    pub struct GoogleConfig {
        /// API root, normally [`super::BASE_URL`].
        pub base_url: String,
        pub auth: CredentialSource,
        pub model: ModelInfo,
        pub http: HttpClient,
        pub retry: RetryPolicy,
        pub idle_timeout: Duration,
    }
    pub struct GoogleModel {
        config: GoogleConfig,
    }
    impl GoogleModel {
        #[must_use]
        pub fn new(config: GoogleConfig) -> Self {
            Self { config }
        }
    }
    #[async_trait::async_trait]
    impl Model for GoogleModel {
        fn info(&self) -> &ModelInfo {
            &self.config.model
        }
        async fn stream(&self, mut req: Request, cx: CallCx) -> Result<EventStream, LlmError> {
            if req.model.is_empty() {
                req.model.clone_from(&self.config.model.id);
            }
            let body = convert::request(&req)?;
            let Credential::ApiKey { key } = self.config.auth.current().await? else {
                return Err(LlmError::new(crate::ErrorClass::Auth, "Gemini requires an API key"));
            };
            let url = format!("{}/interactions", self.config.base_url.trim_end_matches('/'));
            let response =
                send_with_retry(|| self.config.http.client().post(&url).header("x-goog-api-key", &key).json(&body), &self.config.retry, &cx.cancel)
                    .await?;
            Ok(stream::stream(byte_stream_with_idle_timeout(response, self.config.idle_timeout, cx.cancel), req.model))
        }
    }
}
