//! Google Antigravity through Cloud Code Assist (`POST {base}/v1internal:streamGenerateContent?alt=sse`):
//! Gemini, plus the Claude and GPT-OSS models the same endpoint serves. Not the Gemini
//! Interactions API of the parent module: a different wire, signed in with Google OAuth.
pub mod auth;
pub mod convert;
pub mod project;
pub mod stream;

use crate::{Effort, ModelInfo, catalog::remote::RemoteModel};

/// Endpoint bases in fallback order.
pub const ENDPOINTS: [&str; 3] =
    ["https://daily-cloudcode-pa.googleapis.com", "https://daily-cloudcode-pa.sandbox.googleapis.com", "https://cloudcode-pa.googleapis.com"];
/// The Antigravity CLI's fingerprint; the version gates which models the catalog lists.
pub const USER_AGENT: &str = "antigravity/cli/1.4.0 (aidev_client; os_type=linux; arch=amd64; cl=982146307; auth_method=consumer)";

/// The [`ModelInfo`] of a model `fetchAvailableModels` offers (its id is the runtime id
/// `streamGenerateContent` takes), under provider `antigravity`.
#[must_use]
pub fn info(model: &RemoteModel) -> ModelInfo {
    let gemini = model.id.starts_with("gemini-");
    ModelInfo {
        id: model.id.clone(),
        provider: convert::PROVIDER.into(),
        name: model.name.clone(),
        caps: crate::Caps {
            reasoning: model.reasoning,
            reasoning_replay: true,
            vision: model.image_input,
            pdf: gemini,
            audio_in: gemini,
            parallel_tools: true,
            context_window: model.context_window,
            max_output: model.max_output,
            tool_family: if gemini { crate::ToolFamily::Gemini } else { crate::ToolFamily::Generic },
            ..crate::Caps::default()
        },
        pricing: None,
    }
}

/// `models` entries of a `fetchAvailableModels` response: Gemini, Claude and GPT-OSS runtime
/// models, without internal, chat/tab and image ones.
#[must_use]
pub fn parse_models(models: &serde_json::Map<String, serde_json::Value>) -> Vec<RemoteModel> {
    let usable = |id: &str| {
        id.starts_with(|c: char| c.is_ascii_alphanumeric())
            && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            && !id.to_ascii_lowercase().contains("image")
    };
    let mut out: Vec<RemoteModel> = models
        .iter()
        .filter(|(id, info)| {
            usable(id) && ["gemini-", "claude-", "gpt-oss-"].iter().any(|p| id.starts_with(p)) && info.is_object() && info["isInternal"] != true
        })
        .map(|(id, info)| {
            let text = |key: &str| info[key].as_str().filter(|s| !s.is_empty());
            let reasoning = info["supportsThinking"] == true;
            RemoteModel {
                id: id.clone(),
                name: text("displayName").or_else(|| text("label")).or_else(|| text("modelName")).unwrap_or(id).into(),
                context_window: info["maxTokens"].as_u64().unwrap_or(0),
                max_output: info["maxOutputTokens"].as_u64().unwrap_or(0),
                reasoning,
                image_input: info["supportsImages"] == true,
                efforts: match (reasoning, id.starts_with("gemini-")) {
                    (false, _) => Vec::new(),
                    (true, true) => vec![Effort::Low, Effort::Medium, Effort::High],
                    (true, false) => vec![Effort::High],
                },
            }
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

pub use auth::{SPEC, stored_project};
pub use client::{AntigravityConfig, AntigravityModel, fetch_antigravity_models, fetch_antigravity_usage};
mod client {
    use super::{ENDPOINTS, RemoteModel, USER_AGENT, convert, parse_models, project::Project, stream};
    use crate::{
        CallCx, ErrorClass, EventStream, LlmError, Model, ModelInfo, PlanUsage, PlanWindow, Request,
        auth::{Credential, CredentialSource, now_ms},
        http::{HttpClient, RetryPolicy, byte_stream_with_idle_timeout, classify_status, network, provider_message, send_with_retry},
        plan_usage::{label, window_label},
    };
    use serde_json::{Value, json};
    use std::{
        sync::{Mutex, PoisonError},
        time::{Duration, Instant},
    };

    const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
    /// The quota summary is asked at most this often, across models.
    const USAGE_EVERY: Duration = Duration::from_secs(60);
    static USAGE_AT: Mutex<Option<Instant>> = Mutex::new(None);
    /// Whether a usage poll is due now (and if so, it counts as done).
    pub(super) fn usage_due(now: Instant) -> bool {
        let mut at = USAGE_AT.lock().unwrap_or_else(PoisonError::into_inner);
        if at.is_some_and(|at| now.saturating_duration_since(at) < USAGE_EVERY) {
            return false;
        }
        *at = Some(now);
        true
    }

    pub(super) fn endpoints() -> Vec<String> {
        ENDPOINTS.iter().map(|base| (*base).to_owned()).collect()
    }

    /// Statuses worth trying the next endpoint for (a network failure has none).
    fn next_endpoint(error: &LlmError) -> bool {
        error.status.is_none_or(|status| matches!(status, 403 | 404 | 429 | 500 | 502 | 503 | 504))
    }

    /// A JSON call (`POST` with a body, else `GET`) to the first of `bases` that answers.
    pub(super) async fn call(http: &reqwest::Client, bases: &[String], path: &str, bearer: &str, body: Option<&Value>) -> Result<Value, LlmError> {
        let mut last = LlmError::new(ErrorClass::Network, "no Antigravity endpoint");
        for base in bases {
            let url = format!("{}{path}", base.trim_end_matches('/'));
            let request = match body {
                Some(body) => http.post(&url).json(body),
                None => http.get(&url),
            };
            let error = match request.bearer_auth(bearer).header("user-agent", USER_AGENT).timeout(DISCOVERY_TIMEOUT).send().await {
                Ok(response) if response.status().is_success() => return response.json().await.map_err(network),
                Ok(response) => {
                    let (status, headers) = (response.status(), response.headers().clone());
                    let text = response.text().await.map_err(network)?;
                    LlmError { message: provider_message(&text), ..classify_status(status, &headers, &text) }
                }
                Err(error) => network(error),
            };
            if !next_endpoint(&error) {
                return Err(error);
            }
            last = error;
        }
        Err(last)
    }

    /// The models of every endpoint merged (the daily hosts list models production lacks).
    pub(super) async fn models_from(http: &reqwest::Client, bases: &[String], bearer: &str, project: &str) -> Result<Vec<RemoteModel>, LlmError> {
        let body = json!({"project":project});
        let answers = futures::future::join_all(
            bases.iter().map(|base| call(http, std::slice::from_ref(base), "/v1internal:fetchAvailableModels", bearer, Some(&body))),
        )
        .await;
        let mut merged = serde_json::Map::new();
        let mut first_error = None;
        for answer in answers {
            match answer {
                Ok(value) => merged.extend(value["models"].as_object().cloned().unwrap_or_default()),
                Err(error) => first_error = first_error.or(Some(error)),
            }
        }
        match (merged.is_empty(), first_error) {
            (true, Some(error)) => Err(error),
            (true, None) => Err(LlmError::new(ErrorClass::Other, "Antigravity returned no models")),
            _ => Ok(parse_models(&merged)),
        }
    }

    /// The models this account can run.
    ///
    /// # Errors
    /// No endpoint answered, or none listed a model.
    pub async fn fetch_antigravity_models(http: &reqwest::Client, bearer: &str, project: &str) -> Result<Vec<RemoteModel>, LlmError> {
        models_from(http, &endpoints(), bearer, project).await
    }

    /// The retrieveUserQuotaSummary buckets as plan windows: `used = 1 - remainingFraction`, one
    /// window per bucket (grouped, or top-level `buckets`), named `{5h|7d|…}_{group}`.
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn parse_usage(summary: &Value) -> PlanUsage {
        let mut groups = summary["groups"].as_array().cloned().unwrap_or_default();
        if summary["buckets"].is_array() {
            groups.push(json!({"buckets": summary["buckets"]}));
        }
        let slug =
            |s: &str| s.to_ascii_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("_");
        let mut windows: Vec<PlanWindow> = Vec::new();
        for group in &groups {
            let group_name = group["displayName"].as_str().unwrap_or_default();
            for bucket in group["buckets"].as_array().into_iter().flatten() {
                let used = if bucket["disabled"] == true {
                    1.0
                } else if let Some(remaining) = bucket["remainingFraction"].as_f64().filter(|r| r.is_finite()) {
                    (1.0 - remaining.clamp(0.0, 1.0)) as f32
                } else {
                    continue;
                };
                let (key, minutes) = match bucket["window"].as_str().unwrap_or("limit").to_ascii_uppercase().as_str() {
                    "FIVE_HOUR" | "FIVE_HOURS" => ("5h".to_owned(), Some(300)),
                    "WEEKLY" => ("7d".to_owned(), Some(10_080)),
                    "DAILY" => ("1d".to_owned(), Some(1440)),
                    other => (slug(other), None),
                };
                let suffix = [group_name, bucket["displayName"].as_str().unwrap_or_default()]
                    .iter()
                    .map(|s| slug(s))
                    .find(|s| !s.is_empty())
                    .unwrap_or_default();
                let mut name = if suffix.is_empty() { key.clone() } else { format!("{key}_{suffix}") };
                while windows.iter().any(|w| w.name == name) {
                    name.push_str("_2");
                }
                let base = minutes.map_or_else(|| window_label(&key), label);
                windows.push(PlanWindow {
                    label: if groups.len() > 1 && !group_name.is_empty() { format!("{base} {group_name}") } else { base },
                    name,
                    used,
                    resets_at: bucket["resetTime"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp()),
                });
            }
        }
        let mut usage = PlanUsage::default();
        usage.merge(PlanUsage { windows });
        usage
    }

    /// `Ok` and empty when the account has no quota summary (the free tier answers 403).
    pub(super) async fn usage_from(http: &reqwest::Client, bases: &[String], bearer: &str, project: &str) -> Result<PlanUsage, LlmError> {
        match call(http, bases, "/v1internal:retrieveUserQuotaSummary", bearer, Some(&json!({"project":project}))).await {
            Ok(summary) => Ok(parse_usage(&summary)),
            Err(error) if error.status == Some(403) => Ok(PlanUsage::default()),
            Err(error) => Err(error),
        }
    }

    /// The account's usage windows; empty when it has none to report (free tier).
    ///
    /// # Errors
    /// Network failures and error statuses other than 403.
    pub async fn fetch_antigravity_usage(http: &reqwest::Client, bearer: &str, project: &str) -> Result<PlanUsage, LlmError> {
        usage_from(http, &endpoints(), bearer, project).await
    }

    #[derive(Clone)]
    pub struct AntigravityConfig {
        pub model: ModelInfo,
        pub auth: CredentialSource,
        pub http: HttpClient,
        /// The Cloud Code Assist project, resolved on first use and shared by clones.
        pub project: Project,
        /// Endpoint bases in fallback order, normally [`ENDPOINTS`].
        pub bases: Vec<String>,
        pub retry: RetryPolicy,
        pub idle_timeout: Duration,
    }
    impl AntigravityConfig {
        #[must_use]
        pub fn new(model: ModelInfo, auth: CredentialSource, http: HttpClient, project: Project) -> Self {
            Self { model, auth, http, project, bases: endpoints(), retry: RetryPolicy::default(), idle_timeout: Duration::from_secs(120) }
        }
    }
    pub struct AntigravityModel {
        config: AntigravityConfig,
    }
    impl AntigravityModel {
        #[must_use]
        pub fn new(config: AntigravityConfig) -> Self {
            Self { config }
        }
        /// One pass over the endpoints; a quota wall or an error no other endpoint would fix ends it.
        async fn pass(&self, body: &Value, token: &str, cx: &CallCx) -> Result<reqwest::Response, LlmError> {
            let once = RetryPolicy { max_attempts: 1, ..self.config.retry.clone() };
            let mut last = LlmError::new(ErrorClass::Network, "no Antigravity endpoint");
            for base in &self.config.bases {
                let url = format!("{}/v1internal:streamGenerateContent?alt=sse", base.trim_end_matches('/'));
                match send_with_retry(
                    || self.config.http.client().post(&url).bearer_auth(token).header("user-agent", USER_AGENT).json(body),
                    &once,
                    cx,
                )
                .await
                {
                    Ok(response) => return Ok(response),
                    Err(error) if next_endpoint(&error) && !quota_wall(&error) && error.class != ErrorClass::Cancelled => last = error,
                    Err(error) => return Err(error),
                }
            }
            Err(last)
        }
        async fn send(&self, req: &Request, ids: &convert::Ids, credential: &Credential, cx: &CallCx) -> Result<reqwest::Response, LlmError> {
            let Credential::OAuth { access, .. } = credential else {
                return Err(LlmError::new(ErrorClass::Auth, "Antigravity requires a Google sign-in"));
            };
            let project = self.config.project.get(access).await?;
            let envelope = convert::Envelope { project: &project, ids, now_ms: now_ms(), max_output: self.config.model.caps.max_output };
            let body = convert::request(req, &envelope)?;
            let attempts = self.config.retry.max_attempts.max(1);
            for attempt in 0..attempts {
                let error = match self.pass(&body, access, cx).await {
                    Ok(response) => return Ok(response),
                    Err(error) if quota_wall(&error) => return Err(LlmError { class: ErrorClass::QuotaExhausted, ..error }),
                    Err(error) => error,
                };
                crate::http::wait_to_retry(&self.config.retry, attempt, error, cx).await?;
            }
            unreachable!("at least one attempt")
        }
    }

    /// A 429 that is the account's quota, not a throttle: retrying cannot help.
    fn quota_wall(error: &LlmError) -> bool {
        let message = error.message.to_ascii_lowercase();
        error.class == ErrorClass::QuotaExhausted
            || (error.status == Some(429) && (message.contains("individual quota reached") || message.contains("resets in")))
    }

    #[async_trait::async_trait]
    impl Model for AntigravityModel {
        fn info(&self) -> &ModelInfo {
            &self.config.model
        }
        async fn stream(&self, mut req: Request, cx: CallCx) -> Result<EventStream, LlmError> {
            if req.model.is_empty() {
                req.model.clone_from(&self.config.model.id);
            }
            let ids = convert::Ids::new(cx.session.as_deref());
            let credential = self.config.auth.current().await?;
            let response = match self.send(&req, &ids, &credential, &cx).await {
                Err(error) if error.status == Some(401) => {
                    let fresh = self.config.auth.unauthorized(&credential).await?;
                    self.send(&req, &ids, &fresh, &cx).await?
                }
                result => result?,
            };
            Ok(stream::stream(byte_stream_with_idle_timeout(response, self.config.idle_timeout, cx.cancel), req.model))
        }
        /// The quota summary, at most once a minute.
        async fn plan_usage(&self) -> Option<PlanUsage> {
            let Ok(Credential::OAuth { access, .. }) = self.config.auth.current().await else { return None };
            if !usage_due(Instant::now()) {
                return None;
            }
            let usage = match self.config.project.get(&access).await {
                Ok(project) => usage_from(self.config.http.client(), &self.config.bases, &access, &project).await,
                Err(error) => Err(error),
            };
            match usage {
                Ok(usage) => Some(usage).filter(|u| !u.is_empty()),
                Err(error) => {
                    tracing::debug!(%error, "antigravity usage");
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{client::*, info, parse_models, project::resolve};
    use crate::{ErrorClass, PlanWindow};
    use serde_json::{Value, json};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    /// A server answering `path → (status, body)`; it records each request's path and JSON body.
    async fn serve(routes: &[(&str, u16, Value)]) -> (String, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: HashMap<String, (u16, String)> = routes.iter().map(|(p, s, b)| ((*p).to_owned(), (*s, b.to_string()))).collect();
        let seen = Seen::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let mut raw = Vec::new();
                let mut buffer = [0; 4096];
                let (head, length) = loop {
                    let n = tcp.read(&mut buffer).await.unwrap_or(0);
                    raw.extend_from_slice(&buffer[..n]);
                    if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&raw[..end]).to_string();
                        let length = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                            .unwrap_or(0);
                        raw.drain(..end + 4);
                        break (head, length);
                    }
                    if n == 0 {
                        break (String::new(), 0);
                    }
                };
                while raw.len() < length {
                    let n = tcp.read(&mut buffer).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buffer[..n]);
                }
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                log.lock().unwrap().push((path.clone(), serde_json::from_slice(&raw).unwrap_or(Value::Null)));
                let (status, body) = routes.get(&path).cloned().unwrap_or((404, r#"{"error":{"message":"no route"}}"#.into()));
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tcp.write_all(response.as_bytes()).await;
            }
        });
        (base, seen)
    }
    fn paths(seen: &Seen) -> Vec<String> {
        seen.lock().unwrap().iter().map(|(p, _)| p.clone()).collect()
    }

    #[tokio::test]
    async fn a_known_project_is_loaded() {
        let (base, seen) =
            serve(&[("/v1internal:loadCodeAssist", 200, json!({"cloudaicompanionProject":"proj-1","currentTier":{"id":"free-tier"}}))]).await;
        assert_eq!(resolve(&reqwest::Client::new(), &[base], "tok", Duration::ZERO).await.unwrap(), "proj-1");
        assert_eq!(seen.lock().unwrap()[0].1, json!({"metadata":{"ideType":"ANTIGRAVITY"}}));
    }

    #[tokio::test]
    async fn the_free_tier_is_onboarded_and_its_operation_polled() {
        let load = json!({"allowedTiers":[{"id":"legacy-tier"},{"id":"free-tier","isDefault":true}]});
        let (base, seen) = serve(&[
            ("/v1internal:loadCodeAssist", 200, load),
            ("/v1internal:onboardUser", 200, json!({"name":"operations/op1","done":false})),
            ("/v1internal/operations/op1", 200, json!({"name":"operations/op1","done":true,"response":{"cloudaicompanionProject":{"id":"proj-2"}}})),
        ])
        .await;
        assert_eq!(resolve(&reqwest::Client::new(), &[base], "tok", Duration::ZERO).await.unwrap(), "proj-2");
        assert_eq!(paths(&seen), ["/v1internal:loadCodeAssist", "/v1internal:onboardUser", "/v1internal/operations/op1"]);
        assert_eq!(seen.lock().unwrap()[1].1, json!({"tierId":"free-tier","metadata":{"ideType":"ANTIGRAVITY"}}));
    }

    #[tokio::test]
    async fn a_paid_tier_without_a_project_and_a_stuck_operation_fail() {
        let paid = json!({"allowedTiers":[{"id":"standard-tier","isDefault":true}],"ineligibleTiers":[{"reasonMessage":"Set a project"}]});
        let (base, _) = serve(&[("/v1internal:loadCodeAssist", 200, paid)]).await;
        let error = resolve(&reqwest::Client::new(), &[base], "tok", Duration::ZERO).await.unwrap_err();
        assert!(error.class == ErrorClass::Auth && error.message.contains("Set a project"), "{error:?}");
        let (base, _) = serve(&[
            ("/v1internal:loadCodeAssist", 200, json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]})),
            ("/v1internal:onboardUser", 200, json!({"name":"operations/op1"})),
            ("/v1internal/operations/op1", 200, json!({"name":"operations/op1","done":false})),
        ])
        .await;
        assert_eq!(resolve(&reqwest::Client::new(), &[base], "tok", Duration::ZERO).await.unwrap_err().class, ErrorClass::Timeout);
    }

    #[tokio::test]
    async fn endpoints_fall_back_and_models_merge() {
        let daily = json!({"models":{
            "gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash","maxTokens":1_048_576,"supportsThinking":true,"supportsImages":true},
            "claude-sonnet-4-6":{"label":"Claude Sonnet 4.6","maxTokens":200_000,"supportsThinking":true},
            "chat_20706":{"isInternal":true},
            "gemini-3-pro-image":{"displayName":"Image"},
            "tab_flash_lite":{}
        }});
        let (a, _) = serve(&[("/v1internal:fetchAvailableModels", 200, daily)]).await;
        let (b, _) = serve(&[("/v1internal:fetchAvailableModels", 503, json!({"error":{"message":"down"}}))]).await;
        let (c, seen) = serve(&[("/v1internal:fetchAvailableModels", 200, json!({"models":{"gpt-oss-120b-medium":{"maxTokens":131_072}}}))]).await;
        let models = models_from(&reqwest::Client::new(), &[a, b, c], "tok", "proj").await.unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["claude-sonnet-4-6", "gemini-3.8-flash-high", "gpt-oss-120b-medium"]);
        assert_eq!(seen.lock().unwrap()[0].1, json!({"project":"proj"}));
        let (claude, gemini, oss) = (&models[0], &models[1], &models[2]);
        assert_eq!((claude.name.as_str(), claude.efforts.as_slice()), ("Claude Sonnet 4.6", &[crate::Effort::High][..]));
        assert!(gemini.reasoning && gemini.image_input && gemini.efforts.len() == 3 && gemini.context_window == 1_048_576);
        assert!(!oss.reasoning && oss.efforts.is_empty() && oss.name == "gpt-oss-120b-medium");
        let info = info(gemini);
        assert_eq!((info.provider.as_str(), info.caps.tool_family), ("antigravity", crate::ToolFamily::Gemini));
        assert!(info.caps.vision && info.caps.pdf && !super::info(claude).caps.pdf);
        assert!(parse_models(&serde_json::Map::new()).is_empty());
        // One failing call: a 400 ends the search instead of trying the next host.
        let (bad, _) = serve(&[("/v1internal:loadCodeAssist", 400, json!({"error":{"message":"bad"}}))]).await;
        let (good, seen) = serve(&[("/v1internal:loadCodeAssist", 200, json!({}))]).await;
        let error = call(&reqwest::Client::new(), &[bad, good], "/v1internal:loadCodeAssist", "tok", Some(&json!({}))).await.unwrap_err();
        assert_eq!((error.status, error.message.as_str()), (Some(400), "bad"));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn quota_buckets_become_plan_windows() {
        let summary = json!({"groups":[
            {"displayName":"Gemini Pro","buckets":[
                {"bucketId":"a","window":"FIVE_HOUR","remainingFraction":0.75,"resetTime":"2026-10-24T12:00:00Z"},
                {"bucketId":"b","window":"WEEKLY","remainingFraction":0.1}
            ]},
            {"displayName":"Claude","buckets":[{"bucketId":"c","window":"WEEKLY","disabled":true},{"bucketId":"d","window":"WEEKLY"}]}
        ]});
        let usage = parse_usage(&summary);
        let find = |name: &str| usage.windows.iter().find(|w| w.name == name).cloned();
        assert_eq!(
            find("5h_gemini_pro"),
            Some(PlanWindow { name: "5h_gemini_pro".into(), label: "5-hour Gemini Pro".into(), used: 0.25, resets_at: Some(1_792_843_200) })
        );
        assert!((find("7d_gemini_pro").unwrap().used - 0.9).abs() < 1e-6);
        assert_eq!(find("7d_claude").map(|w| (w.label, w.used)), Some(("Weekly Claude".into(), 1.0)));
        // A bucket without a fraction is skipped.
        assert_eq!(usage.windows.len(), 3);
        let single = parse_usage(&json!({"buckets":[{"window":"DAILY","displayName":"Requests","remainingFraction":1.0}]}));
        assert_eq!(single.windows, [PlanWindow { name: "1d_requests".into(), label: "1d".into(), used: 0.0, resets_at: None }]);
        assert!(parse_usage(&json!({})).is_empty());
    }

    #[tokio::test]
    async fn the_free_tier_has_no_usage() {
        let path = "/v1internal:retrieveUserQuotaSummary";
        let (base, seen) = serve(&[(path, 403, json!({"error":{"code":403,"message":"The caller does not have permission"}}))]).await;
        assert!(usage_from(&reqwest::Client::new(), &[base], "tok", "proj").await.unwrap().is_empty());
        assert_eq!(seen.lock().unwrap()[0].1, json!({"project":"proj"}));
        let (base, _) = serve(&[(path, 401, json!({"error":{"message":"expired"}}))]).await;
        assert_eq!(usage_from(&reqwest::Client::new(), &[base], "tok", "proj").await.unwrap_err().status, Some(401));
        let (base, _) = serve(&[(path, 200, json!({"groups":[{"buckets":[{"window":"WEEKLY","remainingFraction":0.5}]}]}))]).await;
        let usage = usage_from(&reqwest::Client::new(), &[base], "tok", "proj").await.unwrap();
        assert_eq!(usage.windows.iter().map(|w| (w.name.as_str(), w.used)).collect::<Vec<_>>(), [("7d", 0.5)]);
    }

    #[test]
    fn usage_is_polled_once_a_minute() {
        let start = std::time::Instant::now() + Duration::from_secs(3600);
        assert!(usage_due(start));
        assert!(!usage_due(start + Duration::from_secs(59)));
        assert!(usage_due(start + Duration::from_secs(60)));
    }

    #[test]
    fn the_sign_in_spec_is_google_pkce_with_offline_access() {
        use crate::auth::flows::{self, Callback, Port};
        let spec = flows::spec("antigravity").expect("registered");
        assert!(std::ptr::eq(spec, &super::SPEC));
        assert!(spec.client_id.starts_with("1071006060591-") && spec.client_secret.is_some_and(|s| s.starts_with("GOCSPX-")));
        assert!(!spec.api_key && spec.device.is_none() && spec.finish.is_some());
        assert_eq!((spec.token, spec.params), ("https://oauth2.googleapis.com/token", &[("access_type", "offline"), ("prompt", "consent")][..]));
        for scope in ["cloud-platform", "userinfo.email", "userinfo.profile", "cclog", "experimentsandconfigs"] {
            assert!(spec.scopes.contains(&format!("https://www.googleapis.com/auth/{scope}").as_str()), "{scope}");
        }
        assert!(matches!(spec.callbacks[0], Callback::Loopback { host: "localhost", port: Port::Fixed(51121), path: "/oauth-callback" }));
        assert!(matches!(spec.callbacks[1], Callback::Paste { redirect: "https://antigravity.google/oauth-callback", bare_code: true }));
    }

    #[tokio::test]
    async fn sign_in_stores_the_project_and_a_refresh_keeps_it() {
        use crate::auth::Credential;
        let oauth = |extra| Credential::OAuth { access: "tok".into(), refresh: "r".into(), expires_at_ms: 0, account_id: None, extra };
        let (base, seen) = serve(&[("/v1internal:loadCodeAssist", 200, json!({"cloudaicompanionProject":"proj-1"}))]).await;
        let signed = super::auth::with_project(&reqwest::Client::new(), vec![base.clone()], oauth(Value::Null), Duration::ZERO).await.unwrap();
        assert_eq!(super::stored_project(&signed).as_deref(), Some("proj-1"));
        let kept = super::auth::with_project(&reqwest::Client::new(), vec![base], oauth(json!({"project":"proj-0"})), Duration::ZERO).await.unwrap();
        assert_eq!(super::stored_project(&kept).as_deref(), Some("proj-0"));
        assert_eq!(paths(&seen), ["/v1internal:loadCodeAssist"], "a stored project is not looked up again");
    }
}
