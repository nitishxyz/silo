//! The session-less Studio methods (docs/STUDIO.md §6.2, §6.3) over a real socket: `studio.run_card`,
//! `stop_card`, `preview_card`, `studio.mentions`, and the media catalog methods the app's card inspector reads.
use super::*;
use otto_media::{Call, Generated, ImageRequest, MediaError, MediaProvider, ModelEntry, Output};
use otto_tools::ToolError;
use otto_tools::studio::{Card, Edited, Makes, Source, Status, Studio};
use serde_json::Value;
use std::path::Path;

struct Pix;
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
    async fn generate_image(&self, _: &str, r: ImageRequest, _: &Call) -> Result<Output, MediaError> {
        // A real 1×1 PNG.
        let png = otto_tools::attachments::decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(Output { media: vec![Generated { bytes: png, mime: "image/png".into() }], model: r.model, ..Output::default() })
    }
}

/// One canvas `ep` of bodies by id, no wires; every edit recorded.
struct Cards {
    root: std::path::PathBuf,
    bodies: StdMutex<std::collections::BTreeMap<String, Value>>,
}

fn merge(target: &mut Value, patch: &Value) {
    if let (Some(t), Some(p)) = (target.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                t.insert(k.clone(), v.clone());
            }
        }
    }
}

#[async_trait::async_trait]
impl Studio for Cards {
    async fn block(&self, cwd: &Path, _: &[String], _: bool) -> Result<Option<String>, ToolError> {
        Ok(cwd.starts_with(&self.root).then(|| "b_1".to_owned()))
    }
    async fn project(&self, _: &str) -> Result<Value, ToolError> {
        Ok(json!({}))
    }
    async fn canvas(&self, _: &str, _: &str) -> Result<Value, ToolError> {
        Ok(json!({"nodes": []}))
    }
    async fn world(&self, _: &str) -> Result<Value, ToolError> {
        Ok(json!({"sections": []}))
    }
    async fn edit(&self, _: &str, _: &str, ops: Vec<Value>) -> Result<Edited, ToolError> {
        let mut bodies = self.bodies.lock().unwrap();
        for op in ops {
            let body = bodies.get_mut(op["id"].as_str().unwrap()).unwrap();
            merge(body, &op["body"]);
        }
        Ok(Edited::default())
    }
    async fn card(&self, _: &str, canvas: &str, node: &str) -> Result<Card, ToolError> {
        let body = self.bodies.lock().unwrap().get(node).cloned().ok_or_else(|| ToolError::invalid(format!("no node {node}")))?;
        Ok(Card {
            root: self.root.clone(),
            canvas: canvas.into(),
            node: node.into(),
            label: node.into(),
            frame: None,
            makes: serde_json::from_value(body["makes"].clone()).unwrap_or(Makes::Unknown),
            prompt: body["prompt"].as_str().unwrap_or_default().into(),
            model: body["model"].as_str().map(str::to_owned),
            mode: None,
            options: Value::Null,
            status: serde_json::from_value(body["status"].clone()).unwrap_or(Status::Draft),
            takes: body["takes"].as_array().cloned().unwrap_or_default(),
            inputs: vec![],
            keys: vec![],
            format: None,
            mentions: vec![],
        })
    }
    async fn sources(&self, _: &str, _: &str, paths: &[String]) -> Result<Vec<Option<Source>>, ToolError> {
        Ok(vec![None; paths.len()])
    }
}

struct Studio1 {
    fixture: Fixture,
    cards: Arc<Cards>,
    root: std::path::PathBuf,
}

async fn studio_fixture() -> (Studio1, ServerHandle) {
    let fixture = Fixture::new(vec![]).await;
    let root = fixture.dir.path().join("show");
    std::fs::create_dir_all(root.join("world/characters/mara")).unwrap();
    std::fs::write(root.join("studio.toml"), "name = \"Show\"\n").unwrap();
    std::fs::write(root.join("world/characters/mara/mara.md"), "---\nname: Mara Venn\n---\n").unwrap();
    std::fs::create_dir_all(root.join("canvases/ep01-pilot")).unwrap();
    std::fs::write(root.join("canvases/ep01-pilot/canvas.json"), r#"{"version":1,"title":"Pilot","episode":1,"nodes":[],"edges":[]}"#).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let cards = Arc::new(Cards { root: root.clone(), bodies: StdMutex::default() });
    for (id, makes, prompt) in [("n_img", "image", "A lighthouse"), ("n_vid", "video", "Waves"), ("n_empty", "image", "")] {
        cards.bodies.lock().unwrap().insert(id.into(), json!({"kind": "gen", "makes": makes, "prompt": prompt}));
    }
    let mut services = fixture.services();
    services.media = otto_media::Media::with(
        otto_llm::auth::CredentialStore::open(fixture.dir.path().join("auth2")).unwrap(),
        fixture.dir.path().join("media"),
        vec![Arc::new(Pix) as Arc<dyn MediaProvider>, otto_media::providers::all().remove(2)],
        Arc::new(|_: &str| None),
    );
    services.media.set_key("pix", Some("key".into())).unwrap();
    services.media.set_studio(cards.clone());
    let server = Server::start(fixture.config()).await.unwrap();
    server.ready(services).await.unwrap();
    (Studio1 { fixture, cards, root }, server)
}

impl Studio1 {
    fn body(&self, id: &str) -> Value {
        self.cards.bodies.lock().unwrap()[id].clone()
    }
    async fn until(&self, id: &str, status: &str) -> Value {
        for _ in 0..500 {
            if self.body(id)["status"] == status {
                return self.body(id);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} never {status}: {}", self.body(id))
    }
}

#[tokio::test]
async fn run_card_returns_at_once_and_the_card_carries_the_state() {
    let (s, server) = studio_fixture().await;
    let c = s.fixture.connect().await;
    let cwd = s.root.to_string_lossy().into_owned();
    let started = std::time::Instant::now();
    c.studio_run_card(&cwd, "ep", "n_img").await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(100), "returns before the 100 ms generation");
    let body = s.until("n_img", "done").await;
    let take = body["takes"][0]["path"].as_str().unwrap();
    assert_eq!(take, "canvases/ep/assets/a-lighthouse.png");
    assert!(s.root.join(take).is_file());
    assert_eq!((body["pick"].as_u64(), body["tool"].as_str()), (Some(0), Some("generate_image")));
    // No attachment kept anywhere in otto's home.
    assert!(!s.fixture.dir.path().join("attachments").exists());

    // Preview: the compiled request, ports and fits.
    let preview = c.studio_preview_card(&cwd, "ep", "n_img", vec!["pix-1".into(), "grok-imagine-video-1.5".into()]).await.unwrap();
    let compiled = preview.compiled.unwrap();
    assert_eq!((compiled.tool.as_str(), compiled.model.as_str(), compiled.provider.as_str()), ("generate_image", "pix-1", "pix"));
    assert_eq!(compiled.input["prompt"], "A lighthouse");
    assert_eq!(preview.ports, ["reference", "text"]);
    assert!(preview.fits[0].fits && !preview.fits[1].fits, "{:?}", preview.fits);
    let preview = c.studio_preview_card(&cwd, "ep", "n_empty", vec![]).await.unwrap();
    assert_eq!(preview.error.unwrap().code, "invalid");

    // no_provider: an RPC error the app turns into its setup card.
    let error = c.studio_run_card(&cwd, "ep", "n_vid").await.unwrap_err();
    let otto_client::ClientError::Rpc(rpc) = error else { panic!("{error:?}") };
    assert_eq!(rpc.code, "no_provider");
    assert_eq!(rpc.data, Some(json!({"kind": "video", "provider": null})));
    assert_eq!((s.body("n_vid")["status"].as_str(), s.body("n_vid")["error_code"].as_str()), (Some("failed"), Some("no_provider")));
    // Elsewhere: no Studio block. A missing card: not found.
    let error = c.studio_run_card(&s.fixture.dir.path().to_string_lossy(), "ep", "n_img").await.unwrap_err();
    assert!(matches!(&error, otto_client::ClientError::Rpc(e) if e.code == "not_found"), "{error:?}");
    let error = c.studio_preview_card(&cwd, "ep", "n_gone", vec![]).await.unwrap_err();
    assert!(matches!(&error, otto_client::ClientError::Rpc(e) if e.code == "not_found"), "{error:?}");

    // Stopping a card not running here marks it stopped.
    s.cards.bodies.lock().unwrap().get_mut("n_empty").unwrap()["status"] = json!("running");
    c.studio_stop_card(&cwd, "ep", "n_empty").await.unwrap();
    assert_eq!(s.body("n_empty")["status"], "stopped");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn mentions_and_the_media_catalog() {
    let (s, server) = studio_fixture().await;
    let c = s.fixture.connect().await;
    let cwd = s.root.join("canvases/ep01-pilot").to_string_lossy().into_owned();
    let found = c.studio_mentions(&cwd, "mvn", 10).await.unwrap();
    assert_eq!(
        found,
        [StudioMention { kind: MentionKind::Entity, target: "characters/mara".into(), name: "Mara Venn".into(), detail: "Character".into() }]
    );
    let found = c.studio_mentions(&cwd, "", 10).await.unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!((found[1].kind, found[1].target.as_str(), found[1].detail.as_str()), (MentionKind::Canvas, "ep01-pilot", "Canvas · Ep 1"));
    assert_eq!(c.studio_mentions(&cwd, "", 1).await.unwrap().len(), 1);
    assert!(c.studio_mentions(&s.fixture.dir.path().to_string_lossy(), "", 10).await.unwrap().is_empty(), "not a studio");
    // `canvas#text`: that canvas's titled nodes, as canvas mentions of `canvas#node_id`.
    std::fs::create_dir_all(s.root.join("canvases/board")).unwrap();
    let board = json!({"version": 1, "nodes": [
        {"id": "n_key", "x": 0, "y": 0, "w": 1, "h": 1, "body": {"kind": "gen", "makes": "image", "title": "S2.3 key"}},
        {"id": "n_note", "x": 0, "y": 0, "w": 1, "h": 1, "body": {"kind": "note", "text": "Keep it cold."}}
    ], "edges": []});
    std::fs::write(s.root.join("canvases/board/canvas.json"), board.to_string()).unwrap();
    let found = c.studio_mentions(&cwd, "board#key", 10).await.unwrap();
    assert_eq!(
        found,
        [StudioMention { kind: MentionKind::Canvas, target: "board#n_key".into(), name: "S2.3 key".into(), detail: "Image card".into() }]
    );
    assert_eq!(c.studio_mentions(&cwd, "board#", 10).await.unwrap().len(), 2);

    let models = c.media_models(Some(MediaKind::Image), None, None).await.unwrap();
    let pix = models.iter().find(|m| m.handle == "pix-1").expect("pix-1");
    assert_eq!((pix.provider.as_str(), pix.connected, pix.default), ("pix", true, true));
    assert_eq!(pix.inputs, ["text", "reference images"]);
    assert!(models.iter().all(|m| m.kind == MediaKind::Image));
    let schema = c.media_model_schema("pix-1", None).await.unwrap();
    assert_eq!((schema.handle.as_str(), schema.connected, schema.modes.len()), ("pix-1", true, 1));
    let mode = &schema.modes[0];
    assert_eq!((mode.mode.as_str(), mode.endpoint.as_deref()), ("", None));
    let references = mode.fields.iter().find(|f| f.name == "references").expect("a references field");
    assert_eq!((references.media.as_str(), references.many, references.port.as_deref()), ("image", true, Some("reference")));
    assert!(mode.schema["properties"].get("provider").is_none());
    let error = c.media_model_schema("nope-9", None).await.unwrap_err();
    assert!(matches!(&error, otto_client::ClientError::Rpc(e) if e.code == "bad_request"), "{error:?}");
    server.shutdown().await.unwrap();
}
