//! Running a card (docs/STUDIO.md §6.2 "Running"): the one path every take is made by. The card goes `queued` →
//! `running` (progress at most once a second) → `done` with its new takes in `canvases/<name>/assets/`, or
//! `failed`/`stopped`, all written with `update_node` ops through the [`Studio`] bridge.
use super::compile::{Compiled, Problem, Setup, compile};
use crate::Media;
use crate::tools::{noun, save};
use otto_tools::studio::{Card, Studio};
use otto_tools::{OutputPart, ToolCx, ToolError, ToolErrorKind, ToolOutput};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A card on a canvas of a Studio block.
#[derive(Clone, Debug)]
pub struct Target {
    pub block: String,
    pub canvas: String,
    pub node: String,
    /// The session job running it (`canvas_run`, a background `generate_* {canvas}`), once known: written to the
    /// card's `job` with `running`.
    pub job: Arc<std::sync::OnceLock<String>>,
}

impl Target {
    pub fn new(block: impl Into<String>, canvas: impl Into<String>, node: impl Into<String>) -> Self {
        Self { block: block.into(), canvas: canvas.into(), node: node.into(), job: Arc::default() }
    }
}

/// Unix ms.
fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Merge `body` into the card's body.
pub(crate) async fn update(studio: &dyn Studio, target: &Target, body: Value) -> Result<(), ToolError> {
    let edited = studio.edit(&target.block, &target.canvas, vec![json!({"op": "update_node", "id": target.node, "body": body})]).await?;
    match edited.failed.first() {
        Some(why) => Err(ToolError::failed(format!("can't update card {}: {why}", target.node))),
        None => Ok(()),
    }
}

/// Mark the card failed with `message` and `code` (`CardProblem`'s, `failed` for the provider's), or stopped when
/// cancelled; errors writing it are logged.
pub(crate) async fn fail(studio: &dyn Studio, target: &Target, message: &str, code: Option<&str>, stopped: bool) {
    let status = if stopped { "stopped" } else { "failed" };
    let (error, code) = if stopped { (Value::Null, Value::Null) } else { (json!(message), json!(code)) };
    let body = json!({"status": status, "error": error, "error_code": code, "progress": null, "job": null});
    if let Err(e) = update(studio, target, body).await {
        tracing::warn!("can't mark card {} {status}: {}", target.node, e.message);
    }
}

// The cards this process runs, persisted so a restarted daemon marks them stopped.

fn runs_file(media: &Media) -> PathBuf {
    media.state_dir().join("studio-runs.json")
}

fn key(root: &Path, canvas: &str, node: &str) -> String {
    format!("{}|{canvas}|{node}", root.display())
}

/// The runs file: the cards running here, and those a previous process left that aren't marked yet.
fn persist(media: &Media, runs: &std::collections::HashMap<String, CancellationToken>) {
    let orphans = media.orphans.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let mut list: Vec<&String> = runs.keys().chain(&orphans).collect();
    list.dedup();
    let path = runs_file(media);
    let _ = std::fs::create_dir_all(media.state_dir());
    let tmp = path.with_extension("json.tmp");
    let written =
        serde_json::to_vec(&list).map_err(std::io::Error::other).and_then(|b| std::fs::write(&tmp, b)).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        tracing::warn!("can't save the running studio cards: {e}");
    }
}

/// Holds a card's run registration; dropping it ends the registration.
struct Running<'a> {
    media: &'a Media,
    key: String,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut runs = self.media.runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        runs.remove(&self.key);
        persist(self.media, &runs);
    }
}

fn register<'a>(media: &'a Media, key: String, token: CancellationToken) -> Result<Running<'a>, ToolError> {
    let mut runs = media.runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if runs.contains_key(&key) {
        return Err(ToolError::invalid("This card is already running"));
    }
    runs.insert(key.clone(), token);
    persist(media, &runs);
    Ok(Running { media, key })
}

/// Stop the card's run in this process; `false`: it isn't running here.
pub(crate) fn cancel(media: &Media, root: &Path, canvas: &str, node: &str) -> bool {
    let runs = media.runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    runs.get(&key(root, canvas, node)).inspect(|t| t.cancel()).is_some()
}

/// Cards a previous process was running: `(root, canvas, node)`. They stay in the runs file (as orphans) until
/// [`set_orphans`] says they are dealt with, so a daemon restarted before Silo answers still knows them.
pub(crate) fn left_running(media: &Media) -> Vec<(PathBuf, String, String)> {
    let list: Vec<String> = std::fs::read(runs_file(media)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let running: Vec<String> = media.runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).keys().cloned().collect();
    let list: Vec<String> = list.into_iter().filter(|k| !running.contains(k)).collect();
    media.orphans.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone_from(&list);
    list.into_iter()
        .filter_map(|k| {
            let mut parts = k.rsplitn(3, '|');
            let (node, canvas, root) = (parts.next()?, parts.next()?, parts.next()?);
            Some((PathBuf::from(root), canvas.to_owned(), node.to_owned()))
        })
        .collect()
}

/// The orphans still to mark: `left`; the runs file follows.
pub(crate) fn set_orphans(media: &Media, left: &[(PathBuf, String, String)]) {
    *media.orphans.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = left.iter().map(|(r, c, n)| key(r, c, n)).collect();
    let runs = media.runs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    persist(media, &runs);
}

/// `card`'s prompt with each mention as its label (`@characters/mara` → `Mara Venn`), for naming its files.
pub(crate) fn with_labels(card: &Card) -> String {
    let mut out = String::with_capacity(card.prompt.len());
    let mut at = 0;
    for token in silo_proto::studio::mention::tokens(&card.prompt) {
        let written = token.target.token();
        let Some(m) = card.mentions.iter().find(|m| m.token == written) else { continue };
        out.push_str(&card.prompt[at..token.range.start]);
        out.push_str(&m.label);
        at = token.range.end;
    }
    out.push_str(&card.prompt[at..]);
    out
}

/// A free stem in `dir` for `slug`: no file there is named `<stem>.*` or `<stem>-<n>.*`, nor reserved by a run.
pub(crate) fn free_stem(dir: &Path, slug: &str) -> String {
    static RESERVED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> = std::sync::LazyLock::new(Default::default);
    let names: Vec<String> =
        std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str().map(str::to_owned)).collect();
    let taken = |stem: &str| {
        names.iter().any(|n| {
            let base = n.rsplit_once('.').map_or(n.as_str(), |(b, _)| b);
            base == stem
                || base.strip_prefix(stem).and_then(|r| r.strip_prefix('-')).is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
        })
    };
    let mut reserved = RESERVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let stem = (1..)
        .map(|i| if i == 1 { slug.to_owned() } else { format!("{slug}-{i}") })
        .find(|s| !taken(s) && !reserved.contains(&dir.join(s)))
        .unwrap_or_default();
    // Kept for this process's life: cheap, and a take written later can't be overwritten by a twin run.
    reserved.insert(dir.join(&stem));
    stem
}

/// The card, waiting while a wired card is queued or running.
async fn ready_card(studio: &dyn Studio, target: &Target, cancel: &CancellationToken) -> Result<Card, ToolError> {
    loop {
        let card = studio.card(&target.block, &target.canvas, &target.node).await?;
        if !card.inputs.iter().any(|i| i.pending) {
            return Ok(card);
        }
        tokio::select! {
            () = cancel.cancelled() => return Err(ToolError::new(ToolErrorKind::Cancelled, "cancelled")),
            () = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
        }
    }
}

/// A compile problem as a tool error.
pub(crate) fn problem_error(p: &Problem) -> ToolError {
    let kind = if p.code == "no_provider" { ToolErrorKind::Failed } else { ToolErrorKind::InvalidInput };
    ToolError::new(kind, p.message.clone())
}

/// Run card `target` with `cx` (a session's, or a bare one for the app's Run button: then nothing is kept as an
/// attachment and no question can be asked). `extra`: request fields that aren't wires (`compile`). Returns the
/// generation's result with `Placed on canvas … as …`; the card holds the state either way.
pub(crate) async fn run(
    media: &Arc<Media>,
    studio: &Arc<dyn Studio>,
    target: &Target,
    extra: Map<String, Value>,
    cx: ToolCx,
) -> Result<ToolOutput, ToolError> {
    let card = studio.card(&target.block, &target.canvas, &target.node).await?;
    let token = cx.cancel.child_token();
    let _running = match register(media, key(&card.root, &target.canvas, &target.node), token.clone()) {
        Ok(r) => r,
        Err(e) => return Err(e),
    };
    match run_registered(media, studio.as_ref(), target, extra, cx, token).await {
        Ok(output) => Ok(output),
        Err((e, code)) => {
            fail(studio.as_ref(), target, &e.message, Some(code.unwrap_or("failed")), e.kind == ToolErrorKind::Cancelled).await;
            Err(e)
        }
    }
}

async fn run_registered(
    media: &Arc<Media>,
    studio: &dyn Studio,
    target: &Target,
    extra: Map<String, Value>,
    cx: ToolCx,
    token: CancellationToken,
) -> Result<ToolOutput, (ToolError, Option<&'static str>)> {
    update(studio, target, json!({"status": "queued", "error": null, "error_code": null, "progress": null})).await.map_err(|e| (e, None))?;
    let card = ready_card(studio, target, &token).await.map_err(|e| (e, None))?;
    let setup = Setup::read(&card.root);
    let compiled = compile(media, &card, &setup, &extra).await.map_err(|p| (problem_error(&p), Some(p.code)))?;
    take(media, studio, target, &card, compiled, cx, token).await.map_err(|e| (e, None))
}

/// Run `compiled` for `card`: the generation through its tool, its files saved as new takes.
async fn take(
    media: &Arc<Media>,
    studio: &dyn Studio,
    target: &Target,
    card: &Card,
    compiled: Compiled,
    cx: ToolCx,
    token: CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let tool = crate::tools::named(media, compiled.tool).ok_or_else(|| ToolError::failed(format!("no tool {}", compiled.tool)))?;
    let dir = card.root.join("canvases").join(&card.canvas).join("assets");
    // Named from the card's own prompt (the style line and wired texts would make every name alike).
    let named = if card.prompt.trim().is_empty() { compiled.prompt.clone() } else { with_labels(card) };
    let stem = free_stem(&dir, &save::slug(&named, noun(compiled.kind)));
    let mut input = compiled.input.clone();
    input["path"] = json!(format!("canvases/{}/assets/{stem}", card.canvas));
    input["background"] = json!(false);
    update(studio, target, json!({"status": "running", "progress": null, "job": target.job.get(), "tool": compiled.tool, "input": compiled.input}))
        .await?;
    // The provider's progress onto the card, at most once a second, and on to the session's call.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<otto_tools::Progress>();
    let forward = cx.progress.clone();
    let progress = {
        let studio_target = target.clone();
        async move {
            let mut last: Option<std::time::Instant> = None;
            while let Some(p) = rx.recv().await {
                if let Some(f) = &forward {
                    let _ = f.send(p.clone());
                }
                let Some(message) = p.message else { continue };
                if last.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1)) {
                    continue;
                }
                last = Some(std::time::Instant::now());
                let _ = update(studio, &studio_target, json!({"progress": message})).await;
            }
        }
    };
    let mut inner = cx.clone();
    inner.jobs = None;
    inner.cwd.clone_from(&card.root);
    inner.project_root.clone_from(&card.root);
    inner.cancel = token;
    inner.progress = Some(tx);
    // The progress task ends when the call drops its sender.
    let (output, ()) = tokio::join!(tool.call(input, inner), progress);
    let mut output = output?;
    if output.is_error {
        return Err(ToolError::failed(output.joined_text()));
    }
    let made = output.metadata["media"].as_array().cloned().unwrap_or_default();
    let model = output.metadata["model"].as_str().map(str::to_owned);
    let media_name = match compiled.kind {
        crate::MediaKind::Image => "image",
        crate::MediaKind::Video => "video",
        crate::MediaKind::Audio => "audio",
    };
    let mut takes = card.takes.clone();
    let first = takes.len();
    for item in &made {
        let Some(path) = item["path"].as_str() else { continue };
        let rel = Path::new(path).strip_prefix(&card.root).map_or_else(|_| path.to_owned(), |p| p.to_string_lossy().into_owned());
        let mut one = json!({"path": rel, "media": media_name, "prompt": compiled.prompt, "at": now()});
        for (from, to) in [("width", "width"), ("height", "height"), ("duration", "seconds")] {
            if let Some(v) = item.get(from).filter(|v| !v.is_null()) {
                one[to] = v.clone();
            }
        }
        if let Some(m) = &model {
            one["model"] = json!(m);
        }
        takes.push(one);
    }
    if takes.len() == first {
        return Err(ToolError::failed("The generation returned no file to keep"));
    }
    update(
        studio,
        target,
        json!({"status": "done", "progress": null, "job": null, "error": null, "error_code": null, "takes": takes, "pick": first, "inputs_used": card.keys}),
    )
    .await?;
    let placed = format!("Placed on canvas {} as {}", target.canvas, target.node);
    match output.content.iter_mut().find_map(|p| if let OutputPart::Text { text } = p { Some(text) } else { None }) {
        Some(text) => {
            text.push('\n');
            text.push_str(&placed);
            for note in &compiled.notes {
                text.push('\n');
                text.push_str(note);
            }
        }
        None => output.content.insert(0, OutputPart::Text { text: placed }),
    }
    output.metadata["canvas"] = json!({"name": target.canvas, "node": target.node, "block": target.block});
    Ok(output)
}
