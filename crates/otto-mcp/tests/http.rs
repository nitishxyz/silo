//! The client against the in-process streamable-HTTP fake (`otto_mcp::fake`): SSE
//! responses, the GET stream and its resume, server requests, resources and prompts.
use otto_mcp::{Event, Mcp, Status, fake::Fake};
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
