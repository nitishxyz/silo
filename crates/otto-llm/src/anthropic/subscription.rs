//! Claude Pro/Max code-paste OAuth and subscription-only request shaping.
use crate::{
    ErrorClass, LlmError,
    auth::{
        Credential, RefreshFn, now_ms,
        oauth::{self, TokenEncoding, TokenResponse},
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
const PROFILE: &str = "https://api.anthropic.com/api/oauth/profile";
pub const USER_AGENT: &str = "claude-cli/2.1.309 (external, cli)";
pub const SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
pub const BETAS: &[&str] = &["claude-code-20250219", "oauth-2025-04-20", "interleaved-thinking-2025-05-14"];
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Max,
    Console,
}

/// Returns the authorization URL, PKCE verifier, and expected pasted state.
#[must_use]
pub fn authorize(mode: Mode) -> (String, String, String) {
    let pkce = oauth::pkce_pair();
    let state = oauth::random_state();
    let mut url = reqwest::Url::parse(match mode {
        Mode::Max => "https://claude.ai/oauth/authorize",
        Mode::Console => "https://platform.claude.com/oauth/authorize",
    })
    .expect("static URL");
    url.query_pairs_mut().extend_pairs([
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT),
        ("scope", "org:create_api_key user:profile user:inference"),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ]);
    (url.into(), pkce.verifier, state)
}
/// Exchange a pasted `code#state`. Callers must compare the state with `authorize`.
pub async fn exchange(code_with_state: &str, verifier: &str) -> Result<Credential, LlmError> {
    let (code, state) = code_with_state
        .trim()
        .split_once('#')
        .filter(|(c, s)| !c.is_empty() && !s.is_empty() && !s.contains('#'))
        .ok_or_else(|| LlmError::new(ErrorClass::Auth, "expected code#state"))?;
    let fields = BTreeMap::from([
        ("code".into(), code.into()),
        ("state".into(), state.into()),
        ("client_id".into(), CLIENT_ID.into()),
        ("redirect_uri".into(), REDIRECT.into()),
        ("code_verifier".into(), verifier.into()),
    ]);
    let token = oauth::exchange_code(&client()?, ENDPOINT, &fields, TokenEncoding::Json).await?;
    if token.refresh.is_none() {
        return Err(LlmError::new(ErrorClass::Auth, "missing OAuth refresh token"));
    }
    let plan = signed_plan(&token, None).await;
    Ok(credential(token, "", plan))
}
/// `previous_plan` is kept when neither the token nor the profile reports one.
pub async fn refresh(refresh_token: &str, previous_plan: Option<String>) -> Result<Credential, LlmError> {
    let fields = BTreeMap::from([("client_id".into(), CLIENT_ID.into()), ("refresh_token".into(), refresh_token.into())]);
    let token = oauth::refresh_token(&client()?, ENDPOINT, &fields, TokenEncoding::Form).await.map_err(|error| {
        let message = error.message.to_ascii_lowercase();
        if error.class == ErrorClass::Auth
            || message.contains("invalid_grant")
            || message.contains("not_found_error")
            || message.contains("not found")
            || (message.contains("invalid_request")
                && (message.contains("refresh_token") || message.contains("refresh token") || message.contains("refresh-token")))
        {
            LlmError::new(ErrorClass::Auth, "Claude OAuth refresh token rejected. Run `otto auth login anthropic` again.")
        } else {
            error
        }
    })?;
    let plan = signed_plan(&token, previous_plan).await;
    Ok(credential(token, refresh_token, plan))
}
#[must_use]
pub fn refresh_fn() -> RefreshFn {
    Arc::new(|credential| {
        Box::pin(async move {
            match credential {
                Credential::OAuth { refresh: token, extra, .. } => refresh(&token, extra["plan"].as_str().map(String::from)).await,
                _ => Err(LlmError::new(ErrorClass::Auth, "expected Claude OAuth credential")),
            }
        })
    })
}
fn client() -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder().user_agent(USER_AGENT).build().map_err(crate::http::network)
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
/// The plan the token response reports, else the profile's (best effort).
async fn signed_plan(token: &TokenResponse, previous: Option<String>) -> Option<String> {
    if let Some(plan) = plan(&token.raw) {
        return Some(plan);
    }
    let profile = async {
        let response = client().ok()?.get(PROFILE).bearer_auth(&token.access).header("anthropic-beta", "oauth-2025-04-20").send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        plan(&serde_json::from_str(&response.text().await.ok()?).ok()?)
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), profile).await.ok().flatten().or(previous)
}
fn credential(token: TokenResponse, fallback: &str, plan: Option<String>) -> Credential {
    Credential::OAuth {
        access: token.access,
        refresh: token.refresh.unwrap_or_else(|| fallback.into()),
        expires_at_ms: now_ms().saturating_add(if token.expires_in == 0 { 3_600_000 } else { token.expires_in.saturating_mul(1000) }),
        account_id: None,
        extra: plan.map_or(Value::Null, |plan| json!({"plan":plan})),
    }
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
    fn authorization_and_shaping() {
        let (url, verifier, state) = authorize(Mode::Max);
        let url = reqwest::Url::parse(&url).unwrap();
        assert_eq!(url.host_str(), Some("claude.ai"));
        assert!(url.query_pairs().any(|(k, v)| k == "state" && v == state));
        assert_eq!(verifier.len(), 43);
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
