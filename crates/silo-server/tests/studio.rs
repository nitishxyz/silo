//! Studio blocks (docs/STUDIO.md §5): Setup and `studio.init`,
//! `studio.toml` edits that keep comments and unknown keys, the world
//! (frontmatter, sheets, refs, sections, `used_in`), canvases and every
//! `canvas_edit` op, outside edits, out of date at read time, assets and
//! imports, path safety, `files.*` on a studio block, `studio.changed`,
//! `studio.set`, restarts, agent links, access rights and older clients.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use silo_client::{Control, Events};
use silo_proto::{
    access::{Rights, Scope},
    control::{Created, Event, GrantCreated, GrantKind, Method, Request, RpcError, ServerMessage, SpawnSpec},
    files::{FileBase, FileRead, FileWritten, FilesCreateParams, FilesMkdirParams, FilesReadParams, FilesWriteParams},
    model::{AgentLink, Axis, BlockInfo, BlockKind, SessionView, TermSize},
    paths,
    studio::{
        Approval, Canvas, CanvasEdited, CanvasInfo, EdgeRole, Entity, EntityInfo, EntityUse, Format, GenKind, GenStatus, Node, NodeBody, Op,
        Production, Project, RefChange, RefInfo, SectionKind, StepKind, StudioAsset, StudioAssetParams, StudioCanvasCreateParams,
        StudioCanvasEditParams, StudioCanvasParams, StudioCanvasUpdateParams, StudioChange, StudioEntityApproveParams, StudioEntityCreateParams,
        StudioEntityLinkParams, StudioEntityParams, StudioEntityRefParams, StudioEntityUpdateParams, StudioImportParams, StudioImported,
        StudioInitParams, StudioMedia, StudioProjectParams, StudioSetFormatParams, StudioSetFormatsParams, StudioSetModelsParams, StudioSetParams,
        StudioState, StudioView, StudioWorld, mention::Target, size,
    },
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);

// ---- harness -------------------------------------------------------------

fn write(dir: &Path, rel: &str, bytes: impl AsRef<[u8]>) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// A PNG header of `w`×`h` (enough for its size to be read).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    out.extend(w.to_be_bytes());
    out.extend(h.to_be_bytes());
    out.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    out
}

/// A fresh home for the server and a studio folder (canonical).
fn temp(name: &str) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(format!("/tmp/silo-studio-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("Harbor Lights");
    std::fs::create_dir_all(&root).unwrap();
    (dir.join("home"), root.canonicalize().unwrap())
}

fn config(home: &Path) -> silo_server::Config {
    silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        trash: Some(home.join("trash")),
        ..Default::default()
    }
}

async fn start(home: &Path) -> JoinHandle<anyhow::Result<()>> {
    let socket = paths::socket(home);
    let inode = |path: &Path| std::fs::metadata(path).ok().map(|m| std::os::unix::fs::MetadataExt::ino(&m));
    let stale = inode(&socket);
    let task = tokio::spawn(silo_server::run(config(home)));
    for _ in 0..300 {
        if inode(&socket).is_some_and(|ino| Some(ino) != stale) && silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path) -> (Control, Events) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok(pair) = Control::connect(&paths::socket(home), "studio-test").await {
            return pair;
        }
        assert!(tokio::time::Instant::now() < deadline, "no server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(control: Control, task: JoinHandle<anyhow::Result<()>>) {
    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap();
}

fn cleanup(home: &Path) {
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// A session whose terminal (in `cwd`) is split with a studio block on
/// `dir` (`None`: the terminal's folder).
async fn studio(control: &Control, cwd: &Path, dir: Option<&Path>) -> Created {
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some(cwd.display().to_string()), size: Some(TermSize::new(80, 24)) };
    let main: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id,
        axis: Axis::Horizontal,
        spawn: SpawnSpec { cwd: dir.map(|d| d.display().to_string()), ..SpawnSpec::default() },
        kind: BlockKind::Studio,
        url: None,
        agent: None,
        ids: None,
    };
    control.call(split).await.unwrap()
}

async fn info(control: &Control, created: &Created) -> BlockInfo {
    let view: SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    view.blocks.into_iter().find(|b| b.id == created.block_id).expect("studio block")
}

async fn state(control: &Control, created: &Created) -> StudioState {
    *info(control, created).await.studio.expect("studio state")
}

async fn call<T: DeserializeOwned>(control: &Control, method: Method) -> T {
    let name = silo_proto::api::method_name(&method);
    control.call(method).await.unwrap_or_else(|e| panic!("{name}: {e:#}"))
}

/// The error code of a call that must fail.
async fn code(control: &Control, method: Method) -> String {
    let name = silo_proto::api::method_name(&method);
    let error = control.call_value(method).await.err().unwrap_or_else(|| panic!("{name} succeeded"));
    error.downcast_ref::<RpcError>().unwrap_or_else(|| panic!("not an rpc error: {error:#}")).code.clone()
}

/// A method from its JSON form (for `null`s and unknown values).
fn raw(method: &str, params: Value) -> Method {
    serde_json::from_value::<Request>(json!({"id": 1, "method": method, "params": params})).unwrap().method
}

fn ops(value: Value) -> Vec<Op> {
    serde_json::from_value(value).unwrap()
}

async fn init(control: &Control, block: &str) -> Project {
    call(control, Method::StudioInit(StudioInitParams { block_id: block.into(), name: "Harbor Lights".into(), format: None })).await
}

async fn project(control: &Control, block: &str) -> Project {
    call(control, Method::StudioProject(StudioProjectParams { block_id: block.into() })).await
}

async fn create(control: &Control, block: &str, title: &str, episode: Option<u32>) -> CanvasInfo {
    let params = StudioCanvasCreateParams { block_id: block.into(), title: title.into(), episode, format: None, name: None };
    call(control, Method::StudioCanvasCreate(params)).await
}

async fn canvas(control: &Control, block: &str, name: &str) -> Canvas {
    call(control, Method::StudioCanvas(StudioCanvasParams { block_id: block.into(), name: name.into() })).await
}

fn edit_method(block: &str, name: &str, ops: Vec<Op>, base_rev: Option<u64>) -> Method {
    Method::StudioCanvasEdit(StudioCanvasEditParams { block_id: block.into(), name: name.into(), ops, base_rev })
}

async fn edit(control: &Control, block: &str, name: &str, list: Value) -> CanvasEdited {
    call(control, edit_method(block, name, ops(list), None)).await
}

async fn entity(control: &Control, block: &str, id: &str) -> Entity {
    call(control, Method::StudioEntity(StudioEntityParams { block_id: block.into(), id: id.into() })).await
}

async fn set(control: &Control, params: StudioSetParams) -> StudioState {
    call(control, Method::StudioSet(params)).await
}

fn node<'a>(c: &'a Canvas, id: &str) -> &'a Node {
    c.node(id).unwrap_or_else(|| panic!("no node {id} in {:?}", c.nodes.iter().map(|n| &n.id).collect::<Vec<_>>()))
}

fn wires_into<'a>(c: &'a Canvas, to: &str, role: EdgeRole) -> Vec<&'a str> {
    c.edges.iter().filter(|e| e.to == to && e.role == role).map(|e| e.from.as_str()).collect()
}

fn failed(edited: &CanvasEdited) -> Vec<(u32, &str)> {
    edited.failed.iter().map(|f| (f.op, f.code.as_str())).collect()
}

fn ms(path: &Path) -> u64 {
    path.metadata().unwrap().modified().unwrap().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn touch(path: &Path, secs_later: u64) {
    let at = path.metadata().unwrap().modified().unwrap() + Duration::from_secs(secs_later);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(at).unwrap();
}

/// Whether a `studio.changed` of `block` matching `want` arrives within `wait`.
async fn studio_changed(events: &mut Events, block: &str, wait: Duration, want: impl Fn(StudioChange, Option<&str>, Option<u64>) -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(event) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let (_, Event::StudioChanged { block_id, what, name, rev }) = event.expect("events")
            && block_id == block
            && want(what, name.as_deref(), rev)
        {
            return true;
        }
    }
    false
}

/// The `studio.changed` events of `block` within `wait`.
async fn studio_events(events: &mut Events, block: &str, wait: Duration) -> Vec<(StudioChange, Option<String>, Option<u64>)> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut out = Vec::new();
    while let Ok(event) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let (_, Event::StudioChanged { block_id, what, name, rev }) = event.expect("events")
            && block_id == block
        {
            out.push((what, name, rev));
        }
    }
    out
}

/// The studio state of the next `block.changed` of `block` that `want`s it.
async fn block_changed(events: &mut Events, block: &str, wait: Duration, want: impl Fn(&StudioState) -> bool) -> Option<StudioState> {
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(event) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let (_, Event::BlockChanged { block: info }) = event.expect("events")
            && info.id == block
            && let Some(state) = info.studio
            && want(&state)
        {
            return Some(*state);
        }
    }
    None
}

/// Until the studio watcher of `block` runs: FSEvents can take seconds to
/// start. Writes `world/.probe-<n>.md`-like files under `world/probe/`.
async fn watching(events: &mut Events, root: &Path, block: &str) {
    let deadline = tokio::time::Instant::now() + 3 * TIMEOUT;
    for n in 0.. {
        write(root, &format!("world/probe/p{n}.md"), "probe");
        if studio_changed(events, block, Duration::from_millis(700), |w, _, _| w == StudioChange::World).await {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the studio watcher never reported");
    }
    std::fs::remove_dir_all(root.join("world/probe")).unwrap();
    // Until quiet.
    while !studio_events(events, block, Duration::from_millis(600)).await.is_empty() {}
}

// ---- Setup, init and studio.toml -----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn setup_init_and_toml_edits_keep_what_they_dont_touch() {
    let (home, root) = temp("init");
    // What was there before init stays.
    write(&root, "world/overview.md", "# Our own overview\n");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let created = studio(&control, &root, None).await;
    let id = created.block_id.clone();

    // A folder without studio.toml: Setup, rooted at the terminal's folder.
    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.title.as_str(), block.cwd.as_deref()), (BlockKind::Studio, "Harbor Lights", Some(root.to_str().unwrap())));
    let st = *block.studio.unwrap();
    assert_eq!((st.root.as_str(), st.view, st.open.len(), st.active.as_deref()), (root.to_str().unwrap(), StudioView::Setup, 0, None));
    assert_eq!(code(&control, Method::StudioProject(StudioProjectParams { block_id: id.clone() })).await, "not_found");
    // Studio calls on another kind of block, or on no block, are refused.
    let view: SessionView = call(&control, Method::SessionView { session_id: created.session_id.clone() }).await;
    let terminal = view.blocks.iter().find(|b| b.kind == BlockKind::Terminal).unwrap().id.clone();
    assert_eq!(code(&control, Method::StudioProject(StudioProjectParams { block_id: terminal.clone() })).await, "bad_request");
    assert_eq!(code(&control, Method::StudioProject(StudioProjectParams { block_id: "b-nope".into() })).await, "not_found");

    // init: refused for a bad name or format; else the layout.
    let bad = |name: &str, format: Option<&str>| {
        Method::StudioInit(StudioInitParams { block_id: id.clone(), name: name.into(), format: format.map(str::to_owned) })
    };
    assert_eq!(code(&control, bad("  ", None)).await, "bad_request");
    assert_eq!(code(&control, bad("X", Some("cinema"))).await, "bad_request");
    assert!(!root.join("studio.toml").exists());
    let p: Project = call(&control, bad("Harbor Lights", Some("vertical"))).await;
    assert_eq!((p.name.as_str(), p.format.as_str()), ("Harbor Lights", "vertical"));
    assert_eq!(p.formats.keys().collect::<Vec<_>>(), ["landscape", "portrait", "square", "vertical"]);
    assert_eq!(p.formats["landscape"], Format { aspect: "16:9".into(), resolution: Some("1080p".into()), fps: Some(24) });
    assert_eq!(p.formats["portrait"].aspect, "4:5");
    assert!(p.canvases.is_empty());
    let kinds: Vec<_> = p.sections.iter().map(|s| (s.id.as_str(), s.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("overview", SectionKind::Overview),
            ("style", SectionKind::Style),
            ("characters", SectionKind::Entities),
            ("places", SectionKind::Entities),
            ("props", SectionKind::Entities)
        ]
    );
    for dir in ["world/characters", "world/places", "world/props", "canvases"] {
        assert!(root.join(dir).is_dir(), "{dir}");
    }
    assert_eq!(read(&root, "world/overview.md"), "# Our own overview\n", "never overwritten");
    let style = read(&root, "world/style.md");
    assert!(style.contains("## Style line"));
    assert!(!style.to_lowercase().contains("motion line"), "a project opts into a motion line by writing it: {style}");
    let toml = read(&root, "studio.toml");
    assert!(toml.contains("name = \"Harbor Lights\"") && toml.contains("format = \"vertical\""), "{toml}");
    assert!(toml.contains("# the project default: a preset below"), "the comment on format stays: {toml}");
    // The block left Setup: no canvas yet, so Production.
    assert_eq!(state(&control, &created).await.view, StudioView::Production);

    // init again never overwrites.
    std::fs::write(root.join("world/style.md"), "mine\n").unwrap();
    let again: Project = call(&control, bad("Another Name", None)).await;
    assert_eq!((again.name.as_str(), again.format.as_str()), ("Harbor Lights", "vertical"));
    assert_eq!(read(&root, "studio.toml"), toml);
    assert_eq!(read(&root, "world/style.md"), "mine\n");

    // Hand edits: a comment and keys the server doesn't know.
    let toml = toml.replace("[canvases]", "# our render settings\n[render]\nengine = \"x\"  # keep me\n\n[canvases]");
    let toml = format!("# Harbor Lights, season one\n{toml}\nextra = 1\n");
    std::fs::write(root.join("studio.toml"), &toml).unwrap();
    let p: Project =
        call(&control, Method::StudioSetFormat(StudioSetFormatParams { block_id: id.clone(), canvas: None, format: Some("landscape".into()) })).await;
    assert_eq!(p.format, "landscape");
    let after = read(&root, "studio.toml");
    for kept in
        ["# Harbor Lights, season one", "# our render settings", "engine = \"x\"  # keep me", "# reels, shorts, stories", "# the project default"]
    {
        assert!(after.contains(kept), "{kept:?} kept: {after}");
    }
    assert!(after.contains("format = \"landscape\""));
    // Refusals: no preset, or clearing the default.
    let set_format = |canvas: Option<&str>, format: Option<&str>| {
        Method::StudioSetFormat(StudioSetFormatParams { block_id: id.clone(), canvas: canvas.map(str::to_owned), format: format.map(str::to_owned) })
    };
    assert_eq!(code(&control, set_format(None, Some("cinema"))).await, "bad_request");
    assert_eq!(code(&control, set_format(None, None)).await, "bad_request");

    // set_formats adds and replaces presets, keeping the rest.
    let formats = |list: &[(&str, &str, Option<&str>, Option<u32>)]| {
        Method::StudioSetFormats(StudioSetFormatsParams {
            block_id: id.clone(),
            formats: list
                .iter()
                .map(|(n, a, r, f)| ((*n).to_owned(), Format { aspect: (*a).into(), resolution: r.map(str::to_owned), fps: *f }))
                .collect(),
        })
    };
    let p: Project = call(&control, formats(&[("cinema", "2.39:1", Some("4k"), Some(24)), ("square", "1:1", None, None)])).await;
    assert_eq!(p.formats["cinema"], Format { aspect: "2.39:1".into(), resolution: Some("4k".into()), fps: Some(24) });
    assert_eq!(p.formats["square"], Format { aspect: "1:1".into(), resolution: None, fps: None }, "resolution removed");
    assert_eq!(p.formats.len(), 5);
    let after = read(&root, "studio.toml");
    assert!(after.contains("engine = \"x\"  # keep me") && after.contains("# reels, shorts, stories"), "{after}");
    assert!(after.contains("extra = 1"));
    assert_eq!(code(&control, formats(&[("wide screen", "16:9", None, None)])).await, "bad_request");
    assert_eq!(code(&control, formats(&[("wide", "16x9", None, None)])).await, "bad_request");
    assert_eq!(code(&control, formats(&[("wide", "0:9", None, None)])).await, "bad_request");

    // Canvases: slugs, episodes, taken names, a given name, a format.
    let make = |title: &str, episode: Option<u32>, format: Option<&str>, name: Option<&str>| {
        Method::StudioCanvasCreate(StudioCanvasCreateParams {
            block_id: id.clone(),
            title: title.into(),
            episode,
            format: format.map(str::to_owned),
            name: name.map(str::to_owned),
        })
    };
    let mood: CanvasInfo = call(&control, make("Moodboard", None, None, None)).await;
    assert_eq!((mood.name.as_str(), mood.title.as_str(), mood.rev, mood.nodes, mood.episode), ("moodboard", "Moodboard", 1, 0, None));
    assert!(mood.updated > 0);
    let pilot: CanvasInfo = call(&control, make("The Pilot!", Some(1), None, None)).await;
    assert_eq!((pilot.name.as_str(), pilot.episode), ("ep01-the-pilot", Some(1)));
    let two: CanvasInfo = call(&control, make("Moodboard", None, None, None)).await;
    assert_eq!(two.name, "moodboard-2", "a taken name gets -2");
    let ep12: CanvasInfo = call(&control, make("Storm", Some(12), Some("vertical"), None)).await;
    assert_eq!((ep12.name.as_str(), ep12.format.as_deref()), ("ep12-storm", Some("vertical")));
    let own: CanvasInfo = call(&control, make("Anything", None, None, Some("b-roll"))).await;
    assert_eq!(own.name, "b-roll");
    let emoji: CanvasInfo = call(&control, make("🌊🌊", None, None, None)).await;
    assert_eq!(emoji.name, "canvas", "nothing left of the title: a fallback");
    assert_eq!(code(&control, make("  ", None, None, None)).await, "bad_request");
    assert_eq!(code(&control, make("X", None, Some("imax"), None)).await, "bad_request");
    for bad in ["../x", ".hidden", "a/b"] {
        assert_eq!(code(&control, make("X", None, None, Some(bad))).await, "bad_request", "{bad}");
    }
    let file: Value = serde_json::from_str(&read(&root, "canvases/moodboard/canvas.json")).unwrap();
    assert_eq!((file["version"].as_u64(), file["title"].as_str(), file["rev"].as_u64()), (Some(1), Some("Moodboard"), Some(1)));
    assert!(root.join("canvases/moodboard/assets").is_dir());
    // Tab order: episodes by number, then [canvases] order (creation), then the rest by name.
    write(&root, "canvases/aaa-hand-made/canvas.json", r#"{"title": "By hand"}"#);
    let names = |p: &Project| p.canvases.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
    let p = project(&control, &id).await;
    assert_eq!(names(&p), ["ep01-the-pilot", "ep12-storm", "moodboard", "moodboard-2", "b-roll", "canvas", "aaa-hand-made"]);
    let toml = read(&root, "studio.toml");
    assert!(toml.contains("engine = \"x\"  # keep me") && toml.contains("# tab order"), "{toml}");
    let doc: toml_edit::DocumentMut = toml.parse().unwrap();
    let order: Vec<&str> = doc["canvases"]["order"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(order, ["moodboard", "ep01-the-pilot", "moodboard-2", "ep12-storm", "b-roll", "canvas"]);

    // Update: title, episode (null clears), format (null clears).
    let update = |mut params: Value| {
        params["block_id"] = json!(id);
        params["name"] = json!("moodboard");
        raw("studio.canvas_update", params)
    };
    let up: CanvasInfo = call(&control, update(json!({"title": "Mood", "episode": 3, "format": "square"}))).await;
    assert_eq!((up.title.as_str(), up.episode, up.format.as_deref(), up.rev), ("Mood", Some(3), Some("square"), 2));
    let up: CanvasInfo = call(&control, update(json!({"episode": null}))).await;
    assert_eq!((up.title.as_str(), up.episode, up.format.as_deref(), up.rev), ("Mood", None, Some("square"), 3), "absent keeps, null clears");
    let same: CanvasInfo = call(&control, update(json!({"title": "Mood"}))).await;
    assert_eq!(same.rev, 3, "nothing changed: not written");
    let up: CanvasInfo = call(&control, update(json!({"format": null}))).await;
    assert_eq!(up.format, None);
    assert_eq!(code(&control, update(json!({"format": "imax"}))).await, "bad_request");
    assert_eq!(code(&control, update(json!({"title": " "}))).await, "bad_request");
    let missing = StudioCanvasUpdateParams { block_id: id.clone(), name: "nope".into(), title: Some("x".into()), episode: None, format: None };
    assert_eq!(code(&control, Method::StudioCanvasUpdate(missing)).await, "not_found");
    // set_format on a canvas: its override.
    let p: Project = call(&control, set_format(Some("moodboard"), Some("cinema"))).await;
    assert_eq!(p.canvases.iter().find(|c| c.name == "moodboard").unwrap().format.as_deref(), Some("cinema"));
    let p: Project = call(&control, set_format(Some("moodboard"), None)).await;
    assert_eq!(p.canvases.iter().find(|c| c.name == "moodboard").unwrap().format, None);
    assert_eq!(code(&control, set_format(Some("moodboard"), Some("imax"))).await, "bad_request");

    // Delete: to the Trash, out of the order and of the block's tabs.
    set(
        &control,
        StudioSetParams {
            block_id: id.clone(),
            open: Some(vec!["moodboard".into(), "moodboard-2".into()]),
            active: Some(Some("moodboard-2".into())),
            ..Default::default()
        },
    )
    .await;
    call::<Value>(&control, Method::StudioCanvasDelete(StudioCanvasParams { block_id: id.clone(), name: "moodboard-2".into() })).await;
    assert!(!root.join("canvases/moodboard-2").exists());
    assert!(home.join("trash/files/moodboard-2/canvas.json").is_file(), "in the trash");
    assert!(!names(&project(&control, &id).await).contains(&"moodboard-2".to_owned()));
    assert!(!read(&root, "studio.toml").contains("\"moodboard-2\""));
    let st = state(&control, &created).await;
    assert_eq!((st.open.as_slice(), st.active.as_deref()), (["moodboard".to_owned()].as_slice(), Some("moodboard")));
    assert_eq!(
        code(&control, Method::StudioCanvasDelete(StudioCanvasParams { block_id: id.clone(), name: "moodboard-2".into() })).await,
        "not_found"
    );

    // A new block on an initialised studio opens on its first canvas.
    let second = studio(&control, &root, None).await;
    let st = state(&control, &second).await;
    assert_eq!(
        (st.view, st.open.as_slice(), st.active.as_deref()),
        (StudioView::Canvas, ["ep01-the-pilot".to_owned()].as_slice(), Some("ep01-the-pilot"))
    );

    stop(control, task).await;
    cleanup(&home);
}

// ---- the world -------------------------------------------------------------

const MARA: &str = "---
# the lead
name: Mara Venn
summary: Harbor pilot, 34, scar over the left brow.
sheet: sheet.png
refs: [refs/turnaround.png, ../../style/look.png, ../../../../outside.png]
tags:
  - lead
  - pilot
voice: elevenlabs/rachel
rig:
  height: 170
  rigged: yes
---

# Mara Venn

Grew up on the tugs.
";

#[tokio::test(flavor = "multi_thread")]
async fn world_sections_entities_and_frontmatter() {
    let (home, root) = temp("world");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;
    write(&root, "world/characters/mara/mara.md", MARA);
    write(&root, "world/characters/mara/sheet.png", png(1024, 1024));
    write(&root, "world/characters/mara/refs/turnaround.png", png(10, 10));
    // No frontmatter, another file name, and a sheet.* in the folder.
    write(&root, "world/characters/teo/notes.md", "# Teo\n\nThe mechanic.\n");
    write(&root, "world/characters/teo/sheet.jpg", b"\xff\xd8\xff");
    write(&root, "world/characters/teo/refs/a.png", png(1, 1));
    write(&root, "world/characters/teo/refs/b.webp", b"RIFF");
    write(&root, "world/characters/teo/refs/notes.txt", "not an image");
    // An entity folder with no markdown yet.
    std::fs::create_dir_all(root.join("world/places/harbor")).unwrap();
    // Style's images, nested; a custom section; hidden and file entries ignored.
    write(&root, "world/style/look.png", png(1, 1));
    write(&root, "world/style/boards/b1.jpg", b"\xff\xd8\xff");
    write(&root, "world/style/readme.md", "x");
    write(&root, "world/vehicles/tug/tug.md", "---\nname: The Tug\n---\n");
    write(&root, "world/.hidden/x/x.md", "x");
    write(&root, "world/loose.md", "x");

    let world: StudioWorld = call(&control, Method::StudioWorld(StudioProjectParams { block_id: id.clone() })).await;
    let ids: Vec<_> = world.sections.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["overview", "style", "characters", "places", "props", "vehicles"], "style/ is no section");
    let style = &world.sections[1];
    assert_eq!(style.refs, ["world/style/boards/b1.jpg", "world/style/look.png"]);
    assert_eq!((world.sections[2].title.as_str(), world.sections[5].title.as_str()), ("Characters", "vehicles"));
    let chars = &world.sections[2].entities;
    assert_eq!(chars.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["characters/mara", "characters/teo"]);
    let mara = &chars[0];
    assert_eq!(
        mara,
        &EntityInfo {
            id: "characters/mara".into(),
            name: "Mara Venn".into(),
            file: "world/characters/mara/mara.md".into(),
            summary: Some("Harbor pilot, 34, scar over the left brow.".into()),
            sheet: Some("world/characters/mara/sheet.png".into()),
            refs: vec!["world/characters/mara/refs/turnaround.png".into(), "world/style/look.png".into()],
            looks: ["world/characters/mara/refs/turnaround.png", "world/style/look.png"]
                .map(|p| RefInfo {
                    id: silo_proto::studio::file_stem(p).into(),
                    path: p.into(),
                    name: silo_proto::studio::file_stem(p).into(),
                    defines: None,
                    media: StudioMedia::Image,
                })
                .into(),
            links: Vec::new(),
            tags: vec!["lead".into(), "pilot".into()],
            approval: Approval::None,
            voice: true,
        },
        "paths root-relative; a ref leaving the root dropped"
    );
    let teo = &chars[1];
    assert_eq!((teo.name.as_str(), teo.file.as_str(), teo.summary.as_deref()), ("teo", "world/characters/teo/notes.md", None));
    assert_eq!(teo.sheet.as_deref(), Some("world/characters/teo/sheet.jpg"), "no sheet named: sheet.* in the folder");
    assert_eq!(teo.refs, ["world/characters/teo/refs/a.png", "world/characters/teo/refs/b.webp"], "refs/ images when none named");
    let harbor = &world.sections[3].entities[0];
    assert_eq!((harbor.id.as_str(), harbor.file.as_str(), harbor.sheet.as_deref()), ("places/harbor", "world/places/harbor/harbor.md", None));
    assert_eq!(world.sections[5].entities[0].name, "The Tug");

    // The entity: every frontmatter key (unknown ones too), the body.
    let e = entity(&control, &id, "characters/mara").await;
    assert_eq!(e.info, *mara);
    assert_eq!(e.fields["voice"], json!("elevenlabs/rachel"));
    assert_eq!(e.fields["rig"], json!({"height": 170, "rigged": true}));
    assert_eq!(e.fields["tags"], json!(["lead", "pilot"]));
    assert_eq!(e.body, "\n# Mara Venn\n\nGrew up on the tugs.\n");
    assert!(e.used_in.is_empty());
    assert_eq!(entity(&control, &id, "world/characters/mara").await.info.id, "characters/mara", "a world/ prefix is fine");
    let teo = entity(&control, &id, "characters/teo").await;
    assert!(teo.fields.is_empty() && teo.body.starts_with("# Teo"));
    for (bad, want) in
        [("characters/nobody", "not_found"), ("nobody/x", "not_found"), ("characters", "bad_request"), ("../../etc", RpcError::FORBIDDEN)]
    {
        assert_eq!(code(&control, Method::StudioEntity(StudioEntityParams { block_id: id.clone(), id: bad.into() })).await, want, "{bad}");
    }

    // Update: merged fields (null removes), unknown keys and comments as written.
    let update = |id_: &str, fields: Value, body: Option<&str>| {
        Method::StudioEntityUpdate(StudioEntityUpdateParams {
            block_id: id.clone(),
            id: id_.into(),
            fields: serde_json::from_value(fields).unwrap(),
            body: body.map(str::to_owned),
        })
    };
    let info: EntityInfo = call(&control, update("characters/mara", json!({"summary": "Harbor pilot: 35", "tags": null, "age": 35}), None)).await;
    assert_eq!((info.summary.as_deref(), info.tags.len()), (Some("Harbor pilot: 35"), 0));
    let text = read(&root, "world/characters/mara/mara.md");
    assert!(text.starts_with("---\n# the lead\nname: Mara Venn\n"), "{text}");
    assert!(text.contains("rig:\n  height: 170\n  rigged: yes\n"), "an unknown key as written: {text}");
    assert!(text.contains("summary: \"Harbor pilot: 35\"\n") && text.contains("age: 35\n") && !text.contains("tags"), "{text}");
    assert!(text.ends_with("---\n\n# Mara Venn\n\nGrew up on the tugs.\n"), "the body untouched: {text}");
    assert_eq!(entity(&control, &id, "characters/mara").await.fields["summary"], json!("Harbor pilot: 35"), "reads back");
    // The body alone.
    call::<EntityInfo>(&control, update("characters/mara", Value::Null, Some("# Mara\n\nNew text.\n"))).await;
    let text = read(&root, "world/characters/mara/mara.md");
    assert!(text.contains("voice: elevenlabs/rachel") && text.ends_with("---\n\n# Mara\n\nNew text.\n"), "{text}");
    // A file without frontmatter gets one; its body stays; the sheet is entity-relative.
    let info: EntityInfo = call(&control, update("characters/teo", json!({"sheet": "refs/a.png", "summary": "Mechanic"}), None)).await;
    assert_eq!((info.sheet.as_deref(), info.summary.as_deref()), (Some("world/characters/teo/refs/a.png"), Some("Mechanic")));
    assert_eq!(read(&root, "world/characters/teo/notes.md"), "---\nsheet: refs/a.png\nsummary: Mechanic\n---\n# Teo\n\nThe mechanic.\n");
    // A folder with no markdown: its file is made.
    call::<EntityInfo>(&control, update("places/harbor", json!({"summary": "Fog"}), Some("# Harbor\n"))).await;
    assert_eq!(read(&root, "world/places/harbor/harbor.md"), "---\nsummary: Fog\n---\n\n# Harbor\n");
    assert_eq!(code(&control, update("characters/mara", json!({"bad key: x": 1}), None)).await, "bad_request");
    assert_eq!(code(&control, update("characters/nobody", json!({"a": 1}), None)).await, "not_found");

    // Create: a slug folder and its file; a taken name; a new section.
    let make = |section: &str, name: &str, fields: Value, body: Option<&str>| {
        Method::StudioEntityCreate(StudioEntityCreateParams {
            block_id: id.clone(),
            section: section.into(),
            name: name.into(),
            fields: serde_json::from_value(fields).unwrap(),
            body: body.map(str::to_owned),
        })
    };
    let kraken: EntityInfo = call(&control, make("creatures", "The Kraken!", json!({"summary": "Big", "tags": ["sea", "myth"]}), None)).await;
    assert_eq!(
        (kraken.id.as_str(), kraken.name.as_str(), kraken.file.as_str()),
        ("creatures/the-kraken", "The Kraken!", "world/creatures/the-kraken/the-kraken.md")
    );
    assert_eq!(kraken.tags, ["sea", "myth"]);
    assert_eq!(
        read(&root, "world/creatures/the-kraken/the-kraken.md"),
        "---\nname: The Kraken!\nsummary: Big\ntags: [sea, myth]\n---\n\n# The Kraken!\n\n"
    );
    let again: EntityInfo = call(&control, make("creatures", "The Kraken", Value::Null, Some("Body"))).await;
    assert_eq!(again.id, "creatures/the-kraken-2");
    assert_eq!(read(&root, "world/creatures/the-kraken-2/the-kraken-2.md"), "---\nname: The Kraken\n---\n\nBody");
    let sections: StudioWorld = call(&control, Method::StudioWorld(StudioProjectParams { block_id: id.clone() })).await;
    assert!(sections.sections.iter().any(|s| s.id == "creatures" && s.entities.len() == 2), "entity_create makes its section");
    for (section, name) in [("..", "x"), ("a/b", "x"), (".hidden", "x"), ("props", " "), ("props", "a\nb")] {
        assert_eq!(code(&control, make(section, name, Value::Null, None)).await, "bad_request", "{section} {name:?}");
    }
    assert_eq!(code(&control, make("props", "Bell", json!({"a:b": 1}), None)).await, "bad_request");

    // used_in: Entity nodes, Asset nodes of its files, the cards wired from
    // those, and cards whose last request names one of its files.
    create(&control, &id, "Board", None).await;
    let made = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_ent", "body": {"kind": "entity", "id": "characters/mara"}}},
            {"op": "add_node", "node": {"id": "n_ref", "body": {"kind": "asset", "path": "world/characters/mara/refs/turnaround.png", "media": "image"}}},
            {"op": "add_node", "node": {"id": "n_key", "body": {"kind": "gen", "makes": "image"}}},
            {"op": "add_node", "node": {"id": "n_vid", "body": {"kind": "gen", "makes": "video"}}},
            {"op": "add_node", "node": {"id": "n_asked", "body": {"kind": "gen", "makes": "image", "input": {"references": ["world/characters/mara/sheet.png"]}}}},
            {"op": "add_node", "node": {"id": "n_other", "body": {"kind": "entity", "id": "characters/teo"}}},
            {"op": "add_node", "node": {"id": "n_teo_card", "body": {"kind": "gen", "makes": "image"}}},
            {"op": "add_edge", "edge": {"from": "n_ent", "to": "n_key", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_ref", "to": "n_vid", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_other", "to": "n_teo_card", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_key", "to": "n_vid", "role": "first_frame"}},
        ]),
    )
    .await;
    assert!(made.failed.is_empty(), "{:?}", made.failed);
    let used = entity(&control, &id, "characters/mara").await.used_in;
    let mut nodes: Vec<&str> = used.iter().map(|u| u.node.as_str()).collect();
    nodes.sort();
    assert_eq!(nodes, ["n_asked", "n_ent", "n_key", "n_ref", "n_vid"]);
    assert!(used.contains(&EntityUse { canvas: "board".into(), node: "n_ent".into() }));

    stop(control, task).await;
    cleanup(&home);
}

// ---- canvas_edit -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn canvas_edit_every_op_placement_and_ids() {
    let (home, root) = temp("edit");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;
    create(&control, &id, "Board", None).await;
    write(&root, "canvases/board/assets/harbor.png", png(1600, 900));
    write(&root, "canvases/board/assets/tall.png", png(900, 1600));

    // One batch: a scene, a shot in it, an asset, two cards in the shot and
    // a wire; client ids kept, generated ones aligned with the ops.
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_scene000", "body": {"kind": "frame", "title": "Harbor at dawn", "role": "scene", "order": 1}}},
            {"op": "add_node", "node": {"id": "n_shot0000", "parent": "n_scene000", "body": {"kind": "frame", "title": "Shot 1", "role": "shot"}}},
            {"op": "add_node", "node": {"body": {"kind": "asset", "path": "./canvases/board/assets/harbor.png", "media": "image"}}},
            {"op": "add_node", "node": {"id": "n_key00000", "parent": "n_shot0000", "body": {"kind": "gen", "makes": "image", "prompt": "Mara at the rail", "status": "done"}}},
            {"op": "add_node", "node": {"id": "n_vid00000", "parent": "n_shot0000", "body": {"kind": "gen", "makes": "video", "prompt": "She turns"}}},
            {"op": "add_edge", "edge": {"from": "n_key00000", "to": "n_vid00000", "role": "first_frame"}},
            {"op": "add_node", "node": {"x": 5000, "y": -50, "w": 100, "h": 60, "z": 3, "body": {"kind": "note", "text": "a beat"}}},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    assert_eq!((edited.rev, edited.moved, edited.ids.len()), (2, false, 7));
    assert_eq!(&edited.ids[..2], ["n_scene000", "n_shot0000"]);
    assert!(edited.ids[2].starts_with("n_") && edited.ids[2].len() == 8, "{:?}", edited.ids);
    assert_eq!(&edited.ids[3..5], ["n_key00000", "n_vid00000"]);
    assert!(edited.ids[5].starts_with("e_") && edited.ids[6].starts_with("n_"));
    let asset_id = edited.ids[2].clone();
    let note_id = edited.ids[6].clone();

    let c = canvas(&control, &id, "board").await;
    assert_eq!(c.rev, 2);
    let (scene, shot, asset, key, vid, note) =
        (node(&c, "n_scene000"), node(&c, "n_shot0000"), node(&c, &asset_id), node(&c, "n_key00000"), node(&c, "n_vid00000"), node(&c, &note_id));
    // The asset: path normalised, size from its header, fit 320.
    assert_eq!(
        asset.body,
        NodeBody::Asset {
            path: "canvases/board/assets/harbor.png".into(),
            media: silo_proto::studio::StudioMedia::Image,
            width: Some(1600),
            height: Some(900),
            seconds: None,
            prompt: None
        }
    );
    assert_eq!((asset.w, asset.h), (320.0, 180.0));
    // The first node at the origin; the shot at the scene's corner; cards
    // in the shot left to right (the first one's ports inside the frame),
    // sized as the app draws them at 16:9, CARD_GAP apart.
    assert_eq!((scene.x, scene.y), (0.0, 0.0));
    assert_eq!((shot.x, shot.y, shot.parent.as_deref()), (24.0, 44.0, Some("n_scene000")));
    let card_h = size::card_height(GenKind::Image, 340.0, 16.0 / 9.0, 1).round();
    assert_eq!((key.x, key.y, key.w, key.h), (shot.x + 24.0 + size::PORT_GUTTER, shot.y + 44.0, 340.0, card_h));
    assert_eq!((vid.x, vid.y), (key.x + key.w + size::CARD_GAP, key.y));
    assert!(vid.x - (key.x + key.w) >= size::PORT_GUTTER + size::OUTPUT_GUTTER, "ports clear of the card before");
    assert!(matches!(key.body, NodeBody::Gen { status: GenStatus::Draft, .. }), "a card without takes can't be done");
    // Frames grew to hold what was placed in them, the output dot too.
    assert!(shot.x + shot.w >= vid.x + vid.w + size::OUTPUT_GUTTER + 24.0 && shot.y + shot.h >= vid.y + vid.h + 24.0, "{shot:?} holds {vid:?}");
    assert!(scene.x + scene.w >= shot.x + shot.w + 24.0 && scene.y + scene.h >= shot.y + shot.h + 24.0, "{scene:?} holds {shot:?}");
    // At the top level, right of the content as it was then (the scene,
    // 640 wide: x 680), level with the last top-level node; then pushed
    // right as the cards grew the scene, keeping its gap.
    assert_eq!((asset.x, asset.y, asset.parent.as_deref()), (scene.x + scene.w + size::GAP, 0.0, None));
    // Coordinates given are kept.
    assert_eq!((note.x, note.y, note.w, note.h, note.z), (5000.0, -50.0, 100.0, 60.0, Some(3)));
    assert_eq!(wires_into(&c, "n_vid00000", EdgeRole::FirstFrame), ["n_key00000"]);

    // A top-level frame goes below the content; a vertical shot is sized for 9:16.
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_scene2", "body": {"kind": "frame", "title": "Night", "role": "scene", "format": "vertical"}}},
            {"op": "add_node", "node": {"id": "n_card2", "parent": "n_scene2", "body": {"kind": "gen", "makes": "image"}}},
            {"op": "add_node", "node": {"id": "n_tall", "parent": "n_scene2", "body": {"kind": "asset", "path": "canvases/board/assets/tall.png", "media": "image"}}},
            {"op": "add_node", "node": {"id": "n_audio", "body": {"kind": "asset", "path": "canvases/board/assets/v.mp3", "media": "unknown"}}},
            {"op": "add_node", "node": {"id": "n_speech", "body": {"kind": "gen", "makes": "speech"}}},
            {"op": "add_node", "node": {"id": "n_shot2", "x": 0, "y": 20000, "body": {"kind": "frame", "title": "Empty", "role": "shot"}}},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let c = canvas(&control, &id, "board").await;
    let bottom = [node(&c, "n_scene000"), node(&c, &asset_id), node(&c, &note_id)].iter().map(|n| n.y + n.h).fold(f64::MIN, f64::max);
    let left = [node(&c, "n_scene000"), node(&c, &asset_id), node(&c, &note_id)].iter().map(|n| n.x).fold(f64::MAX, f64::min);
    let s2 = node(&c, "n_scene2");
    assert_eq!((s2.x, s2.y), (left, bottom + 80.0), "below the content");
    let card2 = node(&c, "n_card2");
    assert_eq!((card2.x, card2.y), (s2.x + 24.0 + size::PORT_GUTTER, s2.y + 44.0));
    assert_eq!((card2.w, card2.h), (264.0, size::card_height(GenKind::Image, 264.0, 9.0 / 16.0, 1).round()), "the scene's vertical format");
    let tall = node(&c, "n_tall");
    assert_eq!((tall.w, tall.h), (180.0, 320.0), "an image's own aspect");
    assert_eq!(tall.x, card2.x + card2.w + size::CARD_GAP, "beside a card: its output");
    let audio = node(&c, "n_audio");
    assert!(matches!(audio.body, NodeBody::Asset { media: silo_proto::studio::StudioMedia::Audio, .. }) && audio.h == 96.0);
    assert_eq!((node(&c, "n_speech").w, node(&c, "n_speech").h), (340.0, 32.0 + size::AUDIO_FACE + size::PROMPT_MIN + 36.0));
    // An empty shot holds one card with its ports and output.
    let shot2 = node(&c, "n_shot2");
    assert_eq!(shot2.w, 340.0 + size::PORT_GUTTER + size::OUTPUT_GUTTER + 48.0);
    assert_eq!(shot2.h, (44.0 + size::card_height(GenKind::Video, 340.0, 16.0 / 9.0, 1) + 24.0).round());

    // Failures fail alone; ids stay aligned (empty for a failed add).
    let rev = c.rev;
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "update_node", "id": "n_gone", "x": 1},
            {"op": "add_node", "node": {"id": "n_key00000", "body": {"kind": "note"}}},
            {"op": "add_node", "node": {"id": "bad id", "body": {"kind": "note"}}},
            {"op": "add_node", "node": {"parent": "n_key00000", "body": {"kind": "note"}}},
            {"op": "add_edge", "edge": {"from": asset_id, "to": note_id, "role": "reference"}},
            {"op": "add_edge", "edge": {"from": asset_id, "to": "n_gone", "role": "reference"}},
            {"op": "add_node", "node": {"body": {"kind": "gen", "makes": "image", "prompt": "fine"}}},
            {"op": "add_node", "node": {"body": {"kind": "asset", "path": "", "media": "image"}}},
            {"op": "add_node", "node": {"body": {"kind": "entity", "id": "mara"}}},
            {"op": "add_node", "node": {"body": {"kind": "sticker"}}},
            {"op": "remove_edge", "id": "e_gone"},
            {"op": "remove_node", "id": "n_gone"},
            {"op": "group", "ids": []},
            {"op": "add_node", "node": {"body": {"kind": "gen", "makes": "image", "takes": [{"path": "canvases/board/assets/harbor.png", "media": "image", "at": 1}], "pick": 1}}},
        ]),
    )
    .await;
    assert_eq!(
        failed(&edited),
        [
            (0, "not_found"),
            (1, RpcError::CONFLICT),
            (2, "bad_request"),
            (3, "not_found"),
            (4, "bad_request"),
            (5, "not_found"),
            (7, "bad_request"),
            (8, "bad_request"),
            (9, "bad_request"),
            (10, "not_found"),
            (11, "not_found"),
            (12, "bad_request"),
            (13, "bad_request"),
        ]
    );
    assert!(edited.failed[4].message.contains("is not a card"), "{}", edited.failed[4].message);
    assert_eq!(edited.ids.len(), 10, "one per add op");
    assert_eq!(edited.ids.iter().filter(|i| !i.is_empty()).count(), 1, "{:?}", edited.ids);
    assert!(edited.ids[5].starts_with("n_"), "the one that applied, in its place: {:?}", edited.ids);
    assert_eq!(edited.rev, rev + 1, "one write for the batch");
    // Nothing applied: no write, rev unchanged.
    let before = std::fs::read(root.join("canvases/board/canvas.json")).unwrap();
    let none = edit(&control, &id, "board", json!([{"op": "remove_node", "id": "n_gone"}])).await;
    assert_eq!((none.rev, failed(&none)), (rev + 1, vec![(0, "not_found")]));
    assert_eq!(std::fs::read(root.join("canvases/board/canvas.json")).unwrap(), before);
    // A missing canvas fails the call.
    assert_eq!(code(&control, edit_method(&id, "nope", ops(json!([{"op": "remove_node", "id": "x"}])), None)).await, "not_found");

    // update_node: geometry, a merge-patched body, pick within the takes.
    let takes = json!([
        {"path": "canvases/board/assets/k1.png", "media": "image", "at": 1},
        {"path": "canvases/board/assets/harbor.png", "media": "image", "at": 2},
    ]);
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "update_node", "id": "n_key00000", "body": {"prompt": "Mara at the rail, fog", "options": {"aspect": "16:9", "seed": 7}, "model": "nano-banana"}},
            {"op": "update_node", "id": "n_key00000", "body": {"options": {"seed": null}, "takes": takes, "status": "done", "pick": 0}},
            {"op": "update_node", "id": "n_key00000", "body": {"pick": 5}},
            {"op": "update_node", "id": "n_key00000", "body": {"kind": "note"}},
            {"op": "update_node", "id": note_id, "x": 10, "y": 20, "w": 0, "h": -5, "z": 9},
            {"op": "update_node", "id": note_id, "body": {"text": "a beat, again"}},
            {"op": "update_node", "id": "n_key00000", "body": {"takes": [{"path": "../out.png", "media": "image", "at": 1}]}},
        ]),
    )
    .await;
    assert_eq!(failed(&edited), [(2, "bad_request"), (3, "bad_request"), (6, RpcError::FORBIDDEN)]);
    let c = canvas(&control, &id, "board").await;
    let NodeBody::Gen { prompt, options, model, takes, pick, status, .. } = &node(&c, "n_key00000").body else { panic!("a card") };
    assert_eq!((prompt.as_str(), model.as_deref(), options), ("Mara at the rail, fog", Some("nano-banana"), &json!({"aspect": "16:9"})));
    assert_eq!((takes.len(), *pick, *status), (2, Some(0), GenStatus::Done));
    assert_eq!((takes[1].width, takes[1].height), (Some(1600), Some(900)), "a take's image size from its header");
    let n = node(&c, &note_id);
    assert_eq!((n.x, n.y, n.w, n.h, n.z), (10.0, 20.0, 100.0, 60.0, Some(9)), "sizes must be positive");
    assert_eq!(n.body, NodeBody::Note { text: "a beat, again".into() });

    // Parents: out of a frame, into one, never into itself.
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "update_node", "id": "n_card2", "parent": null},
            {"op": "update_node", "id": "n_scene000", "parent": "n_shot0000"},
            {"op": "update_node", "id": "n_scene000", "parent": "n_scene000"},
            {"op": "update_node", "id": note_id, "parent": asset_id},
            {"op": "update_node", "id": note_id, "parent": "n_scene2"},
        ]),
    )
    .await;
    assert_eq!(failed(&edited), [(1, "bad_request"), (2, "bad_request"), (3, "not_found")]);
    let c = canvas(&control, &id, "board").await;
    assert_eq!((node(&c, "n_card2").parent.as_deref(), node(&c, &note_id).parent.as_deref()), (None, Some("n_scene2")));

    // Layout: a row, a column, a grid in a frame (moved with their contents, the frame grown).
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "a1", "x": 0, "y": 5000, "w": 100, "h": 50, "body": {"kind": "note"}}},
            {"op": "add_node", "node": {"id": "a2", "x": 999, "y": 999, "w": 60, "h": 80, "body": {"kind": "note"}}},
            {"op": "add_node", "node": {"id": "a3", "x": -9, "y": 3, "w": 100, "h": 50, "body": {"kind": "note"}}},
            {"op": "add_node", "node": {"id": "a4", "x": 7, "y": 7, "w": 100, "h": 50, "body": {"kind": "note"}}},
            {"op": "layout", "ids": ["a1", "a2", "a3"], "as": "row", "gap": 10},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let c = canvas(&control, &id, "board").await;
    let at = |c: &Canvas, id: &str| (node(c, id).x, node(c, id).y);
    assert_eq!([at(&c, "a1"), at(&c, "a2"), at(&c, "a3")], [(0.0, 5000.0), (110.0, 5000.0), (180.0, 5000.0)]);
    edit(&control, &id, "board", json!([{"op": "layout", "ids": ["a3", "a1"], "as": "column"}])).await;
    let c = canvas(&control, &id, "board").await;
    assert_eq!([at(&c, "a3"), at(&c, "a1")], [(180.0, 5000.0), (180.0, 5090.0)]);
    edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "f_grid", "x": 1000, "y": 6000, "w": 50, "h": 50, "body": {"kind": "frame", "role": "group"}}},
            {"op": "add_node", "node": {"id": "f_inner", "x": 0, "y": 7000, "w": 300, "h": 300, "body": {"kind": "frame", "role": "shot"}}},
            {"op": "add_node", "node": {"id": "a5", "x": 10, "y": 7100, "w": 20, "h": 20, "parent": "f_inner", "body": {"kind": "note"}}},
        ]),
    )
    .await;
    let edited = edit(&control, &id, "board", json!([{"op": "layout", "ids": ["a1", "a2", "a3", "f_inner"], "as": "grid", "in": "f_grid"}])).await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let c = canvas(&control, &id, "board").await;
    // Cells of the largest node (300×300), 2 columns, from the frame's corner.
    assert_eq!(
        [at(&c, "a1"), at(&c, "a2"), at(&c, "a3"), at(&c, "f_inner")],
        [(1024.0, 6044.0), (1364.0, 6044.0), (1024.0, 6384.0), (1364.0, 6384.0)]
    );
    assert_eq!(at(&c, "a5"), (1374.0, 6484.0), "moved with its frame");
    assert!(["a1", "a2", "a3", "f_inner"].iter().all(|i| node(&c, i).parent.as_deref() == Some("f_grid")));
    let grid = node(&c, "f_grid");
    assert!(grid.x + grid.w >= 1364.0 + 300.0 + 24.0 && grid.y + grid.h >= 6384.0 + 300.0 + 24.0, "grown: {grid:?}");
    // Cards in a row or a grid: CARD_GAP apart, their ports and outputs clear.
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "c1", "x": 0, "y": 9000, "body": {"kind": "gen", "makes": "image", "options": {"aspect_ratio": "9:16"}}}},
            {"op": "add_node", "node": {"id": "c2", "x": 0, "y": 9000, "body": {"kind": "gen", "makes": "image", "options": {"aspect_ratio": "9:16"}}}},
            {"op": "add_node", "node": {"id": "c3", "x": 0, "y": 9000, "body": {"kind": "gen", "makes": "video"}}},
            {"op": "add_node", "node": {"id": "c4", "x": 0, "y": 9000, "body": {"kind": "note"}}},
            {"op": "layout", "ids": ["c1", "c2", "c3", "c4"], "as": "row"},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let c = canvas(&control, &id, "board").await;
    assert_eq!(node(&c, "c1").w, 264.0, "a card's own aspect");
    for pair in ["c1", "c2", "c3", "c4"].windows(2) {
        let (prev, next) = (node(&c, pair[0]), node(&c, pair[1]));
        assert_eq!(next.x - (prev.x + prev.w), size::CARD_GAP, "{} → {}", pair[0], pair[1]);
        assert!(next.x - (prev.x + prev.w) >= size::PORT_GUTTER + size::OUTPUT_GUTTER);
    }
    edit(&control, &id, "board", json!([{"op": "layout", "ids": ["c1", "c2", "c3", "c4"], "as": "grid"}])).await;
    let c = canvas(&control, &id, "board").await;
    let (c1, c2, c3) = (node(&c, "c1"), node(&c, "c2"), node(&c, "c3"));
    assert!(c2.x - (c1.x + c3.w.max(c1.w)) >= size::PORT_GUTTER + size::OUTPUT_GUTTER, "cells are the widest card plus CARD_GAP");
    assert_eq!((c2.y, c3.x), (c1.y, c1.x));
    assert!(c3.y >= c1.y + c1.h + size::GAP, "rows GAP apart");

    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "layout", "ids": [], "as": "row"},
            {"op": "layout", "ids": ["a1"], "as": "circle"},
            {"op": "layout", "ids": ["a1", "n_gone"]},
            {"op": "layout", "ids": ["a1"], "in": "a2"},
            {"op": "layout", "ids": ["f_grid"], "in": "f_inner"},
        ]),
    )
    .await;
    assert_eq!(failed(&edited), [(0, "bad_request"), (1, "bad_request"), (2, "not_found"), (3, "not_found"), (4, "bad_request")]);

    // remove_node: its edges go; a frame's children stay, in its parent.
    edit(&control, &id, "board", json!([{"op": "add_edge", "edge": {"id": "e_ref", "from": asset_id, "to": "n_vid00000", "role": "reference"}}]))
        .await;
    let edited = edit(&control, &id, "board", json!([{"op": "remove_node", "id": "n_shot0000"}, {"op": "remove_node", "id": asset_id}])).await;
    assert!(edited.failed.is_empty());
    let c = canvas(&control, &id, "board").await;
    assert!(c.node("n_shot0000").is_none());
    assert_eq!((node(&c, "n_key00000").parent.as_deref(), node(&c, "n_vid00000").parent.as_deref()), (Some("n_scene000"), Some("n_scene000")));
    assert!(c.edges.iter().all(|e| e.id != "e_ref"), "the removed node's edges went");
    assert_eq!(wires_into(&c, "n_vid00000", EdgeRole::FirstFrame), ["n_key00000"], "others stay");

    // base_rev: current → not moved; behind → applied anyway, moved.
    let rev = c.rev;
    let now: CanvasEdited = call(&control, edit_method(&id, "board", ops(json!([{"op": "update_node", "id": "a1", "x": 1}])), Some(rev))).await;
    assert_eq!((now.rev, now.moved), (rev + 1, false));
    let behind: CanvasEdited = call(&control, edit_method(&id, "board", ops(json!([{"op": "update_node", "id": "a1", "x": 2}])), Some(rev))).await;
    assert_eq!((behind.rev, behind.moved, behind.failed.len()), (rev + 2, true, 0));
    assert_eq!(node(&canvas(&control, &id, "board").await, "a1").x, 2.0);

    // Every write is whole: no temporary file is left, and it parses.
    let names: Vec<String> =
        std::fs::read_dir(root.join("canvases/board")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    let file: Value = serde_json::from_str(&read(&root, "canvases/board/canvas.json")).unwrap();
    assert_eq!(file["rev"].as_u64(), Some(rev + 2));
    assert!(file.get("out_of_date").is_none(), "computed at read time, never saved");

    stop(control, task).await;
    cleanup(&home);
}

/// Pairs of nodes in the same frame (or both at the top level) whose rects overlap.
fn overlapping(c: &Canvas) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, a) in c.nodes.iter().enumerate() {
        for b in &c.nodes[i + 1..] {
            let apart = a.x + a.w <= b.x + 0.5 || b.x + b.w <= a.x + 0.5 || a.y + a.h <= b.y + 0.5 || b.y + b.h <= a.y + 0.5;
            if a.parent == b.parent && !apart {
                out.push((a.id.clone(), b.id.clone()));
            }
        }
    }
    out
}

/// Every node lies inside its frame.
fn inside_frames(c: &Canvas) -> bool {
    c.nodes.iter().all(|n| {
        n.parent.as_deref().is_none_or(|p| {
            let f = node(c, p);
            n.x >= f.x && n.y >= f.y && n.x + n.w <= f.x + f.w && n.y + n.h <= f.y + f.h
        })
    })
}

fn kids<'a>(c: &'a Canvas, parent: &str) -> Vec<&'a Node> {
    c.nodes.iter().filter(|n| n.parent.as_deref() == Some(parent)).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn growing_frames_push_what_follows_and_tidy_repairs() {
    let (home, root) = temp("grow");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;
    create(&control, &id, "Intro", None).await;

    // The real canvas's story: a brief, three scenes made first (below the content: y 240, 720, 1200), then
    // filled shot by shot with a note and a tall card each.
    let scene = |n: u32| json!({"op": "add_node", "node": {"id": format!("s{n}"), "body": {"kind": "frame", "title": format!("Scene {n}"), "role": "scene", "order": n}}});
    edit(
        &control,
        &id,
        "intro",
        json!([{"op": "add_node", "node": {"id": "brief", "body": {"kind": "note", "text": "brief"}}}, scene(1), scene(2), scene(3)]),
    )
    .await;
    let c = canvas(&control, &id, "intro").await;
    assert_eq!(["s1", "s2", "s3"].map(|s| node(&c, s).y), [240.0, 720.0, 1200.0]);
    let mut shot = 0;
    for (s, shots) in [("s1", 5), ("s2", 3), ("s3", 3)] {
        for o in 1..=shots {
            shot += 1;
            let edited = edit(
                &control,
                &id,
                "intro",
                json!([
                    {"op": "add_node", "node": {"id": format!("b{shot}"), "parent": s, "body": {"kind": "frame", "title": format!("Shot {o}"), "role": "shot", "order": o}}},
                    {"op": "add_node", "node": {"id": format!("n{shot}"), "parent": format!("b{shot}"), "body": {"kind": "note", "text": "beat"}}},
                    {"op": "add_node", "node": {"id": format!("k{shot}"), "parent": format!("b{shot}"), "body": {"kind": "gen", "makes": "image", "prompt": "a keyframe", "options": {"aspect_ratio": "9:16"}}}},
                ]),
            )
            .await;
            assert!(edited.failed.is_empty(), "{:?}", edited.failed);
            assert_eq!(edited.rev, canvas(&control, &id, "intro").await.rev, "one rev per batch");
        }
    }
    let c = canvas(&control, &id, "intro").await;
    assert_eq!(overlapping(&c), Vec::<(String, String)>::new());
    assert!(inside_frames(&c), "contents moved with their frames");
    let (s1, s2, s3) = (node(&c, "s1"), node(&c, "s2"), node(&c, "s3"));
    assert!(s1.h > 400.0 && s1.w > 640.0, "grown: {s1:?}");
    assert_eq!((s2.y, s3.y), (s1.y + s1.h + 2.0 * size::GAP, s2.y + s2.h + 2.0 * size::GAP), "pushed down, gaps kept");
    for s in ["s1", "s2", "s3"] {
        let shots = kids(&c, s);
        assert!(shots.windows(2).all(|w| w[0].x + w[0].w + size::GAP == w[1].x && w[0].y == w[1].y), "shots in a row, in order");
    }
    assert_eq!((node(&c, "brief").x, node(&c, "brief").y), (0.0, 0.0), "what was above stays");

    // Sideways: shots made first, then filled: later shots move right with their contents.
    edit(
        &control,
        &id,
        "intro",
        json!([
            {"op": "add_node", "node": {"id": "s4", "body": {"kind": "frame", "title": "Scene 4", "role": "scene", "order": 4}}},
            {"op": "add_node", "node": {"id": "x1", "parent": "s4", "body": {"kind": "frame", "role": "shot", "order": 1}}},
            {"op": "add_node", "node": {"id": "x2", "parent": "s4", "body": {"kind": "frame", "role": "shot", "order": 2}}},
            {"op": "add_node", "node": {"id": "x3", "parent": "s4", "body": {"kind": "frame", "role": "shot", "order": 3}}},
            {"op": "add_node", "node": {"id": "xn3", "parent": "x3", "body": {"kind": "note"}}},
            // Unrelated: left of the scene below it, and far right above it.
            {"op": "add_node", "node": {"id": "far_left", "x": -2000, "y": 9000, "w": 100, "h": 100, "body": {"kind": "note"}}},
            {"op": "add_node", "node": {"id": "far_up", "x": 9000, "y": -900, "w": 100, "h": 100, "body": {"kind": "note"}}},
        ]),
    )
    .await;
    let before = canvas(&control, &id, "intro").await;
    edit(
        &control,
        &id,
        "intro",
        json!([
            {"op": "add_node", "node": {"parent": "x1", "body": {"kind": "note"}}},
            {"op": "add_node", "node": {"parent": "x1", "body": {"kind": "gen", "makes": "video", "prompt": "p"}}},
        ]),
    )
    .await;
    let c = canvas(&control, &id, "intro").await;
    let dw = node(&c, "x1").w - node(&before, "x1").w;
    assert!(dw > 0.0);
    for n in ["x2", "x3", "xn3"] {
        assert_eq!((node(&c, n).x, node(&c, n).y), (node(&before, n).x + dw, node(&before, n).y), "{n} moved right by the growth");
    }
    for n in ["far_left", "far_up", "brief", "s1", "s2", "s3", "b1", "k3"] {
        assert_eq!((node(&c, n).x, node(&c, n).y), (node(&before, n).x, node(&before, n).y), "{n} stays");
    }
    assert_eq!(overlapping(&c), Vec::<(String, String)>::new());
    assert!(inside_frames(&c));

    // layout carries contents and uses grown sizes: the scenes in a column in another order.
    edit(&control, &id, "intro", json!([{"op": "layout", "ids": ["s3", "s1", "s2"], "as": "column"}])).await;
    let after = canvas(&control, &id, "intro").await;
    assert_eq!(node(&after, "s1").y, node(&after, "s3").y + node(&after, "s3").h + size::GAP);
    let dy = node(&after, "s1").y - node(&c, "s1").y;
    assert_eq!(node(&after, "k3").y, node(&c, "k3").y + dy, "a card two frames down moved with its scene");
    assert!(inside_frames(&after));

    // Tidy on the real canvas's shape: scenes at y 240, 720, 1200, each 765 tall, overlapping.
    create(&control, &id, "Real", None).await;
    let mut nodes = vec![json!({"id": "brief", "x": 0, "y": 0, "w": 240, "h": 160, "body": {"kind": "note", "text": "brief"}})];
    let mut b = 0;
    for (s, (y, shots)) in [(240.0, 5), (720.0, 3), (1200.0, 3)].into_iter().enumerate() {
        let s = s + 1;
        let w = 8.0 + f64::from(shots) * 744.0; // 3728 for five shots
        nodes.push(
            json!({"id": format!("s{s}"), "x": 0, "y": y, "w": w, "h": 765, "body": {"kind": "frame", "title": "S", "role": "scene", "order": s}}),
        );
        for o in 0..shots {
            b += 1;
            let x = 24.0 + f64::from(o) * 744.0;
            nodes.push(json!({"id": format!("b{b}"), "parent": format!("s{s}"), "x": x, "y": y + 44.0, "w": 704, "h": 697, "body": {"kind": "frame", "title": "B", "role": "shot", "order": o + 1}}));
            nodes.push(json!({"id": format!("n{b}"), "parent": format!("b{b}"), "x": x + 24.0, "y": y + 88.0, "w": 240, "h": 160, "body": {"kind": "note", "text": "t"}}));
            nodes.push(json!({"id": format!("k{b}"), "parent": format!("b{b}"), "x": x + 379.0, "y": y + 88.0, "w": 264, "h": 629, "body": {"kind": "gen", "makes": "image", "prompt": "p"}}));
        }
    }
    let edges = json!([{"id": "e1", "from": "n1", "to": "k1", "role": "text"}]);
    write(
        &root,
        "canvases/real/canvas.json",
        serde_json::to_vec(&json!({"version": 1, "title": "Real", "rev": 5, "nodes": nodes, "edges": edges})).unwrap(),
    );
    let messy = canvas(&control, &id, "real").await;
    assert!(!overlapping(&messy).is_empty());
    let edited = edit(&control, &id, "real", json!([{"op": "tidy"}])).await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    assert_eq!(edited.rev, messy.rev + 1);
    let tidy = canvas(&control, &id, "real").await;
    assert_eq!(overlapping(&tidy), Vec::<(String, String)>::new());
    assert!(inside_frames(&tidy));
    assert_eq!(tidy.edges, messy.edges, "wires untouched");
    let (t1, t2, t3) = (node(&tidy, "s1"), node(&tidy, "s2"), node(&tidy, "s3"));
    assert!(t1.y < t2.y && t2.y < t3.y && t1.y >= 160.0, "order kept, below the brief");
    assert_eq!((t1.x, t2.x, t3.x, t1.h), (0.0, 0.0, 0.0, 765.0), "already fitted");
    for n in &messy.nodes {
        let t = node(&tidy, &n.id);
        if !matches!(n.body, NodeBody::Frame { .. }) {
            assert_eq!((t.w, t.h, &t.parent), (n.w, n.h, &n.parent), "{} keeps its size", n.id);
        }
    }
    // Contents kept their place in their frames.
    let off = |c: &Canvas, id: &str| (node(c, id).x - node(c, "s2").x, node(c, id).y - node(c, "s2").y);
    assert_eq!(off(&tidy, "k7"), off(&messy, "k7"));
    // Idempotent: a second tidy changes nothing, not even the rev.
    let again = edit(&control, &id, "real", json!([{"op": "tidy"}])).await;
    assert_eq!(again.rev, tidy.rev);
    assert_eq!(canvas(&control, &id, "real").await.nodes, tidy.nodes);

    // Tidy with a frame: shots in scene 2 shuffled and overlapping; only that scene re-flows.
    edit(&control, &id, "real", json!([{"op": "update_node", "id": "b7", "x": 30}, {"op": "update_node", "id": "b8", "x": 500, "y": 2000}])).await;
    let shuffled = canvas(&control, &id, "real").await;
    let edited = edit(&control, &id, "real", json!([{"op": "tidy", "frame": "s2"}, {"op": "tidy", "frame": "n1"}])).await;
    assert_eq!(failed(&edited), [(1, "not_found")]);
    let c = canvas(&control, &id, "real").await;
    let row: Vec<&str> = kids(&c, "s2").into_iter().map(|n| n.id.as_str()).collect();
    assert_eq!(row, ["b6", "b7", "b8"]);
    assert_eq!(overlapping(&c), Vec::<(String, String)>::new());
    assert!(inside_frames(&c));
    let (b6, b7, b8) = (node(&c, "b6"), node(&c, "b7"), node(&c, "b8"));
    assert!(b6.x + b6.w + size::GAP == b7.x && b7.x + b7.w + size::GAP == b8.x && b6.y == b8.y, "a row in order");
    for n in ["s1", "s3", "b1", "k11"] {
        assert_eq!((node(&c, n).x, node(&c, n).y), (node(&shuffled, n).x, node(&shuffled, n).y), "{n} outside s2 stays");
    }

    stop(control, task).await;
    cleanup(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn wires_client_ids_and_single_ports() {
    let (home, root) = temp("wires");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;
    create(&control, &id, "Board", None).await;
    write(&root, "canvases/board/assets/a.png", png(16, 9));
    write(&root, "canvases/board/assets/b.mp4", b"\0\0\0\x18ftypmp42");

    // The board's own ids (`n_`/`e_` + 8 hex): one batch adds cards and
    // wires them; the same ids come back.
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_0a1b2c3d", "body": {"kind": "asset", "path": "canvases/board/assets/a.png", "media": "image"}}},
            {"op": "add_node", "node": {"id": "n_11111111", "body": {"kind": "gen", "makes": "image", "prompt": "key"}}},
            {"op": "add_node", "node": {"id": "n_22222222", "body": {"kind": "gen", "makes": "video", "prompt": "move"}}},
            {"op": "add_node", "node": {"id": "n_33333333", "body": {"kind": "note", "text": "She turns"}}},
            {"op": "add_node", "node": {"id": "n_44444444", "body": {"kind": "asset", "path": "canvases/board/assets/b.mp4", "media": "video"}}},
            {"op": "add_edge", "edge": {"id": "e_aaaaaaaa", "from": "n_0a1b2c3d", "to": "n_11111111", "role": "reference"}},
            {"op": "add_edge", "edge": {"id": "e_bbbbbbbb", "from": "n_11111111", "to": "n_22222222", "role": "first_frame"}},
            {"op": "add_edge", "edge": {"id": "e_cccccccc", "from": "n_33333333", "to": "n_22222222", "role": "text", "label": "beat"}},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    assert_eq!(edited.ids, ["n_0a1b2c3d", "n_11111111", "n_22222222", "n_33333333", "n_44444444", "e_aaaaaaaa", "e_bbbbbbbb", "e_cccccccc"]);
    let c = canvas(&control, &id, "board").await;
    assert_eq!(c.edges.iter().find(|e| e.id == "e_cccccccc").unwrap().label.as_deref(), Some("beat"));

    // A batch adds a card and wires an existing node into it by the client's id.
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_55555555", "body": {"kind": "gen", "makes": "video"}}},
            {"op": "add_edge", "edge": {"id": "e_dddddddd", "from": "n_11111111", "to": "n_55555555", "role": "last_frame"}},
            {"op": "add_edge", "edge": {"from": "n_0a1b2c3d", "to": "n_55555555", "role": "reference"}},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    assert_eq!(&edited.ids[..2], ["n_55555555", "e_dddddddd"]);
    assert!(edited.ids[2].starts_with("e_"));
    let c = canvas(&control, &id, "board").await;
    assert_eq!(wires_into(&c, "n_55555555", EdgeRole::LastFrame), ["n_11111111"]);

    // A given id already there fails that op alone (nodes and edges).
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_11111111", "body": {"kind": "note"}}},
            {"op": "add_edge", "edge": {"id": "e_aaaaaaaa", "from": "n_0a1b2c3d", "to": "n_55555555", "role": "reference"}},
            {"op": "add_node", "node": {"id": "n_66666666", "body": {"kind": "note"}}},
        ]),
    )
    .await;
    assert_eq!(failed(&edited), [(0, RpcError::CONFLICT), (1, RpcError::CONFLICT)]);
    assert_eq!(edited.ids, ["", "", "n_66666666"]);

    // Undo of a delete: the same ids again, wires with them.
    let edited = edit(&control, &id, "board", json!([{"op": "remove_node", "id": "n_11111111"}])).await;
    assert!(edited.failed.is_empty());
    let c = canvas(&control, &id, "board").await;
    assert!(c.node("n_11111111").is_none() && !c.edges.iter().any(|e| ["e_aaaaaaaa", "e_bbbbbbbb", "e_dddddddd"].contains(&e.id.as_str())));
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_11111111", "x": 0, "y": 0, "w": 320, "h": 276, "body": {"kind": "gen", "makes": "image", "prompt": "key"}}},
            {"op": "add_edge", "edge": {"id": "e_aaaaaaaa", "from": "n_0a1b2c3d", "to": "n_11111111", "role": "reference"}},
            {"op": "add_edge", "edge": {"id": "e_bbbbbbbb", "from": "n_11111111", "to": "n_22222222", "role": "first_frame"}},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    assert_eq!(edited.ids, ["n_11111111", "e_aaaaaaaa", "e_bbbbbbbb"]);
    let c = canvas(&control, &id, "board").await;
    assert_eq!(wires_into(&c, "n_22222222", EdgeRole::FirstFrame), ["n_11111111"]);

    // A single-wire port: the board's explicit remove then add leaves one wire.
    let position = |c: &Canvas, id: &str| c.edges.iter().position(|e| e.id == id);
    let before = position(&c, "e_bbbbbbbb").unwrap();
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "remove_edge", "id": "e_bbbbbbbb"},
            {"op": "add_edge", "edge": {"id": "e_eeeeeeee", "from": "n_0a1b2c3d", "to": "n_22222222", "role": "first_frame"}},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let c = canvas(&control, &id, "board").await;
    assert_eq!(wires_into(&c, "n_22222222", EdgeRole::FirstFrame), ["n_0a1b2c3d"]);
    // Its undo: remove the new, add the old back.
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "remove_edge", "id": "e_eeeeeeee"},
            {"op": "add_edge", "edge": {"id": "e_bbbbbbbb", "from": "n_11111111", "to": "n_22222222", "role": "first_frame"}},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    // An add alone onto the occupied port replaces the old wire, in its place.
    let c = canvas(&control, &id, "board").await;
    let before_add = position(&c, "e_bbbbbbbb").unwrap();
    let edited = edit(
        &control,
        &id,
        "board",
        json!([{"op": "add_edge", "edge": {"id": "e_ffffffff", "from": "n_0a1b2c3d", "to": "n_22222222", "role": "first_frame"}}]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let c = canvas(&control, &id, "board").await;
    assert_eq!(wires_into(&c, "n_22222222", EdgeRole::FirstFrame), ["n_0a1b2c3d"]);
    assert_eq!(position(&c, "e_ffffffff"), Some(before_add), "in the old wire's place");
    assert!(position(&c, "e_bbbbbbbb").is_none());
    let _ = before;

    // Wire validation through the server, each failing with its reason.
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_edge", "edge": {"from": "n_0a1b2c3d", "to": "n_11111111", "role": "first_frame"}},
            {"op": "add_edge", "edge": {"from": "n_44444444", "to": "n_11111111", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_0a1b2c3d", "to": "n_11111111", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_22222222", "to": "n_11111111", "role": "reference"}},
            {"op": "add_node", "node": {"id": "n_77777777", "body": {"kind": "gen", "makes": "image"}}},
            {"op": "add_edge", "edge": {"from": "n_11111111", "to": "n_77777777", "role": "reference"}},
            {"op": "add_node", "node": {"id": "n_88888888", "body": {"kind": "gen", "makes": "image"}}},
            {"op": "add_edge", "edge": {"from": "n_77777777", "to": "n_88888888", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_88888888", "to": "n_11111111", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_11111111", "to": "n_11111111", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_33333333", "to": "n_11111111", "role": "next"}},
            {"op": "add_edge", "edge": {"from": "n_44444444", "to": "n_22222222", "role": "video"}},
            {"op": "add_edge", "edge": {"from": "n_33333333", "to": "n_11111111", "role": "sideways"}},
            {"op": "add_edge", "edge": {"from": "n_33333333", "to": "n_0a1b2c3d", "role": "text"}},
        ]),
    )
    .await;
    let reasons: Vec<(u32, &str)> = edited.failed.iter().map(|f| (f.op, f.message.as_str())).collect();
    assert_eq!(reasons.iter().map(|r| r.0).collect::<Vec<_>>(), [0, 1, 2, 3, 8, 9, 12, 13], "{reasons:?}");
    assert!(reasons[0].1.contains("has no first frame port"), "{reasons:?}");
    assert!(reasons[1].1.contains("takes an image"), "{reasons:?}");
    assert!(reasons[2].1.contains("already wired"), "{reasons:?}");
    assert!(reasons[3].1.contains("outputs a video"), "{reasons:?}");
    assert!(reasons[4].1.contains("loop"), "through two cards: {reasons:?}");
    assert!(reasons[5].1.contains("itself"), "{reasons:?}");
    assert!(reasons[6].1.contains("unknown edge role"), "{reasons:?}");
    assert!(reasons[7].1.contains("is not a card"), "{reasons:?}");
    assert!(edited.failed.iter().all(|f| f.code == "bad_request"));

    stop(control, task).await;
    cleanup(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_edits_are_serialised() {
    let (home, root) = temp("concurrent");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;
    create(&control, &id, "Board", None).await;
    let mut clients = Vec::new();
    for _ in 0..4 {
        clients.push(connect(&home).await.0);
    }
    let mut tasks = Vec::new();
    for (c, client) in clients.iter().enumerate() {
        for n in 0..8 {
            let (client, id) = (client.clone(), id.clone());
            tasks.push(tokio::spawn(async move {
                let list = ops(json!([{"op": "add_node", "node": {"id": format!("n_c{c}n{n}"), "body": {"kind": "note", "text": "x"}}}]));
                client.call::<CanvasEdited>(edit_method(&id, "board", list, None)).await.unwrap()
            }));
        }
    }
    let mut revs = Vec::new();
    for t in tasks {
        let edited = t.await.unwrap();
        assert!(edited.failed.is_empty());
        revs.push(edited.rev);
    }
    revs.sort_unstable();
    assert_eq!(revs, (2..34).collect::<Vec<_>>(), "each edit its own rev");
    let c = canvas(&control, &id, "board").await;
    assert_eq!((c.nodes.len(), c.rev), (32, 33), "no edit lost");
    stop(control, task).await;
    cleanup(&home);
}

// ---- outside edits and out of date -----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn outside_edits_and_out_of_date_at_read_time() {
    let (home, root) = temp("outside");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;

    // A canvas written by hand: unknown keys, a node of a newer kind, a node
    // and an edge without ids, a broken node.
    let hand = json!({
        "version": 1, "title": "By hand", "rev": 5, "camera": {"x": 10, "zoom": 2},
        "nodes": [
            {"id": "n_st", "x": 1, "y": 2, "w": 3, "h": 4, "body": {"kind": "sticker", "emoji": "🌊", "spin": 3}},
            {"x": 5, "body": {"kind": "note", "text": "no id"}},
            "not a node",
            {"id": "n_a", "body": {"kind": "note", "text": "a"}},
        ],
        "edges": [{"from": "n_st", "to": "n_a", "role": "next"}],
    });
    write(&root, "canvases/hand/canvas.json", serde_json::to_vec_pretty(&hand).unwrap());
    let c = canvas(&control, &id, "hand").await;
    assert_eq!((c.rev, c.title.as_str(), c.nodes.len(), c.edges.len()), (5, "By hand", 3, 1));
    assert_eq!(node(&c, "n_st").body, NodeBody::Unknown);
    assert!(c.nodes[1].id.starts_with("n_") && c.edges[0].id.starts_with("e_"), "ids given");
    let edited =
        edit(&control, &id, "hand", json!([{"op": "update_node", "id": "n_st", "x": 50}, {"op": "update_node", "id": "n_st", "body": {"spin": 4}}]))
            .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    assert_eq!(edited.rev, 6);
    let file: Value = serde_json::from_str(&read(&root, "canvases/hand/canvas.json")).unwrap();
    assert_eq!(file["camera"], json!({"x": 10, "zoom": 2}), "unknown keys kept");
    assert_eq!(file["nodes"][0]["body"], json!({"kind": "sticker", "emoji": "🌊", "spin": 4}), "an unknown kind kept as written, merge-patched");
    assert_eq!(file["nodes"][0]["x"], json!(50.0));
    assert_eq!(file["nodes"][1]["id"], json!(c.nodes[1].id), "the id it got is written");

    // Edited outside again with the same rev: reads one above the last seen.
    let mut again = file.clone();
    again["nodes"][2]["body"]["text"] = json!("changed by an agent");
    write(&root, "canvases/hand/canvas.json", serde_json::to_vec_pretty(&again).unwrap());
    let c = canvas(&control, &id, "hand").await;
    assert_eq!((c.rev, node(&c, "n_a").body.clone()), (7, NodeBody::Note { text: "changed by an agent".into() }));
    assert_eq!(canvas(&control, &id, "hand").await.rev, 7, "stable until it changes again");
    let edited = edit(&control, &id, "hand", json!([{"op": "update_node", "id": "n_a", "x": 1}])).await;
    assert_eq!(edited.rev, 8);
    // Not JSON: a bad request, the file left alone.
    write(&root, "canvases/broken/canvas.json", "{ nope");
    assert_eq!(code(&control, Method::StudioCanvas(StudioCanvasParams { block_id: id.clone(), name: "broken".into() })).await, "bad_request");
    assert_eq!(read(&root, "canvases/broken/canvas.json"), "{ nope");

    // Out of date: a card made from a wired file, an entity's sheet and a note.
    create(&control, &id, "Board", None).await;
    write(&root, "canvases/board/assets/ref.png", png(16, 9));
    write(&root, "canvases/board/assets/take.png", png(16, 9));
    write(&root, "world/characters/mara/mara.md", "---\nname: Mara Venn\nsheet: sheet.png\n---\n");
    write(&root, "world/characters/mara/sheet.png", png(8, 8));
    edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_ref", "body": {"kind": "asset", "path": "canvases/board/assets/ref.png", "media": "image"}}},
            {"op": "add_node", "node": {"id": "n_mara", "body": {"kind": "entity", "id": "characters/mara"}}},
            {"op": "add_node", "node": {"id": "n_beat", "body": {"kind": "note", "text": "She turns"}}},
            {"op": "add_node", "node": {"id": "n_card", "body": {"kind": "gen", "makes": "image"}}},
            {"op": "add_node", "node": {"id": "n_draft", "body": {"kind": "gen", "makes": "image"}}},
            {"op": "add_edge", "edge": {"from": "n_ref", "to": "n_card", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_mara", "to": "n_card", "role": "reference"}},
            {"op": "add_edge", "edge": {"from": "n_beat", "to": "n_card", "role": "text"}},
            {"op": "add_edge", "edge": {"from": "n_ref", "to": "n_draft", "role": "reference"}},
        ]),
    )
    .await;
    // The keys otto writes: the files at their mtimes in unix ms.
    let c = canvas(&control, &id, "board").await;
    let sections = project(&control, &id).await.sections;
    let mtime = |rel: &str| root.join(rel).metadata().ok().map(|_| ms(&root.join(rel)));
    let keys = c.input_keys("n_card", &sections, mtime);
    assert_eq!(keys.len(), 3, "{keys:?}");
    assert_eq!(keys[0], format!("canvases/board/assets/ref.png@{}", ms(&root.join("canvases/board/assets/ref.png"))));
    let made = json!({"status": "done", "takes": [{"path": "canvases/board/assets/take.png", "media": "image", "at": 1}], "inputs_used": keys});
    edit(&control, &id, "board", json!([{"op": "update_node", "id": "n_card", "body": made}])).await;
    let c = canvas(&control, &id, "board").await;
    assert!(c.out_of_date.is_empty(), "made from what is wired now: {:?}", c.out_of_date_why);

    // The wired file changes (a later mtime): out of date, saying why.
    touch(&root.join("canvases/board/assets/ref.png"), 5);
    let c = canvas(&control, &id, "board").await;
    assert_eq!(c.out_of_date, ["n_card"], "drafts never are");
    assert_eq!(c.out_of_date_why["n_card"], ["ref.png changed"]);
    // The sheet too, and the note's text.
    touch(&root.join("world/characters/mara/sheet.png"), 5);
    edit(&control, &id, "board", json!([{"op": "update_node", "id": "n_beat", "body": {"text": "She turns back"}}])).await;
    let c = canvas(&control, &id, "board").await;
    let mut why = c.out_of_date_why["n_card"].clone();
    why.sort();
    assert_eq!(why, ["Mara Venn changed", "She turns back changed", "ref.png changed"]);
    // Never saved.
    assert!(!read(&root, "canvases/board/canvas.json").contains("out_of_date\""));
    // A card running again is not.
    edit(&control, &id, "board", json!([{"op": "update_node", "id": "n_card", "body": {"status": "running"}}])).await;
    assert!(canvas(&control, &id, "board").await.out_of_date.is_empty());

    // A document (the script) pinned without `media`: inferred, sized as a document card; a card wired from it
    // goes out of date when the file is rewritten.
    write(&root, "canvases/board/script.md", "# Beats\n1. The river");
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"id": "n_doc", "body": {"kind": "asset", "path": "canvases/board/script.md"}}},
            {"op": "add_node", "node": {"id": "n_img", "body": {"kind": "asset", "path": "canvases/board/assets/ref.png"}}},
            {"op": "add_node", "node": {"id": "n_k", "body": {"kind": "gen", "makes": "image"}}},
            {"op": "add_edge", "edge": {"from": "n_doc", "to": "n_k", "role": "text"}},
        ]),
    )
    .await;
    assert!(edited.failed.is_empty(), "{:?}", edited.failed);
    let c = canvas(&control, &id, "board").await;
    let doc = node(&c, "n_doc");
    assert!(matches!(doc.body, NodeBody::Asset { media: StudioMedia::Text, .. }), "{:?}", doc.body);
    assert_eq!((doc.w, doc.h), (silo_proto::studio::size::DOC_W, silo_proto::studio::size::DOC_H));
    assert!(matches!(node(&c, "n_img").body, NodeBody::Asset { media: StudioMedia::Image, .. }), "every asset's media is inferred");
    let keys = c.input_keys("n_k", &sections, mtime);
    assert_eq!(keys, [format!("canvases/board/script.md@{}", ms(&root.join("canvases/board/script.md")))]);
    let made = json!({"status": "done", "takes": [{"path": "canvases/board/assets/take.png", "media": "image", "at": 1}], "inputs_used": keys});
    edit(&control, &id, "board", json!([{"op": "update_node", "id": "n_k", "body": made}])).await;
    assert!(!canvas(&control, &id, "board").await.out_of_date.contains(&"n_k".to_owned()));
    touch(&root.join("canvases/board/script.md"), 5);
    let c = canvas(&control, &id, "board").await;
    assert_eq!(c.out_of_date_why["n_k"], ["script.md changed"]);

    stop(control, task).await;
    cleanup(&home);
}

// ---- assets, imports, paths, files.* -----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn assets_imports_path_safety_and_files_methods() {
    use data_encoding::BASE64;
    let (home, root) = temp("assets");
    let outside = home.parent().unwrap().join("outside");
    write(&outside, "secret.png", png(4, 4));
    write(&outside, "canvases/evil/canvas.json", r#"{"title": "evil"}"#);
    write(&outside, "people/x/x.md", "---\nname: Outsider\n---\n");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;
    create(&control, &id, "Board", None).await;
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    std::os::unix::fs::symlink(outside.join("canvases/evil"), root.join("canvases/evil")).unwrap();
    std::os::unix::fs::symlink(outside.join("people"), root.join("world/people")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("world/characters/sneaky")).unwrap();

    // studio.asset: chunks of at most 4 MiB; mime by name, else by bytes.
    let big: Vec<u8> = (0..StudioAsset::MAX_CHUNK + 1000).map(|i| (i % 251) as u8).collect();
    write(&root, "canvases/board/assets/clip.mp4", &big);
    let asset = |path: &str, offset: Option<u64>, length: Option<u64>| {
        Method::StudioAsset(StudioAssetParams { block_id: id.clone(), path: path.into(), offset, length })
    };
    let first: StudioAsset = call(&control, asset("canvases/board/assets/clip.mp4", None, None)).await;
    assert_eq!((first.mime.as_str(), first.size, first.offset), ("video/mp4", big.len() as u64, 0));
    let data = BASE64.decode(first.data_base64.as_bytes()).unwrap();
    assert_eq!(data.len() as u64, StudioAsset::MAX_CHUNK);
    let rest: StudioAsset = call(&control, asset("canvases/board/assets/clip.mp4", Some(StudioAsset::MAX_CHUNK), Some(u64::MAX))).await;
    let mut all = data;
    all.extend(BASE64.decode(rest.data_base64.as_bytes()).unwrap());
    assert_eq!(all, big, "two chunks make the file");
    let part: StudioAsset = call(&control, asset("canvases/board/assets/clip.mp4", Some(10), Some(5))).await;
    assert_eq!(BASE64.decode(part.data_base64.as_bytes()).unwrap(), &big[10..15]);
    let end: StudioAsset = call(&control, asset("canvases/board/assets/clip.mp4", Some(big.len() as u64), None)).await;
    assert!(end.data_base64.is_empty());
    assert_eq!(code(&control, asset("canvases/board/assets/clip.mp4", Some(big.len() as u64 + 1), None)).await, "bad_request");
    assert_eq!(code(&control, asset("canvases/board/assets", None, None)).await, "bad_request", "a folder");
    assert_eq!(code(&control, asset("canvases/board/assets/none.png", None, None)).await, "not_found");
    let text: StudioAsset = call(&control, asset("world/overview.md", None, None)).await;
    assert_eq!(
        (text.mime.as_str(), BASE64.decode(text.data_base64.as_bytes()).unwrap()),
        ("text/markdown", read(&root, "world/overview.md").into_bytes())
    );
    write(&root, "canvases/board/assets/noext", png(2, 2));
    let sniffed: StudioAsset = call(&control, asset("canvases/board/assets/noext", None, None)).await;
    assert_eq!(sniffed.mime, "image/png");

    // studio.import: from the root, a taken name, a file already there.
    write(&root, "inbox/ref.png", png(3, 3));
    let import =
        |canvas: &str, from: &str| Method::StudioImport(StudioImportParams { block_id: id.clone(), canvas: canvas.into(), from: from.into() });
    let one: StudioImported = call(&control, import("board", "inbox/ref.png")).await;
    assert_eq!(one.path, "canvases/board/assets/ref.png");
    let two: StudioImported = call(&control, import("board", "inbox/ref.png")).await;
    assert_eq!(two.path, "canvases/board/assets/ref-2.png");
    assert_eq!(std::fs::read(root.join(&two.path)).unwrap(), png(3, 3));
    let same: StudioImported = call(&control, import("board", "canvases/board/assets/ref.png")).await;
    assert_eq!(same.path, "canvases/board/assets/ref.png", "already in assets: as is");
    assert!(!root.join("canvases/board/assets/ref-3.png").exists());
    // A text document: the canvas's own is pinned where it is; another is copied in like any file.
    write(&root, "canvases/board/script.md", "# Beats");
    write(&root, "inbox/notes.txt", "x");
    let script: StudioImported = call(&control, import("board", "canvases/board/script.md")).await;
    assert_eq!(script.path, "canvases/board/script.md");
    let notes: StudioImported = call(&control, import("board", "inbox/notes.txt")).await;
    assert_eq!(notes.path, "canvases/board/assets/notes.txt");
    // The owner may import from anywhere it can read.
    let abs: StudioImported = call(&control, import("board", outside.join("secret.png").to_str().unwrap())).await;
    assert_eq!(abs.path, "canvases/board/assets/secret.png");
    let names: Vec<String> =
        std::fs::read_dir(root.join("canvases/board/assets")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    assert_eq!(code(&control, import("nope", "inbox/ref.png")).await, "not_found");
    assert_eq!(code(&control, import("board", "inbox")).await, "bad_request", "a folder");
    assert_eq!(code(&control, import("board", "/nonexistent/x.png")).await, "not_found");

    // Paths that leave the root, on every method that takes one.
    for path in ["../outside/secret.png", "inbox/../../outside/secret.png", "escape/secret.png"] {
        assert_eq!(code(&control, asset(path, None, None)).await, RpcError::FORBIDDEN, "asset {path}");
        assert_eq!(code(&control, import("board", path)).await, RpcError::FORBIDDEN, "import {path}");
        let files = Method::FilesRead(FilesReadParams { block_id: id.clone(), path: path.into(), max_bytes: None });
        assert_eq!(code(&control, files).await, RpcError::FORBIDDEN, "files.read {path}");
    }
    for path in ["/etc/passwd", "~/x"] {
        assert_eq!(code(&control, asset(path, None, None)).await, "bad_request", "{path}");
    }
    let edited = edit(
        &control,
        &id,
        "board",
        json!([
            {"op": "add_node", "node": {"body": {"kind": "asset", "path": "../outside/secret.png", "media": "image"}}},
            {"op": "add_node", "node": {"body": {"kind": "asset", "path": "/etc/passwd", "media": "image"}}},
            {"op": "add_node", "node": {"body": {"kind": "entity", "id": "../../x"}}},
        ]),
    )
    .await;
    assert_eq!(failed(&edited), [(0, RpcError::FORBIDDEN), (1, "bad_request"), (2, RpcError::FORBIDDEN)]);
    // A canvas folder linked out of the root is no canvas.
    assert_eq!(code(&control, Method::StudioCanvas(StudioCanvasParams { block_id: id.clone(), name: "evil".into() })).await, RpcError::FORBIDDEN);
    assert_eq!(code(&control, edit_method(&id, "evil", ops(json!([{"op": "remove_node", "id": "x"}])), None)).await, RpcError::FORBIDDEN);
    assert_eq!(code(&control, import("evil", "inbox/ref.png")).await, RpcError::FORBIDDEN);
    assert!(!outside.join("canvases/evil/assets").exists(), "nothing made outside");
    assert!(project(&control, &id).await.canvases.iter().all(|c| c.name != "evil"));
    for name in ["../x", "a/b", ".x"] {
        assert_eq!(
            code(&control, Method::StudioCanvas(StudioCanvasParams { block_id: id.clone(), name: name.into() })).await,
            "bad_request",
            "{name}"
        );
    }
    // World folders linked out of the root are not listed, nor read.
    let world: StudioWorld = call(&control, Method::StudioWorld(StudioProjectParams { block_id: id.clone() })).await;
    assert!(world.sections.iter().all(|s| s.id != "people"), "{:?}", world.sections.iter().map(|s| &s.id).collect::<Vec<_>>());
    assert!(world.sections.iter().flat_map(|s| &s.entities).all(|e| e.id != "characters/sneaky"));
    assert_eq!(code(&control, Method::StudioEntity(StudioEntityParams { block_id: id.clone(), id: "people/x".into() })).await, RpcError::FORBIDDEN);
    let update = StudioEntityUpdateParams { block_id: id.clone(), id: "people/x".into(), fields: None, body: Some("x".into()) };
    assert_eq!(code(&control, Method::StudioEntityUpdate(update)).await, RpcError::FORBIDDEN);
    let create = StudioEntityCreateParams { block_id: id.clone(), section: "people".into(), name: "Intruder".into(), fields: None, body: None };
    assert_eq!(code(&control, Method::StudioEntityCreate(create)).await, RpcError::FORBIDDEN);
    assert_eq!(read(&outside, "people/x/x.md"), "---\nname: Outsider\n---\n", "untouched");
    assert!(!outside.join("people/intruder").exists());
    // studio.set: names are checked too.
    for bad in [
        StudioSetParams { block_id: id.clone(), open: Some(vec!["../x".into()]), ..Default::default() },
        StudioSetParams { block_id: id.clone(), active: Some(Some("a/b".into())), ..Default::default() },
        StudioSetParams { block_id: id.clone(), world_section: Some(Some("..".into())), ..Default::default() },
        StudioSetParams { block_id: id.clone(), world_entity: Some(Some("mara".into())), ..Default::default() },
    ] {
        assert_eq!(code(&control, Method::StudioSet(bad)).await, "bad_request");
    }

    // files.* on a studio block: read, write with a base (conflicts), create, mkdir.
    let read_: FileRead =
        call(&control, Method::FilesRead(FilesReadParams { block_id: id.clone(), path: "world/style.md".into(), max_bytes: None })).await;
    assert!(read_.text.as_deref().unwrap().contains("## Style line"));
    let base = FileBase { mtime: read_.mtime, size: read_.size, sha256: read_.sha256.clone() };
    let write_ = |text: &str, base: Option<FileBase>| {
        Method::FilesWrite(FilesWriteParams { block_id: id.clone(), path: "world/style.md".into(), text: text.into(), base, force: false })
    };
    let saved: FileWritten = call(&control, write_("# Style\n\nNeon noir.\n", Some(base.clone()))).await;
    assert_eq!(saved.path, "world/style.md");
    assert_eq!(read(&root, "world/style.md"), "# Style\n\nNeon noir.\n");
    assert_eq!(code(&control, write_("stale", Some(base))).await, RpcError::CONFLICT);
    // (files.create makes no folders: the pane makes the entity's first.)
    call::<Value>(&control, Method::FilesMkdir(FilesMkdirParams { block_id: id.clone(), path: "world/props/bell".into(), unique: false })).await;
    let made: FileWritten = call(
        &control,
        Method::FilesCreate(FilesCreateParams { block_id: id.clone(), path: "world/props/bell/bell.md".into(), text: "# Bell\n".into() }),
    )
    .await;
    assert_eq!(made.path, "world/props/bell/bell.md");
    call::<Value>(&control, Method::FilesMkdir(FilesMkdirParams { block_id: id.clone(), path: "world/vehicles".into(), unique: false })).await;
    let world: StudioWorld = call(&control, Method::StudioWorld(StudioProjectParams { block_id: id.clone() })).await;
    assert!(world.sections.iter().any(|s| s.id == "vehicles"), "+ Section is files.mkdir");
    assert!(world.sections.iter().any(|s| s.id == "props" && s.entities.iter().any(|e| e.id == "props/bell")));
    let escape = Method::FilesCreate(FilesCreateParams { block_id: id.clone(), path: "escape/new.md".into(), text: "x".into() });
    assert_eq!(code(&control, escape).await, RpcError::FORBIDDEN);
    assert!(!outside.join("new.md").exists());

    stop(control, task).await;
    cleanup(&home);
}

// ---- events, state, restore ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn studio_changed_follows_server_writes_and_outside_edits() {
    let (home, root) = temp("events");
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();
    let created = studio(&control, &root, None).await;
    let id = created.block_id.clone();
    assert_eq!(state(&control, &created).await.view, StudioView::Setup);

    // studio.toml written by hand moves the block out of Setup (onto Production).
    let deadline = tokio::time::Instant::now() + 3 * TIMEOUT;
    let mut n = 0;
    let st = loop {
        // Rewritten until the watcher (which can take seconds to start) sees it.
        write(&root, "studio.toml", format!("name = \"By hand {n}\"\n"));
        n += 1;
        if let Some(st) = block_changed(&mut events, &id, Duration::from_millis(700), |s| s.view != StudioView::Setup).await {
            break st;
        }
        assert!(tokio::time::Instant::now() < deadline, "studio.toml appearing was never seen");
    };
    assert_eq!(st.view, StudioView::Production);
    assert_eq!(project(&control, &id).await.name, format!("By hand {}", n - 1));
    watching(&mut events, &root, &id).await;

    // The server's own writes: at once, on the calling block.
    let info = create(&control, &id, "Board", None).await;
    let seen = studio_events(&mut events, &id, Duration::from_millis(400)).await;
    assert!(seen.contains(&(StudioChange::Project, None, None)), "{seen:?}");
    assert!(seen.contains(&(StudioChange::Canvas, Some("board".into()), Some(info.rev))), "{seen:?}");
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}
    let edited = edit(&control, &id, "board", json!([{"op": "add_node", "node": {"body": {"kind": "note"}}}])).await;
    let seen = studio_events(&mut events, &id, Duration::from_millis(400)).await;
    assert!(seen.contains(&(StudioChange::Canvas, Some("board".into()), Some(edited.rev))), "{seen:?}");
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}
    // A batch where nothing applied sends nothing from the server.
    edit(&control, &id, "board", json!([{"op": "remove_node", "id": "n_gone"}])).await;
    assert!(studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty());
    let params = StudioEntityCreateParams { block_id: id.clone(), section: "characters".into(), name: "Mara".into(), fields: None, body: None };
    call::<EntityInfo>(&control, Method::StudioEntityCreate(params)).await;
    assert!(studio_changed(&mut events, &id, TIMEOUT, |w, _, _| w == StudioChange::World).await);
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}
    // Approving: the world, at once.
    write(&root, "world/characters/mara/sheet.png", png(1, 1));
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}
    let approve = StudioEntityApproveParams { block_id: id.clone(), id: "characters/mara".into(), approved: true };
    assert_eq!(call::<EntityInfo>(&control, Method::StudioEntityApprove(approve)).await.approval, Approval::Approved);
    let seen = studio_events(&mut events, &id, Duration::from_millis(400)).await;
    assert!(seen.contains(&(StudioChange::World, None, None)), "{seen:?}");
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}
    // Model picks: the project, at once.
    let models = StudioSetModelsParams { block_id: id.clone(), models: [("video".to_owned(), Some("kling-3.0-pro".to_owned()))].into() };
    call::<Project>(&control, Method::StudioSetModels(models)).await;
    let seen = studio_events(&mut events, &id, Duration::from_millis(400)).await;
    assert!(seen.contains(&(StudioChange::Project, None, None)), "{seen:?}");
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}

    // Outside edits, debounced: a burst of world files is one or two events.
    for i in 0..20 {
        write(&root, &format!("world/characters/mara/note{i}.md"), "x");
    }
    let seen = studio_events(&mut events, &id, Duration::from_millis(1500)).await;
    let world = seen.iter().filter(|(w, ..)| *w == StudioChange::World).count();
    assert!((1..=3).contains(&world), "debounced: {seen:?}");
    // canvas.json edited by hand: the canvas with its new rev.
    let rel = "canvases/board/canvas.json";
    let mut file: Value = serde_json::from_str(&read(&root, rel)).unwrap();
    file["title"] = json!("Retitled");
    write(&root, rel, serde_json::to_vec_pretty(&file).unwrap());
    let rev = edited.rev + 1;
    assert!(
        studio_changed(&mut events, &id, TIMEOUT, |w, n, r| w == StudioChange::Canvas && n == Some("board") && r == Some(rev)).await,
        "rev {rev}"
    );
    assert_eq!(canvas(&control, &id, "board").await.rev, rev);
    // An asset dropped in: that canvas.
    write(&root, "canvases/board/assets/new.png", png(1, 1));
    assert!(studio_changed(&mut events, &id, TIMEOUT, |w, n, _| w == StudioChange::Canvas && n == Some("board")).await);
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}
    // A document of it rewritten (the script): that canvas (its document cards follow, wired cards go out of date).
    write(&root, "canvases/board/script.md", "# Beats");
    assert!(studio_changed(&mut events, &id, TIMEOUT, |w, n, _| w == StudioChange::Canvas && n == Some("board")).await);
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}
    // A cut written into out/: that canvas (its Production row's cut).
    write(&root, "canvases/board/out/board-v1.mp4", b"cut");
    assert!(studio_changed(&mut events, &id, TIMEOUT, |w, n, _| w == StudioChange::Canvas && n == Some("board")).await);
    // A new canvas folder: the project.
    write(&root, "canvases/other/canvas.json", "{}");
    assert!(studio_changed(&mut events, &id, TIMEOUT, |w, _, _| w == StudioChange::Project).await);
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}
    // Elsewhere in the root: nothing.
    write(&root, "notes/todo.md", "x");
    assert!(studio_events(&mut events, &id, Duration::from_secs(1)).await.is_empty());

    // studio.toml going: back to Setup.
    std::fs::remove_file(root.join("studio.toml")).unwrap();
    let st = block_changed(&mut events, &id, TIMEOUT, |s| s.view == StudioView::Setup).await;
    assert!(st.is_some(), "back to Setup");
    while !studio_events(&mut events, &id, Duration::from_millis(600)).await.is_empty() {}

    // A closed block's watch stops.
    control.call_value(Method::BlockClose { block_id: id.clone(), force: false }).await.unwrap();
    write(&root, "world/after.md", "x");
    assert!(studio_events(&mut events, &id, Duration::from_secs(1)).await.is_empty());

    stop(control, task).await;
    cleanup(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn state_is_echoed_and_survives_a_restart() {
    let (home, root) = temp("state");
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();
    let created = studio(&control, &root, None).await;
    let id = created.block_id.clone();
    init(&control, &id).await;

    let params = StudioSetParams {
        block_id: id.clone(),
        view: Some(StudioView::Canvas),
        open: Some(vec!["ep01-pilot".into(), "moodboard".into(), "ep01-pilot".into()]),
        active: Some(Some("b-roll".into())),
        world_section: Some(Some("characters".into())),
        world_entity: Some(Some("world/characters/mara".into())),
        agent: Some(Some(AgentLink { otto_session: "ses_1".into(), block_id: None })),
    };
    let st = set(&control, params).await;
    assert_eq!(st.open, ["ep01-pilot", "moodboard", "b-roll"], "deduplicated; the active one opened");
    assert_eq!(
        (st.active.as_deref(), st.world_section.as_deref(), st.world_entity.as_deref()),
        (Some("b-roll"), Some("characters"), Some("characters/mara"))
    );
    let echoed = block_changed(&mut events, &id, TIMEOUT, |s| s.active.as_deref() == Some("b-roll")).await;
    assert_eq!(echoed.as_ref(), Some(&st), "block.changed carries it");
    assert_eq!(state(&control, &created).await, st);
    // Nothing given: nothing changes, no event.
    assert_eq!(set(&control, StudioSetParams { block_id: id.clone(), ..Default::default() }).await, st);
    assert!(block_changed(&mut events, &id, Duration::from_millis(500), |_| true).await.is_none());
    // null clears; an active tab closed falls back to the last open.
    let cleared: StudioState = call(&control, raw("studio.set", json!({"block_id": id, "world_entity": null, "agent": null}))).await;
    assert_eq!((cleared.world_entity, cleared.agent, cleared.world_section.as_deref()), (None, None, Some("characters")));
    let closed =
        set(&control, StudioSetParams { block_id: id.clone(), open: Some(vec!["moodboard".into(), "ep01-pilot".into()]), ..Default::default() })
            .await;
    assert_eq!(closed.active.as_deref(), Some("ep01-pilot"));
    let none: StudioState = call(&control, raw("studio.set", json!({"block_id": id, "active": null}))).await;
    assert_eq!(none.active, None);
    // Refusals.
    assert_eq!(code(&control, raw("studio.set", json!({"block_id": id, "view": "timeline"}))).await, "bad_request");
    let too_many = (0..=StudioState::OPEN_MAX).map(|i| format!("c{i}")).collect();
    assert_eq!(
        code(&control, Method::StudioSet(StudioSetParams { block_id: id.clone(), open: Some(too_many), ..Default::default() })).await,
        "bad_request"
    );
    let bad_agent =
        StudioSetParams { block_id: id.clone(), agent: Some(Some(AgentLink { otto_session: String::new(), block_id: None })), ..Default::default() };
    assert_eq!(code(&control, Method::StudioSet(bad_agent)).await, "bad_request");
    let st = set(
        &control,
        StudioSetParams {
            block_id: id.clone(),
            agent: Some(Some(AgentLink { otto_session: "ses_9".into(), block_id: None })),
            active: Some(Some("moodboard".into())),
            ..Default::default()
        },
    )
    .await;
    drop(events);
    stop(control, task).await;

    // Restored with its state, and watching again.
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();
    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.studio.as_deref()), (BlockKind::Studio, Some(&st)));
    assert_eq!(project(&control, &id).await.name, "Harbor Lights");
    watching(&mut events, &root, &id).await;

    stop(control, task).await;
    cleanup(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_studio_split_off_an_agent_is_linked_and_tabs_open_in_the_focused_folder() {
    let (home, root) = temp("agent");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let spawn = SpawnSpec { cwd: Some(root.display().to_string()), ..SpawnSpec::default() };
    let agent: Created = call(&control, Method::SessionCreate { name: None, spawn, kind: BlockKind::Agent, url: None }).await;
    control
        .call_value(Method::AgentSet { block_id: agent.block_id.clone(), session_id: Some("ses_42".into()), title: None, cwd: None })
        .await
        .unwrap();
    let split = Method::BlockSplit {
        block_id: agent.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Studio,
        url: None,
        agent: None,
        ids: None,
    };
    let created: Created = call(&control, split).await;
    let st = state(&control, &created).await;
    assert_eq!(st.root, root.display().to_string(), "in the agent's folder");
    assert_eq!(st.agent, Some(AgentLink { otto_session: "ses_42".into(), block_id: Some(agent.block_id.clone()) }));
    // A split off a terminal is not linked.
    let other = studio(&control, &root, None).await;
    assert_eq!(state(&control, &other).await.agent, None);
    // A tab opens in the focused block's folder.
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let term = studio(&control, &sub, Some(&sub)).await;
    let tab = Method::WindowCreate {
        session_id: term.session_id.clone(),
        spawn: SpawnSpec::default(),
        kind: BlockKind::Studio,
        url: None,
        background: false,
        ids: None,
    };
    let tab: Created = call(&control, tab).await;
    assert_eq!(state(&control, &tab).await.root, sub.display().to_string());
    // A folder that isn't there: refused.
    let missing = Method::WindowCreate {
        session_id: term.session_id.clone(),
        spawn: SpawnSpec { cwd: Some(root.join("nope").display().to_string()), ..SpawnSpec::default() },
        kind: BlockKind::Studio,
        url: None,
        background: true,
        ids: None,
    };
    assert_eq!(code(&control, missing).await, "not_found");
    stop(control, task).await;
    cleanup(&home);
}

// ---- access and compat ------------------------------------------------------

async fn token(owner: &Control, block: &str, rights: Rights) -> String {
    let grant = Method::GrantCreate {
        scope: Scope::Block { block_id: block.into() },
        rights,
        kind: GrantKind::Token,
        ttl_secs: None,
        label: Some("test".into()),
    };
    let created: GrantCreated = call(owner, grant).await;
    created.secret
}

#[tokio::test(flavor = "multi_thread")]
async fn view_rights_read_and_interact_rights_edit_but_only_the_owner_imports_from_outside() {
    let (home, root) = temp("access");
    let outside = home.parent().unwrap().join("outside");
    write(&outside, "secret.png", png(4, 4));
    write(&root, "inbox/in.png", png(4, 4));
    let task = start(&home).await;
    let (owner, _) = connect(&home).await;
    let id = studio(&owner, &root, None).await.block_id;
    init(&owner, &id).await;
    create(&owner, &id, "Board", None).await;

    let socket = paths::socket(&home);
    let viewer = Control::connect_with_secret(&socket, "viewer", token(&owner, &id, Rights::View).await).await.unwrap().0;
    let typist = Control::connect_with_secret(&socket, "typist", token(&owner, &id, Rights::Interact).await).await.unwrap().0;

    // View: every read.
    project(&viewer, &id).await;
    canvas(&viewer, &id, "board").await;
    call::<StudioWorld>(&viewer, Method::StudioWorld(StudioProjectParams { block_id: id.clone() })).await;
    call::<StudioAsset>(
        &viewer,
        Method::StudioAsset(StudioAssetParams { block_id: id.clone(), path: "inbox/in.png".into(), offset: None, length: None }),
    )
    .await;
    // …and no change.
    let writes = [
        edit_method(&id, "board", ops(json!([{"op": "add_node", "node": {"body": {"kind": "note"}}}])), None),
        Method::StudioCanvasCreate(StudioCanvasCreateParams { block_id: id.clone(), title: "X".into(), episode: None, format: None, name: None }),
        Method::StudioCanvasDelete(StudioCanvasParams { block_id: id.clone(), name: "board".into() }),
        Method::StudioInit(StudioInitParams { block_id: id.clone(), name: "X".into(), format: None }),
        Method::StudioSetFormat(StudioSetFormatParams { block_id: id.clone(), canvas: None, format: Some("square".into()) }),
        Method::StudioEntityCreate(StudioEntityCreateParams {
            block_id: id.clone(),
            section: "props".into(),
            name: "X".into(),
            fields: None,
            body: None,
        }),
        Method::StudioImport(StudioImportParams { block_id: id.clone(), canvas: "board".into(), from: "inbox/in.png".into() }),
        Method::StudioSet(StudioSetParams { block_id: id.clone(), view: Some(StudioView::World), ..Default::default() }),
    ];
    for method in writes {
        let name = silo_proto::api::method_name(&method);
        assert_eq!(code(&viewer, method).await, RpcError::FORBIDDEN, "{name}");
    }
    assert_eq!(canvas(&owner, &id, "board").await.nodes.len(), 0);

    // Interact: edits, imports from the root, but nothing from outside it.
    let edited = edit(&typist, &id, "board", json!([{"op": "add_node", "node": {"body": {"kind": "note"}}}])).await;
    assert!(edited.failed.is_empty());
    let import =
        |from: &Path| Method::StudioImport(StudioImportParams { block_id: id.clone(), canvas: "board".into(), from: from.display().to_string() });
    let inside: StudioImported = call(&typist, import(&root.join("inbox/in.png"))).await;
    assert_eq!(inside.path, "canvases/board/assets/in.png", "an absolute path in the root is fine");
    assert_eq!(code(&typist, import(&outside.join("secret.png"))).await, RpcError::FORBIDDEN);
    assert!(!root.join("canvases/board/assets/secret.png").exists());
    let theirs: StudioImported = call(&owner, import(&outside.join("secret.png"))).await;
    assert_eq!(theirs.path, "canvases/board/assets/secret.png");

    drop((viewer, typist));
    stop(owner, task).await;
    cleanup(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_client_gets_no_studio_changed() {
    use silo_proto::{Frame, kind, read_frame, write_frame};
    use tokio::{io::AsyncWriteExt, net::UnixStream};
    let (home, root) = temp("compat");
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();

    // A client of the same protocol from before Studio: no features.
    let mut old = UnixStream::connect(paths::socket(&home)).await.unwrap();
    let hello = json!({"protocol": silo_proto::PROTOCOL, "vt": silo_proto::VT, "build": "older", "client_name": "old"});
    write_frame(&mut old, &Frame::json(kind::CONTROL_HELLO, &hello).unwrap()).await.unwrap();
    old.flush().await.unwrap();
    tokio::time::timeout(TIMEOUT, read_frame(&mut old)).await.unwrap().unwrap().expect("hello");
    let subscribe = json!({"id": 1, "method": "events.subscribe", "params": {}});
    write_frame(&mut old, &Frame::json(kind::CONTROL, &subscribe).unwrap()).await.unwrap();
    old.flush().await.unwrap();

    let created = studio(&control, &root, None).await;
    init(&control, &created.block_id).await;
    create(&control, &created.block_id, "Board", None).await;
    assert!(studio_changed(&mut events, &created.block_id, TIMEOUT, |_, _, _| true).await, "a current client gets them");

    let mut seen = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok(Some(frame))) = tokio::time::timeout_at(deadline, read_frame(&mut old)).await {
        if let Ok(ServerMessage::Event { .. }) = frame.parse::<ServerMessage>() {
            seen += 1;
        }
        let text = String::from_utf8_lossy(&frame.payload).into_owned();
        assert!(!text.contains("studio.changed"), "an older client got {text}");
    }
    assert!(seen > 0, "it gets the other events");

    drop(old);
    stop(control, task).await;
    cleanup(&home);
}

// ---- Production: approval, model picks, the derived state -----------------

const APPROVABLE: &str = "---
# the lead
name: Mara Venn
sheet: sheet.png
voice: elevenlabs/JBFqnCBsd6RMkjVDRZzb
tags: [lead]
rig:
  height: 170
---

# Mara Venn

Grew up on the tugs.
";

fn set_mtime(path: &Path, at_ms: u64) {
    let at = SystemTime::UNIX_EPOCH + Duration::from_millis(at_ms);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(at).unwrap();
}

fn parse_utc(text: &str) -> u64 {
    silo_server::studio::approval::parse(text).unwrap_or_else(|| panic!("not an RFC 3339 time: {text}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_round_trips_in_the_frontmatter_and_voice_is_detected() {
    let (home, root) = temp("approve");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;
    write(&root, "world/characters/mara/mara.md", APPROVABLE);
    write(&root, "world/characters/mara/sheet.png", png(4, 4));
    write(&root, "world/characters/teo/teo.md", "---\nname: Teo\nvoice: (pending, see voices.md)\n---\nThe mechanic.\n");
    write(&root, "world/characters/teo/sheet.png", png(4, 4));
    write(&root, "world/places/quay/quay.md", "# The quay\n");
    let sheet = root.join("world/characters/mara/sheet.png");
    let approve =
        |entity: &str, approved: bool| Method::StudioEntityApprove(StudioEntityApproveParams { block_id: id.clone(), id: entity.into(), approved });

    let mara = entity(&control, &id, "characters/mara").await.info;
    assert_eq!((mara.approval, mara.voice), (Approval::None, true), "a provider/voice");
    let teo = entity(&control, &id, "characters/teo").await.info;
    assert_eq!((teo.approval, teo.voice), (Approval::None, false), "prose is no voice");

    // Approve: one line added at the end of the frontmatter, the rest byte for byte.
    let before = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() as u64;
    let info: EntityInfo = call(&control, approve("characters/mara", true)).await;
    assert_eq!(info.approval, Approval::Approved);
    let text = read(&root, "world/characters/mara/mara.md");
    let stamp = text.lines().find_map(|l| l.strip_prefix("approved: ")).unwrap_or_else(|| panic!("{text}")).to_owned();
    assert!(stamp.ends_with('Z') && stamp.len() == 20, "RFC 3339 UTC to the second: {stamp}");
    let at = parse_utc(&stamp);
    assert!(at + 1000 >= before && at <= before + 60_000, "now: {stamp}");
    assert_eq!(text, APPROVABLE.replace("  height: 170\n---\n", &format!("  height: 170\napproved: {stamp}\n---\n")));
    let e = entity(&control, &id, "characters/mara").await;
    assert_eq!((e.info.approval, &e.fields["approved"], e.fields["rig"].clone()), (Approval::Approved, &json!(stamp), json!({"height": 170})));
    assert_eq!(e.body, "\n# Mara Venn\n\nGrew up on the tugs.\n", "the body untouched");

    // The sheet changes after it (beyond the slack): changed; within it: still approved.
    set_mtime(&sheet, at + 1_500);
    assert_eq!(entity(&control, &id, "characters/mara").await.info.approval, Approval::Approved, "within the slack");
    set_mtime(&sheet, at + 5_000);
    assert_eq!(entity(&control, &id, "characters/mara").await.info.approval, Approval::Changed);
    let world: StudioWorld = call(&control, Method::StudioWorld(StudioProjectParams { block_id: id.clone() })).await;
    let listed = world.sections.iter().flat_map(|s| &s.entities).find(|e| e.id == "characters/mara").unwrap();
    assert_eq!(listed.approval, Approval::Changed, "the world listing too");
    // Approved again with the sheet back in the past.
    set_mtime(&sheet, at - 60_000);
    assert_eq!(call::<EntityInfo>(&control, approve("characters/mara", true)).await.approval, Approval::Approved);
    assert_eq!(read(&root, "world/characters/mara/mara.md").matches("approved:").count(), 1, "replaced, not added");

    // Withdraw: the file as it was; again: nothing to do.
    let info: EntityInfo = call(&control, approve("characters/mara", false)).await;
    assert_eq!(info.approval, Approval::None);
    assert_eq!(read(&root, "world/characters/mara/mara.md"), APPROVABLE);
    let file = root.join("world/characters/mara/mara.md");
    let written = ms(&file);
    tokio::time::sleep(Duration::from_millis(20)).await;
    call::<EntityInfo>(&control, approve("characters/mara", false)).await;
    assert_eq!((read(&root, "world/characters/mara/mara.md"), ms(&file)), (APPROVABLE.to_owned(), written), "not rewritten");

    // A hand-written approval of another shape still reads as approved.
    write(&root, "world/characters/teo/teo.md", "---\nname: Teo\napproved: true\n---\n");
    assert_eq!(entity(&control, &id, "characters/teo").await.info.approval, Approval::Approved);
    // Nothing to approve without a sheet; no such entity.
    assert_eq!(code(&control, approve("places/quay", true)).await, "bad_request");
    assert_eq!(read(&root, "world/places/quay/quay.md"), "# The quay\n");
    assert_eq!(code(&control, approve("characters/nobody", true)).await, "not_found");

    stop(control, task).await;
    cleanup(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_models_merges_and_keeps_the_file() {
    let (home, root) = temp("models");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    let toml = "name = \"Harbor Lights\"   # the show\nformat = \"landscape\"\n\n[models]   # our picks\nimage = \"soul-2\"   # the user's\nvideo = \"old\"   # to replace\n\n[formats.landscape]\naspect = \"16:9\"\n";
    write(&root, "studio.toml", toml);
    assert_eq!(project(&control, &id).await.models, [("image".to_owned(), "soul-2".to_owned()), ("video".to_owned(), "old".to_owned())].into());
    let set =
        |models: Value| Method::StudioSetModels(StudioSetModelsParams { block_id: id.clone(), models: serde_json::from_value(models).unwrap() });

    let p: Project = call(&control, set(json!({"video": "kling-3.0-pro", "speech": "eleven-v3", "image": null}))).await;
    assert_eq!(p.models, [("speech".to_owned(), "eleven-v3".to_owned()), ("video".to_owned(), "kling-3.0-pro".to_owned())].into());
    let text = read(&root, "studio.toml");
    assert_eq!(
        text,
        "name = \"Harbor Lights\"   # the show\nformat = \"landscape\"\n\n[models]   # our picks\nvideo = \"kling-3.0-pro\"   # to replace\nspeech = \"eleven-v3\"\n\n[formats.landscape]\naspect = \"16:9\"\n"
    );
    // Removing what isn't there, or everything, keeps the rest.
    let p: Project = call(&control, set(json!({"sound": null}))).await;
    assert_eq!(p.models.len(), 2);
    assert_eq!(read(&root, "studio.toml"), text);
    let production: Production = call(&control, Method::StudioProduction(StudioProjectParams { block_id: id.clone() })).await;
    assert_eq!(production.models, p.models, "Production carries the picks");

    // A file without [models] gets the table; only nulls leave it as it was.
    let plain = "name = \"X\"\n# formats below\n[formats.square]\naspect = \"1:1\"\n";
    write(&root, "studio.toml", plain);
    call::<Project>(&control, set(json!({"image": null}))).await;
    assert_eq!(read(&root, "studio.toml"), plain);
    let p: Project = call(&control, set(json!({"image": "gpt-image-2.6"}))).await;
    assert_eq!(p.models, [("image".to_owned(), "gpt-image-2.6".to_owned())].into());
    assert_eq!(read(&root, "studio.toml"), format!("{plain}\n[models]\nimage = \"gpt-image-2.6\"\n"));
    for bad in [json!({"bad kind": "x"}), json!({"image": " "}), json!({"image": "a\nb"})] {
        assert_eq!(code(&control, set(bad.clone())).await, "bad_request", "{bad}");
    }
    std::fs::remove_file(root.join("studio.toml")).unwrap();
    assert_eq!(code(&control, set(json!({"image": "x"}))).await, "not_found");
    assert_eq!(code(&control, Method::StudioProduction(StudioProjectParams { block_id: id.clone() })).await, "not_found");

    stop(control, task).await;
    cleanup(&home);
}

/// A card node of `canvas.json` with `takes` of `media` (paths under the canvas's assets/).
fn card_node(id: &str, parent: Option<&str>, makes: &str, status: &str, takes: &[(&str, u64)], used: &[&str]) -> Value {
    let media = match makes {
        "video" => "video",
        "speech" | "sound" | "music" => "audio",
        _ => "image",
    };
    let takes: Vec<Value> = takes.iter().map(|(p, at)| json!({"path": p, "media": media, "at": at})).collect();
    json!({"id": id, "x": 0, "y": 0, "w": 10, "h": 10, "parent": parent,
           "body": {"kind": "gen", "makes": makes, "prompt": "p", "status": status, "takes": takes, "inputs_used": used}})
}

async fn production(control: &Control, block: &str) -> Production {
    call(control, Method::StudioProduction(StudioProjectParams { block_id: block.into() })).await
}

fn frame(id: &str, parent: Option<&str>, role: &str) -> Value {
    json!({"id": id, "x": 0, "y": 0, "w": 10, "h": 10, "parent": parent, "body": {"kind": "frame", "title": id, "role": role}})
}

fn plain(id: &str, parent: Option<&str>, body: Value) -> Value {
    json!({"id": id, "x": 0, "y": 0, "w": 10, "h": 10, "parent": parent, "body": body})
}

#[tokio::test(flavor = "multi_thread")]
async fn production_is_derived_from_a_fixture_studio() {
    let (home, root) = temp("production");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    write(&root, "studio.toml", "name = \"Harbor Lights\"\n\n[models]\nimage = \"soul-2\" # picked\n\n[canvases]\norder = [\"moodboard\"]\n");
    write(&root, "world/overview.md", "# Overview\n\nA salvage diver finds a bell that rings under water.\n");
    // Characters: Mara (sheet, voice, a video sheet in refs/), Teo (sheet, voice prose); a place; a prop.
    write(
        &root,
        "world/characters/mara/mara.md",
        "---\nname: Mara Venn\nvoice: elevenlabs/JBFqnCBsd6RMkjVDRZzb\napproved: 2100-01-01T00:00:00Z\n---\n",
    );
    write(&root, "world/characters/mara/sheet.png", png(4, 4));
    write(&root, "world/characters/mara/refs/mara-360.mp4", b"v");
    write(&root, "world/characters/teo/teo.md", "---\nname: Teo\nvoice: (pending, see voices.md)\n---\n");
    write(&root, "world/characters/teo/sheet.png", png(4, 4));
    write(&root, "world/characters/gull/gull.md", "---\nname: The Gull\nvoice: none\n---\n");
    write(&root, "world/places/quay/quay.md", "---\nname: Old Quay\n---\n");
    write(&root, "world/places/quay/sheet.png", png(4, 4));
    write(&root, "world/props/oar/oar.md", "---\nname: Oar\nrefs: [refs/oar-360.mp4]\n---\n");

    let canvas_json = |title: &str, episode: Option<u32>, nodes: Vec<Value>| {
        serde_json::to_vec_pretty(&json!({"version": 1, "title": title, "episode": episode, "rev": 1, "nodes": nodes, "edges": []})).unwrap()
    };
    let later = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() as u64 + 3_600_000;
    // Ep 1: three shots: an image and a video card (out of date), a failed image card, a video asset.
    let a = |f: &str| format!("canvases/ep01-pilot/assets/{f}");
    let ep1 = vec![
        frame("s1", None, "scene"),
        frame("sh1", Some("s1"), "shot"),
        frame("sh2", Some("s1"), "shot"),
        frame("sh3", Some("s1"), "shot"),
        card_node("k1", Some("sh1"), "image", "done", &[(&*a("k1.png"), 1_000)], &[]),
        card_node("v1", Some("sh1"), "video", "done", &[(&*a("v1.mp4"), 2_000)], &["canvases/ep01-pilot/assets/k0.png"]),
        card_node("k2", Some("sh2"), "image", "failed", &[], &[]),
        plain("clip", Some("sh3"), json!({"kind": "asset", "path": a("clip.mp4")})),
        card_node("line", None, "speech", "running", &[(&*a("line.mp3"), later)], &[]),
        plain("mara", None, json!({"kind": "entity", "id": "characters/mara"})),
        {
            let mut q = card_node("plate", None, "image", "queued", &[], &[]);
            q["body"]["input"] = json!({"references": ["world/places/quay/sheet.png"]});
            q
        },
    ];
    write(&root, "canvases/ep01-pilot/canvas.json", canvas_json("Pilot", Some(1), ep1));
    write(&root, "canvases/ep01-pilot/script.md", "# Pilot\n\nINT. QUAY - NIGHT\n");
    for f in ["ep01-pilot-v2.mp4", "ep01-pilot-v10.mp4", "ep01-pilot-final.mp4", "ep02-v3.mp4", "ep01-pilot-v.mp4"] {
        write(&root, &format!("canvases/ep01-pilot/out/{f}"), b"cut");
    }
    // Ep 2: a script, no shots; Ep 3: a blank script; a moodboard with an out-of-date card using Mara's sheet.
    write(&root, "canvases/ep02-undertow/canvas.json", canvas_json("Undertow", Some(2), vec![]));
    write(&root, "canvases/ep02-undertow/script.md", "# Undertow\n");
    write(&root, "canvases/ep03-bell/canvas.json", canvas_json("Bell", Some(3), vec![]));
    write(&root, "canvases/ep03-bell/script.md", "  \n");
    let mood = vec![
        plain("ref", None, json!({"kind": "asset", "path": "world/characters/mara/sheet.png"})),
        card_node("m1", None, "image", "done", &[("canvases/moodboard/assets/m1.png", 5)], &["note:00000000deadbeef"]),
        plain("song", None, json!({"kind": "asset", "path": "canvases/moodboard/assets/song.mp3"})),
    ];
    write(&root, "canvases/moodboard/canvas.json", canvas_json("Moodboard", None, mood));

    let p = production(&control, &id).await;
    assert_eq!(p.models, [("image".to_owned(), "soul-2".to_owned())].into());

    let assets: Vec<_> = p.assets.iter().map(|a| (a.id.as_str(), a.sheet.is_some(), a.approval, a.voice, a.video_sheet, a.used)).collect();
    assert_eq!(
        assets,
        [
            ("characters/mara", true, Approval::Approved, Some(true), Some(true), 2),
            ("characters/teo", true, Approval::None, Some(false), Some(false), 0),
            ("characters/gull", false, Approval::None, None, Some(false), 0),
            ("places/quay", true, Approval::None, None, None, 1),
            ("props/oar", false, Approval::None, None, Some(true), 0),
        ],
        "by section, then name; voice only for characters who speak; video sheets for characters and props"
    );
    assert_eq!((p.assets[0].name.as_str(), p.assets[0].section.as_str()), ("Mara Venn", "characters"));

    let canvases: Vec<_> = p.episodes.iter().map(|e| (e.canvas.as_str(), e.episode)).collect();
    assert_eq!(canvases, [("ep01-pilot", Some(1)), ("ep02-undertow", Some(2)), ("ep03-bell", Some(3)), ("moodboard", None)]);
    let ep1 = &p.episodes[0];
    assert_eq!(
        (ep1.title.as_str(), ep1.script, ep1.shots, ep1.keyframes, ep1.videos, ep1.sound, ep1.out_of_date, ep1.failed, ep1.running),
        ("Pilot", true, 3, 1, 2, 1, 1, 1, 2)
    );
    let cut = ep1.cut.as_ref().expect("a cut");
    assert_eq!((cut.path.as_str(), cut.version), ("canvases/ep01-pilot/out/ep01-pilot-v10.mp4", 10), "the highest version, not the newest name");
    assert_eq!(cut.older, ["canvases/ep01-pilot/out/ep01-pilot-v2.mp4"]);
    assert!(cut.stale, "the speech take is newer than the cut");
    assert_eq!((p.episodes[1].script, p.episodes[1].shots, p.episodes[1].cut.is_none()), (true, 0, true));
    assert!(!p.episodes[2].script, "a blank script is none");
    let mood = &p.episodes[3];
    assert_eq!((mood.out_of_date, mood.sound, mood.shots), (1, 1, 0));

    // Next: the place's sheet to approve (used on a canvas; Teo isn't), then by stage.
    let next: Vec<_> = p.next.iter().map(|s| (s.kind, s.entity.as_deref().or(s.canvas.as_deref()).unwrap_or_default())).collect();
    assert_eq!(next, [(StepKind::Approve, "places/quay"), (StepKind::Script, "ep03-bell"), (StepKind::Shots, "ep02-undertow")]);
    assert!(p.next[0].ask.is_none() && p.next[1].ask.as_deref().unwrap().contains("canvas ep03-bell"), "{:?}", p.next);

    // Approving it moves the steps on.
    let approve = StudioEntityApproveParams { block_id: id.clone(), id: "places/quay".into(), approved: true };
    call::<EntityInfo>(&control, Method::StudioEntityApprove(approve)).await;
    set_mtime(&root.join("world/places/quay/sheet.png"), 1_000);
    let p = production(&control, &id).await;
    assert_eq!(p.assets[3].approval, Approval::Approved);
    let next: Vec<_> = p.next.iter().map(|s| s.kind).collect();
    assert_eq!(next, [StepKind::Script, StepKind::Shots, StepKind::Keyframes]);

    // What it costs: a dozen canvases more.
    for n in 0..12 {
        let nodes: Vec<Value> =
            (0..40).map(|i| card_node(&format!("c{i}"), None, "image", "done", &[(&*format!("canvases/x{n}/assets/{i}.png"), 1)], &[])).collect();
        write(&root, &format!("canvases/x{n}/canvas.json"), canvas_json(&format!("X{n}"), None, nodes));
    }
    let started = std::time::Instant::now();
    let p = production(&control, &id).await;
    let took = started.elapsed();
    assert_eq!(p.episodes.len(), 16);
    eprintln!("studio.production over 16 canvases (12 of 40 cards): {took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");

    stop(control, task).await;
    cleanup(&home);
}

const KING: &str = "---
# the old king
name: Dhritarashtra
sheet: sheet.png
refs:
  - refs/face.png
  - file: refs/crown-off.png
    name: Crown off
    defines: his bare head and hair, without the crown
  - file: refs/reverse.png   # from behind the throne
    name: Reverse angle
links: [props/kuru-crown]
voice: none
---

# Dhritarashtra

Blind from birth.
";

#[tokio::test(flavor = "multi_thread")]
async fn named_references_and_links_are_edited_in_place() {
    let (home, root) = temp("refs");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = studio(&control, &root, None).await.block_id;
    init(&control, &id).await;
    write(&root, "world/characters/dhritarashtra/dhritarashtra.md", KING);
    for f in ["sheet.png", "refs/face.png", "refs/crown-off.png", "refs/reverse.png", "refs/walk.mp4"] {
        write(&root, &format!("world/characters/dhritarashtra/{f}"), png(2, 2));
    }
    write(&root, "world/props/kuru-crown/kuru-crown.md", "---\nname: Kuru crown\n---\n");
    write(&root, "world/props/bhima-mace/bhima-mace.md", "---\nname: Bhima's mace\nlinks:\n  - characters/bhima\n---\n");
    write(&root, "world/characters/bhima/bhima.md", "---\nname: Bhima\n---\n");
    write(&root, "elsewhere/new crown.png", png(2, 2));
    let file = "world/characters/dhritarashtra/dhritarashtra.md";
    let king = "characters/dhritarashtra";
    let change = |change: Value| {
        Method::StudioEntityRef(StudioEntityRefParams { block_id: id.clone(), id: king.into(), change: serde_json::from_value(change).unwrap() })
    };
    let ids = |info: &EntityInfo| info.looks.iter().map(|l| l.id.clone()).collect::<Vec<_>>();

    // As listed: plain and table entries, ids, names, jobs, media.
    let info = entity(&control, &id, king).await.info;
    assert_eq!(ids(&info), ["face", "crown-off", "reverse"], "each its file's stem");
    assert_eq!(info.looks[2].name, "Reverse angle");
    assert_eq!(info.refs, info.looks.iter().map(|l| l.path.clone()).collect::<Vec<_>>(), "refs keeps the paths");
    assert_eq!((info.looks[0].name.as_str(), info.looks[0].defines.as_deref()), ("face", None));
    assert_eq!(info.looks[1].defines.as_deref(), Some("his bare head and hair, without the crown"));
    assert_eq!(info.links, ["props/kuru-crown"]);
    let world: StudioWorld = call(&control, Method::StudioWorld(StudioProjectParams { block_id: id.clone() })).await;
    let listed = |w: &StudioWorld, e: &str| w.sections.iter().flat_map(|s| &s.entities).find(|x| x.id == e).cloned().unwrap();
    assert_eq!(listed(&world, king), info);
    assert_eq!(listed(&world, "props/kuru-crown").links, [king], "a link reads from both ends");
    assert_eq!(listed(&world, "characters/bhima").links, ["props/bhima-mace"]);

    // Describe the plain one: it becomes a table; every other line as written.
    let done: silo_proto::studio::EntityRefEdited =
        call(&control, change(json!({"op": "describe", "ref": "face", "defines": "his face, eyes closed"}))).await;
    assert_eq!(done.ref_id.as_deref(), Some("face"));
    assert_eq!(done.info.looks[0].defines.as_deref(), Some("his face, eyes closed"));
    let text = read(&root, file);
    assert_eq!(
        text,
        KING.replace("  - refs/face.png\n", "  - file: refs/face.png\n    defines: his face, eyes closed\n")
            .replace("   # from behind the throne", "")
    );
    // Rename: the id stays (it is the file's); cleared, back to the stem and a plain path again.
    let done: silo_proto::studio::EntityRefEdited = call(&control, change(json!({"op": "rename", "ref": "reverse", "name": "From behind"}))).await;
    assert_eq!((done.ref_id.as_deref(), done.info.looks[2].name.as_str()), (Some("reverse"), "From behind"));
    let done: silo_proto::studio::EntityRefEdited = call(&control, change(json!({"op": "rename", "ref": "reverse", "name": null}))).await;
    assert_eq!((done.ref_id.as_deref(), done.info.looks[2].name.as_str()), (Some("reverse"), "reverse"));
    assert!(read(&root, file).contains("  - refs/reverse.png\nlinks:"), "{}", read(&root, file));

    // A card mentioning a reference keeps its wire and its token through a rename.
    create(&control, &id, "Board", None).await;
    let made = edit(
        &control,
        &id,
        "board",
        json!([{"op": "add_node", "node": {"id": "n_key", "body": {"kind": "gen", "makes": "image", "prompt": "@characters/dhritarashtra#crown-off on the throne"}}}]),
    )
    .await;
    assert!(made.failed.is_empty(), "{:?}", made.failed);
    let crown_path = "world/characters/dhritarashtra/refs/crown-off.png";
    let crown_asset = |c: &Canvas| {
        c.nodes.iter().find(|n| matches!(&n.body, NodeBody::Asset { path, .. } if path == crown_path)).map(|n| n.id.clone()).expect("an Asset node")
    };
    let board = canvas(&control, &id, "board").await;
    let asset = crown_asset(&board);
    assert_eq!(wires_into(&board, "n_key", EdgeRole::Reference), [asset.as_str()]);
    let done: silo_proto::studio::EntityRefEdited = call(&control, change(json!({"op": "rename", "ref": "crown-off", "name": "Bare head"}))).await;
    assert_eq!((done.ref_id.as_deref(), done.info.looks[1].name.as_str()), (Some("crown-off"), "Bare head"), "renamed, same id");
    let made = edit(
        &control,
        &id,
        "board",
        json!([{"op": "update_node", "id": "n_key", "body": {"prompt": "@characters/dhritarashtra#crown-off on the throne, at night"}}]),
    )
    .await;
    assert!(made.failed.is_empty() && made.mentions.is_empty(), "nothing unwired: {:?}", made.mentions);
    let board = canvas(&control, &id, "board").await;
    assert_eq!(wires_into(&board, "n_key", EdgeRole::Reference), [asset.as_str()], "the wire stays");
    let world: StudioWorld = call(&control, Method::StudioWorld(StudioProjectParams { block_id: id.clone() })).await;
    let crown = Target::Ref { entity: king.into(), id: "crown-off".into() };
    assert_eq!(board.mentions_of("n_key", &world.sections), [(crown.clone(), "Dhritarashtra, Bare head".to_owned())], "it still resolves");
    assert_eq!(board.mention_sources(&crown, "n_key", &world.sections), [asset.as_str()]);
    let _: silo_proto::studio::EntityRefEdited = call(&control, change(json!({"op": "rename", "ref": "crown-off", "name": "Crown off"}))).await;
    // Move to the front; past the end: last.
    let done: silo_proto::studio::EntityRefEdited = call(&control, change(json!({"op": "move", "ref": "crown-off", "to": 0}))).await;
    assert_eq!(ids(&done.info), ["crown-off", "face", "reverse"]);
    let done: silo_proto::studio::EntityRefEdited = call(&control, change(json!({"op": "move", "ref": "crown-off", "to": 9}))).await;
    assert_eq!(ids(&done.info), ["face", "reverse", "crown-off"]);
    // Add: a file already in the folder, where asked; one from elsewhere, copied into refs/.
    let done: silo_proto::studio::EntityRefEdited =
        call(&control, change(json!({"op": "add", "path": "world/characters/dhritarashtra/refs/walk.mp4", "name": "Walk", "at": 1}))).await;
    assert_eq!(
        (done.ref_id.as_deref(), ids(&done.info)),
        (Some("walk"), vec!["face".to_owned(), "walk".into(), "reverse".into(), "crown-off".into()])
    );
    assert_eq!(done.info.looks[1].media, StudioMedia::Video);
    // From elsewhere, copied in named after the name's slug: its id reads well.
    let done: silo_proto::studio::EntityRefEdited =
        call(&control, change(json!({"op": "add", "path": "elsewhere/new crown.png", "name": "Hood up", "defines": "the hood alone"}))).await;
    assert_eq!(done.ref_id.as_deref(), Some("hood-up"));
    assert_eq!((done.info.looks[4].path.as_str(), done.info.looks[4].name.as_str()), ("world/characters/dhritarashtra/refs/hood-up.png", "Hood up"));
    assert!(root.join("elsewhere/new crown.png").is_file(), "copied, not moved");
    // A file with that stem there already: copied under its own name, which makes the id.
    let done: silo_proto::studio::EntityRefEdited =
        call(&control, change(json!({"op": "add", "path": "elsewhere/new crown.png", "name": "Crown off", "defines": "the crown alone"}))).await;
    assert_eq!(done.ref_id.as_deref(), Some("new-crown"));
    assert_eq!(
        (done.info.looks[5].path.as_str(), done.info.looks[5].name.as_str()),
        ("world/characters/dhritarashtra/refs/new crown.png", "Crown off")
    );
    // Adding one listed already names it where it is.
    let done: silo_proto::studio::EntityRefEdited =
        call(&control, change(json!({"op": "add", "path": "world/characters/dhritarashtra/refs/face.png", "name": "Face"}))).await;
    assert_eq!((done.info.looks.len(), done.info.looks[0].name.as_str()), (6, "Face"));
    // Remove: off the list, the file stays.
    let done: silo_proto::studio::EntityRefEdited = call(&control, change(json!({"op": "remove", "ref": "walk"}))).await;
    assert_eq!(
        (done.ref_id, ids(&done.info)),
        (None, vec!["face".to_owned(), "reverse".into(), "crown-off".into(), "hood-up".into(), "new-crown".into()])
    );
    assert!(root.join("world/characters/dhritarashtra/refs/walk.mp4").is_file());
    let text = read(&root, file);
    assert!(text.starts_with("---\n# the old king\nname: Dhritarashtra\nsheet: sheet.png\nrefs:\n  - file: refs/face.png\n"), "{text}");
    assert!(text.ends_with("links: [props/kuru-crown]\nvoice: none\n---\n\n# Dhritarashtra\n\nBlind from birth.\n"), "{text}");
    assert_eq!(entity(&control, &id, king).await.fields["voice"], json!("none"));
    for (bad, want) in [
        (json!({"op": "rename", "ref": "nope", "name": "x"}), "not_found"),
        (json!({"op": "add", "path": "elsewhere/missing.png"}), "not_found"),
        (json!({"op": "add", "path": "../outside.png"}), RpcError::FORBIDDEN),
    ] {
        assert_eq!(code(&control, change(bad.clone())).await, want, "{bad}");
    }

    // No `refs` key: the folder's images are written out first.
    write(&root, "world/places/quay/quay.md", "---\nname: The quay\n---\n");
    write(&root, "world/places/quay/refs/dawn.png", png(2, 2));
    write(&root, "world/places/quay/refs/dusk.png", png(2, 2));
    let quay = Method::StudioEntityRef(StudioEntityRefParams {
        block_id: id.clone(),
        id: "places/quay".into(),
        change: RefChange::Describe { ref_id: "dusk".into(), defines: Some("the light at dusk".into()) },
    });
    call::<silo_proto::studio::EntityRefEdited>(&control, quay).await;
    assert_eq!(
        read(&root, "world/places/quay/quay.md"),
        "---\nname: The quay\nrefs:\n  - refs/dawn.png\n  - file: refs/dusk.png\n    defines: the light at dusk\n---\n"
    );

    // Links: added on the entity given; removed from either end.
    let link = |a: &str, b: &str, linked: bool| {
        Method::StudioEntityLink(StudioEntityLinkParams { block_id: id.clone(), id: a.into(), other: b.into(), linked })
    };
    let info: EntityInfo = call(&control, link("characters/bhima", "props/kuru-crown", true)).await;
    assert_eq!(info.links, ["props/kuru-crown", "props/bhima-mace"], "its own first, then those naming it");
    assert_eq!(read(&root, "world/characters/bhima/bhima.md"), "---\nname: Bhima\nlinks: [props/kuru-crown]\n---\n");
    let file_before = read(&root, "world/props/bhima-mace/bhima-mace.md");
    let info: EntityInfo = call(&control, link("characters/bhima", "props/bhima-mace", true)).await;
    assert_eq!(info.links, ["props/kuru-crown", "props/bhima-mace"], "linked from the other end already");
    assert_eq!(read(&root, "world/characters/bhima/bhima.md"), "---\nname: Bhima\nlinks: [props/kuru-crown]\n---\n", "nothing written");
    let info: EntityInfo = call(&control, link("characters/bhima", "props/bhima-mace", false)).await;
    assert_eq!(info.links, ["props/kuru-crown"]);
    assert_eq!(
        read(&root, "world/props/bhima-mace/bhima-mace.md"),
        file_before.replace("links:\n  - characters/bhima\n", ""),
        "the declaration at the other end goes"
    );
    let info: EntityInfo = call(&control, link("props/kuru-crown", king, false)).await;
    assert!(info.links.contains(&"characters/bhima".to_owned()) && !info.links.contains(&king.to_owned()));
    assert!(!read(&root, file).contains("links:"));
    assert_eq!(code(&control, link(king, king, true)).await, "bad_request");
    assert_eq!(code(&control, link(king, "props/nothing", true)).await, "not_found");

    stop(control, task).await;
    cleanup(&home);
}
