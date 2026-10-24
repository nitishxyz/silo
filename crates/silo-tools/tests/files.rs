//! `files_show` against a real server: a Files block beside the agent,
//! reused for later files under its folder, with tab, scroll and
//! highlights set.

use std::time::Duration;

use serde_json::{Value, json};
use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    model::{Axis, BlockKind, FileHighlight, FilesState, SessionView},
    paths,
};
use silo_tools::{Context, Part, Tools};
use tokio_util::sync::CancellationToken;

async fn show(tools: &Tools, anchor: &str, input: Value) -> anyhow::Result<Value> {
    let result = tokio::time::timeout(Duration::from_secs(15), tools.call_in("files_show", input, CancellationToken::new(), Some(anchor.into())))
        .await
        .unwrap()?;
    match result.content.into_iter().next().unwrap() {
        Part::Json(value) => Ok(value),
        other => panic!("expected JSON: {other:?}"),
    }
}

async fn files_blocks(control: &Control, session: &str) -> Vec<(String, FilesState)> {
    let view: SessionView = control.call(Method::SessionView { session_id: session.into() }).await.unwrap();
    view.blocks.into_iter().filter(|b| b.kind == BlockKind::Files).map(|b| (b.id, *b.files.unwrap())).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn files_show_opens_beside_the_agent_and_reuses_the_block() {
    let home = tempfile::Builder::new().prefix("silo-tools-files-").tempdir_in("/tmp").unwrap();
    let project = tempfile::Builder::new().prefix("silo-tools-project-").tempdir_in("/tmp").unwrap();
    let project_path = std::fs::canonicalize(project.path()).unwrap();
    std::fs::create_dir_all(project_path.join("src")).unwrap();
    std::fs::write(project_path.join("src/main.rs"), "fn main() {}\n".repeat(80)).unwrap();
    std::fs::write(project_path.join("README.md"), "# hi\n").unwrap();
    let elsewhere = tempfile::Builder::new().prefix("silo-tools-elsewhere-").tempdir_in("/tmp").unwrap();
    let other = std::fs::canonicalize(elsewhere.path()).unwrap().join("notes.txt");
    std::fs::write(&other, "x\n").unwrap();

    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    while !silo_client::server_running(home.path()).await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (control, events) = Control::connect(&paths::socket(home.path()), "tools-test").await.unwrap();
    let cx = Context { agent_block: None, cwd: Some(project_path.clone()), ..Default::default() };
    let tools = Tools::new(control.clone(), events, cx);
    let main: Created =
        control.call(Method::SessionCreate { name: None, spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Agent,
        url: None,
        agent: None,
    };
    let agent: Created = control.call(split).await.unwrap();

    // A relative path: a new Files block rooted at the agent's folder.
    let input = json!({"path": "src/main.rs", "highlight": [{"start": 40, "end": 42, "note": "the loop"}, {"start": 60}]});
    let shown = show(&tools, &agent.block_id, input).await.unwrap();
    assert_eq!((shown["path"].as_str(), shown["created"].as_bool(), shown["line"].as_u64()), (Some("src/main.rs"), Some(true), Some(40)));
    let blocks = files_blocks(&control, &main.session_id).await;
    let [(block, state)] = blocks.as_slice() else { panic!("one files block: {blocks:?}") };
    assert_eq!(state.root, project_path.to_string_lossy());
    assert_eq!((state.open.as_slice(), state.active.as_deref()), (["src/main.rs".to_owned()].as_slice(), Some("src/main.rs")));
    assert_eq!(state.scroll.get("src/main.rs"), Some(&34), "five lines of context above line 40");
    let marks = &state.highlights["src/main.rs"];
    assert_eq!(marks[0], FileHighlight { start: 40, end: 42, note: Some("the loop".into()) });
    assert_eq!((marks[1].start, marks[1].end), (60, 60));
    let view: SessionView = control.call(Method::SessionView { session_id: main.session_id.clone() }).await.unwrap();
    assert!(silo_tools::files::window_of(&view, block).is_some_and(|w| w.layout.contains(&agent.block_id)), "split beside the agent");

    // An absolute path in the same folder reuses it; no highlights clear the old ones there.
    let readme = project_path.join("README.md");
    let shown = show(&tools, &agent.block_id, json!({"path": readme, "line": 1})).await.unwrap();
    assert_eq!((shown["block_id"].as_str(), shown["created"].as_bool()), (Some(block.as_str()), Some(false)));
    show(&tools, &agent.block_id, json!({"path": "src/main.rs"})).await.unwrap();
    let state = files_blocks(&control, &main.session_id).await.remove(0).1;
    assert_eq!(state.open, ["src/main.rs", "README.md"]);
    assert!(state.highlights.is_empty(), "{:?}", state.highlights);
    assert_eq!(state.scroll.get("src/main.rs"), Some(&34), "no line: the tab keeps its place");

    // A file outside it opens a second block at its folder; a missing file fails.
    let shown = show(&tools, &agent.block_id, json!({"path": other, "cwd": "/"})).await.unwrap();
    assert_eq!(shown["created"].as_bool(), Some(true));
    assert_eq!(files_blocks(&control, &main.session_id).await.len(), 2);
    let missing = show(&tools, &agent.block_id, json!({"path": "nope.rs"})).await.unwrap_err();
    assert!(format!("{missing:#}").contains("does not exist"), "{missing:#}");
    let bad = show(&tools, &agent.block_id, json!({"path": "src/main.rs", "line": 0})).await.unwrap_err();
    assert!(format!("{bad:#}").contains("line"), "{bad:#}");

    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), server).await;
}
