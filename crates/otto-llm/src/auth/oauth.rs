//! Provider-neutral OAuth primitives.
use crate::{
    ErrorClass, LlmError,
    http::{classify_status, network},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}
#[must_use]
pub fn pkce_pair() -> PkcePair {
    let verifier = random_state();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkcePair { verifier, challenge }
}
#[must_use]
pub fn random_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub struct LoopbackCallback {
    listener: TcpListener,
    port: u16,
    path: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizationCode {
    pub code: String,
    pub state: String,
}
impl LoopbackCallback {
    pub async fn bind(port: u16, path: impl Into<String>) -> Result<Self, LlmError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await.map_err(io_error)?;
        let path = path.into();
        if !path.starts_with('/') || path.contains(['?', '#']) {
            return Err(LlmError::new(ErrorClass::InvalidRequest, "invalid callback path"));
        }
        let port = listener.local_addr().map_err(io_error)?.port();
        Ok(Self { listener, port, path })
    }
    pub fn redirect_uri(&self) -> Result<String, LlmError> {
        Ok(format!("http://127.0.0.1:{}{}", self.port, self.path))
    }
    /// The bound loopback port (the requested one, or the system's pick for 0).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
    pub async fn wait(self, expected_state: &str, timeout: Duration) -> Result<AuthorizationCode, LlmError> {
        tokio::time::timeout(timeout, async {
            loop {
                let (mut socket, _) = self.listener.accept().await.map_err(io_error)?;
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|part| part == b"\r\n\r\n") && request.len() < 16384 {
                    let read = socket.read(&mut buffer).await.map_err(io_error)?;
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let text = String::from_utf8_lossy(&request);
                let result = parse_callback(&text, &self.path, expected_state);
                let (status, message) = if result.is_ok() {
                    ("200 OK", "Authorization complete. You can close this window.")
                } else {
                    ("400 Bad Request", "Invalid authorization callback.")
                };
                let body = format!("<!doctype html><html><body>{message}</body></html>");
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.map_err(io_error)?;
                if let Ok(code) = result {
                    return Ok(code);
                }
            }
        })
        .await
        .map_err(|_| LlmError::new(ErrorClass::Timeout, "OAuth callback timed out"))?
    }
}
fn parse_callback(request: &str, path: &str, expected: &str) -> Result<AuthorizationCode, LlmError> {
    let mut fields = request.lines().next().unwrap_or_default().split_whitespace();
    if fields.next() != Some("GET") {
        return Err(invalid_callback());
    }
    let target = fields.next().ok_or_else(invalid_callback)?;
    if !target.starts_with('/') || target.starts_with("//") {
        return Err(invalid_callback());
    }
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| invalid_callback())?;
    if url.path() != path {
        return Err(invalid_callback());
    }
    let mut code = None;
    let mut state = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" | "state" | "error" => return Err(invalid_callback()),
            _ => {}
        }
    }
    let state = state.filter(|state| state == expected).ok_or_else(invalid_callback)?;
    let code = code.filter(|code| !code.is_empty()).ok_or_else(invalid_callback)?;
    Ok(AuthorizationCode { code, state })
}
/// An authorization code the user pasted: the full callback URL, its query (`code=…&state=…`),
/// `code#state`, or, only with `bare_code`, the code alone. The state must match `expected`.
pub fn parse_pasted(input: &str, expected: &str, bare_code: bool) -> Result<AuthorizationCode, LlmError> {
    let input = input.trim();
    let query = if input.contains("://") {
        reqwest::Url::parse(input).map_err(|_| invalid_paste())?.query().map(String::from)
    } else if input.contains("code=") || input.contains("error=") {
        Some(input.trim_start_matches('?').to_owned())
    } else {
        None
    };
    let (code, state) = if let Some(query) = query {
        let url = reqwest::Url::parse(&format!("http://localhost/?{query}")).map_err(|_| invalid_paste())?;
        let (mut code, mut state) = (None, None);
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" if code.is_none() => code = Some(value.into_owned()),
                "state" if state.is_none() => state = Some(value.into_owned()),
                "error" => return Err(LlmError::new(ErrorClass::Auth, format!("authorization failed: {value}"))),
                "code" | "state" => return Err(invalid_paste()),
                _ => {}
            }
        }
        (code.unwrap_or_default(), state)
    } else if let Some((code, state)) = input.split_once('#') {
        (code.to_owned(), Some(state.to_owned()))
    } else {
        (input.to_owned(), None)
    };
    if code.is_empty() || state.as_deref().is_some_and(|s| s.is_empty() || s.contains('#')) {
        return Err(invalid_paste());
    }
    match state {
        Some(state) if state == expected => Ok(AuthorizationCode { code, state }),
        Some(_) => Err(LlmError::new(ErrorClass::Auth, "OAuth state mismatch; restart login")),
        None if bare_code => Ok(AuthorizationCode { code, state: expected.into() }),
        None => Err(invalid_paste()),
    }
}
fn invalid_paste() -> LlmError {
    LlmError::new(ErrorClass::Auth, "expected the callback URL or code#state")
}
fn invalid_callback() -> LlmError {
    LlmError::new(ErrorClass::Auth, "invalid OAuth callback or state")
}
fn io_error(error: std::io::Error) -> LlmError {
    LlmError::new(ErrorClass::Network, error.to_string())
}

/// Launch the platform browser, without making login depend on its availability.
pub fn open_browser(url: &str) {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return;
    }
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let url = url.to_owned();
    std::thread::spawn(move || {
        let _ = std::process::Command::new(program)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

#[derive(Clone, Copy, Debug)]
pub enum TokenEncoding {
    Form,
    Json,
}
#[derive(Clone)]
pub struct TokenResponse {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_in: u64,
    pub id_token: Option<String>,
    pub raw: serde_json::Value,
}

/// Extra request headers, e.g. a client fingerprint.
pub type Headers = reqwest::header::HeaderMap;
/// A raw HTTP reply, for endpoints whose error bodies carry protocol state.
pub struct Reply {
    pub status: reqwest::StatusCode,
    pub headers: reqwest::header::HeaderMap,
    pub body: String,
}
impl Reply {
    /// The body as an error, classified by status.
    #[must_use]
    pub fn error(&self) -> LlmError {
        classify_status(self.status, &self.headers, &self.body)
    }
}

pub async fn exchange_code(
    client: &reqwest::Client,
    url: &str,
    fields: &BTreeMap<String, String>,
    encoding: TokenEncoding,
    headers: &Headers,
) -> Result<TokenResponse, LlmError> {
    token_request(client, url, fields, "authorization_code", encoding, headers).await
}
pub async fn refresh_token(
    client: &reqwest::Client,
    url: &str,
    fields: &BTreeMap<String, String>,
    encoding: TokenEncoding,
    headers: &Headers,
) -> Result<TokenResponse, LlmError> {
    token_request(client, url, fields, "refresh_token", encoding, headers).await
}
async fn token_request(
    client: &reqwest::Client,
    url: &str,
    fields: &BTreeMap<String, String>,
    grant: &str,
    encoding: TokenEncoding,
    headers: &Headers,
) -> Result<TokenResponse, LlmError> {
    let mut fields = fields.clone();
    fields.insert("grant_type".into(), grant.into());
    let reply = post(client, url, &fields, encoding, headers).await?;
    if !reply.status.is_success() {
        return Err(reply.error());
    }
    parse_token(&reply.body)
}
/// POST `fields` (30 s timeout) and return the reply whatever its status.
pub async fn post(
    client: &reqwest::Client,
    url: &str,
    fields: &BTreeMap<String, String>,
    encoding: TokenEncoding,
    headers: &Headers,
) -> Result<Reply, LlmError> {
    let request = client.post(url).timeout(Duration::from_secs(30)).headers(headers.clone());
    let request = match encoding {
        TokenEncoding::Json => request.json(fields),
        TokenEncoding::Form => {
            let mut encoded = reqwest::Url::parse("http://localhost").expect("static URL");
            encoded.query_pairs_mut().extend_pairs(fields);
            request.header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(encoded.query().unwrap_or_default().to_owned())
        }
    };
    let response = request.send().await.map_err(network)?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await.map_err(network)?;
    Ok(Reply { status, headers, body })
}
pub(crate) fn parse_token(body: &str) -> Result<TokenResponse, LlmError> {
    let raw: serde_json::Value = serde_json::from_str(body).map_err(|_| LlmError::new(ErrorClass::Auth, "invalid token response JSON"))?;
    let access = raw
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LlmError::new(ErrorClass::Auth, "missing access_token"))?
        .into();
    let refresh = raw.get("refresh_token").and_then(|v| v.as_str()).map(String::from);
    let expires_in = raw.get("expires_in").and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())).unwrap_or(0);
    let id_token = raw.get("id_token").and_then(|v| v.as_str()).map(String::from);
    Ok(TokenResponse { access, refresh, expires_in, id_token, raw })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkce_and_callback() {
        let pair = pkce_pair();
        assert_eq!(pair.verifier.len(), 43);
        assert_eq!(pair.challenge, URL_SAFE_NO_PAD.encode(Sha256::digest(pair.verifier.as_bytes())));
        assert_ne!(random_state(), random_state());
        let valid = "GET /callback?code=a%2Bb&state=abc HTTP/1.1\r\n\r\n";
        assert_eq!(parse_callback(valid, "/callback", "abc").unwrap().code, "a+b");
        assert!(parse_callback(valid, "/other", "abc").is_err());
        assert!(parse_callback(valid, "/callback", "wrong").is_err());
        assert!(parse_callback("GET /callback?code=a&state=x&state=x HTTP/1.1", "/callback", "x").is_err());
    }
    #[test]
    fn pasted_input_forms() {
        let ok = |input: &str, bare| parse_pasted(input, "st", bare).map(|c| c.code);
        assert_eq!(ok("c%2Bd#st", false).unwrap(), "c%2Bd");
        assert_eq!(ok(" http://localhost:1455/auth/callback?code=a%2Bb&state=st \n", false).unwrap(), "a+b");
        assert_eq!(ok("https://x.test/cb?code=a&state=st&extra=1", false).unwrap(), "a");
        assert_eq!(ok("?code=a&state=st", false).unwrap(), "a");
        assert_eq!(ok("code=a&state=st", false).unwrap(), "a");
        assert_eq!(ok("plain", true).unwrap(), "plain");
        assert!(ok("plain", false).is_err());
        assert!(ok("a#wrong", false).unwrap_err().message.contains("state mismatch"));
        assert!(ok("http://x.test/cb?code=a&state=wrong", true).is_err());
        assert!(ok("http://x.test/cb?code=a&code=b&state=st", false).is_err());
        assert!(ok("a#st#st", false).is_err());
        assert!(ok("#st", false).is_err());
        assert!(ok("a#", false).is_err());
        assert!(ok("", true).is_err());
        assert!(ok("http://x.test/cb?error=access_denied&state=st", false).unwrap_err().message.contains("access_denied"));
    }
    #[tokio::test]
    async fn loopback_reports_its_port_and_path() {
        let callback = LoopbackCallback::bind(0, "/auth/callback").await.unwrap();
        assert_ne!(callback.port(), 0);
        assert_eq!(callback.path(), "/auth/callback");
        assert_eq!(callback.redirect_uri().unwrap(), format!("http://127.0.0.1:{}/auth/callback", callback.port()));
    }
    #[test]
    fn token_fields() {
        let token = parse_token(r#"{"access_token":"a","expires_in":"3600","id_token":"id"}"#).unwrap();
        assert_eq!(token.expires_in, 3600);
        assert!(token.refresh.is_none());
        assert!(parse_token("{}").is_err());
    }
}
