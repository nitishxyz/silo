use otto_proto::*;
use serde_json::json;

#[test]
fn debug_redacts_credentials_without_changing_serialization() {
    let methods = [
        Method::AuthLogin {
            provider: "openai".into(),
            mode: AuthMode::ApiKey,
            api_key: Some("secret-api-key".into()),
            device: false,
            base_url: None,
            paste: false,
        },
        Method::AuthLoginComplete { login_id: "login".into(), code: Some("secret-login-code".into()) },
        Method::McpLoginComplete { login_id: "login".into(), code: Some("secret-mcp-code".into()) },
        Method::McpAdd {
            project_root: None,
            scope: McpScope::Global,
            name: "web".into(),
            transport: McpTransport::Http { url: "https://x.test".into(), headers: [("Authorization".into(), "secret-header".into())].into() },
            enabled: true,
        },
        Method::McpUpdate {
            project_root: None,
            scope: McpScope::Global,
            name: "local".into(),
            transport: Some(McpTransport::Stdio { command: "x".into(), args: vec![], env: [("TOKEN".into(), "secret-env".into())].into() }),
            enabled: None,
        },
    ];
    for (method, secret) in methods.into_iter().zip(["secret-api-key", "secret-login-code", "secret-mcp-code", "secret-header", "secret-env"]) {
        assert!(!format!("{method:?}").contains(secret));
        let request = Request { id: 1, method };
        assert!(!format!("{request:?}").contains(secret));
        assert!(format!("{request:?}").contains("REDACTED"));
        assert!(serde_json::to_string(&request).unwrap().contains(secret));
    }
    let mut hello = ClientHello::ours("test", ClientRole::Owner);
    hello.token = Some("secret-hello-token".into());
    assert!(!format!("{hello:?}").contains("secret-hello-token"));
    assert!(format!("{hello:?}").contains("REDACTED"));
    assert_eq!(serde_json::to_value(hello).unwrap()["token"], json!("secret-hello-token"));
}

#[test]
fn malformed_live_payloads_are_skipped_but_envelopes_are_validated() {
    let mut wire = json!({"live":true,"id":0,"session_id":"s","after_seq":2,"event":{"event":"future.delta","data":{}}});
    assert!(matches!(serde_json::from_value::<ServerMessage>(wire.clone()).unwrap(), ServerMessage::LiveUnknown { after_seq: 2, .. }));
    wire["event"] = json!({"event":"text.delta","data":{"text":7}});
    assert!(matches!(serde_json::from_value::<ServerMessage>(wire.clone()).unwrap(), ServerMessage::LiveUnknown { .. }));
    wire["session_id"] = json!(null);
    assert!(serde_json::from_value::<ServerMessage>(wire).is_err());
}
