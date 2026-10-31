//! `changes_show` against a real server and a temporary git repo: the
//! agent's own Changes block beside it, linked to it, reused and focused
//! on later calls, with scope and marks set; a floating agent gets a tab in
//! its own session.

use std::{path::Path, process::Command, time::Duration};

use serde_json::{Value, json};
use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    model::{AgentLink, Axis, BlockKind, ChangeMark, ChangesScope, ChangesState, SessionView},
    paths,
};
use silo_tools::{Context, Part, Tools};
use tokio_util::sync::CancellationToken;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

async fn show(tools: &Tools, anchor: &str, input: Value) -> Value {
    let result = tokio::time::timeout(Duration::from_secs(15), tools.call_in("changes_show", input, CancellationToken::new(), Some(anchor.into())))
        .await
        .unwrap()
        .unwrap();
    match result.content.into_iter().next().unwrap() {
        Part::Json(value) => value,
        other => panic!("expected JSON: {other:?}"),
    }
}

async fn changes_blocks(control: &Control, session: &str) -> Vec<(String, ChangesState)> {
    let view: SessionView = control.call(Method::SessionView { session_id: session.into() }).await.unwrap();
    view.blocks.into_iter().filter(|b| b.kind == BlockKind::Changes).map(|b| (b.id, *b.changes.unwrap())).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_show_opens_the_agents_own_block() {
    let home = tempfile::Builder::new().prefix("silo-tools-changes-").tempdir_in("/tmp").unwrap();
    let project = tempfile::Builder::new().prefix("silo-tools-repo-").tempdir_in("/tmp").unwrap();
    let repo = std::fs::canonicalize(project.path()).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Silo Test"]);
    git(&repo, &["config", "user.email", "test@silo.invalid"]);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "first"]);
    std::fs::write(repo.join("src/main.rs"), "fn main() {\n    run();\n}\n").unwrap();

    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    while !silo_client::server_running(home.path()).await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (control, events) = Control::connect(&paths::socket(home.path()), "tools-test").await.unwrap();
    let tools = Tools::new(control.clone(), events, Context { agent_block: None, cwd: Some(repo.clone()), ..Default::default() });
    let create = |name: &str| Method::SessionCreate { name: Some(name.into()), spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None };
    // The default session exists too: nothing may land there.
    let agents: Created = control.call(create("agents")).await.unwrap();
    let main: Created = control.call(create("work")).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Agent,
        url: None,
        agent: None,
    };
    let agent: Created = control.call(split).await.unwrap();
    control
        .call_value(Method::AgentSet { block_id: agent.block_id.clone(), session_id: Some("ses_7".into()), title: None, cwd: None })
        .await
        .unwrap();

    // First call: a block split off the agent, linked to it, on the work
    // tree (no turn snapshot given), the file shown and a line marked.
    let input =
        json!({"path": "src/main.rs", "hunks": [{"path": "src/main.rs", "line": 2, "note": "the new call"}, {"path": "/elsewhere/x.rs", "line": 1}]});
    let shown = show(&tools, &agent.block_id, input).await;
    assert_eq!((shown["created"].as_bool(), shown["path"].as_str(), shown["marks"].as_u64()), (Some(true), Some("src/main.rs"), Some(1)), "{shown}");
    assert!(shown["note"].as_str().is_some_and(|n| n.contains("no snapshot")), "{shown}");
    let blocks = changes_blocks(&control, &main.session_id).await;
    let [(block, state)] = blocks.as_slice() else { panic!("one changes block: {blocks:?}") };
    assert_eq!(state.root, repo.to_string_lossy());
    assert_eq!((state.scope.clone(), state.active.as_deref()), (ChangesScope::Working, Some("src/main.rs")));
    assert_eq!(state.marks, [ChangeMark { path: "src/main.rs".into(), line: 2, note: Some("the new call".into()) }]);
    assert_eq!(state.agent, Some(AgentLink { otto_session: "ses_7".into(), block_id: Some(agent.block_id.clone()) }));
    let view: SessionView = control.call(Method::SessionView { session_id: main.session_id.clone() }).await.unwrap();
    assert!(silo_tools::files::window_of(&view, block).is_some_and(|w| w.layout.contains(&agent.block_id)), "split beside the agent");
    assert!(changes_blocks(&control, &agents.session_id).await.is_empty());

    // Again, with a turn's snapshot: the same block, focused, on that
    // tree; no marks clear the old ones.
    control.call_value(Method::BlockFocus { block_id: agent.block_id.clone() }).await.unwrap();
    let shown = show(&tools, &agent.block_id, json!({"scope": "turn", "turn_base": "4b825dc642cb6eb9a060e54bf8d69288fbee4904"})).await;
    assert_eq!((shown["created"].as_bool(), shown["block_id"].as_str()), (Some(false), Some(block.as_str())));
    let state = &changes_blocks(&control, &main.session_id).await[0].1;
    assert_eq!(state.scope, ChangesScope::Tree { base: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(), head: None, label: "This turn".into() });
    assert!(state.marks.is_empty());
    let view: SessionView = control.call(Method::SessionView { session_id: main.session_id.clone() }).await.unwrap();
    assert_eq!(silo_tools::files::window_of(&view, block).unwrap().focused, *block, "focused");
    let shown = show(&tools, &agent.block_id, json!({"scope": "staged"})).await;
    assert_eq!(shown["scope"], json!({"kind": "staged"}));

    // A floating agent whose block is gone: beside it in its float (a
    // float is a window of its own).
    control.call_value(Method::BlockClose { block_id: block.clone(), force: true }).await.unwrap();
    control.call_value(Method::BlockFloat { block_id: agent.block_id.clone(), rect: None }).await.unwrap();
    let shown = show(&tools, &agent.block_id, json!({"scope": "working"})).await;
    assert_eq!(shown["created"].as_bool(), Some(true));
    let view: SessionView = control.call(Method::SessionView { session_id: main.session_id.clone() }).await.unwrap();
    let new = shown["block_id"].as_str().unwrap();
    assert!(view.session.windows[0].float_of(&agent.block_id).is_some_and(|f| f.contains(new)), "in the agent's float: {view:?}");
    assert_eq!(changes_blocks(&control, &main.session_id).await.len(), 1);
    assert!(changes_blocks(&control, &agents.session_id).await.is_empty());

    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap();
}
