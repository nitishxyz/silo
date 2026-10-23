//! xAI with a Grok Build sign-in, as the TS otto does it (`xai-oauth.ts`, `xai-client.ts`,
//! `fetchXaiGrokUsage`): the Responses wire of [`OpenAiModel`] with the subscription bearer.
//! grok-4.x goes to `api.x.ai`; Grok Build and Composer go to the Grok CLI proxy with the
//! Grok CLI's fingerprint.
use super::{AuthMode, OpenAiConfig, OpenAiModel};
use crate::{
    ErrorClass, LlmError, Model, ModelInfo, PlanUsage, PlanWindow,
    auth::{
        Credential, CredentialSource,
        flows::{self, Callback, Device, Port, Refresh, Rotation, SubscriptionSpec},
        oauth::TokenEncoding,
    },
    http::{HttpClient, RetryPolicy, classify_status, network, provider_message},
    plan_usage::timestamp,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

pub const ID: &str = "xai";
pub const API_URL: &str = "https://api.x.ai/v1";
pub const PROXY_URL: &str = "https://cli-chat-proxy.grok.com/v1";
pub const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// The Grok CLI version old otto claims on inference and billing calls.
pub const CLIENT_VERSION: &str = "0.2.22";
/// The Grok CLI version old otto claims on sign-in calls.
pub const LOGIN_VERSION: &str = "1.0.3";
pub const ISSUER: &str = "https://auth.x.ai";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// Models served only through the Grok CLI proxy.
pub const PROXY_MODELS: &[&str] = &["grok-build", "grok-composer-2.5-fast"];
/// The subscription's fixed model list (overlay `subscription_ids` plus Composer's override).
pub const SUBSCRIPTION_MODELS: &[&str] = &["grok-4.5", "grok-4.6", "grok-composer-2.5-fast"];

/// Grok Build sign-in: an RFC 8628 device flow (`referrer=grok-build`) or PKCE with a nonce on
/// the Grok CLI's fixed loopback. Tokens last about an hour; refresh keeps the old refresh
/// token when no new one comes.
pub static SPEC: SubscriptionSpec = SubscriptionSpec {
    provider: ID,
    api_key: true,
    client_id: CLIENT_ID,
    client_secret: None,
    authorize: "https://auth.x.ai/oauth2/authorize",
    console: None,
    token: "https://auth.x.ai/oauth2/token",
    scopes: &[
        "openid",
        "profile",
        "email",
        "offline_access",
        "grok-cli:access",
        "api:access",
        "conversations:read",
        "conversations:write",
        "workspaces:read",
        "workspaces:write",
    ],
    params: &[],
    nonce: true,
    callbacks: &[Callback::Loopback { host: "127.0.0.1", port: Port::Fixed(56121), path: "/callback" }],
    device: Some(Device::Rfc8628 { url: "https://auth.x.ai/oauth2/device/code", params: &[("referrer", "grok-build")] }),
    headers: Some(login_headers),
    exchange_encoding: TokenEncoding::Form,
    exchange_state: false,
    refresh: Refresh {
        encoding: TokenEncoding::Form,
        rotation: Rotation::Keep,
        rejected: flows::rejected_on_invalid_grant,
        rejected_message: Some("Grok sign-in expired. Sign in again."),
    },
    credential: flows::plain_credential,
    finish: None,
    plan: flows::no_plan,
};

/// `getDeviceHeaders()` of old otto.
///
/// # Errors
/// Never; a [`flows::HeaderFn`].
pub fn login_headers(_: &Path) -> Result<HeaderMap, LlmError> {
    Ok(HeaderMap::from_iter([
        (HeaderName::from_static("x-grok-client-surface"), HeaderValue::from_static("ui")),
        (HeaderName::from_static("x-grok-client-version"), HeaderValue::from_static(LOGIN_VERSION)),
    ]))
}

/// The bare model id: `xai/Grok-Build` → `grok-build`.
fn bare(model: &str) -> String {
    let model = model.to_ascii_lowercase();
    model.rsplit('/').next().unwrap_or_default().to_owned()
}

#[must_use]
pub fn is_proxy_model(model: &str) -> bool {
    PROXY_MODELS.contains(&bare(model).as_str())
}

/// Node's `process.platform` / `process.arch` names, as old otto reports them.
fn platform() -> (&'static str, &'static str) {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "ia32",
        arch => arch,
    };
    (std::env::consts::OS, arch)
}

/// `getGrokCliHeaders(model)` of old otto.
#[must_use]
pub fn proxy_headers(model: &str) -> HeaderMap {
    let (os, arch) = platform();
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("x-xai-token-auth", "xai-grok-cli".to_owned()),
        ("x-authenticateresponse", "authenticate-response".to_owned()),
        ("x-grok-client-version", CLIENT_VERSION.to_owned()),
        ("x-grok-client-identifier", "grok-shell".to_owned()),
        ("x-grok-model-override", model.to_owned()),
        ("user-agent", format!("grok-shell/{CLIENT_VERSION} ({os}; {arch})")),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
    headers
}

/// Where a model goes and with which extra headers.
#[must_use]
pub fn route(model: &str) -> (&'static str, HeaderMap) {
    if is_proxy_model(model) { (PROXY_URL, proxy_headers(model)) } else { (API_URL, HeaderMap::new()) }
}

/// Body shaping for a signed-in request: the session is the cache key unless one is set.
pub fn shape(body: &mut Value, session: Option<&str>) {
    if !body["prompt_cache_key"].is_string()
        && let Some(session) = session
    {
        body["prompt_cache_key"] = json!(session);
    }
}

/// A Grok Build signed-in model, routed by its id.
#[must_use]
pub fn model(info: ModelInfo, auth: CredentialSource, http: HttpClient, retry: RetryPolicy, idle_timeout: Duration) -> Arc<dyn Model> {
    let (base, headers) = route(&info.id);
    Arc::new(
        OpenAiModel::new(OpenAiConfig {
            base_url: base.into(),
            auth,
            mode: AuthMode::Subscription,
            model: info,
            http,
            retry,
            idle_timeout,
            organization: None,
            project: None,
        })
        .with_headers(headers),
    )
}

/// Billing is asked at most this often, across models.
const USAGE_EVERY: Duration = Duration::from_secs(60);
static USAGE_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// [`fetch_xai_usage`] for a signed-in model, at most once a minute; `None` when not due or failed.
pub async fn plan_usage(auth: &CredentialSource, http: &HttpClient) -> Option<PlanUsage> {
    let Ok(Credential::OAuth { access, .. }) = auth.current().await else { return None };
    {
        let now = Instant::now();
        let mut at = USAGE_AT.lock().unwrap_or_else(PoisonError::into_inner);
        if at.is_some_and(|at| now.saturating_duration_since(at) < USAGE_EVERY) {
            return None;
        }
        *at = Some(now);
    }
    fetch_xai_usage(http.client(), &access).await.inspect_err(|error| tracing::debug!(%error, "xai usage")).ok()
}

/// `GET /v1/billing?format=credits` on the proxy, with the Grok CLI headers.
///
/// # Errors
/// Network and status failures, or a body without a credit usage percent.
pub async fn fetch_xai_usage(http: &reqwest::Client, bearer: &str) -> Result<PlanUsage, LlmError> {
    let response = http
        .get(BILLING_URL)
        .bearer_auth(bearer)
        .header("accept", "application/json")
        .headers(proxy_headers("grok-build"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(network)?;
    let (status, headers) = (response.status(), response.headers().clone());
    let text = response.text().await.map_err(network)?;
    if !status.is_success() {
        return Err(LlmError { message: provider_message(&text), ..classify_status(status, &headers, &text) });
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| LlmError::new(ErrorClass::Other, format!("xAI billing: {e}")))?;
    parse_usage(&body).ok_or_else(|| LlmError::new(ErrorClass::Other, "xAI billing returned no credit usage"))
}

/// The billing response as one `Credits` window: `creditUsagePercent` used, resetting at the
/// period end (`currentPeriod.end`, else `billingPeriodEnd`, as old otto reads it).
#[must_use]
pub fn parse_usage(body: &Value) -> Option<PlanUsage> {
    let config = &body["config"];
    let percent = config["creditUsagePercent"].as_f64().filter(|p| p.is_finite())?;
    let end = config["currentPeriod"]["end"].as_str().or_else(|| config["billingPeriodEnd"].as_str()).and_then(timestamp);
    #[allow(clippy::cast_possible_truncation)]
    let used = (percent / 100.0).clamp(0.0, 1.0) as f32;
    Some(PlanUsage { windows: vec![PlanWindow { name: "credits".into(), label: "Credits".into(), used, resets_at: end }] })
}
