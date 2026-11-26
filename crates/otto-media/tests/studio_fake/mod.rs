//! A studio on disk, fake media providers (image, video with live modes, audio) and an in-memory [`Studio`] that
//! applies canvas ops and resolves cards with `silo_proto::studio`'s own helpers, recording every op.
#![allow(dead_code)]

use otto_llm::auth::CredentialStore;
use otto_media::{
    AudioRequest, AudioType, Call, Endpoint, Generated, ImageRequest, Media, MediaError, MediaKind, MediaProvider, ModelEntry, Output, VideoRequest,
};
use otto_tools::ToolError;
use otto_tools::studio::{Card, CardInput, CardMention, Edited, Format, Makes, Port, Source, SourceMedia, Status, Studio};
use serde_json::{Value, json};
use silo_proto::studio::{Canvas, Edge, Node, NodeBody, Section};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "../fake/mod.rs"]
pub mod fake;

/// What the providers were asked, in order.
#[derive(Default)]
pub struct Log(pub Mutex<Vec<Value>>);

impl Log {
    pub fn all(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }
    pub fn last(&self) -> Value {
        self.0.lock().unwrap().last().cloned().expect("a request")
    }
    fn push(&self, v: Value) {
        self.0.lock().unwrap().push(v);
    }
}

fn entry(kind: MediaKind, id: &str, name: &str, inputs: &[&str], default: bool) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        kind,
        name: name.into(),
        about: String::new(),
        inputs: inputs.iter().map(|i| (*i).to_owned()).collect(),
        outputs: String::new(),
        price: None,
        default,
    }
}

pub struct Pix(pub Arc<Log>);

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
        vec![
            entry(MediaKind::Image, "pix-1", "Pix 1", &["text", "reference images"], true),
            entry(MediaKind::Image, "pix-solo", "Pix Solo", &["text"], false),
        ]
    }
    fn env(&self) -> &'static [&'static str] {
        &[]
    }
    async fn generate_image(&self, _key: &str, r: ImageRequest, _call: &Call) -> Result<Output, MediaError> {
        self.0.push(json!({"tool": "image", "model": r.model, "prompt": r.prompt, "references": r.references.len(), "aspect_ratio": r.aspect_ratio, "quality": r.quality, "n": r.n}));
        Ok(Output {
            media: (0..r.n).map(|_| Generated { bytes: fake::png(), mime: "image/png".into() }).collect(),
            model: r.model,
            ..Output::default()
        })
    }
}

/// How the video provider answers.
#[derive(Clone, Debug, Default)]
pub enum Behaviour {
    #[default]
    Ok,
    Fail(String),
    /// Many progress messages over `ms`, then the clip.
    Slow(u64),
    /// Until cancelled.
    Hang,
}

pub struct Vid(pub Arc<Log>, pub Mutex<Behaviour>);

#[async_trait::async_trait]
impl MediaProvider for Vid {
    fn id(&self) -> &'static str {
        "vid"
    }
    fn name(&self) -> &'static str {
        "Vid"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Video]
    }
    fn models(&self) -> Vec<ModelEntry> {
        vec![
            entry(MediaKind::Video, "vid-1", "Vid 1", &["text", "first/last frame", "reference images"], true),
            entry(MediaKind::Video, "vid-lite", "Vid Lite", &["text", "first frame"], false),
        ]
    }
    fn env(&self) -> &'static [&'static str] {
        &[]
    }
    async fn generate_video(&self, _key: &str, r: VideoRequest, call: &Call) -> Result<Output, MediaError> {
        self.0.push(json!({
            "tool": "video", "model": r.model, "prompt": r.prompt, "first_frame": r.first_frame.is_some(), "last_frame": r.last_frame.is_some(),
            "references": r.references.len(), "aspect_ratio": r.aspect_ratio, "resolution": r.resolution, "duration": r.duration,
        }));
        let behaviour = self.1.lock().unwrap().clone();
        match behaviour {
            Behaviour::Ok => {}
            Behaviour::Fail(message) => return Err(MediaError::failed(message)),
            Behaviour::Slow(ms) => {
                for i in 0..20 {
                    (call.progress)(format!("rendering {}%", i * 5));
                    tokio::time::sleep(Duration::from_millis(ms / 20)).await;
                }
            }
            Behaviour::Hang => {
                call.cancel.cancelled().await;
                return Err(MediaError::cancelled());
            }
        }
        Ok(Output { media: vec![Generated { bytes: fake::mp4(), mime: "video/mp4".into() }], model: r.model, ..Output::default() })
    }
}

fn image_to_video() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string"},
            "start_image_url": {"type": "string", "format": "uri"},
            "middle_image_url": {"type": "string", "format": "uri"},
            "end_image_url": {"type": "string", "format": "uri"},
            "aspect_ratio": {"type": "string", "enum": ["16:9", "9:16"]},
            "resolution": {"type": "string", "enum": ["720p", "1080p"]},
            "fps": {"type": "integer"},
            "duration": {"type": "number"}
        },
        "required": ["prompt", "start_image_url"]
    })
}

fn reference_to_video() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string"},
            "reference_image_urls": {"type": "array", "items": {"type": "string", "format": "uri"}, "maxItems": 3},
            "video_url": {"type": "string", "format": "uri"},
            "audio_url": {"type": "string", "format": "uri"},
            "aspect_ratio": {"type": "string", "enum": ["16:9", "9:16", "1:1"]}
        },
        "required": ["prompt"]
    })
}

fn endpoints() -> Vec<Endpoint> {
    [("agg/motion/image-to-video", image_to_video()), ("agg/motion/reference-to-video", reference_to_video())]
        .into_iter()
        .map(|(id, schema)| Endpoint {
            id: id.into(),
            name: "Agg Motion".into(),
            about: String::new(),
            kind: MediaKind::Video,
            group: None,
            operations: vec![],
            price: None,
            schema: Some(schema),
        })
        .collect()
}

/// An aggregator with one video family of two modes (image-to-video: start/middle/end frames; reference-to-video:
/// up to 3 references, a video and an audio).
pub struct Agg(pub Arc<Log>);

#[async_trait::async_trait]
impl MediaProvider for Agg {
    fn id(&self) -> &'static str {
        "agg"
    }
    fn name(&self) -> &'static str {
        "Agg"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Video]
    }
    fn models(&self) -> Vec<ModelEntry> {
        vec![entry(MediaKind::Video, "agg/motion/image-to-video", "Agg Motion", &["text", "first frame"], true)]
    }
    fn env(&self) -> &'static [&'static str] {
        &[]
    }
    async fn endpoints(&self, _http: &reqwest::Client, _key: Option<&str>) -> Option<Vec<Endpoint>> {
        Some(endpoints())
    }
    async fn endpoint(&self, _http: &reqwest::Client, _key: Option<&str>, id: &str) -> Option<Endpoint> {
        endpoints().into_iter().find(|e| e.id == id)
    }
    async fn generate(&self, _key: &str, id: &str, input: Value, _call: &Call) -> Result<Output, MediaError> {
        self.0.push(json!({"tool": "media", "endpoint": id, "input": input}));
        Ok(Output { media: vec![Generated { bytes: fake::mp4(), mime: "video/mp4".into() }], model: id.into(), ..Output::default() })
    }
}

pub struct Voc(pub Arc<Log>);

#[async_trait::async_trait]
impl MediaProvider for Voc {
    fn id(&self) -> &'static str {
        "voc"
    }
    fn name(&self) -> &'static str {
        "Voc"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Audio]
    }
    fn models(&self) -> Vec<ModelEntry> {
        vec![entry(MediaKind::Audio, "voc-speak", "Voc Speak", &["text"], true)]
    }
    fn env(&self) -> &'static [&'static str] {
        &[]
    }
    fn audio_types(&self) -> &'static [AudioType] {
        &[AudioType::Speech, AudioType::Sound, AudioType::Music]
    }
    async fn generate_audio(&self, _key: &str, r: AudioRequest, _call: &Call) -> Result<Output, MediaError> {
        self.0.push(json!({"tool": "audio", "type": format!("{:?}", r.kind), "text": r.text, "model": r.model}));
        Ok(Output {
            media: vec![Generated { bytes: fake::mp3(), mime: "audio/mpeg".into() }],
            model: r.model.unwrap_or_else(|| "voc-speak".into()),
            ..Output::default()
        })
    }
}

/// Merge `patch` into `target` (RFC 7396), as the server's `update_node` does.
pub fn merge(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(fields) => {
            if !target.is_object() {
                *target = json!({});
            }
            let map = target.as_object_mut().unwrap();
            for (k, v) in fields {
                if v.is_null() {
                    map.remove(k);
                } else {
                    merge(map.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        other => *target = other.clone(),
    }
}

/// An in-memory Studio: canvases as `silo_proto::studio::Canvas`, the world read from `root`.
pub struct FakeStudio {
    pub root: PathBuf,
    pub canvases: Mutex<BTreeMap<String, Canvas>>,
    /// Each canvas's effective format.
    pub formats: Mutex<BTreeMap<String, Format>>,
    /// Every op applied: `(canvas, op)`.
    pub ops: Mutex<Vec<(String, Value)>>,
    /// `block` answers "silo is not running" this many more times.
    pub down: AtomicUsize,
    /// No Studio block is open.
    pub closed: AtomicBool,
    next: AtomicUsize,
}

impl FakeStudio {
    pub fn new(root: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: root.to_path_buf(),
            canvases: Mutex::default(),
            formats: Mutex::default(),
            ops: Mutex::default(),
            down: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            next: AtomicUsize::new(1),
        })
    }

    pub fn sections(&self) -> Vec<Section> {
        let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for e in otto_tools::studio::entities(&self.root) {
            let looks: Vec<Value> = e
                .looks
                .iter()
                .map(|l| {
                    let media = if l.kind() == "look" { "image" } else { l.kind() };
                    json!({"id": l.id, "path": l.path, "name": l.name, "defines": l.defines, "media": media})
                })
                .collect();
            by.entry(e.section.clone())
                .or_default()
                .push(json!({"id": e.id, "name": e.name, "file": e.file, "sheet": e.sheet, "refs": e.refs, "looks": looks, "links": e.links}));
        }
        by.into_iter()
            .map(|(id, entities)| serde_json::from_value(json!({"id": id, "title": id, "kind": "entities", "entities": entities})).unwrap())
            .collect()
    }

    /// A canvas named `name` (no nodes).
    pub fn canvas_new(&self, name: &str, format: Option<Format>) {
        let canvas: Canvas = serde_json::from_value(json!({"name": name, "title": name})).unwrap();
        self.canvases.lock().unwrap().insert(name.into(), canvas);
        if let Some(f) = format {
            self.formats.lock().unwrap().insert(name.into(), f);
        }
    }

    /// Add node `id` with `body` (and `parent`) directly, unrecorded.
    pub fn put(&self, canvas: &str, id: &str, body: Value) {
        let node: Node = serde_json::from_value(json!({"id": id, "x": 0.0, "y": 0.0, "w": 100.0, "h": 100.0, "body": body})).unwrap();
        self.canvases.lock().unwrap().get_mut(canvas).unwrap().nodes.push(node);
    }

    /// Wire `from` into `to`'s `role` port directly, unrecorded.
    pub fn wire(&self, canvas: &str, from: &str, to: &str, role: &str) {
        let id = format!("e_{}", self.next.fetch_add(1, Ordering::SeqCst));
        let edge: Edge = serde_json::from_value(json!({"id": id, "from": from, "to": to, "role": role})).unwrap();
        self.canvases.lock().unwrap().get_mut(canvas).unwrap().edges.push(edge);
    }

    /// Node `id`'s body as JSON.
    pub fn body(&self, canvas: &str, id: &str) -> Value {
        let canvases = self.canvases.lock().unwrap();
        serde_json::to_value(&canvases[canvas].node(id).unwrap_or_else(|| panic!("no node {id}")).body).unwrap()
    }

    pub fn node(&self, canvas: &str, id: &str) -> Node {
        self.canvases.lock().unwrap()[canvas].node(id).unwrap().clone()
    }

    pub fn edges(&self, canvas: &str) -> Vec<Edge> {
        self.canvases.lock().unwrap()[canvas].edges.clone()
    }

    pub fn nodes(&self, canvas: &str) -> Vec<Node> {
        self.canvases.lock().unwrap()[canvas].nodes.clone()
    }

    /// The `update_node` bodies written to node `id`, in order.
    pub fn updates(&self, id: &str) -> Vec<Value> {
        self.ops.lock().unwrap().iter().filter(|(_, op)| op["op"] == "update_node" && op["id"] == id).map(|(_, op)| op["body"].clone()).collect()
    }

    /// The statuses written to `id`, in order.
    pub fn statuses(&self, id: &str) -> Vec<String> {
        self.updates(id).iter().filter_map(|b| b["status"].as_str().map(str::to_owned)).collect()
    }

    /// Wait until card `id` has `status`.
    pub async fn until(&self, canvas: &str, id: &str, status: &str) -> Value {
        for _ in 0..1000 {
            let body = self.body(canvas, id);
            if body["status"] == status {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} never {status}: {}", self.body(canvas, id))
    }

    /// Wait until `status` was written to card `id` the `nth` time; its body then.
    pub async fn written(&self, id: &str, status: &str, nth: usize) -> Value {
        for _ in 0..1000 {
            if self.statuses(id).iter().filter(|s| *s == status).count() >= nth {
                let canvas = self.ops.lock().unwrap().iter().rev().find(|(_, op)| op["id"] == id).map(|(c, _)| c.clone()).unwrap();
                return self.body(&canvas, id);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} never {status} ({nth}): {:?}", self.statuses(id))
    }

    fn apply(&self, canvas: &mut Canvas, op: &Value) -> Result<Option<String>, String> {
        let next = || format!("n_{}", self.next.fetch_add(1, Ordering::SeqCst));
        match op["op"].as_str().unwrap_or_default() {
            "add_node" => {
                let mut node = op["node"].clone();
                let id = node["id"].as_str().map_or_else(next, str::to_owned);
                for (k, v) in [("id", json!(id)), ("x", json!(0.0)), ("y", json!(0.0)), ("w", json!(100.0)), ("h", json!(100.0))] {
                    if node.get(k).is_none_or(Value::is_null) {
                        node[k] = v;
                    }
                }
                let node: Node = serde_json::from_value(node).map_err(|e| e.to_string())?;
                canvas.nodes.push(node);
                Ok(Some(id))
            }
            "update_node" => {
                let id = op["id"].as_str().unwrap_or_default();
                let node = canvas.nodes.iter_mut().find(|n| n.id == id).ok_or(format!("no node {id}"))?;
                let mut body = serde_json::to_value(&node.body).unwrap();
                merge(&mut body, &op["body"]);
                node.body = serde_json::from_value(body).map_err(|e| e.to_string())?;
                if let Some(parent) = op.get("parent") {
                    node.parent = parent.as_str().map(str::to_owned);
                }
                Ok(None)
            }
            "remove_edge" => {
                let id = op["id"].as_str().unwrap_or_default();
                canvas.edges.retain(|e| e.id != id);
                Ok(None)
            }
            "add_edge" => {
                let mut edge = op["edge"].clone();
                let id = edge["id"].as_str().map_or_else(|| format!("e_{}", self.next.fetch_add(1, Ordering::SeqCst)), str::to_owned);
                edge["id"] = json!(id);
                let edge: Edge = serde_json::from_value(edge).map_err(|e| e.to_string())?;
                if canvas.node(&edge.from).is_none() || canvas.node(&edge.to).is_none() {
                    return Err(format!("no node for edge {} → {}", edge.from, edge.to));
                }
                let replaced = edge.role.single().then(|| canvas.edges.iter().position(|e| e.to == edge.to && e.role == edge.role)).flatten();
                match replaced {
                    Some(at) => canvas.edges[at] = edge,
                    None => canvas.edges.push(edge),
                }
                Ok(Some(id))
            }
            "remove_node" => {
                let id = op["id"].as_str().unwrap_or_default();
                canvas.nodes.retain(|n| n.id != id);
                canvas.edges.retain(|e| e.from != id && e.to != id);
                Ok(None)
            }
            other => Err(format!("unknown op {other}")),
        }
    }
}

fn wire_enum<T: serde::de::DeserializeOwned>(value: impl serde::Serialize, unknown: T) -> T {
    serde_json::from_value(json!(value)).unwrap_or(unknown)
}

fn mtime(root: &Path, path: &str) -> Option<u64> {
    let modified = std::fs::metadata(root.join(path)).ok()?.modified().ok()?;
    u64::try_from(modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis()).ok()
}

#[async_trait::async_trait]
impl Studio for FakeStudio {
    async fn block(&self, _cwd: &Path, _sessions: &[String], _open: bool) -> Result<Option<String>, ToolError> {
        if self.down.load(Ordering::SeqCst) > 0 {
            self.down.fetch_sub(1, Ordering::SeqCst);
            return Err(ToolError::failed("silo is not running"));
        }
        Ok((!self.closed.load(Ordering::SeqCst)).then(|| "b_studio".to_owned()))
    }
    async fn project(&self, _block: &str) -> Result<Value, ToolError> {
        Ok(json!({"name": "Show"}))
    }
    async fn canvas(&self, _block: &str, canvas: &str) -> Result<Value, ToolError> {
        let canvases = self.canvases.lock().unwrap();
        Ok(serde_json::to_value(canvases.get(canvas).ok_or_else(|| ToolError::invalid(format!("no canvas {canvas}")))?).unwrap())
    }
    async fn world(&self, _block: &str) -> Result<Value, ToolError> {
        Ok(json!({"sections": self.sections()}))
    }
    async fn edit(&self, _block: &str, canvas: &str, ops: Vec<Value>) -> Result<Edited, ToolError> {
        let mut canvases = self.canvases.lock().unwrap();
        let c = canvases.get_mut(canvas).ok_or_else(|| ToolError::invalid(format!("no canvas {canvas}")))?;
        let mut out = Edited::default();
        for op in ops {
            self.ops.lock().unwrap().push((canvas.to_owned(), op.clone()));
            let adds = matches!(op["op"].as_str(), Some("add_node" | "add_edge"));
            match self.apply(c, &op) {
                Ok(Some(id)) => out.ids.push(id),
                Ok(None) => {}
                Err(why) => {
                    if adds {
                        out.ids.push(String::new());
                    }
                    out.failed.push(why);
                }
            }
        }
        c.rev += 1;
        out.rev = c.rev;
        Ok(out)
    }
    async fn card(&self, _block: &str, canvas_name: &str, node: &str) -> Result<Card, ToolError> {
        let canvas = self.canvases.lock().unwrap().get(canvas_name).cloned().ok_or_else(|| ToolError::invalid("no canvas"))?;
        let sections = self.sections();
        let found = canvas.node(node).ok_or_else(|| ToolError::invalid(format!("no node {node} on canvas {canvas_name}")))?;
        let NodeBody::Gen { makes, prompt, model, mode, options, status, takes, .. } = &found.body else {
            return Err(ToolError::invalid(format!("{node} is not a card")));
        };
        let inputs = canvas
            .inputs_of(node)
            .into_iter()
            .filter_map(|edge| {
                let out = canvas.output_of(&edge.from, &sections)?;
                Some(CardInput {
                    port: wire_enum(edge.role, Port::Unknown),
                    from: edge.from.clone(),
                    media: wire_enum(out.media, SourceMedia::Unknown),
                    path: out.path,
                    text: out.text,
                    label: out.label,
                    entity: match canvas.node(&edge.from).map(|n| &n.body) {
                        Some(NodeBody::Entity { id }) => Some(id.clone()),
                        _ => None,
                    },
                    pending: out.pending,
                    problem: out.problem,
                })
            })
            .collect();
        let keys = canvas.input_keys(node, &sections, |p| mtime(&self.root, p));
        Ok(Card {
            root: self.root.clone(),
            canvas: canvas_name.into(),
            node: node.into(),
            label: canvas.label_of(node, &sections),
            frame: found.parent.clone(),
            makes: wire_enum(makes, Makes::Unknown),
            prompt: prompt.clone(),
            model: model.clone(),
            mode: mode.clone(),
            options: options.clone(),
            status: wire_enum(status, Status::Unknown),
            takes: takes.iter().map(|t| serde_json::to_value(t).unwrap()).collect(),
            inputs,
            keys,
            format: self.formats.lock().unwrap().get(canvas_name).cloned(),
            mentions: canvas
                .mentions_of(node, &sections)
                .into_iter()
                .map(|(t, label)| {
                    let look = t.look(&sections).map(|(_, l)| l);
                    CardMention { token: t.token(), label, path: look.map(|l| l.path.clone()), defines: look.and_then(|l| l.defines.clone()) }
                })
                .collect(),
        })
    }
    async fn sources(&self, _block: &str, canvas_name: &str, paths: &[String]) -> Result<Vec<Option<Source>>, ToolError> {
        let canvas = self.canvases.lock().unwrap().get(canvas_name).cloned().ok_or_else(|| ToolError::invalid("no canvas"))?;
        let sections = self.sections();
        Ok(paths
            .iter()
            .map(|path| {
                if let Some(n) = canvas.nodes.iter().find(|n| canvas.output_of(&n.id, &sections).is_some_and(|o| o.path.as_deref() == Some(path))) {
                    return Some(Source { node: Some(n.id.clone()), entity: None });
                }
                let e = sections.iter().flat_map(|s| &s.entities).find(|e| e.sheet.as_deref() == Some(path) || e.refs.iter().any(|r| r == path))?;
                Some(Source { node: None, entity: Some(e.id.clone()) })
            })
            .collect())
    }
}

pub fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

pub const STYLE: &str = "Muted teal and amber, 35mm grain.";
pub const MOTION: &str = "Limited animation on twos, snappy holds.";

pub struct Fx {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub media: Arc<Media>,
    pub studio: Arc<FakeStudio>,
    pub log: Arc<Log>,
    pub vid: Arc<Vid>,
}

pub fn format(aspect: &str, resolution: Option<&str>, fps: Option<u32>) -> Format {
    Format { name: "f".into(), aspect: aspect.into(), resolution: resolution.map(str::to_owned), fps }
}

/// A studio at `<tmp>/show` (style line, motion line, Mara with a sheet and a ref, a plate), canvas `ep` (16:9 1080p), the
/// providers with keys for `connected`, the fake Studio set unless `offline`.
pub fn fx(connected: &[&str], offline: bool) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("show")).unwrap();
    let root = std::fs::canonicalize(dir.path().join("show")).unwrap();
    write(&root, "studio.toml", b"name = \"Show\"\nformat = \"landscape\"\n\n[formats.landscape]\naspect = \"16:9\"\nresolution = \"1080p\"\nfps = 24\n\n[formats.portrait]\naspect = \"4:5\"\nresolution = \"1080p\"\n");
    write(&root, "world/style.md", format!("# Style\n\n## Style line\n\n{STYLE}\n\n## Motion line\n\n{MOTION}\n\n## Palette\n\nTeal.\n").as_bytes());
    write(&root, "world/characters/mara/mara.md", b"---\nname: Mara Venn\nsheet: sheet.png\n---\nA diver.\n");
    write(&root, "world/characters/mara/sheet.png", &fake::png());
    write(&root, "world/characters/mara/refs/face.png", &fake::png());
    write(&root, "plates/harbor.png", &fake::png());
    write(&root, "plates/dock.png", &fake::png());
    write(&root, "clips/wave.mp4", &fake::mp4());
    write(&root, "sounds/gulls.mp3", &fake::mp3());
    let log: Arc<Log> = Arc::default();
    let vid = Arc::new(Vid(log.clone(), Mutex::default()));
    let providers: Vec<Arc<dyn MediaProvider>> =
        vec![Arc::new(Pix(log.clone())), vid.clone(), Arc::new(Agg(log.clone())), Arc::new(Voc(log.clone()))];
    let store = CredentialStore::open(dir.path().join("auth")).unwrap();
    let media = Media::with(store, dir.path().join("state"), providers, Arc::new(|_: &str| None));
    for id in connected {
        media.set_key(id, Some(format!("key-{id}"))).unwrap();
    }
    let studio = FakeStudio::new(&root);
    studio.canvas_new("ep", Some(format("16:9", Some("1080p"), Some(24))));
    if !offline {
        media.set_studio(studio.clone());
    }
    Fx { dir, root, media, studio, log, vid }
}

pub const ALL: &[&str] = &["pix", "vid", "agg", "voc"];

impl Fx {
    pub async fn card(&self, id: &str) -> Card {
        self.studio.card("b_studio", "ep", id).await.unwrap()
    }
    pub async fn compile(&self, id: &str) -> Result<otto_media::studio::Compiled, otto_media::studio::Problem> {
        let card = self.card(id).await;
        otto_media::studio::compile(&self.media, &card, &otto_media::studio::Setup::read(&self.root), &serde_json::Map::new()).await
    }
    /// A session's call context at the studio root: attachments kept, no jobs.
    pub fn cx(&self) -> otto_tools::ToolCx {
        let mut cx = otto_tools::ToolCx::new("call1", self.root.clone());
        cx.attachments = Some(self.dir.path().join("attachments"));
        cx.sessions = vec!["s1".into()];
        cx
    }
    pub async fn tool(&self, name: &str, input: Value, cx: otto_tools::ToolCx) -> Result<otto_tools::ToolOutput, ToolError> {
        let tool = self.media.tools().into_iter().find(|t| t.spec().name == name).unwrap();
        tool.call(input, cx).await
    }
}

/// A card body.
pub fn card(makes: &str, prompt: &str) -> Value {
    json!({"kind": "gen", "makes": makes, "prompt": prompt})
}

pub fn asset(path: &str, media: &str) -> Value {
    json!({"kind": "asset", "path": path, "media": media})
}

pub fn note(text: &str) -> Value {
    json!({"kind": "note", "text": text})
}

/// A card that has run: a take at `path`.
pub fn done(makes: &str, title: &str, path: &str) -> Value {
    json!({"kind": "gen", "makes": makes, "title": title, "prompt": "p", "status": "done", "takes": [{"path": path, "media": "image", "at": 1}]})
}
