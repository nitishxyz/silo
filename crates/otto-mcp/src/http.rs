//! Streamable HTTP: requests are POSTed (answered with JSON or an SSE stream), server
//! messages also arrive on a GET stream resumed with `Last-Event-ID`, and an OAuth
//! bearer token is sent when one is stored (refreshed once after a `401`).
use crate::{
    client::{CallError, Client, PROTOCOL_VERSION, Transport, lock},
    oauth::TokenStore,
    sse::Parser,
};
use futures::StreamExt;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Consecutive GET stream attempts without an event before otto stops listening.
const STREAM_ATTEMPTS: u32 = 5;

pub(crate) struct Http {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<Option<String>>,
    pub(crate) initialized: AtomicBool,
    /// `None` when the configuration sends its own `Authorization`.
    tokens: Option<Arc<TokenStore>>,
}

fn is_sse(response: &reqwest::Response) -> bool {
    response.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/event-stream"))
}

impl Http {
    pub(crate) fn new(url: String, headers: &BTreeMap<String, String>, tokens: Arc<TokenStore>) -> Self {
        let own_auth = headers.keys().any(|k| k.eq_ignore_ascii_case("authorization"));
        Self {
            client: reqwest::Client::new(),
            url,
            headers: headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            session: Mutex::new(None),
            initialized: AtomicBool::new(false),
            tokens: (!own_auth).then_some(tokens),
        }
    }

    async fn bearer(&self) -> Option<String> {
        self.tokens.as_ref()?.bearer(&self.url).await
    }

    fn builder(&self, method: Method, accept: &str, bearer: Option<&str>) -> reqwest::RequestBuilder {
        let mut request = self.client.request(method, &self.url).header("Accept", accept);
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(session) = lock(&self.session).clone() {
            request = request.header("Mcp-Session-Id", session);
        }
        if self.initialized.load(Ordering::SeqCst) {
            request = request.header("MCP-Protocol-Version", PROTOCOL_VERSION);
        }
        request
    }

    async fn post_once(&self, body: &str, bearer: Option<&str>) -> Result<reqwest::Response, CallError> {
        self.builder(Method::POST, "application/json, text/event-stream", bearer)
            .header("Content-Type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| CallError::Closed(e.to_string()))
    }

    /// End the session (`DELETE`).
    pub(crate) async fn end(&self) {
        if lock(&self.session).is_some() {
            let bearer = self.bearer().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), self.builder(Method::DELETE, "application/json", bearer.as_deref()).send()).await;
        }
    }
}

fn unauthorized(response: &reqwest::Response) -> bool {
    response.status() == StatusCode::UNAUTHORIZED
        || (response.status() == StatusCode::FORBIDDEN
            && response.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("insufficient_scope")))
}

impl Client {
    fn http(&self) -> Option<&Http> {
        match &self.transport {
            Transport::Http(http) => Some(http),
            Transport::Stdio { .. } => None,
        }
    }

    pub(crate) async fn post(self: &Arc<Self>, message: Value) -> Result<(), CallError> {
        let http = self.http().ok_or_else(|| CallError::Closed("not an HTTP server".into()))?;
        let had_session = lock(&http.session).is_some();
        let body = message.to_string();
        let bearer = http.bearer().await;
        let mut response = http.post_once(&body, bearer.as_deref()).await?;
        if unauthorized(&response) {
            let tokens =
                http.tokens.as_ref().ok_or_else(|| CallError::Closed(format!("HTTP {} (check the Authorization header)", response.status())))?;
            let Some(used) = &bearer else { return Err(CallError::NeedsAuth) };
            if !tokens.refresh(&http.url, used).await {
                return Err(CallError::NeedsAuth);
            }
            response = http.post_once(&body, http.bearer().await.as_deref()).await?;
            if unauthorized(&response) {
                return Err(CallError::NeedsAuth);
            }
        }
        if let Some(session) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *lock(&http.session) = Some(session.to_owned());
        }
        let status = response.status();
        if status == StatusCode::NOT_FOUND && had_session {
            self.close("session expired");
            return Err(CallError::Closed("session expired".into()));
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CallError::Closed(format!("HTTP {status}: {}", body.chars().take(300).collect::<String>())));
        }
        if status == StatusCode::ACCEPTED {
            return Ok(());
        }
        if !is_sse(&response) {
            let body = response.bytes().await.map_err(|e| CallError::Closed(e.to_string()))?;
            if body.is_empty() {
                return Ok(());
            }
            let value: Value = serde_json::from_slice(&body).map_err(|e| CallError::Closed(format!("invalid JSON from server: {e}")))?;
            for message in value.as_array().cloned().unwrap_or_else(|| vec![value]) {
                self.incoming(message).await;
            }
            return Ok(());
        }
        let mut stream = response.bytes_stream();
        let mut parser = Parser::default();
        while let Some(chunk) = stream.next().await {
            for event in parser.push(&chunk.map_err(|e| CallError::Closed(e.to_string()))?) {
                if let Ok(message) = serde_json::from_str::<Value>(&event.data) {
                    self.incoming(message).await;
                }
            }
        }
        Ok(())
    }
}

/// The GET notification stream, reopened with `Last-Event-ID` after a drop (waiting the
/// server's `retry`, 1 s by default). A `405` means the server offers none. Ends with the client.
pub(crate) async fn listen(weak: Weak<Client>) {
    let mut last_id: Option<String> = None;
    let mut delay = Duration::from_secs(1);
    let mut failures = 0;
    loop {
        let (response, stop) = {
            let Some(client) = weak.upgrade() else { return };
            let Some(http) = client.http().filter(|_| client.alive()) else { return };
            let bearer = http.bearer().await;
            let mut request = http.builder(Method::GET, "text/event-stream", bearer.as_deref());
            if let Some(id) = &last_id {
                request = request.header("Last-Event-ID", id.as_str());
            }
            let stop = client.stop.clone();
            drop(client);
            tokio::select! {
                response = request.send() => (response, stop),
                () = stop.cancelled() => return,
            }
        };
        let mut heard = false;
        match response {
            Ok(response) if response.status() == StatusCode::METHOD_NOT_ALLOWED => return,
            Ok(response) if response.status() == StatusCode::NOT_FOUND => {
                if let Some(client) = weak.upgrade() {
                    client.close("session expired");
                }
                return;
            }
            Ok(response) if response.status().is_success() && is_sse(&response) => {
                let mut stream = response.bytes_stream();
                let mut parser = Parser::default();
                loop {
                    let chunk = tokio::select! {
                        chunk = stream.next() => chunk,
                        () = stop.cancelled() => return,
                    };
                    let Some(Ok(chunk)) = chunk else { break };
                    for event in parser.push(&chunk) {
                        heard = true;
                        if let Some(id) = event.id {
                            last_id = Some(id).filter(|id| !id.is_empty());
                        }
                        if let Some(retry) = event.retry {
                            delay = Duration::from_millis(retry.clamp(50, 60_000));
                        }
                        if let Ok(message) = serde_json::from_str::<Value>(&event.data) {
                            let Some(client) = weak.upgrade() else { return };
                            client.incoming(message).await;
                        }
                    }
                }
            }
            Ok(response) => tracing::debug!(status=%response.status(), "MCP notification stream refused"),
            Err(error) => tracing::debug!(%error, "MCP notification stream failed"),
        }
        failures = if heard { 0 } else { failures + 1 };
        if failures >= STREAM_ATTEMPTS {
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {},
            () = stop.cancelled() => return,
        }
    }
}
