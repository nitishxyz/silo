//! Configuration edits: add, update (transport, on/off) and remove, per scope.
use otto_mcp::{Event, Mcp, Scope, Status, config::ServerConfig};
use otto_tools::ProjectSources;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn read(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn http(url: &str) -> ServerConfig {
    ServerConfig { kind: Some("http".into()), url: Some(url.into()), ..Default::default() }
}

#[tokio::test]
async fn add_update_remove_by_scope() {
    let dir = tempfile::tempdir().unwrap();
    let (user, root) = (dir.path().join("user"), dir.path().join("project"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(".mcp.json"), r#"{"mcpServers":{"shared":{"command":"/nonexistent/x","timeout":5}},"other":1}"#).unwrap();
    let mcp = Mcp::new(&user, dir.path().join("tokens"));
    let mut events = mcp.subscribe();
    let stdio = ServerConfig {
        command: Some("/nonexistent/y".into()),
        args: vec!["-v".into()],
        env: BTreeMap::from([("A".into(), "${B}".into())]),
        ..Default::default()
    };
    mcp.add(Some(&root), Scope::Global, "tools", &stdio, false).await.unwrap();
    assert_eq!(events.recv().await.unwrap(), Event::Changed);
    assert!(mcp.add(Some(&root), Scope::Global, "tools", &stdio, true).await.unwrap_err().contains("already exists"));
    assert!(mcp.add(None, Scope::Project, "p", &stdio, true).await.is_err(), "a project server needs a root");
    assert!(mcp.add(None, Scope::Global, "bad name", &stdio, true).await.is_err());
    assert!(mcp.add(None, Scope::Global, "web", &http("ftp://x"), true).await.is_err());
    assert_eq!(
        read(&user.join("mcp.json"))["mcpServers"]["tools"],
        json!({"command":"/nonexistent/y","args":["-v"],"env":{"A":"${B}"},"disabled":true})
    );

    mcp.add(Some(&root), Scope::Project, "web", &http("http://127.0.0.1:9/mcp"), true).await.unwrap();
    // Turning a project server off is the user's choice: the shared file is untouched.
    let before = std::fs::read_to_string(root.join(".mcp.json")).unwrap();
    mcp.update(Some(&root), Scope::Project, "shared", None, Some(false)).await.unwrap();
    assert_eq!(std::fs::read_to_string(root.join(".mcp.json")).unwrap().len(), before.len());
    assert_eq!(read(&user.join("mcp.json"))["disabledProjectServers"][root.to_string_lossy().as_ref()], json!(["shared"]));

    let listed = mcp.list(Some(&root)).await;
    let row = |name: &str| listed.iter().find(|s| s.name == name).unwrap().clone();
    assert_eq!((row("tools").scope, row("tools").enabled), (Scope::Global, false));
    assert_eq!(row("tools").config.env["A"], "${B}", "listed as written");
    assert_eq!((row("shared").scope, row("shared").enabled), (Scope::Project, false));
    assert_eq!(row("web").scope, Scope::Project);
    assert_ne!(row("web").status, Status::Idle);
    // Other projects and the global list don't see it.
    assert_eq!(mcp.list(None).await.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["tools"]);

    // A new transport keeps the other keys.
    mcp.update(Some(&root), Scope::Project, "shared", Some(&http("https://example.test/mcp")), Some(true)).await.unwrap();
    let file = read(&root.join(".mcp.json"));
    assert_eq!(file["mcpServers"]["shared"], json!({"type":"http","url":"https://example.test/mcp","timeout":5}));
    assert_eq!(file["other"], 1);
    assert!(read(&user.join("mcp.json")).get("disabledProjectServers").is_none());
    mcp.update(Some(&root), Scope::Global, "tools", None, Some(true)).await.unwrap();
    assert!(read(&user.join("mcp.json"))["mcpServers"]["tools"].get("disabled").is_none());
    assert!(mcp.update(Some(&root), Scope::Global, "missing", None, Some(true)).await.is_err());

    mcp.remove(Some(&root), Scope::Project, "web").await.unwrap();
    mcp.remove(Some(&root), Scope::Global, "tools").await.unwrap();
    assert!(mcp.remove(Some(&root), Scope::Global, "tools").await.is_err());
    assert_eq!(mcp.list(Some(&root)).await.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["shared"]);
    mcp.shutdown().await;
}
