//! A Studio card run end to end (docs/STUDIO.md §6.2, §8): a real Silo server in a temp home, otto's silo tools
//! and Studio bridge over it, a fake media provider. The card is made and wired with `canvas_edit`, run with
//! `canvas_run`; `canvas.json` gets the picked take, the file lands in `assets/`, and touching a wired source makes
//! the server call the card out of date: otto's `inputs_used` keys and the server's agree.
use otto_llm::auth::CredentialStore;
use otto_media::{Call, Generated, ImageRequest, Media, MediaError, MediaKind, MediaProvider, ModelEntry, Output};
use otto_tools::{Mailbox, Tool, ToolCx};
use serde_json::{Value, json};
use silo_client::transport::RelayConfig;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A real 1×1 PNG.
fn png() -> Vec<u8> {
    otto_tools::attachments::decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==").unwrap()
}

struct Pix(Arc<Mutex<Vec<ImageRequest>>>);

#[async_trait::async_trait]
impl MediaProvider for Pix {
    fn id(&self) -> &'static str {
        "pix"
    }
    fn name(&self) -> &'static str {
        "Pix"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Image]
    }
    fn models(&self) -> Vec<ModelEntry> {
        vec![ModelEntry {
            id: "pix-1".into(),
            kind: MediaKind::Image,
            name: "Pix 1".into(),
            about: String::new(),
            inputs: vec!["text".into(), "reference images".into()],
            outputs: String::new(),
            price: None,
            default: true,
        }]
    }
    fn env(&self) -> &'static [&'static str] {
        &[]
    }
    async fn generate_image(&self, _key: &str, request: ImageRequest, _call: &Call) -> Result<Output, MediaError> {
        let model = request.model.clone();
        self.0.lock().unwrap().push(request);
        Ok(Output { media: vec![Generated { bytes: png(), mime: "image/png".into() }], model, ..Output::default() })
    }
}

#[derive(Default)]
struct Mail(Mutex<Vec<String>>);
impl Mailbox for Mail {
    fn deliver(&self, text: String) {
        self.0.lock().unwrap().push(text);
    }
}

async fn call(tools: &[Arc<dyn Tool>], name: &str, input: Value, cx: &ToolCx) -> Value {
    let tool = tools.iter().find(|t| t.spec().name == name).unwrap_or_else(|| panic!("no tool {name}"));
    let out = tool.call(input, cx.clone()).await.unwrap();
    assert!(!out.is_error, "{name}: {}", out.joined_text());
    serde_json::from_str(&out.joined_text()).unwrap_or(Value::Null)
}

fn canvas_json(root: &Path, name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(root.join("canvases").join(name).join("canvas.json")).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_runs_through_silo_and_its_keys_match_the_servers() {
    // Short paths: the socket lives in the home.
    let home = tempfile::Builder::new().prefix("otto-studio-").tempdir_in("/tmp").unwrap();
    let work = tempfile::Builder::new().prefix("otto-show-").tempdir_in("/tmp").unwrap();
    let root = std::fs::canonicalize(work.path()).unwrap();
    std::fs::write(root.join("studio.toml"), "name = \"Show\"\nformat = \"landscape\"\n\n[formats.landscape]\naspect = \"16:9\"\n").unwrap();
    std::fs::create_dir_all(root.join("plates")).unwrap();
    std::fs::write(root.join("plates/harbor.png"), png()).unwrap();

    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    while !silo_client::server_running(home.path()).await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let silo = otto_silo_tools::connect(otto_silo_tools::Config { home: home.path().into(), client_name: "otto test".into(), token: None });

    let dir = tempfile::tempdir().unwrap();
    let got: Arc<Mutex<Vec<ImageRequest>>> = Arc::default();
    let providers: Vec<Arc<dyn MediaProvider>> = vec![Arc::new(Pix(got.clone()))];
    let media = Media::with(CredentialStore::open(dir.path().join("auth")).unwrap(), dir.path().join("state"), providers, Arc::new(|_: &str| None));
    media.set_key("pix", Some("key-pix".into())).unwrap();
    media.set_studio(silo.studio.clone());

    let (mail, jobs): (Arc<Mail>, Arc<otto_tools::jobs::Jobs>) = (Arc::default(), Arc::default());
    let mut cx = ToolCx::new("call1", root.clone());
    cx.attachments = Some(dir.path().join("attachments"));
    cx.sessions = vec!["s1".into()];
    cx.mailbox = Some(mail.clone());
    cx.jobs = Some(jobs);

    // The canvas, then a plate, a note and an image card wired from both (the tools get otto's cwd).
    let created = call(&silo.tools, "canvas_create", json!({"title": "Pilot", "episode": 1}), &cx).await;
    let canvas = created["canvas"].as_str().unwrap().to_owned();
    let ops = json!([
        {"op": "add_node", "node": {"id": "n_plate", "body": {"kind": "asset", "path": "plates/harbor.png", "media": "image"}}},
        {"op": "add_node", "node": {"id": "n_note", "body": {"kind": "note", "text": "Fog over the water."}}},
        {"op": "add_node", "node": {"id": "n_card", "body": {"kind": "gen", "makes": "image", "prompt": "The harbor at dawn"}}},
        {"op": "add_edge", "edge": {"from": "n_plate", "to": "n_card", "role": "reference"}},
        {"op": "add_edge", "edge": {"from": "n_note", "to": "n_card", "role": "text"}}
    ]);
    let edited = call(&silo.tools, "canvas_edit", json!({"canvas": canvas, "ops": ops}), &cx).await;
    assert!(edited["failed"].as_array().is_none_or(Vec::is_empty), "{edited}");

    // canvas_run: one background job; its message carries the result and where it was placed.
    let run = media.tools().into_iter().find(|t| t.spec().name == "canvas_run").unwrap();
    let out = run.call(json!({"canvas": canvas, "nodes": ["n_card"]}), cx.clone()).await.unwrap();
    assert!(out.joined_text().contains("Started media"), "{}", out.joined_text());
    let report = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(m) = mail.0.lock().unwrap().first().cloned() {
                return m;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the job's report");
    assert!(report.contains("[generated att_") && report.contains(&format!("Placed on canvas {canvas} as n_card")), "{report}");
    let sent = got.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].prompt, "The harbor at dawn\n\nFog over the water.");
    assert_eq!((sent[0].references.len(), sent[0].aspect_ratio.as_deref()), (1, Some("16:9")));

    // On disk: done, the take picked, its file in assets/, the inputs it was made from.
    let saved = canvas_json(&root, &canvas);
    let card = saved["nodes"].as_array().unwrap().iter().find(|n| n["id"] == "n_card").unwrap()["body"].clone();
    assert_eq!(card["status"], "done", "{card}");
    assert_eq!(card["pick"], 0);
    let take = card["takes"][0]["path"].as_str().unwrap();
    assert!(take.starts_with(&format!("canvases/{canvas}/assets/the-harbor-at-dawn")), "{take}");
    assert!(root.join(take).is_file());
    assert_eq!(card["inputs_used"].as_array().unwrap().len(), 2, "{card}");
    assert_eq!(card["tool"], "generate_image");
    assert!(card.get("job").is_none() && card.get("error").is_none(), "{card}");

    // The server agrees it is up to date; touching the plate makes it out of date (mtimes in ms).
    let block = silo.studio.block(&root, &[], false).await.unwrap().expect("the Studio block canvas_create opened");
    let now = silo.studio.canvas(&block, &canvas).await.unwrap();
    assert!(now["out_of_date"].as_array().is_none_or(Vec::is_empty), "fresh: {now}");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let file = std::fs::OpenOptions::new().write(true).open(root.join("plates/harbor.png")).unwrap();
    file.set_modified(std::time::SystemTime::now()).unwrap();
    drop(file);
    let now = silo.studio.canvas(&block, &canvas).await.unwrap();
    assert_eq!(now["out_of_date"], json!(["n_card"]), "{now}");

    // A document pinned without media and wired into a text port: the bridge gives its path, compiling reads it.
    std::fs::write(root.join(format!("canvases/{canvas}/script.md")), "Beat 1: the river.").unwrap();
    let ops = json!([
        {"op": "add_node", "node": {"id": "n_doc", "body": {"kind": "asset", "path": format!("canvases/{canvas}/script.md")}}},
        {"op": "add_node", "node": {"id": "n_two", "body": {"kind": "gen", "makes": "image", "prompt": "Keyframe"}}},
        {"op": "add_edge", "edge": {"from": "n_doc", "to": "n_two", "role": "text"}}
    ]);
    let edited = call(&silo.tools, "canvas_edit", json!({"canvas": canvas, "ops": ops}), &cx).await;
    assert!(edited["failed"].as_array().is_none_or(Vec::is_empty), "{edited}");
    let two = silo.studio.card(&block, &canvas, "n_two").await.unwrap();
    let input = &two.inputs[0];
    assert_eq!(
        (input.media, input.path.clone(), input.text.clone()),
        (otto_tools::studio::SourceMedia::Text, Some(format!("canvases/{canvas}/script.md")), None)
    );
    assert!(two.keys[0].starts_with(&format!("canvases/{canvas}/script.md@")), "{:?}", two.keys);
    let compiled = otto_media::studio::compile(&media, &two, &otto_media::studio::Setup::default(), &serde_json::Map::new()).await.unwrap();
    assert_eq!(compiled.prompt, "Keyframe\n\nBeat 1: the river.");

    // The app's Run on a Studio block in the user's own session (not the agents' default one): found without an
    // agent block to start from.
    let other = tempfile::Builder::new().prefix("otto-show2-").tempdir_in("/tmp").unwrap();
    let other_root = std::fs::canonicalize(other.path()).unwrap();
    std::fs::write(other_root.join("studio.toml"), "name = \"Other\"\n").unwrap();
    std::fs::create_dir_all(other_root.join("canvases")).unwrap();
    assert_eq!(silo.studio.block(&other_root, &[], false).await.unwrap(), None);
    let (control, _events) = silo_client::Control::connect(&silo_proto::paths::socket(home.path()), "test").await.unwrap();
    let spawn = silo_proto::control::SpawnSpec { cwd: Some(other_root.to_string_lossy().into_owned()), ..Default::default() };
    let create =
        silo_proto::control::Method::SessionCreate { name: Some("work".into()), spawn, kind: silo_proto::model::BlockKind::Studio, url: None };
    let created: Value = control.call(create).await.unwrap();
    let found = silo.studio.block(&other_root.join("canvases"), &[], false).await.unwrap();
    assert_eq!(found.as_deref(), created["block_id"].as_str(), "{created}");

    server.abort();
}
