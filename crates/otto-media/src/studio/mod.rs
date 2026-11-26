//! Studio cards (docs/STUDIO.md §6.2): compiling a card into one request ([`compile`]), and the one run path every
//! take is made by, whoever asks: the app's Run button ([`start_card`], no session), the agent's `canvas_run`
//! ([`CanvasRun`], session jobs), and `generate_* {canvas}` ([`place`]: a card made from the call, then run).
mod compile;
mod run;

pub(crate) use compile::port_of;
pub use compile::{Compiled, DOCUMENT_CAP, Problem, Setup, compile, fit};

use crate::tools::{Launch, launch, noun, pick, text};
use crate::{Media, MediaKind};
use otto_proto::CardPreview;
use otto_tools::studio::{Makes, Port, Studio};
use otto_tools::{Tool, ToolCx, ToolError, ToolOutput, ToolSpec, attachments};
use run::{Target, fail, problem_error};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn problem(code: &'static str, message: impl Into<String>) -> Problem {
    Problem { code, message: message.into(), kind: None, provider: None }
}

/// The bridge and the Studio block for the studio holding `cwd` (`open`: open one beside the session's agent block).
async fn block(media: &Media, cwd: &Path, sessions: &[String], open: bool) -> Result<(Arc<dyn Studio>, String), Problem> {
    let studio = media.studio().ok_or_else(|| problem("no_silo", "Studio canvases need Silo (this otto runs without it)"))?;
    match studio.block(cwd, sessions, open).await {
        Ok(Some(block)) => Ok((studio, block)),
        Ok(None) => Err(problem("not_found", format!("No Studio block is open on {}", cwd.display()))),
        Err(e) => Err(problem("no_silo", e.message)),
    }
}

/// A card's provider and model for its job's metadata.
fn launch_for(media: &Media, compiled: &Compiled, label: String, target: &Target) -> Result<Launch, ToolError> {
    let provider = media.provider(&compiled.provider).ok_or_else(|| ToolError::failed(format!("unknown media provider {}", compiled.provider)))?;
    Ok(Launch {
        kind: compiled.kind,
        label,
        provider,
        model: compiled.model.clone(),
        name: Some(compiled.model_name.clone()),
        extra: json!({"canvas": target.canvas, "node": target.node, "handle": compiled.model, "mode": compiled.mode}),
    })
}

// The app's path: no session, no model turn.

/// `studio.run_card`: start card `node` and return; the card carries the state. A card no connected provider can
/// make is failed at once and `no_provider` (with its kind) returned, so the app can offer the media setup.
///
/// # Errors
/// No Silo or Studio block, an unknown card, or `no_provider`.
pub async fn start_card(media: &Arc<Media>, cwd: &Path, canvas: &str, node: &str) -> Result<(), Problem> {
    let (studio, block) = block(media, cwd, &[], false).await?;
    let target = Target::new(block, canvas, node);
    let card = studio.card(&target.block, canvas, node).await.map_err(|e| problem("not_found", e.message))?;
    // Only a missing provider is answered here; other problems fail the card when it runs (after its inputs).
    if let Err(p) = compile(media, &card, &Setup::read(&card.root), &Map::new()).await
        && p.code == "no_provider"
    {
        fail(studio.as_ref(), &target, &p.message, Some(p.code), false).await;
        return Err(p);
    }
    let media = media.clone();
    let cx = ToolCx::new(format!("studio-{node}"), card.root.clone());
    tokio::spawn(async move {
        if let Err(e) = run::run(&media, &studio, &target, Map::new(), cx).await {
            tracing::info!(node = target.node, "studio card ended: {}", e.message);
        }
    });
    Ok(())
}

/// `studio.stop_card`: cancel its run here (the provider's request is withdrawn); a card left `queued` or `running`
/// by no run of this process is marked stopped.
///
/// # Errors
/// No Silo or Studio block.
pub async fn stop_card(media: &Arc<Media>, cwd: &Path, canvas: &str, node: &str) -> Result<(), Problem> {
    let (studio, block) = block(media, cwd, &[], false).await?;
    let root = otto_tools::studio::root_for(cwd).unwrap_or_else(|| cwd.to_path_buf());
    if !run::cancel(media, &root, canvas, node) {
        fail(studio.as_ref(), &Target::new(block, canvas, node), "", None, true).await;
    }
    Ok(())
}

/// `studio.preview_card`: what the card compiles to now, the ports its model takes, and whether each of `models`
/// could make it.
///
/// # Errors
/// No Silo or Studio block, an unknown card.
pub async fn preview_card(media: &Arc<Media>, cwd: &Path, canvas: &str, node: &str, models: &[String]) -> Result<CardPreview, Problem> {
    let (studio, block) = block(media, cwd, &[], false).await?;
    let card = studio.card(&block, canvas, node).await.map_err(|e| problem("not_found", e.message))?;
    let mut preview = CardPreview { waiting: card.inputs.iter().filter(|i| i.pending).map(|i| i.label.clone()).collect(), ..CardPreview::default() };
    match compile(media, &card, &Setup::read(&card.root), &Map::new()).await {
        Ok(c) => {
            preview.ports = c.ports.iter().map(|p| p.id().to_owned()).collect();
            preview.compiled = Some(c.wire());
        }
        Err(p) => preview.error = Some(p.wire()),
    }
    for name in models {
        preview.fits.push(fit(media, &card, name).await);
    }
    Ok(preview)
}

/// Cards a previous otto process was running when it stopped, marked `stopped`. While Silo isn't answering they
/// are tried again (2 s, doubling to a minute), and kept in the runs file meanwhile.
pub async fn recover(media: &Arc<Media>) {
    recover_every(media, std::time::Duration::from_secs(2)).await;
}

#[doc(hidden)]
/// [`recover`] trying again after `wait` (doubling, at most a minute); for tests.
pub async fn recover_every(media: &Arc<Media>, mut wait: std::time::Duration) {
    let mut left = run::left_running(media);
    while !left.is_empty() {
        let mut later = Vec::new();
        for (root, canvas, node) in left {
            let (studio, block) = match block(media, &root, &[], false).await {
                Ok(found) => found,
                Err(p) if p.code == "no_silo" => {
                    later.push((root, canvas, node));
                    continue;
                }
                // No Studio block on that root: nothing to write through.
                Err(_) => continue,
            };
            let target = Target::new(block, canvas, node);
            if let Ok(card) = studio.card(&target.block, &target.canvas, &target.node).await
                && matches!(card.status, otto_tools::studio::Status::Queued | otto_tools::studio::Status::Running)
            {
                fail(studio.as_ref(), &target, "", None, true).await;
            }
        }
        run::set_orphans(media, &later);
        left = later;
        if !left.is_empty() {
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(std::time::Duration::from_secs(60));
        }
    }
}

// The agent's paths.

/// Card nodes in dependency order: a card wired from another of `nodes` comes after it.
fn ordered(canvas: &Value, nodes: &[String]) -> Vec<String> {
    let feeds = |from: &str, to: &str| {
        canvas["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| e["from"] == from && e["to"] == to && e["role"].as_str().is_some_and(|r| r != "next"))
    };
    let (mut done, mut left): (Vec<String>, Vec<String>) = (Vec::new(), nodes.to_vec());
    left.dedup();
    while !left.is_empty() {
        let ready = left.iter().position(|n| !left.iter().any(|m| m != n && feeds(m, n))).unwrap_or(0);
        done.push(left.remove(ready));
    }
    done
}

pub(crate) struct CanvasRun {
    media: Arc<Media>,
    spec: ToolSpec,
}

impl CanvasRun {
    pub(crate) fn new(media: Arc<Media>) -> Self {
        let spec = crate::tools::spec(
            "canvas_run",
            "Run cards on a Studio canvas: each card (a gen node) is compiled from its prompt (each mention, @characters/mara or @#n_3f2a9c, becomes its name with where it went: \"Mara Venn (image 2)\"), what is wired into its ports (references, frames, video, audio, texts) and its options (sizes from the canvas's format; the project's style line added to image and video cards, and its motion line after it on video cards, unless options.style is false), then made as a new take that the card shows. Pass every card to make in one call: they run in the background as jobs of this session, in dependency order when one feeds another (a keyframe card before the video card it is wired into), and each result arrives as a message with its [generated …] line and `Placed on canvas … as …`. A wired card that has never run and is not in nodes is an error (\"Run Keyframe 3 first\"). Build the cards and wires with canvas_edit first (canvas_read shows ids); the model is the card's own (model, mode), else the project's [models] pick in studio.toml, else the kind's default. Returns at once: tell the user what is being made and keep working; don't wait or poll.",
            json!({
                "type": "object",
                "properties": {
                    "canvas": {"type": "string", "description": "The canvas's name (its folder, as canvas_read shows)"},
                    "nodes": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Card node ids to run"}
                },
                "required": ["canvas", "nodes"],
                "additionalProperties": false
            }),
        );
        Self { media, spec }
    }
}

#[async_trait::async_trait]
impl Tool for CanvasRun {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let canvas = text(&input, "canvas").ok_or_else(|| ToolError::invalid("canvas must be a canvas name"))?.to_owned();
        let nodes = crate::tools::strings(&input, "nodes")?;
        if nodes.is_empty() {
            return Err(ToolError::invalid("nodes must name at least one card"));
        }
        let (studio, block) = block(&self.media, &cx.cwd, &cx.sessions, true).await.map_err(|p| ToolError::failed(p.message))?;
        let order = ordered(&studio.canvas(&block, &canvas).await?, &nodes);
        // All queued first: a card wired from another in this call waits for it rather than failing.
        let ops = order.iter().map(|n| json!({"op": "update_node", "id": n, "body": {"status": "queued", "error": null}})).collect();
        studio.edit(&block, &canvas, ops).await?;
        let mut lines = Vec::new();
        for node in order {
            let target = Target::new(block.clone(), canvas.clone(), node.clone());
            let card = match studio.card(&block, &canvas, &node).await {
                Ok(c) => c,
                Err(e) => {
                    lines.push(format!("{node}: {}", e.message));
                    continue;
                }
            };
            let compiled = match compile(&self.media, &card, &Setup::read(&card.root), &Map::new()).await {
                Ok(c) => c,
                Err(p) => {
                    fail(studio.as_ref(), &target, &p.message, Some(p.code), false).await;
                    lines.push(format!("{} ({node}) not run: {}", card.label, p.message));
                    continue;
                }
            };
            let what = launch_for(&self.media, &compiled, format!("canvas_run: {}", card.label), &target)?;
            let (media, studio, job) = (self.media.clone(), studio.clone(), target.job.clone());
            let started = launch(&cx, true, what, move |cx| async move { run::run(&media, &studio, &target, Map::new(), cx).await }).await;
            if let Some(id) = started.as_ref().ok().and_then(|o| o.metadata["job_id"].as_str()) {
                let _ = job.set(id.to_owned());
            }
            match started {
                Ok(out) => lines.push(format!("{} ({node}): {}", card.label, out.joined_text())),
                Err(e) => lines.push(format!("{} ({node}) failed: {}", card.label, e.message)),
            }
        }
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

/// The `canvas` field of a generation call.
struct Aim {
    name: String,
    frame: Option<String>,
    node: Option<String>,
    title: Option<String>,
    style: bool,
}

fn aim(value: &Value) -> Result<Aim, ToolError> {
    let name = text(value, "name").ok_or_else(|| ToolError::invalid("canvas.name must be a canvas name"))?.to_owned();
    let style = match value.get("style") {
        None | Some(Value::Null) => true,
        Some(v) => v.as_bool().ok_or_else(|| ToolError::invalid("canvas.style must be true or false"))?,
    };
    let field = |k: &str| text(value, k).map(str::to_owned);
    Ok(Aim { name, frame: field("frame"), node: field("node"), title: field("title"), style })
}

/// A file a call names: wired from a source, or passed as it is.
struct Named {
    field: String,
    port: Option<Port>,
    raw: String,
    many: bool,
}

/// The call's file fields and what a card makes of it: its kind, and (`generate_media`) its model and mode.
async fn call_files(media: &Media, tool: &str, input: &Value) -> Result<(Makes, Vec<Named>, Map<String, Value>), ToolError> {
    let mut out = Vec::new();
    let mut add = |field: &str, port: Option<Port>, value: &Value| match value {
        Value::String(s) if !s.trim().is_empty() => out.push(Named { field: field.into(), port, raw: s.trim().into(), many: false }),
        Value::Array(items) => {
            out.extend(items.iter().filter_map(Value::as_str).map(|s| Named { field: field.into(), port, raw: s.trim().into(), many: true }));
        }
        _ => {}
    };
    let mut rest = input.as_object().cloned().unwrap_or_default();
    for key in ["prompt", "text", "type", "model", "provider", "path", "background", "canvas", "mode", "input"] {
        rest.remove(key);
    }
    let makes = match tool {
        "generate_image" => {
            add("references", Some(Port::Reference), &input["references"]);
            add("markup", None, &input["markup"]);
            Makes::Image
        }
        "generate_video" => {
            add("first_frame", Some(Port::FirstFrame), &input["first_frame"]);
            add("last_frame", Some(Port::LastFrame), &input["last_frame"]);
            add("references", Some(Port::Reference), &input["references"]);
            Makes::Video
        }
        "generate_audio" => match input["type"].as_str() {
            Some("sound") => Makes::Sound,
            Some("music") => Makes::Music,
            _ => Makes::Speech,
        },
        _ => {
            let name = text(input, "model").ok_or_else(|| ToolError::invalid("model must be a model handle or id"))?;
            let (model, named) = pick::model(media, name).await?;
            let body = input["input"].as_object().cloned().unwrap_or_default();
            rest = body.clone();
            rest.remove("prompt");
            rest.remove("text");
            let schema = match pick::mode(&model, text(input, "mode").or(named.as_deref()))? {
                _ if model.modes.is_empty() => pick::quick_schema(&model),
                Some(i) => pick::with_schema(media, &model, i).await?.schema.unwrap_or(Value::Null),
                None => model.modes.iter().find_map(|(_, e)| e.schema.clone()).unwrap_or(Value::Null),
            };
            for f in crate::schema::fields(&schema).into_iter().filter(|f| !f.name.contains(['.', '['])) {
                if let Some(value) = body.get(&f.name) {
                    add(&f.name, compile::port_of(&f.name, f.media, f.many.is_some(), model.kind), value);
                }
            }
            match model.kind {
                MediaKind::Image => Makes::Image,
                MediaKind::Video => Makes::Video,
                MediaKind::Audio => {
                    let about = format!("{} {}", model.name, model.modes.iter().map(|(m, e)| format!("{m} {}", e.id)).collect::<Vec<_>>().join(" "))
                        .to_lowercase();
                    if about.contains("music") {
                        Makes::Music
                    } else if about.contains("sound") || about.contains("sfx") {
                        Makes::Sound
                    } else {
                        Makes::Speech
                    }
                }
            }
        }
    };
    for f in &out {
        rest.remove(&f.field);
    }
    Ok((makes, out, rest))
}

/// `generate_* {canvas}`: a card made from the call (or `canvas.node` given the call's prompt, model and options),
/// wired from the call's files that are nodes of the canvas or world entity files, then run like any card; its
/// result becomes the card's take. Without Silo the files are saved under `canvases/<name>/assets/` alone.
pub(crate) async fn place(media: &Arc<Media>, tool: &str, mut input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let aim = aim(&input["canvas"])?;
    if let Some(obj) = input.as_object_mut() {
        obj.remove("canvas");
    }
    let root = otto_tools::studio::root_for(&cx.cwd)
        .ok_or_else(|| ToolError::invalid(format!("{} isn't in a studio (no studio.toml above it): canvas needs one", cx.cwd.display())))?;
    let Ok((studio, block)) = block(media, &cx.cwd, &cx.sessions, true).await else {
        return offline(media, tool, input, cx, &root, &aim).await;
    };
    let (makes, files, options) = call_files(media, tool, &input).await?;
    let prompt =
        text(&input, "prompt").or_else(|| text(&input, "text")).or_else(|| text(&input["input"], "prompt")).or_else(|| text(&input["input"], "text"));
    let prompt = prompt.unwrap_or_default().to_owned();
    let model = text(&input, "model").map(str::to_owned).or_else(|| {
        let provider = media.provider(text(&input, "provider")?)?;
        let kind = compile::kind_of(makes)?.0;
        Some(format!("{}/{}", provider.id(), provider.default_model(kind)?))
    });
    let mut options = Value::Object(options);
    if !aim.style {
        options["style"] = json!(false);
    }
    // The card's own take (an edit of it) is a file, not a wire.
    let own: Vec<String> = match &aim.node {
        Some(node) => studio.card(&block, &aim.name, node).await?.takes.iter().filter_map(|t| t["path"].as_str().map(str::to_owned)).collect(),
        None => Vec::new(),
    };
    let mut extra = Map::new();
    let mut wires: Vec<(String, Port)> = Vec::new();
    let mut lookups: Vec<(String, Port)> = Vec::new();
    for f in files {
        let as_file = |extra: &mut Map<String, Value>, value: String| {
            if f.many {
                extra.entry(f.field.clone()).or_insert_with(|| json!([])).as_array_mut().into_iter().for_each(|a| a.push(json!(value)));
            } else {
                extra.insert(f.field.clone(), json!(value));
            }
        };
        let local = !crate::tools::is_link(&f.raw) && !attachments::is_id(&f.raw);
        let resolved = local.then(|| otto_tools::policy::resolve_path(&cx.cwd, &cx.project_root, &f.raw).ok()).flatten();
        let rel = resolved.as_ref().and_then(|r| r.path.strip_prefix(&root).ok()).map(|p| p.to_string_lossy().into_owned());
        match (rel, f.port) {
            (Some(rel), Some(port)) if !own.contains(&rel) => lookups.push((rel, port)),
            (Some(rel), _) => as_file(&mut extra, rel),
            (None, _) => as_file(&mut extra, resolved.map_or(f.raw.clone(), |r| r.path.to_string_lossy().into_owned())),
        }
    }
    // Sources: a node outputting the file, else an Entity node (its sheet) or an Asset node (a ref) added for it.
    let paths: Vec<String> = lookups.iter().map(|(p, _)| p.clone()).collect();
    let sources = studio.sources(&block, &aim.name, &paths).await?;
    let mut ops: Vec<Value> = Vec::new();
    let mut added: Vec<(String, usize)> = Vec::new();
    let mut pending: Vec<(usize, Port)> = Vec::new();
    for ((path, port), source) in lookups.into_iter().zip(sources) {
        match source {
            Some(s) if s.node.is_some() => wires.push((s.node.unwrap_or_default(), port)),
            Some(s) => {
                let id = s.entity.unwrap_or_default();
                let sheet = otto_tools::studio::entity(&root, &id).and_then(|e| e.sheet).is_some_and(|sheet| sheet == path);
                let body = if sheet { json!({"kind": "entity", "id": id}) } else { json!({"kind": "asset", "path": path, "media": "image"}) };
                let at = match added.iter().find(|(p, _)| *p == path) {
                    Some((_, at)) => *at,
                    None => {
                        let mut node = json!({"body": body});
                        if let Some(frame) = &aim.frame {
                            node["parent"] = json!(frame);
                        }
                        ops.push(json!({"op": "add_node", "node": node}));
                        added.push((path.clone(), ops.len() - 1));
                        ops.len() - 1
                    }
                };
                pending.push((at, port));
            }
            None => {
                let field = match port {
                    Port::FirstFrame => "first_frame",
                    Port::LastFrame => "last_frame",
                    _ => "references",
                };
                if field == "references" {
                    extra.entry(field.to_owned()).or_insert_with(|| json!([])).as_array_mut().into_iter().for_each(|a| a.push(json!(path)));
                } else {
                    extra.insert(field.into(), json!(path));
                }
            }
        }
    }
    let mut body = json!({"prompt": prompt, "options": options});
    if let Some(m) = &model {
        body["model"] = json!(m);
    }
    if let Some(mode) = text(&input, "mode") {
        body["mode"] = json!(mode);
    }
    if let Some(title) = &aim.title {
        body["title"] = json!(title);
    }
    let card_op = match &aim.node {
        Some(node) => {
            // Its old wires go: the card now compiles to this call. Those its prompt's mentions made stay; the
            // server takes them away with their tokens.
            let canvas = studio.canvas(&block, &aim.name).await?;
            let old = |e: &&Value| e["to"] == node.as_str() && e["role"] != "next" && e["mention"] != true;
            for e in canvas["edges"].as_array().into_iter().flatten().filter(old) {
                ops.push(json!({"op": "remove_edge", "id": e["id"]}));
            }
            ops.push(json!({"op": "update_node", "id": node, "body": body}));
            None
        }
        None => {
            body["kind"] = json!("gen");
            body["makes"] = json!(makes);
            let mut node = json!({"body": body});
            if let Some(frame) = &aim.frame {
                node["parent"] = json!(frame);
            }
            ops.push(json!({"op": "add_node", "node": node}));
            Some(ops.len() - 1)
        }
    };
    // `ids` has one entry per add op, in op order.
    let add_index = |op: usize, ops: &[Value]| ops[..op].iter().filter(|o| o["op"] == "add_node" || o["op"] == "add_edge").count();
    let edited = studio.edit(&block, &aim.name, ops.clone()).await?;
    let id_of = |op: usize| edited.ids.get(add_index(op, &ops)).cloned().filter(|i| !i.is_empty());
    let node = match (card_op, &aim.node) {
        (Some(op), _) => id_of(op).ok_or_else(|| ToolError::failed(format!("can't add the card to {}: {}", aim.name, edited.failed.join("; "))))?,
        (None, Some(node)) => node.clone(),
        (None, None) => unreachable!("a card op is always made"),
    };
    wires.extend(pending.into_iter().filter_map(|(op, port)| Some((id_of(op)?, port))));
    if !wires.is_empty() {
        let edges = wires.iter().map(|(from, port)| json!({"op": "add_edge", "edge": {"from": from, "to": node, "role": port.id()}})).collect();
        let wired = studio.edit(&block, &aim.name, edges).await?;
        if !wired.failed.is_empty() {
            tracing::info!("some wires into {node} weren't added: {}", wired.failed.join("; "));
        }
    }
    let target = Target::new(block, aim.name.clone(), node);
    let card = studio.card(&target.block, &target.canvas, &target.node).await?;
    let compiled = match compile(media, &card, &Setup::read(&card.root), &extra).await {
        Ok(c) => c,
        Err(p) => {
            fail(studio.as_ref(), &target, &p.message, Some(p.code), false).await;
            return Err(problem_error(&p));
        }
    };
    let music = makes == Makes::Music;
    let background = crate::tools::background(&input, matches!(makes, Makes::Video) || music)?;
    let what = launch_for(media, &compiled, format!("{tool}: {}", compiled.prompt.lines().next().unwrap_or_default()), &target)?;
    let (media, job) = (media.clone(), target.job.clone());
    let out = launch(&cx, background, what, move |cx| async move { run::run(&media, &studio, &target, extra, cx).await }).await;
    if let Some(id) = out.as_ref().ok().filter(|o| o.metadata["background"] == true).and_then(|o| o.metadata["job_id"].as_str()) {
        let _ = job.set(id.to_owned());
    }
    out
}

/// Without Silo: the call as asked, its files saved under `canvases/<name>/assets/`; it compiles as a card would
/// as far as the call goes: the style line added, and on video the motion line after it (both unless
/// `style: false`), unset sizes from the canvas's format.
async fn offline(media: &Arc<Media>, tool: &str, mut input: Value, cx: ToolCx, root: &Path, aim: &Aim) -> Result<ToolOutput, ToolError> {
    let canvas = aim.name.as_str();
    let dir: PathBuf = root.join("canvases").join(canvas).join("assets");
    let prompt = text(&input, "prompt").or_else(|| text(&input, "text")).or_else(|| text(&input["input"], "prompt")).unwrap_or_default().to_owned();
    let stem = save_stem(&dir, &prompt);
    input["path"] = json!(dir.join(stem).to_string_lossy());
    let named = crate::tools::named(media, tool).ok_or_else(|| ToolError::failed(format!("no tool {tool}")))?;
    let mut notes = Vec::new();
    let prompt = expand_offline(&mut input, &prompt, root, canvas, &cx, &mut notes);
    if matches!(tool, "generate_image" | "generate_video") {
        if aim.style && !prompt.is_empty() {
            let motion = if tool == "generate_video" { otto_tools::studio::motion_line(root) } else { None };
            let styled = compile::with_style(prompt.clone(), otto_tools::studio::style_line(root).as_deref(), motion.as_deref());
            if styled != prompt {
                input["prompt"] = json!(styled);
            }
        }
        if let Some(fields) = input.as_object_mut() {
            let schema = named.spec().input_schema.clone();
            let name = if tool == "generate_image" { "This image tool" } else { "This video tool" };
            compile::sizes(otto_tools::studio::format_of(root, canvas).as_ref(), &schema, fields, name, &mut notes);
        }
    }
    let mut out = named.call(input, cx).await?;
    notes.push(format!("The canvas wasn't updated (Silo isn't running); the file is saved under canvases/{canvas}/assets/."));
    let note = notes.join("\n");
    match out.content.iter_mut().find_map(|p| if let otto_tools::OutputPart::Text { text } = p { Some(text) } else { None }) {
        Some(text) => {
            text.push('\n');
            text.push_str(&note);
        }
        None => out.content.insert(0, otto_tools::OutputPart::Text { text: note }),
    }
    Ok(out)
}

/// Without Silo: the call's prompt with its mentions expanded as a card's would be, from the files on disk: an
/// entity's name, a node's label (read from `canvas.json`), numbered by where the call passes its file (its sheet or
/// a ref; the node's output) in `references`, `first_frame` or `last_frame`. Written back into the call; the
/// expanded prompt.
fn expand_offline(input: &mut Value, prompt: &str, root: &Path, canvas: &str, cx: &ToolCx, notes: &mut Vec<String>) -> String {
    use silo_proto::studio::mention::Target;
    if silo_proto::studio::mention::tokens(prompt).is_empty() {
        return prompt.to_owned();
    }
    // On disk a canvas has no name (its folder is).
    let board: Option<silo_proto::studio::Canvas> = otto_tools::studio::canvas_file(root, canvas).and_then(|mut c| {
        c["name"] = json!(canvas);
        serde_json::from_value(c).ok()
    });
    // The call's files, root-relative where they are under the studio.
    let rel = |raw: &str| {
        let resolved = otto_tools::policy::resolve_path(&cx.cwd, &cx.project_root, raw).ok()?;
        Some(resolved.path.strip_prefix(root).ok()?.to_string_lossy().into_owned())
    };
    let references: Vec<Option<String>> = input["references"].as_array().into_iter().flatten().map(|v| v.as_str().and_then(rel)).collect();
    let frame = |field: &str| input[field].as_str().and_then(rel);
    let (first, last) = (frame("first_frame"), frame("last_frame"));
    let place = |files: &[String]| {
        let has = |p: &Option<String>| p.as_ref().is_some_and(|p| files.contains(p));
        if let Some(n) = references.iter().position(has) {
            Some(compile::Placed::Numbered("image", n + 1))
        } else if has(&first) {
            Some(compile::Placed::Frame("first"))
        } else if has(&last) {
            Some(compile::Placed::Frame("last"))
        } else {
            None
        }
    };
    let expanded = compile::expand(
        prompt,
        |target| match target {
            Target::Entity(id) => {
                let e = otto_tools::studio::entity(root, id)?;
                let files: Vec<String> = e.sheet.iter().chain(&e.refs).cloned().collect();
                Some(compile::Expansion::new(e.name, place(&files)))
            }
            Target::Ref { entity, id } => {
                let e = otto_tools::studio::entity(root, entity)?;
                let look = e.look(id)?;
                let placed = place(std::slice::from_ref(&look.path));
                Some(compile::Expansion { label: format!("{}, {}", e.name, look.name), placed, defines: look.defines.clone() })
            }
            Target::Node(id) => {
                let board = board.as_ref()?;
                board.mention_label(target, "", &[])?;
                let files: Vec<String> = board.output_of(id, &[]).and_then(|o| o.path).into_iter().collect();
                Some(compile::Expansion::new(board.label_of(id, &[]), place(&files)))
            }
        },
        notes,
    );
    for field in ["prompt", "text"] {
        if input.get(field).is_some_and(Value::is_string) {
            input[field] = json!(expanded);
            return expanded;
        }
        if input["input"].get(field).is_some_and(Value::is_string) {
            input["input"][field] = json!(expanded);
            return expanded;
        }
    }
    expanded
}

fn save_stem(dir: &Path, prompt: &str) -> String {
    run::free_stem(dir, &crate::tools::save::slug(prompt, noun(MediaKind::Image)))
}
