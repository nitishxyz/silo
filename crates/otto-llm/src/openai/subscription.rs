//! ChatGPT/Codex OAuth and endpoint-specific shaping.
use crate::{
    ErrorClass, LlmError,
    auth::{
        Credential,
        flows::{self, Callback, CustomDevice, Cx, Device, DeviceCode, Fut, Poll, Port, Refresh, Rotation, SubscriptionSpec},
        oauth::{TokenEncoding, TokenResponse},
    },
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{sync::OnceLock, time::Duration};
pub const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
/// WebSocket mode on the Codex endpoint (`codex-rs/core/src/client.rs`,
/// `RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE`).
pub const WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";

fn auth_claims(token: &str) -> Option<Value> {
    let bytes = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?;
    let mut value: Value = serde_json::from_slice(&bytes).ok()?;
    Some(value["https://api.openai.com/auth"].take())
}
#[must_use]
pub fn account_id(token: &str) -> Option<String> {
    auth_claims(token)?["chatgpt_account_id"].as_str().map(Into::into)
}
/// ChatGPT plan label from an id_token's `chatgpt_plan_type` claim (`Plus`, `Pro`, `Team`, …).
#[must_use]
pub fn plan(id_token: &str) -> Option<String> {
    let kind = auth_claims(id_token)?["chatgpt_plan_type"].as_str()?.trim().to_ascii_lowercase();
    let mut chars = kind.chars();
    let first = chars.next()?;
    Some(first.to_ascii_uppercase().to_string() + chars.as_str())
}
fn credential(token: &TokenResponse, previous: Option<&Credential>) -> Result<Credential, LlmError> {
    let (old_refresh, old_account, old_id) = match previous {
        Some(Credential::OAuth { refresh, account_id, extra, .. }) => {
            (refresh.clone(), account_id.clone(), extra["id_token"].as_str().map(String::from))
        }
        _ => (String::new(), None, None),
    };
    let account_id = token.id_token.as_deref().and_then(account_id).or_else(|| account_id(&token.access)).or(old_account);
    let id_token = token.id_token.clone().or(old_id);
    Ok(Credential::OAuth {
        access: token.access.clone(),
        refresh: token.refresh.clone().unwrap_or(old_refresh),
        expires_at_ms: flows::expires_at_ms(token),
        account_id,
        extra: json!({"id_token":id_token}),
    })
}
fn stored_plan(extra: &Value) -> Option<String> {
    extra["id_token"].as_str().and_then(plan)
}

/// Browser sign-in on Codex's fixed loopback port, or the ChatGPT device flow.
pub static SPEC: SubscriptionSpec = SubscriptionSpec {
    provider: "openai",
    api_key: true,
    client_id: CLIENT_ID,
    client_secret: None,
    authorize: "https://auth.openai.com/oauth/authorize",
    console: None,
    token: "https://auth.openai.com/oauth/token",
    scopes: &["openid", "profile", "email", "offline_access"],
    params: &[("id_token_add_organizations", "true"), ("codex_cli_simplified_flow", "true"), ("originator", "codex_cli_rs")],
    nonce: false,
    callbacks: &[Callback::Loopback { host: "localhost", port: Port::Fixed(1455), path: "/auth/callback" }],
    device: Some(Device::Custom(CustomDevice { begin: device_code, poll: poll_device })),
    headers: None,
    exchange_encoding: TokenEncoding::Form,
    exchange_state: false,
    refresh: Refresh { encoding: TokenEncoding::Form, rotation: Rotation::Keep, rejected: is_refresh_rejection, rejected_message: None },
    credential,
    finish: None,
    plan: stored_plan,
};
#[must_use]
pub fn is_refresh_rejection(error: &LlmError) -> bool {
    error.class == ErrorClass::Auth
        || ["invalid_grant", "refresh_token_reused", "refresh_token_expired", "refresh_token_invalid"].iter().any(|s| error.message.contains(s))
}

fn device_code(cx: Cx<'_>) -> Fut<'_, DeviceCode> {
    Box::pin(async move {
        let value = device_request(cx.client, "usercode", json!({"client_id":CLIENT_ID}))
            .await?
            .ok_or_else(|| LlmError::new(ErrorClass::Auth, "missing device code"))?;
        let get = |key: &str| value[key].as_str().map(String::from).ok_or_else(|| LlmError::new(ErrorClass::Auth, "invalid device code response"));
        Ok(DeviceCode {
            verification_uri: format!("{ISSUER}/codex/device"),
            verification_uri_complete: None,
            user_code: get("user_code").or_else(|_| get("usercode"))?,
            device_code: get("device_auth_id")?,
            interval: Duration::from_secs(
                value["interval"].as_u64().or_else(|| value["interval"].as_str()?.parse().ok()).filter(|n| *n > 0).unwrap_or(5),
            ),
            expires_in: None,
        })
    })
}
fn poll_device<'a>(cx: Cx<'a>, code: &'a DeviceCode) -> Fut<'a, Poll> {
    Box::pin(async move {
        let Some(value) = device_request(cx.client, "token", json!({"device_auth_id":code.device_code,"user_code":code.user_code})).await? else {
            return Ok(Poll::Pending);
        };
        let get =
            |key: &str| value[key].as_str().filter(|s| !s.is_empty()).ok_or_else(|| LlmError::new(ErrorClass::Auth, "invalid device token response"));
        get("code_challenge")?;
        let redirect = format!("{ISSUER}/deviceauth/callback");
        flows::exchange(&SPEC, cx, get("authorization_code")?, get("code_verifier")?, &redirect, None).await.map(Poll::Done)
    })
}
async fn device_request(client: &reqwest::Client, path: &str, body: Value) -> Result<Option<Value>, LlmError> {
    let response = client
        .post(format!("{ISSUER}/api/accounts/deviceauth/{path}"))
        .timeout(Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .map_err(crate::http::network)?;
    if path == "token" && matches!(response.status().as_u16(), 403 | 404) {
        return Ok(None);
    }
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.map_err(crate::http::network)?;
    if !status.is_success() {
        return Err(crate::http::classify_status(status, &headers, &body));
    }
    serde_json::from_str(&body).map(Some).map_err(|_| LlmError::new(ErrorClass::Auth, "invalid device JSON"))
}
pub fn shape(body: &mut Value, session: Option<&str>) {
    // Codex HTTP is stateless and streaming-only.
    body["store"] = json!(false);
    body["stream"] = json!(true);
    super::convert::stateless_reasoning(body);
    // The backend requires an instructions string, even without a system prompt.
    if !body["instructions"].is_string() {
        body["instructions"] = json!("");
    }
    // Sampling and output caps are not accepted by the Codex endpoint.
    if let Some(object) = body.as_object_mut() {
        for key in ["max_output_tokens", "temperature", "top_p"] {
            object.remove(key);
        }
    }
    // TS strips input ids on the stateless Codex wire; canonical opaque items stay intact.
    if let Some(items) = body["input"].as_array_mut() {
        for item in items {
            if let Some(object) = item.as_object_mut() {
                object.remove("id");
            }
        }
    }
    // TS uses the session as the cache key, without injecting previous_response_id.
    if !body["prompt_cache_key"].is_string()
        && let Some(session) = session
    {
        body["prompt_cache_key"] = json!(session);
    }
}
pub fn headers(credential: &Credential, session: Option<&str>) -> Result<reqwest::header::HeaderMap, LlmError> {
    let Credential::OAuth { access, account_id, .. } = credential else {
        return Err(LlmError::new(ErrorClass::Auth, "subscription requires OAuth"));
    };
    static INSTALLATION: OnceLock<String> = OnceLock::new();
    let id = INSTALLATION.get_or_init(|| {
        let n = rand::random::<u128>();
        format!("{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}", (n >> 96) as u32, (n >> 80) as u16, (n >> 64) & 0xfff, (n >> 48) & 0xfff, n & 0xffffffffffff)
    });
    let mut headers = reqwest::header::HeaderMap::new();
    for (key, value) in [
        ("authorization", Some(format!("Bearer {access}"))),
        ("originator", Some("otto".into())),
        ("x-codex-installation-id", Some(id.clone())),
        ("chatgpt-account-id", account_id.clone()),
        ("session_id", session.map(Into::into)),
    ] {
        if let Some(value) = value {
            headers.insert(key, value.parse().map_err(|_| LlmError::new(ErrorClass::Auth, "invalid subscription header"))?);
        }
    }
    Ok(headers)
}
