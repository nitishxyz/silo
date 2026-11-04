//! Provided tools through `Tools`: listed with Silo's own, called through
//! the server, answered by `provider::serve`, and gone with their provider.

use serde_json::json;
use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    paths,
    tools::{ProvidedTool, ToolOutcome},
};
use silo_tools::{Context, Part, Tools, provider};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn tool(name: &str, read_only: bool) -> ProvidedTool {
    ProvidedTool {
        name: name.into(),
        description: format!("{name} things."),
        input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}, "additionalProperties": false}),
        read_only,
        approval: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn provided_tools_are_listed_and_called_like_silos_own() {
    let home = tempfile::Builder::new().prefix("silo-tools-provided-").tempdir_in("/tmp").unwrap();
    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    while !silo_client::server_running(home.path()).await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let socket = paths::socket(home.path());
    let (control, events) = Control::connect(&socket, "agent").await.unwrap();
    let tools = Tools::new(control.clone(), events, Context { agent_block: None, ..Default::default() });
    let own = silo_tools::descriptors().len();
    assert_eq!(tools.all_descriptors().await.unwrap().len(), own, "nothing provided yet");
    let mut changes = tools.changes();

    // One of Silo's own names is refused.
    let (taken, taken_events) = Control::connect(&socket, "taker").await.unwrap();
    let refused =
        provider::serve(taken, taken_events, None, vec![tool("session_list", true)], provider::handler(|_| async { ToolOutcome::text("") }), || {});
    assert!(format!("{:#}", refused.await.unwrap_err()).contains("Silo's own"));

    // A provider that upper-cases its input.
    let (provider_control, provider_events) = Control::connect(&socket, "shouter").await.unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let handler = provider::handler(|request| async move {
        let text = request.input["text"].as_str().unwrap_or_default().to_uppercase();
        if text.is_empty() { ToolOutcome::error("no text") } else { ToolOutcome::text(text) }
    });
    let serving = tokio::spawn(provider::serve(provider_control, provider_events, None, vec![tool("shout", true)], handler, move || {
        let _ = ready_tx.send(());
    }));
    ready_rx.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), changes.changed()).await.unwrap().unwrap();

    let all = tools.all_descriptors().await.unwrap();
    assert_eq!(all.len(), own + 1);
    let shout = all.iter().find(|d| d.name == "shout").unwrap();
    assert!(shout.deferred && shout.effects.read && !shout.effects.write);
    assert!(shout.description.contains("shouter"), "{}", shout.description);
    assert!(shout.input_schema["properties"].get("block_id").is_none(), "a server tool needs no block");

    let result = tools.call("shout", json!({"text": "hi"}), CancellationToken::new()).await.unwrap();
    assert!(!result.is_error);
    assert!(matches!(&result.content[..], [Part::Text(t)] if t == "HI"), "{:?}", result.content);
    let failed = tools.call("shout", json!({}), CancellationToken::new()).await.unwrap();
    assert!(failed.is_error);
    assert!(tools.call("whisper", json!({}), CancellationToken::new()).await.is_err(), "unknown");

    // The provider goes: so do its tools.
    serving.abort();
    let _ = serving.await;
    tokio::time::timeout(Duration::from_secs(10), changes.changed()).await.unwrap().unwrap();
    assert_eq!(tools.all_descriptors().await.unwrap().len(), own);
    server.abort();
}

#[test]
fn block_tools_take_a_block_id() {
    use silo_proto::tools::ToolEntry;
    let entry = |block: Option<&str>| ToolEntry {
        tool: tool("peek", true),
        block_id: block.map(str::to_owned),
        block_kind: None,
        provider: "p".into(),
        provider_id: 1,
    };
    let described = silo_tools::provided_descriptors(&[entry(Some("b1")), entry(Some("b2"))], &[]);
    assert_eq!(described.len(), 1, "one per name");
    let schema = &described[0].input_schema;
    assert_eq!(schema["properties"]["block_id"]["type"], "string");
    assert!(schema["properties"]["block_id"]["description"].as_str().unwrap().contains("b1, b2"));
    assert!(schema["properties"]["text"].is_object(), "the provider's own input stays");
}
