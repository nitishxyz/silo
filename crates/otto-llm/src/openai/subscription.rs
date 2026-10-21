//! ChatGPT/Codex OAuth and endpoint-specific shaping.
use crate::{
    ErrorClass, LlmError,
    auth::{
        Credential, RefreshFn, now_ms,
        oauth::{self, LoopbackCallback, TokenEncoding, TokenResponse},
    },
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
    time::Duration,
};
pub const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

#[must_use]
pub fn account_id(token: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value["https://api.openai.com/auth"]["chatgpt_account_id"].as_str().map(Into::into)
}
fn credential(token: TokenResponse, previous: Option<Credential>) -> Credential {
    let (old_refresh, old_account) = match previous {
        Some(Credential::OAuth { refresh, account_id, .. }) => (refresh, account_id),
        _ => (String::new(), None),
    };
    let account_id = token.id_token.as_deref().and_then(account_id).or_else(|| account_id(&token.access)).or(old_account);
    Credential::OAuth {
        access: token.access,
        refresh: token.refresh.unwrap_or(old_refresh),
        expires_at_ms: now_ms().saturating_add(if token.expires_in == 0 { 3600 } else { token.expires_in }.saturating_mul(1000)),
        account_id,
        extra: json!({"id_token":token.id_token}),
    }
}
pub struct Login {
    pub url: String,
    verifier: String,
    state: String,
    callback: LoopbackCallback,
}
impl Login {
    pub async fn begin() -> Result<Self, LlmError> {
        let callback = LoopbackCallback::bind(1455, "/auth/callback").await?;
        let pair = oauth::pkce_pair();
        let state = oauth::random_state();
        let mut url = reqwest::Url::parse(&format!("{ISSUER}/oauth/authorize")).expect("static URL");
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", &pair.challenge),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", &state),
            ("originator", "codex_cli_rs"),
        ]);
        Ok(Self { url: url.into(), verifier: pair.verifier, state, callback })
    }
    pub async fn finish(self, client: &reqwest::Client) -> Result<Credential, LlmError> {
        let code = self.callback.wait(&self.state, Duration::from_secs(300)).await?;
        exchange(client, &code.code, &self.verifier, REDIRECT_URI).await
    }
}
pub async fn exchange(client: &reqwest::Client, code: &str, verifier: &str, redirect: &str) -> Result<Credential, LlmError> {
    let fields = BTreeMap::from([
        ("client_id".into(), CLIENT_ID.into()),
        ("code".into(), code.into()),
        ("code_verifier".into(), verifier.into()),
        ("redirect_uri".into(), redirect.into()),
    ]);
    Ok(credential(oauth::exchange_code(client, &format!("{ISSUER}/oauth/token"), &fields, TokenEncoding::Form).await?, None))
}
#[must_use]
pub fn refresh_fn() -> RefreshFn {
    Arc::new(|previous| {
        Box::pin(async move {
            let Credential::OAuth { refresh, .. } = &previous else {
                return Err(LlmError::new(ErrorClass::Auth, "expected OAuth credential"));
            };
            let fields = BTreeMap::from([("client_id".into(), CLIENT_ID.into()), ("refresh_token".into(), refresh.clone())]);
            let token = oauth::refresh_token(&reqwest::Client::new(), &format!("{ISSUER}/oauth/token"), &fields, TokenEncoding::Form).await.map_err(
                |mut e| {
                    if is_refresh_rejection(&e) {
                        e.class = ErrorClass::Auth;
                    }
                    e
                },
            )?;
            Ok(credential(token, Some(previous)))
        })
    })
}
#[must_use]
pub fn is_refresh_rejection(error: &LlmError) -> bool {
    error.class == ErrorClass::Auth
        || ["invalid_grant", "refresh_token_reused", "refresh_token_expired", "refresh_token_invalid"].iter().any(|s| error.message.contains(s))
}

pub struct DeviceCode {
    pub verification_uri: String,
    pub user_code: String,
    pub device_auth_id: String,
    pub interval: Duration,
}
pub async fn device_code(client: &reqwest::Client) -> Result<DeviceCode, LlmError> {
    let value = device_request(client, "usercode", json!({"client_id":CLIENT_ID}))
        .await?
        .ok_or_else(|| LlmError::new(ErrorClass::Auth, "missing device code"))?;
    let get = |key: &str| value[key].as_str().map(String::from).ok_or_else(|| LlmError::new(ErrorClass::Auth, "invalid device code response"));
    Ok(DeviceCode {
        verification_uri: format!("{ISSUER}/codex/device"),
        user_code: get("user_code").or_else(|_| get("usercode"))?,
        device_auth_id: get("device_auth_id")?,
        interval: Duration::from_secs(
            value["interval"].as_u64().or_else(|| value["interval"].as_str()?.parse().ok()).filter(|n| *n > 0).unwrap_or(5),
        ),
    })
}
pub async fn poll_device(client: &reqwest::Client, code: &DeviceCode) -> Result<Option<Credential>, LlmError> {
    let Some(value) = device_request(client, "token", json!({"device_auth_id":code.device_auth_id,"user_code":code.user_code})).await? else {
        return Ok(None);
    };
    let get =
        |key: &str| value[key].as_str().filter(|s| !s.is_empty()).ok_or_else(|| LlmError::new(ErrorClass::Auth, "invalid device token response"));
    get("code_challenge")?;
    exchange(client, get("authorization_code")?, get("code_verifier")?, &format!("{ISSUER}/deviceauth/callback")).await.map(Some)
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
