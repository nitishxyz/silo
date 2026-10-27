#![cfg(feature = "client")]
//! xAI with a Grok Build sign-in: routing, the Grok CLI fingerprint, no ChatGPT shaping, billing usage, the login spec.
use otto_llm::{
    auth::{Credential, CredentialSource, flows, now_ms, oauth::TokenEncoding},
    http::{HttpClient, RetryPolicy},
    openai::{self, xai},
    *,
};
use serde_json::{Value, json};
use std::time::Duration;

const TEXT: &str = include_str!("fixtures/openai/text.sse");

fn info(id: &str) -> ModelInfo {
    ModelInfo { id: id.into(), provider: "xai".into(), name: id.into(), caps: Caps::default(), pricing: None, list_pricing: None }
}
fn signed_in() -> CredentialSource {
    CredentialSource::Static(Credential::OAuth {
        access: "access".into(),
        refresh: "refresh".into(),
        expires_at_ms: now_ms() + 3_600_000,
        account_id: Some("acct".into()),
        extra: Value::Null,
    })
}

#[test]
fn proxy_models_get_the_grok_cli_fingerprint() {
    for id in ["grok-build", "grok-composer-2.5-fast", "xai/Grok-Build"] {
        let (base, headers) = xai::route(id);
        assert_eq!(base, "https://cli-chat-proxy.grok.com/v1", "{id}");
        assert_eq!(headers["x-xai-token-auth"], "xai-grok-cli");
        assert_eq!(headers["x-authenticateresponse"], "authenticate-response");
        assert_eq!(headers["x-grok-client-identifier"], "grok-shell");
        assert_eq!(headers["x-grok-client-version"], "0.2.22");
        assert_eq!(headers["x-grok-model-override"], id);
        let agent = headers["user-agent"].to_str().unwrap();
        assert!(agent.starts_with("grok-shell/0.2.22 (") && agent.ends_with(')'), "{agent}");
        assert!(!agent.contains("darwin") && !agent.contains("arm64") && !agent.contains("x86_64"), "node names: {agent}");
    }
    for id in ["grok-4.5", "grok-4.6", "grok-build-0.1"] {
        let (base, headers) = xai::route(id);
        assert_eq!(base, "https://api.x.ai/v1", "{id}");
        assert!(headers.is_empty(), "{id}");
    }
}

/// One HTTP request: its head (lowercased) and JSON body; answers with [`TEXT`].
async fn serve_once() -> (std::net::SocketAddr, tokio::task::JoinHandle<(String, Value)>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let (head, end) = loop {
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buf[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let size: usize = head.lines().find_map(|line| line.strip_prefix("content-length: ").map(|s| s.parse().unwrap())).unwrap();
                if bytes.len() >= end + 4 + size {
                    break (head, end);
                }
            }
        };
        let body = serde_json::from_slice(&bytes[end + 4..]).unwrap();
        let reply =
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{TEXT}", TEXT.len());
        socket.write_all(reply.as_bytes()).await.unwrap();
        (head, body)
    });
    (address, task)
}

async fn call(mode: openai::AuthMode, id: &str, auth: CredentialSource) -> (String, Value) {
    let (address, task) = serve_once().await;
    let model = openai::OpenAiModel::new(openai::OpenAiConfig {
        base_url: format!("http://{address}/v1"),
        auth,
        mode,
        model: info(id),
        http: HttpClient::new().unwrap(),
        retry: RetryPolicy::default(),
        idle_timeout: Duration::from_secs(5),
        organization: None,
        project: None,
    })
    .with_headers(xai::route(id).1);
    let req = Request { max_output: Some(100), ..Request::default() };
    let cx = CallCx { session: Some("sess".into()), ..CallCx::default() };
    let events = testing::collect_events(model.stream(req, cx).await.unwrap()).await;
    assert!(matches!(events.last(), Some(StreamEvent::Finish { reason: FinishReason::Stop, .. })));
    task.await.unwrap()
}

#[tokio::test]
async fn signed_in_xai_gets_no_chatgpt_shaping() {
    let (head, body) = call(openai::AuthMode::Subscription, "grok-build", signed_in()).await;
    assert!(head.starts_with("post /v1/responses "), "{head}");
    assert!(head.contains("\r\nauthorization: bearer access"));
    assert!(head.contains("\r\nx-xai-token-auth: xai-grok-cli"));
    assert!(head.contains("\r\nx-grok-client-identifier: grok-shell"));
    assert!(head.contains("\r\nuser-agent: grok-shell/0.2.22 ("));
    for chatgpt in ["chatgpt-account-id", "originator", "x-codex-installation-id", "session_id", "openai-beta"] {
        assert!(!head.contains(&format!("\r\n{chatgpt}:")), "{chatgpt} sent to xAI");
    }
    // The session is the cache key, as old otto does; the Codex body rules stay off.
    assert_eq!(body["prompt_cache_key"], "sess");
    assert_eq!(body["max_output_tokens"], 100);
    assert_eq!(body["instructions"], "", "the plain converter's system prompt: {body}");

    let (head, body) = call(openai::AuthMode::Subscription, "grok-4.6", signed_in()).await;
    assert!(head.contains("\r\nauthorization: bearer access"));
    assert!(!head.contains("x-xai-token-auth") && !head.contains("grok-shell") && !head.contains("chatgpt-account-id"));
    assert_eq!(body["prompt_cache_key"], "sess");
}

#[tokio::test]
async fn xai_api_keys_are_unchanged() {
    let (head, body) = call(openai::AuthMode::ApiKey, "grok-4.6", CredentialSource::api_key("key")).await;
    assert!(head.contains("\r\nauthorization: bearer key"));
    assert!(body.get("prompt_cache_key").is_none(), "{body}");
}

#[test]
fn billing_credits_as_one_window() {
    let body = json!({"config": {
        "creditUsagePercent": 42.5,
        "currentPeriod": {"start": "2026-10-01T00:00:00Z", "end": "2026-11-01T00:00:00Z"},
        "onDemandCap": {"val": 0},
    }});
    let usage = xai::parse_usage(&body).unwrap();
    assert_eq!(usage.windows.len(), 1);
    let window = &usage.windows[0];
    assert_eq!((window.name.as_str(), window.label.as_str()), ("credits", "Credits"));
    assert!((window.used - 0.425).abs() < 1e-6);
    assert_eq!(window.resets_at, Some(1_793_491_200));
    // Clamped; the legacy period field; no period at all.
    let over = xai::parse_usage(&json!({"config": {"creditUsagePercent": 140, "billingPeriodEnd": "2026-11-01T00:00:00Z"}})).unwrap();
    assert_eq!((over.windows[0].used, over.windows[0].resets_at), (1.0, Some(1_793_491_200)));
    assert_eq!(xai::parse_usage(&json!({"config": {"creditUsagePercent": 0}})).unwrap().windows[0].resets_at, None);
    assert!(xai::parse_usage(&json!({"config": {}})).is_none());
    assert!(xai::parse_usage(&json!({"config": {"creditUsagePercent": "12"}})).is_none());
}

#[test]
fn login_spec() {
    let spec = &xai::SPEC;
    assert_eq!((spec.provider, spec.client_id, spec.client_secret), ("xai", "b1a00492-073a-47ea-816f-4c329264a828", None));
    assert!(spec.api_key && spec.nonce && !spec.exchange_state);
    assert_eq!((spec.authorize, spec.token), ("https://auth.x.ai/oauth2/authorize", "https://auth.x.ai/oauth2/token"));
    assert_eq!(
        spec.scopes.join(" "),
        "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write workspaces:read workspaces:write"
    );
    assert!(spec.params.is_empty());
    assert!(matches!(spec.callbacks, [flows::Callback::Loopback { host: "127.0.0.1", port: flows::Port::Fixed(56121), path: "/callback" }]));
    let Some(flows::Device::Rfc8628 { url, params }) = &spec.device else { panic!("device flow") };
    assert_eq!((*url, *params), ("https://auth.x.ai/oauth2/device/code", &[("referrer", "grok-build")][..]));
    assert!(matches!(spec.exchange_encoding, TokenEncoding::Form) && matches!(spec.refresh.encoding, TokenEncoding::Form));
    assert_eq!(spec.refresh.rotation, flows::Rotation::Keep);
    assert!((spec.refresh.rejected)(&LlmError::new(ErrorClass::InvalidRequest, "invalid_grant")));
    let headers = (spec.headers.unwrap())(std::path::Path::new("/nonexistent")).unwrap();
    assert_eq!((headers["x-grok-client-surface"].to_str().unwrap(), headers["x-grok-client-version"].to_str().unwrap()), ("ui", "1.0.3"));
}
