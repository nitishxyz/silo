//! The Studio tools against a real server (docs/STUDIO.md §6.1): finding
//! or opening the agent's own Studio block (linked, beside it, focus kept),
//! the studio root from a folder inside it, `studio_open`, `studio_read`
//! and `canvas_read` outlines, `canvas_create` starting a studio and
//! `canvas_edit` ids and failures, and `cwd`/`path` handling.

use std::{path::Path, time::Duration};

use serde_json::{Value, json};
use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    model::{AgentLink, Axis, BlockKind, SessionSummary, SessionView},
    studio::{StudioState, StudioView},
};
use silo_tools::{Context, Part, Tools};
use tokio_util::sync::CancellationToken;

async fn run(tools: &Tools, name: &str, anchor: Option<&str>, input: Value) -> anyhow::Result<Part> {
    let result = tokio::time::timeout(Duration::from_secs(15), tools.call_in(name, input, CancellationToken::new(), anchor.map(str::to_owned)))
        .await
        .expect("in time")?;
    assert!(!result.is_error);
    Ok(result.content.into_iter().next().unwrap())
}

async fn json_of(tools: &Tools, name: &str, anchor: Option<&str>, input: Value) -> Value {
    match run(tools, name, anchor, input).await.unwrap_or_else(|e| panic!("{name}: {e:#}")) {
        Part::Json(value) => value,
        other => panic!("{name}: expected JSON: {other:?}"),
    }
}

async fn text_of(tools: &Tools, name: &str, anchor: Option<&str>, input: Value) -> String {
    match run(tools, name, anchor, input).await.unwrap_or_else(|e| panic!("{name}: {e:#}")) {
        Part::Text(text) => text,
        other => panic!("{name}: expected text: {other:?}"),
    }
}

async fn studios(control: &Control, session: &str) -> Vec<(String, StudioState)> {
    let view: SessionView = control.call(Method::SessionView { session_id: session.into() }).await.unwrap();
    view.blocks.into_iter().filter(|b| b.kind == BlockKind::Studio).map(|b| (b.id, *b.studio.unwrap())).collect()
}

async fn focused(control: &Control, session: &str, block: &str) -> String {
    let view: SessionView = control.call(Method::SessionView { session_id: session.into() }).await.unwrap();
    silo_tools::files::window_of(&view, block).expect("a window").focused.clone()
}

/// The lines of `text` holding every one of `parts`.
fn line_with<'a>(text: &'a str, parts: &[&str]) -> &'a str {
    text.lines().find(|l| parts.iter().all(|p| l.contains(p))).unwrap_or_else(|| panic!("no line with {parts:?} in:\n{text}"))
}

struct Server {
    _home: tempfile::TempDir,
    control: Control,
    task: tokio::task::JoinHandle<()>,
}

async fn server(cwd: Option<&Path>) -> (Server, Tools) {
    let home = tempfile::Builder::new().prefix("silo-tools-studio-").tempdir_in("/tmp").unwrap();
    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    while !silo_client::server_running(home.path()).await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (control, events) = Control::connect(&silo_proto::paths::socket(home.path()), "tools-test").await.unwrap();
    let tools = Tools::new(control.clone(), events, Context { agent_block: None, cwd: cwd.map(Path::to_path_buf), ..Default::default() });
    (Server { _home: home, control, task }, tools)
}

impl Server {
    async fn stop(self) {
        self.control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.task).await.unwrap().unwrap();
    }
}

/// A `work` session: a terminal split with an agent block (otto session `ses_7`).
async fn agent(control: &Control) -> (Created, Created) {
    let create = |name: &str| Method::SessionCreate { name: Some(name.into()), spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None };
    let main: Created = control.call(create("work")).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Agent,
        url: None,
        agent: None,
        ids: None,
    };
    let agent: Created = control.call(split).await.unwrap();
    control
        .call_value(Method::AgentSet { block_id: agent.block_id.clone(), session_id: Some("ses_7".into()), title: None, cwd: None })
        .await
        .unwrap();
    (main, agent)
}

fn studio_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new().prefix("silo-tools-harbor-").tempdir_in("/tmp").unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

#[tokio::test(flavor = "multi_thread")]
async fn studio_block_finds_links_and_opens_the_agents_own() {
    let (_dir, root) = studio_dir();
    std::fs::write(root.join("studio.toml"), "name = \"Harbor\"\n").unwrap();
    std::fs::create_dir_all(root.join("world/characters/mara")).unwrap();
    let (_other, elsewhere) = studio_dir();
    let (server, tools) = server(None).await;
    let control = server.control.clone();
    let agents: Created = control
        .call(Method::SessionCreate { name: Some("agents".into()), spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None })
        .await
        .unwrap();
    let (main, agent) = agent(&control).await;

    // None yet, and not opened.
    let deep = root.join("world/characters/mara");
    assert_eq!(tools.studio_block(Some(&agent.block_id), &deep, false).await.unwrap(), None);
    assert!(studios(&control, &main.session_id).await.is_empty());
    // Opened: split off the agent, focus back on it, linked, rooted at the
    // nearest folder up with studio.toml.
    let block = tools.studio_block(Some(&agent.block_id), &deep, true).await.unwrap().expect("opened");
    let list = studios(&control, &main.session_id).await;
    let [(id, state)] = list.as_slice() else { panic!("one studio block: {list:?}") };
    assert_eq!(id, &block);
    assert_eq!(state.root, root.to_string_lossy());
    assert_eq!(state.agent, Some(AgentLink { otto_session: "ses_7".into(), block_id: Some(agent.block_id.clone()) }));
    assert_eq!(focused(&control, &main.session_id, &block).await, agent.block_id, "focus stays on the agent");
    let view: SessionView = control.call(Method::SessionView { session_id: main.session_id.clone() }).await.unwrap();
    assert!(silo_tools::files::window_of(&view, &block).is_some_and(|w| w.layout.contains(&agent.block_id)), "beside the agent");
    // Found again (a file in the studio works too), not reopened.
    std::fs::write(deep.join("mara.md"), "# Mara\n").unwrap();
    assert_eq!(tools.studio_block(Some(&agent.block_id), &deep.join("mara.md"), true).await.unwrap(), Some(block.clone()));
    assert_eq!(studios(&control, &main.session_id).await.len(), 1);

    // An unlinked one on the same root in the session is used (and linked)
    // when the agent has none of its own.
    control.call_value(Method::BlockClose { block_id: block.clone(), force: true }).await.unwrap();
    let tab = Method::WindowCreate {
        session_id: main.session_id.clone(),
        spawn: SpawnSpec { cwd: Some(root.to_string_lossy().into_owned()), ..SpawnSpec::default() },
        kind: BlockKind::Studio,
        url: None,
        background: true,
        ids: None,
    };
    let tab: Created = control.call(tab).await.unwrap();
    assert_eq!(studios(&control, &main.session_id).await[0].1.agent, None);
    assert_eq!(tools.studio_block(Some(&agent.block_id), &root, false).await.unwrap(), Some(tab.block_id.clone()));
    let state = &studios(&control, &main.session_id).await[0].1;
    assert_eq!(state.agent.as_ref().and_then(|a| a.block_id.as_deref()), Some(agent.block_id.as_str()), "linked now");
    // One on another root is not it.
    assert_eq!(tools.studio_block(Some(&agent.block_id), &elsewhere, false).await.unwrap(), None, "no studio.toml there: it is its own root");
    let other = tools.studio_block(Some(&agent.block_id), &elsewhere, true).await.unwrap().unwrap();
    assert_eq!(studios(&control, &main.session_id).await.iter().find(|(id, _)| *id == other).unwrap().1.root, elsewhere.to_string_lossy());

    // Without an anchor: the default session (`agents`), a tab there.
    let mine = tools.studio_block(None, &root, true).await.unwrap().unwrap();
    let ids: Vec<String> = studios(&control, &agents.session_id).await.into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, std::slice::from_ref(&mine));
    assert_eq!(tools.studio_block(None, &root, true).await.unwrap(), Some(mine));
    // A folder that isn't there.
    assert!(tools.studio_block(None, &root.join("nope"), true).await.is_err());

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tools_read_create_edit_and_open() {
    let (_dir, root) = studio_dir();
    std::fs::create_dir_all(root.join("shots")).unwrap();
    let (server, tools) = server(Some(&root.join("shots"))).await;
    let control = server.control.clone();
    let (main, agent) = agent(&control).await;
    let a = Some(agent.block_id.as_str());
    let path = json!(root.to_string_lossy());

    // No studio.toml yet: studio_read says so, studio_open opens Setup.
    let text = text_of(&tools, "studio_read", a, json!({"path": path})).await;
    assert!(text.contains("No studio.toml") && text.contains("canvas_create"), "{text}");
    let opened = json_of(&tools, "studio_open", a, json!({"path": path})).await;
    assert_eq!((opened["created"].as_bool(), opened["view"].as_str()), (Some(false), Some("setup")), "studio_read opened it: {opened}");
    assert!(opened["note"].as_str().is_some_and(|n| n.contains("No studio.toml")));
    let block = opened["block_id"].as_str().unwrap().to_owned();

    // canvas_create starts the studio (the folder's name, landscape) and opens the tab.
    let made = json_of(&tools, "canvas_create", a, json!({"path": path, "title": "The Pilot", "episode": 1})).await;
    assert_eq!(
        (made["canvas"].as_str(), made["episode"].as_u64(), made["block_id"].as_str()),
        (Some("ep01-the-pilot"), Some(1), Some(block.as_str()))
    );
    assert_eq!((made["folder"].as_str(), made["assets"].as_str()), (Some("canvases/ep01-the-pilot/"), Some("canvases/ep01-the-pilot/assets/")));
    assert!(made["note"].as_str().is_some_and(|n| n.contains("there was no studio.toml")), "{made}");
    assert!(read_toml(&root).contains(&format!("name = \"{}\"", root.file_name().unwrap().to_string_lossy())));
    let state = &studios(&control, &main.session_id).await[0].1;
    assert_eq!((state.view, state.active.as_deref()), (StudioView::Canvas, Some("ep01-the-pilot")));
    // A second one: no init note; a format; bad input refused.
    let made = json_of(&tools, "canvas_create", a, json!({"path": path, "title": "Moodboard", "format": "vertical"})).await;
    assert!(made.get("note").is_none() && made["format"] == "vertical", "{made}");
    assert!(run(&tools, "canvas_create", a, json!({"path": path})).await.is_err(), "a title is required");
    assert!(run(&tools, "canvas_create", a, json!({"path": path, "title": "X", "episode": "one"})).await.is_err());
    assert!(run(&tools, "canvas_create", a, json!({"path": path, "title": "X", "format": "imax"})).await.is_err());

    // The world, by hand as an agent writes it.
    std::fs::create_dir_all(root.join("world/characters/mara")).unwrap();
    std::fs::write(
        root.join("world/characters/mara/mara.md"),
        "---\nname: Mara Venn\nsummary: Harbor pilot\nsheet: sheet.png\ntags: [lead]\n---\n# Mara\n",
    )
    .unwrap();
    std::fs::write(root.join("world/characters/mara/sheet.png"), b"\x89PNG").unwrap();
    let text = text_of(&tools, "studio_read", a, json!({"path": path})).await;
    assert!(text.starts_with(&format!("Studio \"{}\" at {}", root.file_name().unwrap().to_string_lossy(), root.display())), "{text}");
    line_with(&text, &["Formats (project default: landscape)", "landscape 16:9 1080p 24fps", "vertical 9:16 1080p 30fps", "portrait 4:5"]);
    line_with(&text, &["Canvases (2, in tab order)"]);
    let pilot = line_with(&text, &["- ep01-the-pilot \"The Pilot\""]);
    assert!(pilot.contains("· Ep 1 · format landscape (project default) · 0 nodes · rev 1 · canvases/ep01-the-pilot/"), "{pilot}");
    line_with(&text, &["- moodboard \"Moodboard\"", "format vertical ·"]);
    line_with(&text, &["Overview: world/overview.md"]);
    line_with(&text, &["Characters (world/characters/, 1)"]);
    let mara = line_with(&text, &["- characters/mara \"Mara Venn\" —"]);
    assert!(mara.contains("— Harbor pilot · file world/characters/mara/mara.md · sheet world/characters/mara/sheet.png · tags lead"), "{mara}");
    line_with(&text, &["Places (world/places/, 0)"]);
    // The Production block comes first: assets, episodes by stage, the next steps.
    let production = text.find("Production (derived from the files").unwrap_or_else(|| panic!("{text}"));
    assert!(production < text.find("Canvases (").unwrap(), "read first: {text}");
    line_with(&text, &["- characters/mara \"Mara Venn\": sheet ✓ · not approved · voice ✗ · video sheet ✗ · unused"]);
    line_with(&text, &["- ep01-the-pilot Ep 1 \"The Pilot\": script ✗ · 0 shots · keyframes 0/0 · video 0/0 · sound 0 · no cut"]);
    line_with(&text, &["- moodboard \"Moodboard\": script ✗"]);
    line_with(&text, &["1. Tell the agent about the show"]);
    line_with(&text, &["2. Write episode 1's script [ep01-the-pilot]"]);
    // Text above the cap is truncated by `cap`; this one isn't.
    assert!(text.lines().count() < 45);

    // canvas_edit: ids in op order, per-op failures, base_rev.
    let edited = json_of(
        &tools,
        "canvas_edit",
        a,
        json!({"path": path, "canvas": "ep01-the-pilot", "ops": [
            {"op": "add_node", "node": {"id": "n_scene", "body": {"kind": "frame", "title": "Harbor at dawn", "role": "scene", "order": 1}}},
            {"op": "add_node", "node": {"id": "n_shot", "parent": "n_scene", "body": {"kind": "frame", "title": "Shot 1", "role": "shot", "order": 1}}},
            {"op": "add_node", "node": {"id": "n_mara", "body": {"kind": "entity", "id": "characters/mara"}}},
            {"op": "add_node", "node": {"id": "n_key", "parent": "n_shot", "body": {"kind": "gen", "makes": "image", "title": "Keyframe", "prompt": "Mara at the rail", "model": "nano-banana",
                "status": "done", "pick": 0, "takes": [{"path": "canvases/ep01-the-pilot/assets/k1.png", "media": "image", "at": 1}, {"path": "canvases/ep01-the-pilot/assets/k2.png", "media": "image", "at": 2}],
                "inputs_used": ["world/characters/mara/sheet.png@1"]}}},
            {"op": "add_node", "node": {"id": "n_vid", "parent": "n_shot", "body": {"kind": "gen", "makes": "video", "prompt": "She turns", "options": {"duration": 5}}}},
            {"op": "add_node", "node": {"id": "n_beat", "body": {"kind": "note", "text": "The bell rings"}}},
            {"op": "add_edge", "edge": {"from": "n_beat", "to": "n_vid", "role": "text"}},
            {"op": "add_edge", "edge": {"from": "n_mara", "to": "n_key", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_key", "to": "n_vid", "role": "first_frame"}},
            {"op": "add_edge", "edge": {"from": "n_beat", "to": "n_beat", "role": "text"}},
            {"op": "remove_node", "id": "n_gone"},
        ]}),
    )
    .await;
    let ids: Vec<&str> = edited["ids"].as_array().unwrap().iter().map(|i| i.as_str().unwrap()).collect();
    assert_eq!(&ids[..6], ["n_scene", "n_shot", "n_mara", "n_key", "n_vid", "n_beat"]);
    assert!(ids[6..9].iter().all(|i| i.starts_with("e_")) && ids[9].is_empty(), "{ids:?}");
    assert_eq!(
        edited["failed"].as_array().unwrap().iter().map(|f| (f["op"].as_u64().unwrap(), f["code"].as_str().unwrap())).collect::<Vec<_>>(),
        [(9, "bad_request"), (10, "not_found")]
    );
    assert_eq!((edited["rev"].as_u64(), edited["moved"].as_bool()), (Some(2), Some(false)));
    let moved = json_of(
        &tools,
        "canvas_edit",
        a,
        json!({"path": path, "canvas": "ep01-the-pilot", "base_rev": 1, "ops": [{"op": "update_node", "id": "n_beat", "x": 900}]}),
    )
    .await;
    assert_eq!((moved["rev"].as_u64(), moved["moved"].as_bool()), (Some(3), Some(true)));
    assert!(run(&tools, "canvas_edit", a, json!({"path": path, "canvas": "ep01-the-pilot", "ops": []})).await.is_err(), "empty ops");
    assert!(
        run(&tools, "canvas_edit", a, json!({"path": path, "canvas": "ep01-the-pilot", "ops": [{"op": "add_node"}]})).await.is_err(),
        "ops that don't parse"
    );
    assert!(run(&tools, "canvas_edit", a, json!({"path": path, "canvas": "nope", "ops": [{"op": "remove_node", "id": "x"}]})).await.is_err());

    // canvas_read: frames in order with their nodes, cards, wires in order.
    let text = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "ep01-the-pilot"})).await;
    let head = text.lines().next().unwrap();
    assert!(
        head.starts_with(
            "Canvas ep01-the-pilot \"The Pilot\" · Ep 1 · format landscape (16:9 1080p 24fps) from the project · rev 3 · 6 nodes, 3 edges"
        ),
        "{head}"
    );
    line_with(&text, &["n_scene frame scene 1 \"Harbor at dawn\""]);
    assert!(line_with(&text, &["n_shot frame shot 1 \"Shot 1\""]).starts_with("  n_shot"), "nested in the scene");
    let key = line_with(&text, &["n_key card image \"Keyframe\""]);
    assert!(key.starts_with("    n_key"), "nested in the shot: {key:?}");
    for part in [
        "· nano-banana ·",
        "· done",
        "OUT OF DATE (Mara Venn changed)",
        "· prompt \"Mara at the rail\"",
        "· pick 0 (0-based, of 2 takes): canvases/ep01-the-pilot/assets/k1.png",
        "· takes (* picked) 0* canvases/ep01-the-pilot/assets/k1.png, 1 canvases/ep01-the-pilot/assets/k2.png",
    ] {
        assert!(key.contains(part), "{part:?} in {key}");
    }
    let vid = line_with(&text, &["n_vid card video"]);
    for part in ["· model automatic ·", "· draft (never run)", "· prompt \"She turns\"", "· options {\"duration\":5}"] {
        assert!(vid.contains(part), "{part:?} in {vid}");
    }
    assert!(!vid.contains("OUT OF DATE") && !vid.contains("takes"), "{vid}");
    assert!(text.find("n_key card").unwrap() < text.find("n_vid card").unwrap(), "left to right");
    let outside = text.find("Outside frames:").expect("loose nodes");
    assert!(text.find("  n_mara entity characters/mara").unwrap() > outside);
    line_with(&text, &["n_beat note \"The bell rings\""]);
    // Wires: card by card, by port (the video's first frame before its text).
    let wires: Vec<&str> = text.lines().skip_while(|l| !l.starts_with("Wires")).skip(1).collect();
    assert_eq!(wires.len(), 3, "{text}");
    assert!(wires[0].starts_with("  n_mara → n_key (reference) [e_"), "{wires:?}");
    assert!(wires[1].starts_with("  n_key → n_vid (first_frame) [e_"), "{wires:?}");
    assert!(wires[2].starts_with("  n_beat → n_vid (text) [e_"), "{wires:?}");
    // One frame only.
    let shot = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "ep01-the-pilot", "frame": "n_shot"})).await;
    assert!(
        shot.contains("n_shot frame shot") && shot.contains("n_vid card") && !shot.contains("n_scene frame") && !shot.contains("Outside frames"),
        "{shot}"
    );
    let missing = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "ep01-the-pilot", "frame": "n_nope"})).await;
    assert!(missing.contains("(no frame n_nope; the whole canvas follows)"));
    assert!(run(&tools, "canvas_read", a, json!({"path": path})).await.is_err(), "canvas is required");
    let empty = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "moodboard"})).await;
    assert!(empty.contains("· format vertical (9:16 1080p 30fps) ·") && empty.contains("(empty)"), "{empty}");
    assert!(!empty.contains("Documents"), "none: no list");

    // Documents: listed (script first) and marked when pinned; a document node shows its path; studio_read lists them.
    let canvas_dir = root.join("canvases/ep01-the-pilot");
    std::fs::write(canvas_dir.join("script.md"), "# Beats").unwrap();
    std::fs::write(canvas_dir.join("brief.txt"), "x").unwrap();
    std::fs::write(canvas_dir.join("notes.json"), "{}").unwrap();
    let pinned = json_of(
        &tools,
        "canvas_edit",
        a,
        json!({"path": path, "canvas": "ep01-the-pilot", "ops": [{"op": "add_node", "node": {"id": "n_doc", "body": {"kind": "asset", "path": "canvases/ep01-the-pilot/script.md"}}}]}),
    )
    .await;
    assert!(pinned["failed"].as_array().is_none_or(Vec::is_empty), "no media needed: {pinned}");
    let text = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "ep01-the-pilot"})).await;
    let docs: Vec<&str> = text.lines().skip_while(|l| !l.starts_with("Documents (canvases/ep01-the-pilot/)")).skip(1).take(3).collect();
    assert_eq!(
        docs[..2],
        ["- canvases/ep01-the-pilot/script.md · on the canvas as n_doc", "- canvases/ep01-the-pilot/brief.txt · not on the canvas"],
        "{text}"
    );
    assert!(text.find("Documents").unwrap() < text.find("n_scene frame").unwrap(), "at the top");
    line_with(&text, &["n_doc document canvases/ep01-the-pilot/script.md"]);

    // Mentions: written into a prompt they wire (the result says so); the outline says what each names.
    let edited = json_of(
        &tools,
        "canvas_edit",
        a,
        json!({"path": path, "canvas": "ep01-the-pilot", "ops": [{"op": "update_node", "id": "n_vid", "body": {"prompt": "@characters/mara turns to @#n_doc, mail me@x.com"}}]}),
    )
    .await;
    let mentions = edited["mentions"].as_array().unwrap();
    assert_eq!(mentions.len(), 2, "{edited}");
    assert_eq!(
        (mentions[0]["card"].as_str(), mentions[0]["token"].as_str(), mentions[0]["did"].as_str()),
        (Some("n_vid"), Some("@characters/mara"), Some("wired"))
    );
    assert_eq!(
        (mentions[0]["edge"]["from"].as_str(), mentions[0]["edge"]["mention"].as_bool()),
        (Some("n_mara"), Some(true)),
        "the Entity node there"
    );
    assert_eq!((mentions[1]["edge"]["from"].as_str(), mentions[1]["edge"]["role"].as_str()), (Some("n_doc"), Some("text")));
    let text = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "ep01-the-pilot"})).await;
    let vid = line_with(&text, &["n_vid card video"]);
    assert!(
        vid.contains(" · mentions @characters/mara = \"Mara Venn\" (wired reference from n_mara), @#n_doc = \"script.md\" (wired text from n_doc)"),
        "{vid}"
    );
    let text = text_of(&tools, "studio_read", a, json!({"path": path})).await;
    line_with(&text, &["- ep01-the-pilot", "· documents canvases/ep01-the-pilot/script.md, canvases/ep01-the-pilot/brief.txt"]);

    // Named references and links: studio_read lists them by id; a reference's mention wires its file.
    std::fs::create_dir_all(root.join("world/characters/mara/refs")).unwrap();
    std::fs::create_dir_all(root.join("world/props/oar")).unwrap();
    std::fs::write(root.join("world/props/oar/oar.md"), "---\nname: The oar\nlinks: [characters/mara]\n---\n").unwrap();
    std::fs::write(
        root.join("world/characters/mara/mara.md"),
        "---\nname: Mara Venn\nsummary: Harbor pilot\nsheet: sheet.png\nrefs:\n  - refs/face.png\n  - file: refs/coat.png\n    name: Oilskin coat\n    defines: the coat, buttoned\ntags: [lead]\n---\n# Mara\n",
    )
    .unwrap();
    for f in ["face.png", "coat.png"] {
        std::fs::write(root.join("world/characters/mara/refs").join(f), b"\x89PNG").unwrap();
    }
    let text = text_of(&tools, "studio_read", a, json!({"path": path})).await;
    let mara = line_with(&text, &["- characters/mara \"Mara Venn\" —"]);
    assert!(
        mara.contains(
            " · refs face world/characters/mara/refs/face.png; coat \"Oilskin coat\" world/characters/mara/refs/coat.png defines \"the coat, buttoned\" · links props/oar · tags lead"
        ),
        "{mara}"
    );
    line_with(&text, &["- props/oar \"The oar\"", "· links characters/mara"]);
    let edited = json_of(
        &tools,
        "canvas_edit",
        a,
        json!({"path": path, "canvas": "ep01-the-pilot", "ops": [{"op": "update_node", "id": "n_vid", "body": {"prompt": "@characters/mara in @characters/mara#coat turns to @#n_doc"}}]}),
    )
    .await;
    let coat = edited["mentions"][0].clone();
    assert_eq!((coat["token"].as_str(), coat["did"].as_str()), (Some("@characters/mara#coat"), Some("wired")), "{edited}");
    let node = coat["node"].as_str().unwrap().to_owned();
    let text = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "ep01-the-pilot"})).await;
    let vid = line_with(&text, &["n_vid card video"]);
    assert!(
        vid.contains(&format!(
            "@characters/mara#coat = \"Mara Venn, Oilskin coat\" (world/characters/mara/refs/coat.png, defines \"the coat, buttoned\"; wired reference from {node})"
        )),
        "{vid}"
    );

    // Overlapping frames: a line at the top until a tidy.
    json_of(
        &tools,
        "canvas_edit",
        a,
        json!({"path": path, "canvas": "moodboard", "ops": [
            {"op": "add_node", "node": {"id": "f1", "x": 0, "y": 0, "w": 400, "h": 400, "body": {"kind": "frame", "role": "scene", "order": 1}}},
            {"op": "add_node", "node": {"id": "f2", "x": 0, "y": 200, "w": 400, "h": 400, "body": {"kind": "frame", "role": "scene", "order": 2}}},
            {"op": "add_node", "node": {"id": "f3", "x": 2000, "y": 0, "w": 100, "h": 100, "body": {"kind": "frame", "role": "scene", "order": 3}}},
        ]}),
    )
    .await;
    let text = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "moodboard"})).await;
    assert_eq!(text.lines().nth(1), Some("Layout: 2 frames overlap; run canvas_edit {op: tidy}"), "{text}");
    json_of(&tools, "canvas_edit", a, json!({"path": path, "canvas": "moodboard", "ops": [{"op": "tidy"}]})).await;
    let text = text_of(&tools, "canvas_read", a, json!({"path": path, "canvas": "moodboard"})).await;
    assert!(!text.contains("Layout:"), "{text}");

    // studio_open: a canvas tab, a world section or entity.
    let opened = json_of(&tools, "studio_open", a, json!({"path": path, "canvas": "moodboard"})).await;
    assert_eq!((opened["created"].as_bool(), opened["view"].as_str(), opened["active"].as_str()), (Some(false), Some("canvas"), Some("moodboard")));
    assert!(opened.get("note").is_none());
    assert_eq!(focused(&control, &main.session_id, &block).await, block, "studio_open shows it");
    json_of(&tools, "studio_open", a, json!({"path": path, "world": "world/characters/mara"})).await;
    let state = studios(&control, &main.session_id).await[0].1.clone();
    assert_eq!(
        (state.view, state.world_section.as_deref(), state.world_entity.as_deref()),
        (StudioView::World, Some("characters"), Some("characters/mara"))
    );
    json_of(&tools, "studio_open", a, json!({"path": path, "world": "places"})).await;
    let state = studios(&control, &main.session_id).await[0].1.clone();
    assert_eq!((state.world_section.as_deref(), state.world_entity), (Some("places"), None));
    let opened = json_of(&tools, "studio_open", a, json!({"path": path, "production": true})).await;
    assert_eq!(opened["view"].as_str(), Some("production"), "{opened}");

    // cwd: the input's, else the context's; path relative to it.
    let from_cwd = text_of(&tools, "studio_read", a, json!({})).await;
    assert!(from_cwd.contains("Canvases (2"), "the context's cwd (shots/, inside the studio): {from_cwd}");
    let by_input = text_of(&tools, "studio_read", a, json!({"cwd": root.join("world").to_string_lossy(), "path": "characters"})).await;
    assert_eq!(by_input, from_cwd, "path relative to the input's cwd");
    assert!(run(&tools, "studio_read", a, json!({"path": "nope/nowhere"})).await.is_err(), "a folder that isn't there");
    assert_eq!(studios(&control, &main.session_id).await.len(), 1, "always the same block");
    // No working directory at all: path is needed.
    let (_, events) = Control::connect(&silo_proto::paths::socket(server._home.path()), "bare").await.unwrap();
    let bare = Tools::new(control.clone(), events, Context { agent_block: None, cwd: None, ..Default::default() });
    let error = run(&bare, "studio_read", a, json!({})).await.unwrap_err();
    assert!(format!("{error:#}").contains("no working directory"), "{error:#}");

    server.stop().await;
}

fn read_toml(root: &Path) -> String {
    std::fs::read_to_string(root.join("studio.toml")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn without_an_anchor_the_tools_use_the_default_session() {
    let (_dir, root) = studio_dir();
    let (server, tools) = server(Some(&root)).await;
    let control = server.control.clone();
    // No session yet: one named after the default is made for it.
    let made = json_of(&tools, "canvas_create", None, json!({"title": "Board"})).await;
    assert_eq!(made["canvas"], "board");
    let sessions: Vec<SessionSummary> = control.call(Method::SessionList {}).await.unwrap();
    let agents = sessions.iter().find(|s| s.name == "agents").expect("the default session");
    let list = studios(&control, &agents.id).await;
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].1.root.as_str(), list[0].1.agent.as_ref()), (root.to_str().unwrap(), None), "not linked to anyone");
    let text = text_of(&tools, "canvas_read", None, json!({"canvas": "board"})).await;
    assert!(text.starts_with("Canvas board \"Board\""), "{text}");
    assert_eq!(studios(&control, &agents.id).await.len(), 1);
    server.stop().await;
}
