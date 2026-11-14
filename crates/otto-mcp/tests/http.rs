//! The client against the in-process streamable-HTTP fake (`otto_mcp::fake`): SSE
//! responses, the GET stream and its resume, server requests, resources and prompts.
use otto_mcp::{Event, Mcp, Status, fake::Fake};
use otto_proto::{Elicitation, Elicitor, FieldKind, FormReply, QuestionAction};
use otto_tools::{ProjectSources, Tool, ToolCx};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

fn project(url: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".mcp.json"), json!({"mcpServers":{"remote":{"type":"http","url":url}}}).to_string()).unwrap();
    dir
}

async fn tool(mcp: &Mcp, dir: &tempfile::TempDir, name: &str) -> Arc<dyn Tool> {
    mcp.tools(dir.path()).await.into_iter().find(|t| t.spec().name == name).unwrap_or_else(|| panic!("{name} missing"))
}

async fn call(tool: &Arc<dyn Tool>, dir: &tempfile::TempDir, input: Value) -> String {
    tool.call(input, ToolCx::new("c", dir.path())).await.unwrap().joined_text()
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn tools_stream_resume_and_list_changed() {
    let fake = Fake::start(false).await;
    let dir = project(&fake.url);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let mut events = mcp.subscribe();
    let mut names: Vec<String> = mcp.tools(dir.path()).await.iter().map(|t| t.spec().name.clone()).collect();
    names.sort();
    let expected = ["ask", "echo", "get_prompt", "grow", "list_resources", "read_resource"].map(|n| format!("mcp__remote__{n}"));
    assert_eq!(names, expected);
    // A tools/call answered as an SSE stream.
    assert_eq!(call(&tool(&mcp, &dir, "mcp__remote__echo").await, &dir, json!({"text":"hi"})).await, "hi");
    eventually("the GET stream", || fake.state().gets.len() == 1).await;
    fake.push(json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"hello"}}));
    eventually("delivery", || fake.state().delivered == 1).await;
    fake.kick();
    // list_changed goes out while the stream is down or right after it reopened.
    assert_eq!(call(&tool(&mcp, &dir, "mcp__remote__grow").await, &dir, json!({})).await, "grown");
    eventually("the resumed stream", || fake.state().gets.len() >= 2).await;
    assert_eq!(fake.state().gets[1].as_deref(), Some("1"), "resumed with Last-Event-ID");
    // The server relists on its own and reports the new count.
    tokio::time::timeout(Duration::from_secs(5), async {
        while events.recv().await.unwrap() != (Event::Status { name: "remote".into(), status: Status::Connected, tools: 4 }) {}
    })
    .await
    .unwrap();
    assert!(mcp.tools(dir.path()).await.iter().any(|t| t.spec().name == "mcp__remote__extra"));
    assert!(fake.state().methods.contains(&"notifications/initialized".to_owned()));
    mcp.shutdown().await;
}

#[tokio::test]
async fn server_requests_are_answered() {
    let fake = Fake::start(false).await;
    let dir = project(&fake.url);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let ask = tool(&mcp, &dir, "mcp__remote__ask").await;
    let reply = |text: String| serde_json::from_str::<Value>(&text).unwrap();
    let roots = reply(call(&ask, &dir, json!({"method":"roots/list"})).await);
    let uri = roots["result"]["roots"][0]["uri"].as_str().unwrap().to_owned();
    assert_eq!(reqwest::Url::parse(&uri).unwrap().to_file_path().unwrap(), dir.path());
    assert_eq!(reply(call(&ask, &dir, json!({"method":"ping"})).await)["result"], json!({}));
    let sampling = reply(call(&ask, &dir, json!({"method":"sampling/createMessage","params":{"messages":[]}})).await);
    assert_eq!(sampling["error"]["code"], -1);
    let elicit = reply(call(&ask, &dir, json!({"method":"elicitation/create","params":{"message":"name?"}})).await);
    assert_eq!(elicit["result"]["action"], "decline");
    assert_eq!(reply(call(&ask, &dir, json!({"method":"other/thing"})).await)["error"]["code"], -32601);
    mcp.shutdown().await;
}

#[tokio::test]
async fn resources_and_prompts_as_tools() {
    let fake = Fake::start(false).await;
    let dir = project(&fake.url);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let list = call(&tool(&mcp, &dir, "mcp__remote__list_resources").await, &dir, json!({})).await;
    assert!(list.contains("file:///notes.txt notes (text/plain)"), "{list}");
    let read = call(&tool(&mcp, &dir, "mcp__remote__read_resource").await, &dir, json!({"uri":"file:///notes.txt"})).await;
    assert!(read.contains("hello notes"));
    let prompt = tool(&mcp, &dir, "mcp__remote__get_prompt").await;
    assert!(prompt.spec().description.contains("- review(path*): Review code"));
    assert!(prompt.spec().effects.read_only());
    let text = call(&prompt, &dir, json!({"name":"review","arguments":{"path":"src"}})).await;
    assert!(text.contains("Review src"), "{text}");
    mcp.shutdown().await;
}

/// A tool context whose session answers elicitations with `answer` (recording what was asked).
fn asking(dir: &tempfile::TempDir, asked: Arc<std::sync::Mutex<Vec<Elicitation>>>, answer: impl Fn() -> FormReply + Send + Sync + 'static) -> ToolCx {
    let mut cx = ToolCx::new("c", dir.path());
    let answer = Arc::new(answer);
    cx.services = Some(Arc::new(Elicitor::new(move |elicitation| {
        asked.lock().unwrap().push(elicitation);
        let reply = answer();
        async move { reply }
    })));
    cx
}

fn elicit(schema: Value) -> Value {
    json!({"method":"elicitation/create","params":{"message":"Who are you?","requestedSchema":schema}})
}

#[tokio::test]
async fn elicitation_asks_the_session() {
    let fake = Fake::start(false).await;
    let dir = project(&fake.url);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let ask = tool(&mcp, &dir, "mcp__remote__ask").await;
    let schema = json!({"type":"object","properties":{"name":{"type":"string","title":"Name"},"admin":{"type":"boolean","default":false}},"required":["name"]});
    let run = |cx: ToolCx, input: Value| {
        let ask = ask.clone();
        async move { serde_json::from_str::<Value>(&ask.call(input, cx).await.unwrap().joined_text()).unwrap() }
    };
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let content = json!({"name":"Ann"}).as_object().unwrap().clone();
    let accept = move || FormReply { action: QuestionAction::Accept, content: Some(content.clone()) };
    let reply = run(asking(&dir, asked.clone(), accept), elicit(schema.clone())).await;
    assert_eq!(reply["result"], json!({"action":"accept","content":{"name":"Ann"}}));
    {
        let asked = asked.lock().unwrap();
        assert_eq!((asked[0].server.as_str(), asked[0].message.as_str()), ("remote", "Who are you?"));
        let fields: Vec<(&str, FieldKind, bool)> = asked[0].form.fields.iter().map(|f| (f.name.as_str(), f.kind, f.required)).collect();
        assert_eq!(fields, [("name", FieldKind::Text, true), ("admin", FieldKind::Boolean, false)]);
    }
    let reply = run(asking(&dir, asked.clone(), FormReply::decline), elicit(schema.clone())).await;
    assert_eq!(reply["result"], json!({"action":"decline"}));
    let reply = run(asking(&dir, asked.clone(), FormReply::cancel), elicit(schema)).await;
    assert_eq!(reply["result"], json!({"action":"cancel"}));
    // A form otto cannot show is declined without asking.
    let before = asked.lock().unwrap().len();
    let nested = elicit(json!({"type":"object","properties":{"tags":{"type":"array"}}}));
    assert_eq!(run(asking(&dir, asked.clone(), FormReply::cancel), nested).await["result"], json!({"action":"decline"}));
    // No form at all: a plain confirmation.
    let empty = json!({"method":"elicitation/create","params":{"message":"Continue?"}});
    let ok = || FormReply { action: QuestionAction::Accept, content: Some(Default::default()) };
    assert_eq!(run(asking(&dir, asked.clone(), ok), empty).await["result"], json!({"action":"accept","content":{}}));
    assert_eq!(asked.lock().unwrap().len(), before + 1);
    // Headless (no session to ask): declined, as before.
    let headless = run(ToolCx::new("c", dir.path()), elicit(json!({"type":"object","properties":{}}))).await;
    assert_eq!(headless["result"], json!({"action":"decline"}));
    mcp.shutdown().await;
}

/// While the user has not answered, the connection keeps serving other server requests.
#[tokio::test]
async fn a_pending_elicitation_does_not_block_the_connection() {
    let fake = Fake::start(false).await;
    let dir = project(&fake.url);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let ask = tool(&mcp, &dir, "mcp__remote__ask").await;
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut cx = ToolCx::new("c", dir.path());
    let wait = gate.clone();
    cx.services = Some(Arc::new(Elicitor::new(move |_| {
        let wait = wait.clone();
        async move {
            wait.notified().await;
            FormReply::decline()
        }
    })));
    let pending = tokio::spawn({
        let ask = ask.clone();
        async move { ask.call(elicit(json!({"type":"object","properties":{}})), cx).await.unwrap().joined_text() }
    });
    eventually("the elicitation", || fake.state().log.iter().any(|(_, m)| m.contains("elicitation/create"))).await;
    let ping = tokio::time::timeout(Duration::from_secs(4), call(&ask, &dir, json!({"method":"ping"}))).await.expect("ping answered");
    assert_eq!(serde_json::from_str::<Value>(&ping).unwrap()["result"], json!({}));
    gate.notify_one();
    let reply: Value = serde_json::from_str(&pending.await.unwrap()).unwrap();
    assert_eq!(reply["result"], json!({"action":"decline"}));
    mcp.shutdown().await;
}

#[tokio::test]
async fn initialize_declares_elicitation() {
    let fake = Fake::start(false).await;
    let dir = project(&fake.url);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    tool(&mcp, &dir, "mcp__remote__echo").await;
    let initialize = fake.state().initialize.clone().unwrap();
    assert_eq!(initialize["protocolVersion"], "2025-06-18");
    assert_eq!(initialize["capabilities"]["elicitation"], json!({}));
    assert!(initialize["capabilities"].get("sampling").is_none(), "sampling stays off");
    mcp.shutdown().await;
}
