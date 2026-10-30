//! One browser sign-in: register a client, send the user to the authorization endpoint
//! with PKCE S256 and the `resource` parameter, and exchange the code that comes back
//! to a loopback redirect (or is pasted) for tokens.
use super::{TokenStore, credential, discover};
use otto_llm::auth::oauth::{self, LoopbackCallback};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

pub const CALLBACK_PATH: &str = "/callback";

pub struct Flow {
    /// Where the user signs in.
    pub url: String,
    pub state: String,
    pub port: u16,
    pub path: String,
    callback: Option<LoopbackCallback>,
    server_url: String,
    verifier: String,
    redirect_uri: String,
    client_id: String,
    client_secret: Option<String>,
    token_endpoint: String,
    resource: String,
    scope: Option<String>,
}

/// What a `401` from `server_url` says about signing in (empty when it does not answer `401`).
pub async fn probe(http: &reqwest::Client, server_url: &str, headers: &[(String, String)]) -> discover::Challenge {
    let mut request = http
        .post(server_url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(15))
        .body(
            json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":crate::client::PROTOCOL_VERSION,
            "capabilities":{},"clientInfo":{"name":"otto","version":env!("CARGO_PKG_VERSION")}}})
            .to_string(),
        );
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    match request.send().await {
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
            response.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).map(discover::challenge).unwrap_or_default()
        }
        _ => discover::Challenge::default(),
    }
}

pub async fn begin(tokens: &TokenStore, server_url: &str, headers: &[(String, String)]) -> Result<Flow, String> {
    let http = &tokens.http;
    let challenge = probe(http, server_url, headers).await;
    let metadata = discover::discover(http, server_url, &challenge).await?;
    let registration = metadata
        .registration_endpoint
        .clone()
        .ok_or("the server's sign-in does not allow registering otto (no dynamic client registration); set an Authorization header instead")?;
    let callback = LoopbackCallback::bind(0, CALLBACK_PATH).await.map_err(|e| e.to_string())?;
    let redirect_uri = callback.redirect_uri().map_err(|e| e.to_string())?;
    let mut body = json!({"client_name":"otto","redirect_uris":[redirect_uri],"grant_types":["authorization_code","refresh_token"],
        "response_types":["code"],"token_endpoint_auth_method":"none"});
    if let Some(scope) = &metadata.scope {
        body["scope"] = json!(scope);
    }
    let response =
        http.post(&registration).json(&body).timeout(Duration::from_secs(30)).send().await.map_err(|e| format!("client registration: {e}"))?;
    let status = response.status();
    let registered: Value = response.json().await.unwrap_or_default();
    let client_id = registered["client_id"]
        .as_str()
        .filter(|_| status.is_success())
        .ok_or_else(|| format!("client registration failed (HTTP {status}): {}", registered["error_description"].as_str().unwrap_or("no client_id")))?
        .to_owned();
    let pkce = oauth::pkce_pair();
    let state = oauth::random_state();
    let mut url = reqwest::Url::parse(&metadata.authorization_endpoint).map_err(|e| format!("invalid authorization endpoint: {e}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("resource", &metadata.resource);
    if let Some(scope) = &metadata.scope {
        url.query_pairs_mut().append_pair("scope", scope);
    }
    Ok(Flow {
        url: url.into(),
        state,
        port: callback.port(),
        path: callback.path().to_owned(),
        callback: Some(callback),
        server_url: server_url.to_owned(),
        verifier: pkce.verifier,
        redirect_uri,
        client_id,
        client_secret: registered["client_secret"].as_str().map(String::from),
        token_endpoint: metadata.token_endpoint,
        resource: metadata.resource,
        scope: metadata.scope,
    })
}

impl Flow {
    /// The loopback the browser returns to; taken once, by whoever waits for it.
    pub fn take_callback(&mut self) -> Option<LoopbackCallback> {
        self.callback.take()
    }

    /// Exchange `code` and store the tokens for the server URL.
    pub async fn finish(&self, tokens: &TokenStore, code: &str) -> Result<(), String> {
        let mut fields = BTreeMap::from([
            ("code".to_owned(), code.to_owned()),
            ("code_verifier".to_owned(), self.verifier.clone()),
            ("redirect_uri".to_owned(), self.redirect_uri.clone()),
            ("client_id".to_owned(), self.client_id.clone()),
            ("resource".to_owned(), self.resource.clone()),
        ]);
        if let Some(secret) = &self.client_secret {
            fields.insert("client_secret".into(), secret.clone());
        }
        let token = oauth::exchange_code(&tokens.http, &self.token_endpoint, &fields, oauth::TokenEncoding::Form, &Default::default())
            .await
            .map_err(|e| format!("sign-in failed: {e}"))?;
        let extra = json!({"client_id":self.client_id,"client_secret":self.client_secret,"token_endpoint":self.token_endpoint,
            "resource":self.resource,"scope":self.scope});
        tokens.set(&self.server_url, credential(&token, None, None, extra))
    }
}
