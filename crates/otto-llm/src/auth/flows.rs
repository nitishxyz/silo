//! Subscription sign-in driven by one [`SubscriptionSpec`] per provider: a generic PKCE flow
//! (loopback or pasted callback), an RFC 8628 device flow, and refresh. A new provider is a new
//! spec value plus a row in [`SPECS`].
use super::{
    Credential, RefreshFn, now_ms,
    oauth::{self, AuthorizationCode, LoopbackCallback, TokenEncoding, TokenResponse},
};
use crate::{ErrorClass, LlmError, http::provider_message};
use reqwest::header::HeaderMap;
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T, LlmError>> + Send + 'a>>;
/// What every flow call needs besides the spec: the HTTP client, and the state
/// directory, where a [`HeaderFn`] may keep a device id.
#[derive(Clone, Copy)]
pub struct Cx<'a> {
    pub client: &'a reqwest::Client,
    pub dir: &'a Path,
}
/// Per-call headers for token and device requests (a client fingerprint, a device id under the state directory, …).
pub type HeaderFn = fn(&Path) -> Result<HeaderMap, LlmError>;
/// Turns a token response into the stored credential; `previous` is set on refresh.
pub type Build = fn(&TokenResponse, Option<&Credential>) -> Result<Credential, LlmError>;
/// Runs after every token exchange or refresh (a profile lookup, Google's project setup).
pub type Finish = for<'a> fn(Cx<'a>, Credential, Option<&'a Credential>) -> Fut<'a, Credential>;
/// The subscription plan label, from a credential's `extra`.
pub type PlanFn = fn(&serde_json::Value) -> Option<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    Fixed(u16),
    /// Let the system pick; the redirect URI carries the bound port.
    Any,
}
#[derive(Clone, Copy, Debug)]
pub enum Callback {
    /// Listen on `127.0.0.1`; the registered redirect is `http://{host}:{port}{path}`.
    Loopback { host: &'static str, port: Port, path: &'static str },
    /// The provider shows a code or a callback URL to paste. `bare_code` accepts a code without state.
    Paste { redirect: &'static str, bare_code: bool },
}
/// Whether a refresh token response must carry a new refresh token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    /// A response without one keeps the previous refresh token.
    Keep,
    /// The previous one is spent; a response without a new one is an error.
    Required,
}
#[derive(Clone, Copy)]
pub struct Refresh {
    pub encoding: TokenEncoding,
    pub rotation: Rotation,
    /// Whether a failed refresh means the refresh token is dead (sign in again). Other failures
    /// stay as they are, except that a 401/403 that is not a rejection is no auth error.
    pub rejected: fn(&LlmError) -> bool,
    /// Replaces the provider's message when rejected.
    pub rejected_message: Option<&'static str>,
}
/// 401/403 means the refresh token was rejected.
#[must_use]
pub fn rejected_on_auth_status(error: &LlmError) -> bool {
    error.class == ErrorClass::Auth
}
/// Only an OAuth `invalid_grant` means the refresh token was rejected.
#[must_use]
pub fn rejected_on_invalid_grant(error: &LlmError) -> bool {
    error.message.contains("invalid_grant")
}

pub enum Device {
    /// RFC 8628: POST `url` (form `client_id`, scope, `params`), poll the spec's token URL.
    Rfc8628 {
        url: &'static str,
        params: &'static [(&'static str, &'static str)],
    },
    Custom(CustomDevice),
}
/// A device flow that is not RFC 8628 (ChatGPT's).
pub struct CustomDevice {
    pub begin: for<'a> fn(Cx<'a>) -> Fut<'a, DeviceCode>,
    pub poll: for<'a> fn(Cx<'a>, &'a DeviceCode) -> Fut<'a, Poll>,
}
pub enum Poll {
    Pending,
    /// The server asks for a slower pace (+5 s).
    SlowDown,
    Done(Credential),
}
pub struct DeviceCode {
    pub verification_uri: String,
    /// The URI with the code filled in, when the provider offers one (what the browser opens).
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    /// The provider's opaque handle for polling.
    pub device_code: String,
    pub interval: Duration,
    pub expires_in: Option<Duration>,
}
impl DeviceCode {
    /// What to open in a browser.
    #[must_use]
    pub fn url(&self) -> &str {
        self.verification_uri_complete.as_deref().unwrap_or(&self.verification_uri)
    }
}

pub struct SubscriptionSpec {
    pub provider: &'static str,
    /// The provider also takes an API key; without, sign-in is subscription only.
    pub api_key: bool,
    pub client_id: &'static str,
    pub client_secret: Option<&'static str>,
    /// The browser authorization URL; empty for device-only providers.
    pub authorize: &'static str,
    /// An alternate authorization URL for [`crate::providers::LoginMode::Console`] accounts.
    pub console: Option<&'static str>,
    pub token: &'static str,
    /// Joined with spaces into `scope`; none, no parameter.
    pub scopes: &'static [&'static str],
    /// Extra authorization parameters (`access_type=offline`, …).
    pub params: &'static [(&'static str, &'static str)],
    /// Send a random `nonce` with the authorization request.
    pub nonce: bool,
    /// Browser callbacks, preferred first; none for device-only providers.
    pub callbacks: &'static [Callback],
    pub device: Option<Device>,
    /// Extra headers on every token and device request.
    pub headers: Option<HeaderFn>,
    /// Encoding of the code exchange (device requests are always form-encoded).
    pub exchange_encoding: TokenEncoding,
    /// Send the authorization `state` with the code exchange.
    pub exchange_state: bool,
    pub refresh: Refresh,
    pub credential: Build,
    pub finish: Option<Finish>,
    pub plan: PlanFn,
}
impl SubscriptionSpec {
    fn headers(&self, dir: &Path) -> Result<HeaderMap, LlmError> {
        self.headers.map_or_else(|| Ok(HeaderMap::new()), |f| f(dir))
    }
    fn fields(&self) -> BTreeMap<String, String> {
        let mut fields = BTreeMap::from([("client_id".to_owned(), self.client_id.to_owned())]);
        if let Some(secret) = self.client_secret {
            fields.insert("client_secret".into(), secret.into());
        }
        fields
    }
}

/// Every provider with a subscription sign-in.
pub static SPECS: &[&SubscriptionSpec] = &[
    &crate::anthropic::subscription::SPEC,
    &crate::openai::subscription::SPEC,
    &crate::compat::kimi::SPEC,
    &crate::google::codeassist::SPEC,
    &crate::openai::xai::SPEC,
];
#[must_use]
pub fn spec(provider: &str) -> Option<&'static SubscriptionSpec> {
    SPECS.iter().copied().find(|spec| spec.provider == provider)
}

/// Expiry in ms from now; a response without `expires_in` counts as an hour.
#[must_use]
pub fn expires_at_ms(token: &TokenResponse) -> u64 {
    now_ms().saturating_add(if token.expires_in == 0 { 3600 } else { token.expires_in }.saturating_mul(1000))
}
/// The plain mapping: tokens only, no account or extra. A refresh keeps what `previous` held.
pub fn plain_credential(token: &TokenResponse, previous: Option<&Credential>) -> Result<Credential, LlmError> {
    let (old_refresh, account_id, extra) = match previous {
        Some(Credential::OAuth { refresh, account_id, extra, .. }) => (Some(refresh.clone()), account_id.clone(), extra.clone()),
        _ => (None, None, serde_json::Value::Null),
    };
    let refresh = token.refresh.clone().or(old_refresh).ok_or_else(|| LlmError::new(ErrorClass::Auth, "missing OAuth refresh token"))?;
    Ok(Credential::OAuth { access: token.access.clone(), refresh, expires_at_ms: expires_at_ms(token), account_id, extra })
}
/// For providers that report no plan.
#[must_use]
pub fn no_plan(_: &serde_json::Value) -> Option<String> {
    None
}

/// A token response as the stored credential, through the spec's mapping and `finish` hook.
pub async fn complete(spec: &SubscriptionSpec, cx: Cx<'_>, token: &TokenResponse, previous: Option<&Credential>) -> Result<Credential, LlmError> {
    let credential = (spec.credential)(token, previous)?;
    match spec.finish {
        Some(finish) => finish(cx, credential, previous).await,
        None => Ok(credential),
    }
}
/// Exchange an authorization code. `state` is sent only when the spec asks for it.
pub async fn exchange(
    spec: &SubscriptionSpec,
    cx: Cx<'_>,
    code: &str,
    verifier: &str,
    redirect: &str,
    state: Option<&str>,
) -> Result<Credential, LlmError> {
    let mut fields = spec.fields();
    fields.extend([("code".into(), code.into()), ("code_verifier".into(), verifier.into()), ("redirect_uri".into(), redirect.into())]);
    if let (true, Some(state)) = (spec.exchange_state, state) {
        fields.insert("state".into(), state.into());
    }
    let token = oauth::exchange_code(cx.client, spec.token, &fields, spec.exchange_encoding, &spec.headers(cx.dir)?).await?;
    complete(spec, cx, &token, None).await
}

fn rejection(spec: &SubscriptionSpec, mut error: LlmError) -> LlmError {
    if (spec.refresh.rejected)(&error) {
        error.class = ErrorClass::Auth;
        if let Some(message) = spec.refresh.rejected_message {
            error.message = message.into();
        }
    } else if error.class == ErrorClass::Auth {
        error.class = ErrorClass::Other;
    }
    error
}
/// Refresh a stored credential. A rejected refresh token is an auth error; anything else is not.
pub async fn refresh(spec: &SubscriptionSpec, dir: &Path, previous: Credential) -> Result<Credential, LlmError> {
    let Credential::OAuth { refresh, .. } = &previous else {
        return Err(LlmError::new(ErrorClass::Auth, "expected OAuth credential"));
    };
    let client = reqwest::Client::new();
    let cx = Cx { client: &client, dir };
    let mut fields = spec.fields();
    fields.insert("refresh_token".into(), refresh.clone());
    let token =
        oauth::refresh_token(&client, spec.token, &fields, spec.refresh.encoding, &spec.headers(dir)?).await.map_err(|e| rejection(spec, e))?;
    if spec.refresh.rotation == Rotation::Required && token.refresh.is_none() {
        return Err(LlmError::new(ErrorClass::Auth, "refresh response has no new refresh token"));
    }
    complete(spec, cx, &token, Some(&previous)).await
}
/// [`refresh`] as the credential store's hook; `dir` is the state directory.
#[must_use]
pub fn refresh_fn(spec: &'static SubscriptionSpec, dir: PathBuf) -> RefreshFn {
    Arc::new(move |previous| {
        let dir = dir.clone();
        Box::pin(async move { refresh(spec, &dir, previous).await })
    })
}

const LOOPBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// A PKCE authorization-code login: the browser comes back to a loopback port, or the user
/// pastes the code or callback URL. Either way `finish` takes the pasted input when given.
pub struct Login {
    pub url: String,
    spec: &'static SubscriptionSpec,
    verifier: String,
    state: String,
    redirect: String,
    bare_code: bool,
    listener: Option<LoopbackCallback>,
}
impl Login {
    /// # Errors
    /// Returns an error if the loopback port cannot be bound or `console` is not offered.
    pub async fn begin(spec: &'static SubscriptionSpec, callback: &Callback, console: bool) -> Result<Self, LlmError> {
        let authorize = match (console, spec.console) {
            (false, _) => spec.authorize,
            (true, Some(url)) => url,
            (true, None) => return Err(LlmError::new(ErrorClass::InvalidRequest, "login mode is not supported by this provider")),
        };
        let (redirect, listener, bare_code) = match *callback {
            Callback::Loopback { host, port, path } => {
                let listener = LoopbackCallback::bind(if let Port::Fixed(port) = port { port } else { 0 }, path).await?;
                (format!("http://{host}:{}{path}", listener.port()), Some(listener), false)
            }
            Callback::Paste { redirect, bare_code } => (redirect.to_owned(), None, bare_code),
        };
        let pair = oauth::pkce_pair();
        let state = oauth::random_state();
        let scope = spec.scopes.join(" ");
        let mut url = reqwest::Url::parse(authorize).map_err(|_| LlmError::new(ErrorClass::InvalidRequest, "invalid authorization URL"))?;
        {
            let mut query = url.query_pairs_mut();
            query.extend_pairs([
                ("response_type", "code"),
                ("client_id", spec.client_id),
                ("redirect_uri", &redirect),
                ("code_challenge", &pair.challenge),
                ("code_challenge_method", "S256"),
                ("state", &state),
            ]);
            if !scope.is_empty() {
                query.append_pair("scope", &scope);
            }
            if spec.nonce {
                query.append_pair("nonce", &oauth::random_state());
            }
            query.extend_pairs(spec.params.iter().copied());
        }
        Ok(Self { url: url.into(), spec, verifier: pair.verifier, state, redirect, bare_code, listener })
    }
    /// The bound loopback listener, for loopback logins.
    #[must_use]
    pub fn callback(&self) -> Option<&LoopbackCallback> {
        self.listener.as_ref()
    }
    /// The user has to paste something: there is no listener to wait on.
    #[must_use]
    pub fn needs_paste(&self) -> bool {
        self.listener.is_none()
    }
    /// The code from the loopback callback, or from `input` (callback URL, `code#state`) when not empty.
    /// Dropping `self` frees the loopback port.
    pub async fn finish(self, cx: Cx<'_>, input: &str) -> Result<Credential, LlmError> {
        let AuthorizationCode { code, state } = match self.listener {
            Some(listener) if input.trim().is_empty() => listener.wait(&self.state, LOOPBACK_TIMEOUT).await?,
            _ => oauth::parse_pasted(input, &self.state, self.bare_code)?,
        };
        exchange(self.spec, cx, &code, &self.verifier, &self.redirect, Some(&state)).await
    }
}

const DEVICE_TIMEOUT: Duration = Duration::from_secs(900);
const SLOW_DOWN: Duration = Duration::from_secs(5);
const GRANT_DEVICE: &str = "urn:ietf:params:oauth:grant-type:device_code";
fn device_unsupported() -> LlmError {
    LlmError::new(ErrorClass::InvalidRequest, "login mode is not supported by this provider")
}
fn device_invalid() -> LlmError {
    LlmError::new(ErrorClass::Auth, "invalid device code response")
}
/// Start a device login.
/// # Errors
/// Returns an error if the provider has no device flow or answers badly.
pub async fn begin_device(spec: &SubscriptionSpec, cx: Cx<'_>) -> Result<DeviceCode, LlmError> {
    match spec.device.as_ref().ok_or_else(device_unsupported)? {
        Device::Custom(custom) => (custom.begin)(cx).await,
        Device::Rfc8628 { url, params } => {
            let mut fields = spec.fields();
            fields.remove("client_secret");
            if !spec.scopes.is_empty() {
                fields.insert("scope".into(), spec.scopes.join(" "));
            }
            fields.extend(params.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
            let reply = oauth::post(cx.client, url, &fields, TokenEncoding::Form, &spec.headers(cx.dir)?).await?;
            if !reply.status.is_success() {
                return Err(reply.error());
            }
            parse_device_code(&reply.body)
        }
    }
}
fn parse_device_code(body: &str) -> Result<DeviceCode, LlmError> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| device_invalid())?;
    let text = |key: &str| value[key].as_str().filter(|s| !s.is_empty()).map(String::from);
    let number = |key: &str| value[key].as_u64().or_else(|| value[key].as_str()?.parse().ok()).filter(|n| *n > 0);
    Ok(DeviceCode {
        verification_uri: text("verification_uri").or_else(|| text("verification_uri_complete")).ok_or_else(device_invalid)?,
        verification_uri_complete: text("verification_uri_complete"),
        user_code: text("user_code").ok_or_else(device_invalid)?,
        device_code: text("device_code").ok_or_else(device_invalid)?,
        interval: Duration::from_secs(number("interval").unwrap_or(5)),
        expires_in: number("expires_in").map(Duration::from_secs),
    })
}

enum Step {
    Token(TokenResponse),
    Pending,
    SlowDown,
}
/// One poll answer: a token, "keep waiting", or the end of the login.
fn poll_reply(reply: &oauth::Reply) -> Result<Step, LlmError> {
    if reply.status.is_success() {
        return oauth::parse_token(&reply.body).map(Step::Token);
    }
    let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap_or_default();
    match value["error"].as_str() {
        Some("authorization_pending") => Ok(Step::Pending),
        Some("slow_down") => Ok(Step::SlowDown),
        Some("access_denied") => Err(LlmError::new(ErrorClass::Auth, "sign-in was denied")),
        Some("expired_token") => Err(LlmError::new(ErrorClass::Timeout, "device code expired")),
        _ => Err(LlmError { message: provider_message(&reply.body), ..reply.error() }),
    }
}
/// One poll of the provider's token endpoint.
pub async fn poll_device_once(spec: &SubscriptionSpec, cx: Cx<'_>, code: &DeviceCode) -> Result<Poll, LlmError> {
    if let Some(Device::Custom(custom)) = &spec.device {
        return (custom.poll)(cx, code).await;
    }
    let mut fields = spec.fields();
    fields.extend([("grant_type".into(), GRANT_DEVICE.into()), ("device_code".into(), code.device_code.clone())]);
    let reply = oauth::post(cx.client, spec.token, &fields, TokenEncoding::Form, &spec.headers(cx.dir)?).await?;
    match poll_reply(&reply)? {
        Step::Token(token) => complete(spec, cx, &token, None).await.map(Poll::Done),
        Step::Pending => Ok(Poll::Pending),
        Step::SlowDown => Ok(Poll::SlowDown),
    }
}
/// Poll until the user approves (every `interval`, +5 s per `slow_down`), the code expires or
/// 15 minutes pass. Dropping the future cancels it.
pub async fn poll_device(spec: &SubscriptionSpec, cx: Cx<'_>, code: &DeviceCode) -> Result<Credential, LlmError> {
    let limit = code.expires_in.map_or(DEVICE_TIMEOUT, |d| d.min(DEVICE_TIMEOUT));
    drive(code.interval, limit, SLOW_DOWN, || poll_device_once(spec, cx, code)).await
}
async fn drive<F, Fut>(mut interval: Duration, limit: Duration, slow_down: Duration, mut poll: F) -> Result<Credential, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Poll, LlmError>>,
{
    tokio::time::timeout(limit, async {
        loop {
            match poll().await? {
                Poll::Done(credential) => return Ok(credential),
                Poll::Pending => {}
                Poll::SlowDown => interval += slow_down,
            }
            tokio::time::sleep(interval).await;
        }
    })
    .await
    .map_err(|_| LlmError::new(ErrorClass::Timeout, "device login timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    /// Serves one canned `(status, body)` per connection and returns the raw requests it saw.
    async fn serve(replies: Vec<(u16, &'static str)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                loop {
                    let mut buf = [0; 4096];
                    let read = socket.read(&mut buf).await.unwrap();
                    raw.extend_from_slice(&buf[..read]);
                    if read == 0 {
                        break;
                    }
                    let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                    let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length: ")).map_or(0, |n| n.trim().parse().unwrap());
                    if raw.len() >= end + 4 + length {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&raw).into_owned());
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        (url, task)
    }
    fn leak(text: String) -> &'static str {
        Box::leak(text.into_boxed_str())
    }
    fn test_headers(dir: &Path) -> Result<HeaderMap, LlmError> {
        assert_eq!(dir, Path::new(DIR));
        Ok(HeaderMap::from_iter([(reqwest::header::HeaderName::from_static("x-test"), reqwest::header::HeaderValue::from_static("yes"))]))
    }
    const DIR: &str = "/nonexistent/otto-state";
    fn cx(client: &reqwest::Client) -> Cx<'_> {
        Cx { client, dir: Path::new(DIR) }
    }
    fn rejected_on_gone(error: &LlmError) -> bool {
        error.status == Some(410)
    }
    /// A spec against the mock server: loopback on any port, a secret, a nonce, extra params and headers.
    fn test_spec(base: &str, rotation: Rotation, rejected: fn(&LlmError) -> bool) -> &'static SubscriptionSpec {
        Box::leak(Box::new(SubscriptionSpec {
            provider: "test",
            api_key: true,
            client_id: "client",
            client_secret: Some("secret"),
            authorize: "https://auth.test/authorize",
            console: None,
            token: leak(format!("{base}/token")),
            scopes: &["openid", "offline"],
            params: &[("access_type", "offline"), ("prompt", "consent")],
            nonce: true,
            callbacks: &[
                Callback::Loopback { host: "localhost", port: Port::Any, path: "/cb" },
                Callback::Paste { redirect: "https://paste.test/cb", bare_code: true },
            ],
            device: Some(Device::Rfc8628 { url: leak(format!("{base}/device")), params: &[("referrer", "test")] }),
            headers: Some(test_headers),
            exchange_encoding: TokenEncoding::Form,
            exchange_state: false,
            refresh: Refresh { encoding: TokenEncoding::Form, rotation, rejected, rejected_message: Some("sign in again") },
            credential: plain_credential,
            finish: None,
            plan: no_plan,
        }))
    }
    fn stored(refresh: &str) -> Credential {
        Credential::OAuth {
            access: "old".into(),
            refresh: refresh.into(),
            expires_at_ms: 0,
            account_id: Some("acct".into()),
            extra: serde_json::json!({"k":1}),
        }
    }
    fn oauth(credential: &Credential) -> (&str, &str, &serde_json::Value) {
        let Credential::OAuth { access, refresh, extra, .. } = credential else { panic!("not oauth") };
        (access, refresh, extra)
    }

    #[tokio::test]
    async fn loopback_login_takes_a_pasted_callback_url() {
        let (base, server) = serve(vec![(200, r#"{"access_token":"a","refresh_token":"r","expires_in":60}"#)]).await;
        let spec = test_spec(&base, Rotation::Keep, rejected_on_auth_status);
        let login = Login::begin(spec, &spec.callbacks[0], false).await.unwrap();
        let port = login.callback().unwrap().port();
        assert!(!login.needs_paste());
        let url = reqwest::Url::parse(&login.url).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["redirect_uri"], format!("http://localhost:{port}/cb"));
        assert_eq!(query["scope"], "openid offline");
        assert_eq!((query["access_type"].as_str(), query["prompt"].as_str()), ("offline", "consent"));
        assert_eq!((query["client_id"].as_str(), query["code_challenge_method"].as_str()), ("client", "S256"));
        assert!(!query["nonce"].is_empty() && !query["code_challenge"].is_empty());
        let pasted = format!("http://localhost:{port}/cb?code=the-code&state={}", query["state"]);
        let credential = login.finish(cx(&reqwest::Client::new()), &pasted).await.unwrap();
        assert_eq!(oauth(&credential).0, "a");
        let request = server.await.unwrap().remove(0).to_lowercase();
        assert!(request.contains("x-test: yes") && request.contains("content-type: application/x-www-form-urlencoded"));
        for field in ["grant_type=authorization_code", "code=the-code", "client_secret=secret", "client_id=client", "code_verifier="] {
            assert!(request.contains(field), "{field}");
        }
        assert!(request.contains(&format!("redirect_uri=http%3a%2f%2flocalhost%3a{port}%2fcb")));
        assert!(!request.contains("&state="));
    }

    #[tokio::test]
    async fn paste_login_checks_state_and_console_needs_support() {
        let spec = test_spec("http://127.0.0.1:1", Rotation::Keep, rejected_on_auth_status);
        let login = Login::begin(spec, &spec.callbacks[1], false).await.unwrap();
        assert!(login.needs_paste() && login.callback().is_none());
        assert!(login.url.contains("redirect_uri=https%3A%2F%2Fpaste.test%2Fcb"));
        assert!(login.finish(cx(&reqwest::Client::new()), "code#wrong").await.unwrap_err().message.contains("state mismatch"));
        assert_eq!(Login::begin(spec, &spec.callbacks[1], true).await.err().unwrap().class, ErrorClass::InvalidRequest);
    }

    #[tokio::test]
    async fn fixed_port_is_exclusive() {
        let first = LoopbackCallback::bind(0, "/cb").await.unwrap();
        let spec = test_spec("http://127.0.0.1:1", Rotation::Keep, rejected_on_auth_status);
        let busy = Callback::Loopback { host: "127.0.0.1", port: Port::Fixed(first.port()), path: "/cb" };
        assert!(Login::begin(spec, &busy, false).await.is_err());
    }

    #[tokio::test]
    async fn refresh_rotation_and_rejection() {
        let (base, server) = serve(vec![
            (200, r#"{"access_token":"a1","refresh_token":"r1","expires_in":60}"#),
            (200, r#"{"access_token":"a2","expires_in":60}"#),
            (200, r#"{"access_token":"a3"}"#),
            (401, r#"{"error":"nope"}"#),
            (410, r#"{"error":"gone"}"#),
            (403, "cloudflare"),
            (500, "oops"),
        ])
        .await;
        let keep = test_spec(&base, Rotation::Keep, rejected_on_auth_status);
        let rotated = refresh(keep, Path::new(DIR), stored("r0")).await.unwrap();
        assert_eq!((oauth(&rotated).0, oauth(&rotated).1, oauth(&rotated).2["k"].as_i64()), ("a1", "r1", Some(1)));
        // Keep: a response without a refresh token keeps the old one.
        assert_eq!(oauth(&refresh(keep, Path::new(DIR), stored("r0")).await.unwrap()).1, "r0");
        let required = test_spec(&base, Rotation::Required, rejected_on_auth_status);
        let error = refresh(required, Path::new(DIR), stored("r0")).await.unwrap_err();
        assert!(error.class == ErrorClass::Auth && error.message.contains("new refresh token"));
        // 401/403 means rejected here, with the spec's message.
        let error = refresh(keep, Path::new(DIR), stored("r0")).await.unwrap_err();
        assert!(error.class == ErrorClass::Auth && error.message == "sign in again");
        // Here only 410 does: a 403 is not an auth error, and a 500 stays a server error.
        let gone = test_spec(&base, Rotation::Keep, rejected_on_gone);
        assert_eq!(refresh(gone, Path::new(DIR), stored("r0")).await.unwrap_err().class, ErrorClass::Auth);
        assert_eq!(refresh(gone, Path::new(DIR), stored("r0")).await.unwrap_err().class, ErrorClass::Other);
        assert_eq!(refresh(gone, Path::new(DIR), stored("r0")).await.unwrap_err().class, ErrorClass::Server);
        assert!(refresh(keep, Path::new(DIR), Credential::ApiKey { key: "k".into() }).await.is_err());
        let seen = server.await.unwrap();
        assert!(seen[0].contains("grant_type=refresh_token") && seen[0].contains("refresh_token=r0") && seen[0].contains("client_secret=secret"));
        assert!(seen[0].to_lowercase().contains("x-test: yes"));
    }

    #[tokio::test]
    async fn device_flow_polls_through_pending_and_slow_down() {
        let (base, server) = serve(vec![
            (200, r#"{"device_code":"dc","user_code":"ABCD","verification_uri":"https://v.test/device","verification_uri_complete":"https://v.test/device?c=ABCD","interval":"7","expires_in":300}"#),
            (400, r#"{"error":"authorization_pending"}"#),
            (200, r#"{"access_token":"tok","refresh_token":"ref","expires_in":3600}"#),
        ])
        .await;
        let spec = test_spec(&base, Rotation::Keep, rejected_on_auth_status);
        let client = reqwest::Client::new();
        let mut code = begin_device(spec, cx(&client)).await.unwrap();
        assert_eq!((code.user_code.as_str(), code.device_code.as_str()), ("ABCD", "dc"));
        assert_eq!((code.interval, code.expires_in), (Duration::from_secs(7), Some(Duration::from_secs(300))));
        assert_eq!((code.verification_uri.as_str(), code.url()), ("https://v.test/device", "https://v.test/device?c=ABCD"));
        code.interval = Duration::from_millis(5);
        let credential = poll_device(spec, cx(&client), &code).await.unwrap();
        assert_eq!((oauth(&credential).0, oauth(&credential).1), ("tok", "ref"));
        let seen = server.await.unwrap();
        let begin = seen[0].to_lowercase();
        assert!(begin.starts_with("post /device") && begin.contains("x-test: yes"));
        assert!(
            begin.contains("client_id=client")
                && begin.contains("scope=openid+offline")
                && begin.contains("referrer=test")
                && !begin.contains("client_secret")
        );
        assert!(seen[1].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code") && seen[1].contains("device_code=dc"));
    }

    #[test]
    fn device_replies_and_codes() {
        let reply = |status: u16, body: &str| oauth::Reply {
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            headers: reqwest::header::HeaderMap::new(),
            body: body.into(),
        };
        let poll = |status, body| poll_reply(&reply(status, body));
        assert!(matches!(poll(400, r#"{"error":"authorization_pending"}"#), Ok(Step::Pending)));
        assert!(matches!(poll(400, r#"{"error":"slow_down"}"#), Ok(Step::SlowDown)));
        assert!(matches!(poll(200, r#"{"access_token":"a"}"#), Ok(Step::Token(_))));
        let denied = poll(400, r#"{"error":"access_denied"}"#).err().unwrap();
        assert_eq!((denied.class, denied.message.as_str()), (ErrorClass::Auth, "sign-in was denied"));
        assert_eq!(poll(400, r#"{"error":"expired_token"}"#).err().unwrap().class, ErrorClass::Timeout);
        let other = poll(400, r#"{"error":"invalid_client","error_description":"bad"}"#).err().unwrap();
        assert_eq!((other.class, other.message.as_str()), (ErrorClass::InvalidRequest, "invalid_client: bad"));
        assert_eq!(poll(500, "down").err().unwrap().class, ErrorClass::Server);
        let code = parse_device_code(r#"{"device_code":"d","user_code":"u","verification_uri_complete":"https://v.test/?c=u"}"#).unwrap();
        assert_eq!((code.verification_uri.as_str(), code.interval), ("https://v.test/?c=u", Duration::from_secs(5)));
        assert!(parse_device_code(r#"{"user_code":"u","verification_uri":"x"}"#).is_err());
        assert!(parse_device_code("nope").is_err());
    }

    #[tokio::test]
    async fn poller_backs_off_and_times_out() {
        let done = || Credential::ApiKey { key: "k".into() };
        let mut replies = vec![Poll::Pending, Poll::SlowDown, Poll::Done(done())].into_iter();
        let started = std::time::Instant::now();
        let step = Duration::from_millis(40);
        let credential =
            drive(Duration::from_millis(1), Duration::from_secs(5), step, || std::future::ready(Ok(replies.next().unwrap()))).await.unwrap();
        assert_eq!(credential, done());
        // pending, then slow_down raises the pause by one step before the last poll
        assert!(started.elapsed() >= step);
        let error = drive(Duration::from_millis(1), Duration::from_millis(30), step, || std::future::ready(Ok(Poll::Pending))).await.unwrap_err();
        assert_eq!(error.class, ErrorClass::Timeout);
        let error =
            drive(Duration::from_millis(1), Duration::from_secs(5), step, || std::future::ready(Err(LlmError::new(ErrorClass::Auth, "denied"))))
                .await
                .unwrap_err();
        assert_eq!(error.message, "denied");
    }

    #[test]
    fn spec_table_and_plain_credentials() {
        assert!(spec("anthropic").is_some() && spec("openai").is_some() && spec("nope").is_none());
        assert!(spec("anthropic").unwrap().device.is_none() && spec("openai").unwrap().device.is_some());
        // Kimi Code: device flow only, kimi-cli headers on token calls, rotating refresh tokens.
        let kimi = spec("kimi-code").unwrap();
        assert!(!kimi.api_key && kimi.callbacks.is_empty() && kimi.scopes.is_empty() && kimi.client_secret.is_none());
        assert!(matches!(kimi.device, Some(Device::Rfc8628 { url: "https://auth.kimi.com/api/oauth/device_authorization", params: &[] })));
        assert_eq!((kimi.client_id, kimi.token), ("17e5f671-d194-4dfb-9706-5516cb48c098", "https://auth.kimi.com/api/oauth/token"));
        assert_eq!(kimi.refresh.rotation, Rotation::Required);
        assert!((kimi.refresh.rejected)(&LlmError::new(ErrorClass::Auth, "")) && !(kimi.refresh.rejected)(&LlmError::new(ErrorClass::Server, "")));
        let dir = std::env::temp_dir().join(format!("otto-kimi-{}", rand::random::<u64>()));
        let headers = kimi.headers(&dir).unwrap();
        assert_eq!((headers["x-msh-platform"].to_str().unwrap(), headers.contains_key("x-msh-device-id")), ("kimi_cli", true));
        let _ = std::fs::remove_dir_all(&dir);
        let token = |refresh: Option<&str>| TokenResponse {
            access: "a".into(),
            refresh: refresh.map(Into::into),
            expires_in: 0,
            id_token: None,
            raw: serde_json::Value::Null,
        };
        assert!(plain_credential(&token(None), None).is_err());
        let kept = plain_credential(&token(None), Some(&stored("old"))).unwrap();
        assert_eq!((oauth(&kept).1, oauth(&kept).2["k"].as_i64()), ("old", Some(1)));
        let Credential::OAuth { expires_at_ms, .. } = plain_credential(&token(Some("n")), None).unwrap() else { panic!() };
        assert!(expires_at_ms > now_ms() + 3_500_000);
        assert!(rejected_on_auth_status(&LlmError::new(ErrorClass::Auth, "")) && !rejected_on_auth_status(&LlmError::new(ErrorClass::Server, "")));
        assert!(rejected_on_invalid_grant(&LlmError::new(ErrorClass::InvalidRequest, r#"{"error":"invalid_grant"}"#)));
    }
}
