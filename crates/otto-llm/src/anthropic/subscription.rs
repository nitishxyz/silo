//! Claude Pro/Max code-paste OAuth spec and subscription-only request shaping.
use crate::{
    ErrorClass, LlmError,
    auth::{
        Credential,
        flows::{self, Callback, Cx, Fut, Refresh, Rotation, SubscriptionSpec},
        oauth::{TokenEncoding, TokenResponse},
    },
};
use serde_json::{Value, json};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
const PROFILE: &str = "https://api.anthropic.com/api/oauth/profile";
pub const USER_AGENT: &str = "claude-cli/2.1.309 (external, cli)";
pub const SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
pub const BETAS: &[&str] = &["claude-code-20250219", "oauth-2025-04-20", "interleaved-thinking-2025-05-14"];

/// Sign-in with the pasted `code#state` (or callback URL); `console` accounts use the platform's authorize page.
pub static SPEC: SubscriptionSpec = SubscriptionSpec {
    provider: "anthropic",
    api_key: true,
    client_id: CLIENT_ID,
    client_secret: None,
    authorize: "https://claude.ai/oauth/authorize",
    console: Some("https://platform.claude.com/oauth/authorize"),
    token: ENDPOINT,
    scopes: &["org:create_api_key", "user:profile", "user:inference"],
    params: &[("code", "true")],
    nonce: false,
    callbacks: &[Callback::Paste { redirect: REDIRECT, bare_code: false }],
    device: None,
    headers: Some(token_headers),
    exchange_encoding: TokenEncoding::Json,
    exchange_state: true,
    refresh: Refresh {
        encoding: TokenEncoding::Form,
        rotation: Rotation::Keep,
        rejected: refresh_rejected,
        rejected_message: Some("Claude OAuth refresh token rejected. Run `otto auth login anthropic` again."),
    },
    credential: build,
    finish: Some(finish),
    plan: stored_plan,
};
fn token_headers(_: &std::path::Path) -> Result<reqwest::header::HeaderMap, LlmError> {
    Ok(reqwest::header::HeaderMap::from_iter([(reqwest::header::USER_AGENT, reqwest::header::HeaderValue::from_static(USER_AGENT))]))
}
fn refresh_rejected(error: &LlmError) -> bool {
    let message = error.message.to_ascii_lowercase();
    error.class == ErrorClass::Auth
        || message.contains("invalid_grant")
        || message.contains("not_found_error")
        || message.contains("not found")
        || (message.contains("invalid_request")
            && (message.contains("refresh_token") || message.contains("refresh token") || message.contains("refresh-token")))
}
fn stored_plan(extra: &Value) -> Option<String> {
    extra["plan"].as_str().map(String::from)
}
fn client() -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder().user_agent(USER_AGENT).build().map_err(crate::http::network)
}
/// The plan the token response reports; [`finish`] asks the profile when it does not.
fn build(token: &TokenResponse, previous: Option<&Credential>) -> Result<Credential, LlmError> {
    let Credential::OAuth { access, refresh, expires_at_ms, account_id, .. } = flows::plain_credential(token, previous)? else {
        unreachable!("plain_credential builds OAuth")
    };
    let extra = plan(&token.raw).map_or(Value::Null, |plan| json!({"plan":plan}));
    Ok(Credential::OAuth { access, refresh, expires_at_ms, account_id, extra })
}
/// Without a plan from the token: the profile's (best effort), else the previous one.
fn finish<'a>(_: Cx<'a>, mut credential: Credential, previous: Option<&'a Credential>) -> Fut<'a, Credential> {
    Box::pin(async move {
        if let Credential::OAuth { access, extra, .. } = &mut credential
            && extra["plan"].is_null()
        {
            let previous = match previous {
                Some(Credential::OAuth { extra, .. }) => stored_plan(extra),
                _ => None,
            };
            let plan = profile_plan(access).await.or(previous);
            *extra = plan.map_or(Value::Null, |plan| json!({"plan":plan}));
        }
        Ok(credential)
    })
}
/// Subscription plan label from a token or `/api/oauth/profile` response:
/// `organization.organization_type`, else the account's `has_claude_max` / `has_claude_pro`.
#[must_use]
pub fn plan(value: &Value) -> Option<String> {
    let label = match value["organization"]["organization_type"].as_str() {
        Some("claude_max") => "Max",
        Some("claude_pro") => "Pro",
        Some("claude_team") => "Team",
        Some("claude_enterprise") => "Enterprise",
        _ if value["account"]["has_claude_max"] == true => "Max",
        _ if value["account"]["has_claude_pro"] == true => "Pro",
        _ => return None,
    };
    Some(label.into())
}
async fn profile_plan(token: &str) -> Option<String> {
    let profile = async {
        let response = client().ok()?.get(PROFILE).bearer_auth(token).header("anthropic-beta", "oauth-2025-04-20").send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        plan(&serde_json::from_str(&response.text().await.ok()?).ok()?)
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), profile).await.ok().flatten()
}
/// The required system prefix. Client tool names are sent as they are.
pub fn shape(body: &mut Value) {
    if !body["system"].is_array() {
        body["system"] = json!([]);
    }
    let system = body["system"].as_array_mut().expect("system array");
    if system.first().and_then(|b| b["text"].as_str()) != Some(SYSTEM_PREFIX) {
        system.insert(0, json!({"type":"text","text":SYSTEM_PREFIX}));
    }
}
pub(crate) fn headers(request: reqwest::RequestBuilder, token: &str, betas: &[super::api::Beta]) -> reqwest::RequestBuilder {
    let mut all = BETAS.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    for beta in betas {
        if !all.contains(&beta.0) {
            all.push(beta.0.clone());
        }
    }
    request
        .bearer_auth(token)
        .header("anthropic-beta", all.join(","))
        .header("user-agent", USER_AGENT)
        .header("x-app", "cli")
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("accept", "application/json")
        .header("x-stainless-arch", if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" })
        .header(
            "x-stainless-os",
            if cfg!(target_os = "macos") {
                "MacOS"
            } else if cfg!(target_os = "windows") {
                "Windows"
            } else {
                "Linux"
            },
        )
        .header("x-stainless-helper-method", "stream")
        .header("x-stainless-lang", "js")
        .header("x-stainless-package-version", "0.70.0")
        .header("x-stainless-retry-count", "0")
        .header("x-stainless-runtime", "node")
        .header("x-stainless-runtime-version", "v22.0.0")
        .header("x-stainless-timeout", "600")
}
pub(crate) fn url(base: &str) -> String {
    format!("{base}{}beta=true", if base.contains('?') { "&" } else { "?" })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spec_and_shaping() {
        assert!(SPEC.callbacks.iter().all(|c| matches!(c, Callback::Paste { redirect: REDIRECT, bare_code: false })));
        assert_eq!(flows::spec("anthropic").unwrap().client_id, CLIENT_ID);
        let mut body = json!({"system":[{"type":"text","text":"custom"}]});
        shape(&mut body);
        shape(&mut body);
        assert_eq!(body["system"].as_array().unwrap().len(), 2);
        assert_eq!(body["system"][0]["text"], SYSTEM_PREFIX);
        let request = headers(reqwest::Client::new().post("http://localhost"), "token", &["custom".into()]).build().unwrap();
        assert!(request.headers().get("x-api-key").is_none());
        assert_eq!(request.headers()["authorization"], "Bearer token");
        assert!(request.headers()["anthropic-beta"].to_str().unwrap().contains("oauth-2025-04-20"));
    }
}
