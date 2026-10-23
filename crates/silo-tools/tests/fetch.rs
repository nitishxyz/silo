use serde_json::json;
use silo_client::{Control, transport::RelayConfig};
use silo_proto::{control::RpcError, paths};
use silo_tools::{Context, Tools, fetch_params};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn browser_fetch_descriptor() {
    let fetch = silo_tools::descriptors().into_iter().find(|t| t.name == "browser_fetch").unwrap();
    assert!(fetch.deferred && fetch.effects.read && !fetch.effects.write && !fetch.effects.process);
    let schema = &fetch.input_schema;
    assert_eq!(schema["required"], json!(["url"]));
    assert_eq!(schema["additionalProperties"], json!(false));
    for (field, kind) in [("url", "string"), ("timeout_ms", "integer"), ("screenshot", "boolean"), ("selector", "string")] {
        assert_eq!(schema["properties"][field]["type"], kind, "{field}");
    }
    assert_eq!(schema["properties"].as_object().unwrap().len(), 4);
}

#[test]
fn browser_fetch_input_is_validated() {
    let params = fetch_params(&json!({"url": " https://example.com/a ", "timeout_ms": 999_999, "screenshot": true, "selector": " main "})).unwrap();
    assert_eq!(
        (params.url.as_str(), params.timeout_ms, params.screenshot, params.selector.as_deref()),
        ("https://example.com/a", Some(silo_tools::FETCH_TIMEOUT_MAX), Some(true), Some("main"))
    );
    let bare = fetch_params(&json!({"url": "HTTP://example.com"})).unwrap();
    assert_eq!((bare.timeout_ms, bare.screenshot, bare.selector), (None, None, None));
    for bad in [
        json!({}),
        json!({"url": 3}),
        json!({"url": "example.com"}),
        json!({"url": "file:///etc/passwd"}),
        json!({"url": "javascript:alert(1)"}),
        json!({"url": "https://"}),
        json!({"url": "https://a.b", "timeout_ms": -1}),
        json!({"url": "https://a.b", "timeout_ms": "5"}),
        json!({"url": "https://a.b", "screenshot": "yes"}),
        json!({"url": "https://a.b", "selector": " "}),
    ] {
        assert!(fetch_params(&bad).is_err(), "{bad}");
    }
}

/// A server without the app: `no_app`, as a typed error and as the tool's.
#[tokio::test(flavor = "multi_thread")]
async fn browser_fetch_without_the_app_is_no_app() {
    let home = tempfile::Builder::new().prefix("silo-tools-fetch-").tempdir_in("/tmp").unwrap();
    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    while !silo_client::server_running(home.path()).await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (control, events) = Control::connect(&paths::socket(home.path()), "tools-test").await.unwrap();
    let tools = Tools::new(control.clone(), events, Context { agent_block: None, ..Default::default() });
    let error = tools.fetch(&json!({"url": "https://example.com"})).await.unwrap_err();
    assert_eq!(error.downcast_ref::<RpcError>().unwrap().code, RpcError::NO_APP);
    let error = tools.call("browser_fetch", json!({"url": "https://example.com"}), CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.downcast_ref::<RpcError>().unwrap().code, RpcError::NO_APP);
    assert!(tools.call("browser_fetch", json!({"url": "ftp://x.y"}), CancellationToken::new()).await.is_err());
    control.call_value(silo_proto::control::Method::ServerShutdown { force: true }).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), server).await;
}
