//! The MCP client against the in-repo stdio fixture server, and MCP tools
//! inside the otto runtime (offline: scripted model).
use otto_mcp::{Mcp, Status};
use otto_tools::{Loading, ProjectSources, Tool, ToolCx, ToolErrorKind};
use serde_json::json;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

const FIXTURE: &str = env!("CARGO_BIN_EXE_otto-mcp-fixture");

fn project(extra_tools: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let config = json!({"mcpServers":{"fixture":{"command":FIXTURE,"env":{"OTTO_MCP_FIXTURE_TOOLS":extra_tools.to_string()}}}});
    std::fs::write(dir.path().join(".mcp.json"), config.to_string()).unwrap();
    dir
}

async fn tool(mcp: &Mcp, root: &Path, name: &str) -> Arc<dyn Tool> {
    mcp.tools(root).await.into_iter().find(|t| t.spec().name == name).unwrap_or_else(|| panic!("{name} missing"))
}

#[tokio::test]
async fn stdio_tools_are_namespaced_sanitized_and_callable() {
    let dir = project(0);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let tools = mcp.tools(dir.path()).await;
    let mut names: Vec<_> = tools.iter().map(|t| t.spec().name.clone()).collect();
    names.sort();
    assert_eq!(names, ["mcp__fixture__crash", "mcp__fixture__echo", "mcp__fixture__grow", "mcp__fixture__slow"]);
    let echo = tool(&mcp, dir.path(), "mcp__fixture__echo").await;
    let spec = echo.spec();
    assert!(spec.effects.read_only());
    assert_eq!(spec.loading, Loading::Eager);
    assert_eq!(spec.source, otto_tools::Source::Mcp { server: "fixture".into() });
    assert!(spec.input_schema.get("anyOf").is_none() && spec.input_schema.get("$schema").is_none());
    assert_eq!(spec.input_schema["required"], json!(["text"]));
    assert_eq!(spec.input_schema["properties"]["tags"]["items"], json!({}));
    assert!(!tool(&mcp, dir.path(), "mcp__fixture__slow").await.spec().effects.read_only());
    let output = echo.call(json!({"text":"hi"}), ToolCx::new("c1", dir.path())).await.unwrap();
    assert_eq!(output.joined_text(), "hi");
    assert!(!output.is_error);
    mcp.shutdown().await;
}

#[tokio::test]
async fn list_changed_relists_and_crash_restarts() {
    let dir = project(0);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let grow = tool(&mcp, dir.path(), "mcp__fixture__grow").await;
    assert_eq!(grow.call(json!({}), ToolCx::new("c1", dir.path())).await.unwrap().joined_text(), "grown");
    assert!(mcp.tools(dir.path()).await.iter().any(|t| t.spec().name == "mcp__fixture__extra"));
    let crash = tool(&mcp, dir.path(), "mcp__fixture__crash").await;
    assert!(crash.call(json!({}), ToolCx::new("c2", dir.path())).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The restarted server lists its initial tools again.
    let echo = tool(&mcp, dir.path(), "mcp__fixture__echo").await;
    assert_eq!(echo.call(json!({"text":"back"}), ToolCx::new("c3", dir.path())).await.unwrap().joined_text(), "back");
    assert!(!mcp.tools(dir.path()).await.iter().any(|t| t.spec().name == "mcp__fixture__extra"));
    mcp.shutdown().await;
}

#[tokio::test]
async fn cancel_reaches_the_server() {
    let dir = project(0);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let slow = tool(&mcp, dir.path(), "mcp__fixture__slow").await;
    let cx = ToolCx::new("c1", dir.path());
    let cancel = cx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let error = tokio::time::timeout(Duration::from_secs(5), slow.call(json!({}), cx)).await.unwrap().unwrap_err();
    assert_eq!(error.kind, ToolErrorKind::Cancelled);
    mcp.shutdown().await;
}

#[tokio::test]
async fn many_tools_are_deferred_and_bad_configs_are_skipped() {
    let dir = project(12);
    let mut config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap()).unwrap();
    config["mcpServers"]["broken"] = json!({"command":"/nonexistent/otto-mcp-server"});
    config["mcpServers"]["off"] = json!({"command":FIXTURE,"disabled":true});
    config["mcpServers"]["unset"] = json!({"command":"${OTTO_MCP_TEST_NEVER_SET}"});
    std::fs::write(dir.path().join(".mcp.json"), config.to_string()).unwrap();
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let tools = mcp.tools(dir.path()).await;
    assert_eq!(tools.len(), 16);
    assert!(tools.iter().all(|t| t.spec().loading == Loading::Deferred && t.spec().name.starts_with("mcp__fixture__")));
    let listed = mcp.list(Some(dir.path())).await;
    let state = |name: &str| listed.iter().find(|s| s.name == name).unwrap().clone();
    assert_eq!((state("fixture").status, state("fixture").tools), (Status::Connected, 16));
    assert!(!state("off").enabled && state("fixture").enabled);
    assert!(matches!(state("broken").status, Status::Error(e) if e.contains("cannot start")));
    assert!(matches!(state("unset").status, Status::Error(e) if e.contains("OTTO_MCP_TEST_NEVER_SET")));
    assert!(listed.iter().all(|s| s.scope == otto_mcp::Scope::Project));
    mcp.shutdown().await;
}

// --- inside the runtime ---

use otto_core::{CreateSession, ModelResolver, Runtime, RuntimeConfig};
use otto_llm::{CallCx, CallKind, Caps, EventStream, FinishReason, LlmError, Model, ModelInfo, Part, Request, StreamEvent};

struct Scripted {
    info: ModelInfo,
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<Request>>,
}
#[async_trait::async_trait]
impl Model for Scripted {
    fn info(&self) -> &ModelInfo {
        &self.info
    }
    async fn stream(&self, request: Request, _cx: CallCx) -> Result<EventStream, LlmError> {
        self.requests.lock().unwrap().push(request);
        let script = self.scripts.lock().unwrap().pop_front();
        Ok(match script {
            Some(events) => Box::pin(futures::stream::iter(events.into_iter().map(Ok))),
            None => Box::pin(futures::stream::pending()),
        })
    }
}
struct Resolver(Arc<Scripted>);
impl ModelResolver for Resolver {
    fn resolve(&self, _: &str, _: &str) -> otto_core::Result<Arc<dyn Model>> {
        Ok(self.0.clone())
    }
    fn default_model(&self) -> Option<(String, String)> {
        Some(("test".into(), "test".into()))
    }
    fn list(&self) -> Vec<ModelInfo> {
        vec![self.0.info.clone()]
    }
}
fn call(id: &str, name: &str, input: serde_json::Value) -> StreamEvent {
    StreamEvent::PartDone { part: Part::ToolCall { call_id: id.into(), name: name.into(), input, kind: CallKind::Function, opaque: None } }
}
fn text(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::PartDone { part: Part::text(text) }, StreamEvent::Finish { reason: FinishReason::Stop, response_id: None }]
}

/// The provider-safe rules of `otto-core/tests/runtime.rs`, for MCP-provided schemas.
fn schema_problems(name: &str, schema: &serde_json::Value) -> Vec<String> {
    let mut problems = vec![];
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        problems.push(format!("{name}: invalid tool name"));
    }
    if schema["type"] != "object" || !schema["properties"].is_object() {
        problems.push(format!("{name}: top level must be an object with properties"));
    }
    for key in ["oneOf", "anyOf", "allOf", "not", "enum", "const", "if", "then", "else"] {
        if schema.get(key).is_some() {
            problems.push(format!("{name}: top-level {key}"));
        }
    }
    for required in schema["required"].as_array().into_iter().flatten() {
        if schema["properties"].get(required.as_str().unwrap_or_default()).is_none() {
            problems.push(format!("{name}: required {required} is not a property"));
        }
    }
    fn walk(v: &serde_json::Value, root: &serde_json::Value, name: &str, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(r) = map.get("$ref").and_then(|r| r.as_str())
                    && !r.strip_prefix('#').is_some_and(|p| p.is_empty() || root.pointer(p).is_some())
                {
                    out.push(format!("{name}: unresolved $ref {r}"));
                }
                if map.get("type").is_some_and(|t| t == "array") && !map.contains_key("items") {
                    out.push(format!("{name}: array without items"));
                }
                map.values().for_each(|v| walk(v, root, name, out));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, root, name, out)),
            _ => {}
        }
    }
    walk(schema, schema, name, &mut problems);
    problems
}

#[tokio::test]
async fn mcp_tools_in_a_session_are_provider_safe_and_load_on_demand() {
    let dir = project(12);
    let model = Arc::new(Scripted {
        info: ModelInfo { id: "test".into(), provider: "test".into(), name: "test".into(), caps: Caps::default(), pricing: None, list_pricing: None },
        scripts: Mutex::new(
            vec![
                vec![call("load", "load_tools", json!({"query":"echo"}))],
                vec![call("echo", "mcp__fixture__echo", json!({"text":"from mcp"}))],
                text("done"),
            ]
            .into(),
        ),
        requests: Mutex::new(vec![]),
    });
    let runtime = Runtime::new(RuntimeConfig {
        store: Arc::new(Mutex::new(otto_store::Store::open_in_memory().unwrap())),
        models: Arc::new(Resolver(model.clone())),
        state_dir: dir.path().join("state"),
        user_config_dir: dir.path().join("user"),
    })
    .await
    .unwrap();
    runtime.add_source(Mcp::new(dir.path().join("user"), dir.path().join("tokens")));
    let project = runtime.open_project(dir.path()).await.unwrap();
    let session = runtime.create_session(CreateSession { project_id: project.id, cwd: dir.path().into(), ..Default::default() }).await.unwrap();
    assert!(runtime.tools(&session.id).await.unwrap().iter().any(|t| t.name == "mcp__fixture__echo" && t.source == "mcp:fixture"));
    let (_, mut live) = runtime.subscribe(&session.id, 0).await.unwrap();
    session.prompt(vec![otto_proto::InputPart::Text { text: "use mcp".into() }], otto_proto::Delivery::Queue).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async { while !matches!(live.recv().await.unwrap().1, otto_proto::Event::TurnFinished { .. }) {} })
        .await
        .unwrap();
    let requests = model.requests.lock().unwrap().clone();
    assert!(!requests[0].tools.iter().any(|t| t.name.starts_with("mcp__")), "deferred until loaded");
    let loader = requests[0].tools.iter().find(|t| t.name == "load_tools").unwrap();
    assert!(loader.description.contains("mcp__fixture__echo: Echo the text back."));
    assert!(requests[1].tools.iter().any(|t| t.name == "mcp__fixture__echo"));
    let mut problems = vec![];
    for def in requests.iter().flat_map(|r| &r.tools) {
        problems.extend(schema_problems(&def.name, &def.input_schema));
    }
    assert!(problems.is_empty(), "{problems:#?}");
    let view = session.view().await.unwrap();
    assert!(serde_json::to_string(&view.items).unwrap().contains("from mcp"));
    runtime.shutdown().await.unwrap();
}
