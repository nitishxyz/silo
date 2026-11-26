//! The Studio tools (docs/STUDIO.md §6.1): finding the agent's own Studio
//! block, reading their input, and the compact outlines `studio_read` and
//! `canvas_read` give the model.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use silo_client::Control;
use silo_proto::{
    control::{BlockView, Created, Method, SpawnSpec},
    model::{AgentLink, Axis, BlockInfo, BlockKind, SessionSummary, SessionView},
    studio::{
        Approval, Canvas, CanvasEdited, CanvasInfo, EdgeRole, FrameRole, GenStatus, Node, NodeBody, Op, Production, Project, SectionKind,
        StudioCanvasCreateParams, StudioCanvasEditParams, StudioCanvasParams, StudioInitParams, StudioMedia, StudioProjectParams, StudioSetParams,
        StudioView,
    },
};

use crate::{Part, ToolResult, Tools};

/// The studio holding canonical folder `dir`: the nearest folder up from it
/// with a `studio.toml`, else `dir` itself.
#[must_use]
pub fn root_for(dir: &Path) -> PathBuf {
    dir.ancestors().find(|a| a.join("studio.toml").is_file()).unwrap_or(dir).to_owned()
}

/// A Studio block on `root` in `view`: the one linked to agent block
/// `agent` first, else any; its id, window and whether it is linked to
/// some agent.
#[must_use]
pub fn own_block(view: &SessionView, agent: Option<&str>, root: &Path) -> Option<(String, Option<String>, bool)> {
    let on_root: Vec<_> = view
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Studio)
        .filter_map(|b| b.studio.as_deref().filter(|s| Path::new(&s.root) == root).map(|s| (b, s)))
        .collect();
    let linked = |s: &silo_proto::studio::StudioState| agent.is_some() && s.agent.as_ref().and_then(|a| a.block_id.as_deref()) == agent;
    let (block, state) = on_root.iter().find(|(_, s)| linked(s)).or_else(|| on_root.first())?;
    Some((block.id.clone(), crate::files::window_of(view, &block.id).map(|w| w.id.clone()), state.agent.is_some()))
}

/// `raw` (absolute, `~/…`, or relative to `cwd`) as an absolute path.
fn absolute(raw: &str, cwd: Option<&Path>) -> anyhow::Result<PathBuf> {
    let path = if raw == "~" || raw.starts_with("~/") {
        let home = std::env::var_os("HOME").map(PathBuf::from).context("no home directory for ~")?;
        home.join(raw.trim_start_matches('~').trim_start_matches('/'))
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.context("path is relative and there is no working directory; give an absolute path")?.join(raw)
    };
    Ok(crate::files::normalize(&path))
}

fn text_result(text: &str) -> ToolResult {
    let (text, truncated) = crate::cap(text);
    ToolResult { content: vec![Part::Text(text)], is_error: false, metadata: json!({ "truncated": truncated }) }
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.downcast_ref::<silo_proto::control::RpcError>().is_some_and(|e| e.code == "not_found")
}

fn opt_str<'a>(input: &'a Value, key: &str) -> anyhow::Result<Option<&'a str>> {
    match &input[key] {
        Value::Null => Ok(None),
        v => Ok(Some(v.as_str().map(str::trim).filter(|s| !s.is_empty()).with_context(|| format!("{key} must be a non-empty string"))?)),
    }
}

impl Tools {
    /// The connection, for callers that make `studio.*` calls themselves.
    #[must_use]
    pub fn control(&self) -> &Control {
        &self.control
    }

    /// The Studio block for the studio holding `cwd` (nearest ancestor with `studio.toml`, else `cwd`): one linked
    /// to the `anchor` agent block, else one in its session on that root, else, with `open`, a new one split off
    /// the anchor (linked, without taking focus if the server allows). `None`: no such block and not opened.
    ///
    /// # Errors
    /// A server without Studio blocks, a `cwd` that doesn't exist, or a control failure.
    pub async fn studio_block(&self, anchor: Option<&str>, cwd: &Path, open: bool) -> anyhow::Result<Option<String>> {
        let root = self.studio_root(cwd).await?;
        let _lock = self.show_lock.lock().await;
        Ok(self.find_studio(anchor, &root, open, false).await?.map(|(id, _)| id))
    }

    /// The canonical studio root for folder (or file) `path`.
    async fn studio_root(&self, path: &Path) -> anyhow::Result<PathBuf> {
        if !self.control.peer_supports(silo_proto::compat::feature::STUDIO) {
            bail!("this Silo server has no Studio blocks (update it)");
        }
        let path = tokio::fs::canonicalize(path).await.with_context(|| format!("{} does not exist", path.display()))?;
        let dir =
            if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) { path } else { path.parent().map_or(path.clone(), Path::to_path_buf) };
        Ok(tokio::task::spawn_blocking(move || root_for(&dir)).await?)
    }

    /// The Studio block on `root` for `anchor` (see [`Self::studio_block`]);
    /// `focus` shows it (a found one is selected, a new one keeps focus).
    /// Its id and whether it was made now. Call with `show_lock` held.
    async fn find_studio(&self, anchor: Option<&str>, root: &Path, open: bool, focus: bool) -> anyhow::Result<Option<(String, bool)>> {
        let agent: Option<BlockInfo> = match anchor {
            Some(anchor) => self.control.call::<BlockView>(Method::BlockView { block_id: anchor.to_owned() }).await.ok().map(|v| v.block),
            None => None,
        };
        let link = agent.as_ref().and_then(|a| {
            let otto_session = a.agent.as_ref()?.session_id.clone()?;
            Some(AgentLink { otto_session, block_id: Some(a.id.clone()) })
        });
        let session_id = match &agent {
            Some(agent) => Some(agent.session_id.clone()),
            None => {
                let sessions: Vec<SessionSummary> = self.control.call(Method::SessionList {}).await?;
                let name = &self.cx.default_session;
                sessions.into_iter().find(|s| s.name == *name || s.id == *name).map(|s| s.id)
            }
        };
        if let Some(session_id) = &session_id {
            let view: SessionView = self.control.call(Method::SessionView { session_id: session_id.clone() }).await?;
            if let Some((block, window, linked)) = own_block(&view, agent.as_ref().map(|a| a.id.as_str()), root) {
                if focus {
                    if let Some(window_id) = window {
                        self.control.call_value(Method::WindowSelect { session_id: session_id.clone(), window_id }).await?;
                    }
                    self.control.call_value(Method::BlockFocus { block_id: block.clone() }).await?;
                }
                if !linked && let Some(link) = link {
                    let set = StudioSetParams { block_id: block.clone(), agent: Some(Some(link)), ..Default::default() };
                    let _ = self.control.call_value(Method::StudioSet(set)).await;
                }
                return Ok(Some((block, false)));
            }
        }
        if !open {
            return Ok(None);
        }
        let spawn = SpawnSpec { cwd: Some(root.to_string_lossy().into_owned()), ..Default::default() };
        let tab = |session_id: String, spawn: SpawnSpec| Method::WindowCreate {
            session_id,
            spawn,
            kind: BlockKind::Studio,
            url: None,
            background: !focus,
            ids: None,
        };
        let created: Created = match (&agent, session_id) {
            (Some(agent), _) => {
                let split = Method::BlockSplit {
                    block_id: agent.id.clone(),
                    axis: Axis::Horizontal,
                    spawn: spawn.clone(),
                    kind: BlockKind::Studio,
                    url: None,
                    agent: None,
                    ids: None,
                };
                match self.control.call::<Created>(split).await {
                    Ok(created) => {
                        // A split takes focus: give it back to the agent.
                        if !focus {
                            let _ = self.control.call_value(Method::BlockFocus { block_id: agent.id.clone() }).await;
                        }
                        created
                    }
                    // A floating or parked agent: a tab in its session.
                    Err(_) => self.control.call(tab(agent.session_id.clone(), spawn)).await?,
                }
            }
            (None, Some(session_id)) => self.control.call(tab(session_id, spawn)).await?,
            (None, None) => {
                let create = Method::SessionCreate { name: Some(self.cx.default_session.clone()), spawn, kind: BlockKind::Studio, url: None };
                self.control.call(create).await?
            }
        };
        // A split off an agent is linked by the server; a tab is not.
        if let Some(link) = link {
            let set = StudioSetParams { block_id: created.block_id.clone(), agent: Some(Some(link)), ..Default::default() };
            let _ = self.control.call_value(Method::StudioSet(set)).await;
        }
        Ok(Some((created.block_id, true)))
    }

    /// `studio_open`, `studio_read`, `canvas_read`, `canvas_create` and
    /// `canvas_edit`: on the agent's own Studio block for the studio
    /// holding `path` (else the input's `cwd`, set by hosts such as otto,
    /// else the context's), opened beside `anchor` when there is none.
    pub(crate) async fn studio_tool(&self, name: &str, input: &Value, anchor: Option<&str>) -> anyhow::Result<ToolResult> {
        let cwd = input["cwd"].as_str().map(PathBuf::from).or_else(|| self.cx.cwd.clone());
        let at = match opt_str(input, "path")? {
            Some(path) => absolute(path, cwd.as_deref())?,
            None => cwd.context("no working directory: give path (the studio's folder)")?,
        };
        let root = self.studio_root(&at).await?;
        let anchor = input["block_id"].as_str().or(anchor);
        let (block, created) = {
            let _lock = self.show_lock.lock().await;
            self.find_studio(anchor, &root, true, name == "studio_open").await?.context("no Studio block")?
        };
        let has_project = tokio::fs::metadata(root.join("studio.toml")).await.is_ok();
        let no_project = || {
            format!(
                "No studio.toml in {} yet: the Studio block shows Setup. Write studio.toml (name, format, [formats.<name>] presets) with your file tools, or ask the user to start from Setup; canvas_create starts one with the defaults.",
                root.display()
            )
        };
        let value = match name {
            "studio_open" => {
                let mut set = StudioSetParams { block_id: block.clone(), ..Default::default() };
                if let Some(canvas) = opt_str(input, "canvas")? {
                    set.view = Some(StudioView::Canvas);
                    set.active = Some(Some(canvas.to_owned()));
                } else if input["production"].as_bool() == Some(true) {
                    set.view = Some(StudioView::Production);
                } else if let Some(world) = opt_str(input, "world")? {
                    let world = world.trim_start_matches("world/").trim_matches('/');
                    let (section, entity) = match world.split_once('/') {
                        Some((section, _)) => (section, Some(world.to_owned())),
                        None => (world, None),
                    };
                    set.view = Some(StudioView::World);
                    set.world_section = Some(Some(section.to_owned()));
                    set.world_entity = Some(entity);
                }
                let state: silo_proto::studio::StudioState = self.control.call(Method::StudioSet(set)).await?;
                let mut out =
                    json!({"block_id": block, "root": root, "created": created, "view": state.view, "open": state.open, "active": state.active});
                if !has_project {
                    out["note"] = json!(no_project());
                }
                out
            }
            "studio_read" => {
                return match self.control.call::<Project>(Method::StudioProject(StudioProjectParams { block_id: block.clone() })).await {
                    Ok(project) => {
                        let mut docs = HashMap::new();
                        for c in &project.canvases {
                            docs.insert(c.name.clone(), self.documents(&block, &c.name).await);
                        }
                        // An older server has no studio.production: the outline without it.
                        let production =
                            self.control.call::<Production>(Method::StudioProduction(StudioProjectParams { block_id: block.clone() })).await.ok();
                        let mut text = project_outline(&project, &root, &docs);
                        if let Some(p) = &production {
                            // Read first: right after the name and formats.
                            let at = text.find("\nCanvases (").unwrap_or(text.len());
                            text.insert_str(at, &format!("\n{}", production_outline(p)));
                        }
                        Ok(text_result(&text))
                    }
                    Err(error) if is_not_found(&error) => Ok(text_result(&no_project())),
                    Err(error) => Err(error),
                };
            }
            "canvas_read" => {
                let canvas = opt_str(input, "canvas")?.context("canvas is required")?.to_owned();
                let frame = opt_str(input, "frame")?;
                let read: Canvas = self.control.call(Method::StudioCanvas(StudioCanvasParams { block_id: block.clone(), name: canvas })).await?;
                let project = self.control.call::<Project>(Method::StudioProject(StudioProjectParams { block_id: block.clone() })).await.ok();
                let docs = self.documents(&block, &read.name).await;
                return Ok(text_result(&canvas_outline(&read, project.as_ref(), frame, &docs)));
            }
            "canvas_create" => {
                let title = opt_str(input, "title")?.context("title is required")?.to_owned();
                let episode = match &input["episode"] {
                    Value::Null => None,
                    v => Some(v.as_u64().and_then(|e| u32::try_from(e).ok()).context("episode must be a positive integer")?),
                };
                let format = opt_str(input, "format")?.map(str::to_owned);
                let mut started = false;
                if !has_project {
                    let name = root.file_name().map_or_else(|| "Studio".to_owned(), |n| n.to_string_lossy().into_owned());
                    self.control.call_value(Method::StudioInit(StudioInitParams { block_id: block.clone(), name, format: None })).await?;
                    started = true;
                }
                let params = StudioCanvasCreateParams { block_id: block.clone(), title, episode, format, name: None };
                let info: CanvasInfo = self.control.call(Method::StudioCanvasCreate(params)).await?;
                let set = StudioSetParams {
                    block_id: block.clone(),
                    view: Some(StudioView::Canvas),
                    active: Some(Some(info.name.clone())),
                    ..Default::default()
                };
                self.control.call_value(Method::StudioSet(set)).await?;
                let mut out = json!({
                    "block_id": block, "canvas": info.name, "title": info.title, "episode": info.episode, "format": info.format,
                    "folder": format!("canvases/{}/", info.name), "assets": format!("canvases/{}/assets/", info.name),
                });
                if started {
                    out["note"] = json!("there was no studio.toml: started the studio with its folder's name and the default formats");
                }
                out
            }
            "canvas_edit" => {
                let canvas = opt_str(input, "canvas")?.context("canvas is required")?.to_owned();
                let ops: Vec<Op> = serde_json::from_value(input["ops"].clone()).map_err(|e| anyhow::anyhow!("ops: {e}"))?;
                if ops.is_empty() {
                    bail!("ops is empty");
                }
                let base_rev = input["base_rev"].as_u64();
                let params = StudioCanvasEditParams { block_id: block, name: canvas, ops, base_rev };
                let edited: CanvasEdited = self.control.call(Method::StudioCanvasEdit(params)).await?;
                serde_json::to_value(edited)?
            }
            other => bail!("unknown tool: {other}"),
        };
        Ok(ToolResult::json(value))
    }

    /// Canvas `canvas`'s documents (root-relative, `script.md` first), listed with `files.list` on the Studio
    /// block; none when the folder can't be listed.
    async fn documents(&self, block: &str, canvas: &str) -> Vec<String> {
        let list = Method::FilesList(silo_proto::files::FilesListParams { block_id: block.to_owned(), path: format!("canvases/{canvas}") });
        let Ok(list) = self.control.call::<silo_proto::files::FileList>(list).await else { return Vec::new() };
        let files = list.entries.into_iter().filter(|e| e.kind != silo_proto::files::FileKind::Dir).map(|e| e.name);
        silo_proto::studio::documents_of(canvas, files)
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{cut}…")
}

fn quoted(text: &str, max: usize) -> String {
    format!("\"{}\"", one_line(text, max).replace('"', "'"))
}

fn format_text(f: &silo_proto::studio::Format) -> String {
    let mut out = f.aspect.clone();
    if let Some(r) = &f.resolution {
        let _ = write!(out, " {r}");
    }
    if let Some(fps) = f.fps {
        let _ = write!(out, " {fps}fps");
    }
    out
}

/// `studio_read`'s text: the project, its formats, canvases (with their documents, by canvas name) and world.
#[must_use]
pub fn project_outline(project: &Project, root: &Path, docs: &HashMap<String, Vec<String>>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Studio {} at {} (paths below are relative to it)", quoted(&project.name, 200), root.display());
    let formats: Vec<String> = project.formats.iter().map(|(name, f)| format!("{name} {}", format_text(f))).collect();
    let _ = writeln!(out, "Formats (project default: {}): {}", project.format, if formats.is_empty() { "none".into() } else { formats.join(" · ") });
    let _ = writeln!(out, "\nCanvases ({}, in tab order):", project.canvases.len());
    if project.canvases.is_empty() {
        out.push_str("- none yet (canvas_create makes one)\n");
    }
    for c in &project.canvases {
        let mut line = format!("- {} {}", c.name, quoted(&c.title, 120));
        if let Some(ep) = c.episode {
            let _ = write!(line, " · Ep {ep}");
        }
        let _ = write!(line, " · format {}", c.format.as_deref().map_or_else(|| format!("{} (project default)", project.format), str::to_owned));
        let _ = write!(line, " · {} nodes · rev {} · canvases/{}/", c.nodes, c.rev, c.name);
        if let Some(d) = docs.get(&c.name).filter(|d| !d.is_empty()) {
            let _ = write!(line, " · documents {}", d.join(", "));
        }
        let _ = writeln!(out, "{line}");
    }
    out.push_str("\nWorld (world/; plain files: edit them with read/write/edit, make sheets with generate_image {path: \"world/<section>/<entity>/sheet.png\"}; refs are listed by id, and @<section>/<entity>#<id> in a card's prompt mentions one):\n");
    for section in &project.sections {
        match section.kind {
            SectionKind::Overview => {
                let _ = writeln!(out, "Overview: world/overview.md");
            }
            SectionKind::Style => {
                let _ = writeln!(out, "Style: world/style.md (reference images: world/style/)");
            }
            _ => {
                let _ = writeln!(out, "{} (world/{}/, {}):", section.title, section.id, section.entities.len());
                for e in &section.entities {
                    let mut line = format!("- {} {}", e.id, quoted(&e.name, 120));
                    if let Some(s) = &e.summary {
                        let _ = write!(line, " — {}", one_line(s, 300));
                    }
                    let _ = write!(line, " · file {}", e.file);
                    let _ = write!(line, " · sheet {}", e.sheet.as_deref().unwrap_or("none"));
                    if !e.looks.is_empty() {
                        // By id (what `@<entity>#<id>` mentions), with the name when it isn't the file's stem.
                        let looks: Vec<String> = e
                            .looks
                            .iter()
                            .map(|l| {
                                let mut item = l.id.clone();
                                if l.name != silo_proto::studio::file_stem(&l.path) {
                                    let _ = write!(item, " {}", quoted(&l.name, 80));
                                }
                                let _ = write!(item, " {}", l.path);
                                if let Some(d) = &l.defines {
                                    let _ = write!(item, " defines {}", quoted(d, 160));
                                }
                                item
                            })
                            .collect();
                        let _ = write!(line, " · refs {}", looks.join("; "));
                    } else if !e.refs.is_empty() {
                        let _ = write!(line, " · refs {}", e.refs.join(", "));
                    }
                    if !e.links.is_empty() {
                        let _ = write!(line, " · links {}", e.links.join(", "));
                    }
                    if !e.tags.is_empty() {
                        let _ = write!(line, " · tags {}", e.tags.join(", "));
                    }
                    let _ = writeln!(out, "{line}");
                }
            }
        }
    }
    out
}

/// `studio_read`'s Production block: each asset's marks, each canvas's stages, the next steps.
#[must_use]
pub fn production_outline(p: &Production) -> String {
    let mut out = String::from("Production (derived from the files; an approved sheet is final: never regenerate or overwrite it):\n");
    let mark = |done: bool, what: &str| format!("{what} {}", if done { "✓" } else { "✗" });
    if p.assets.is_empty() {
        out.push_str("Assets: none yet\n");
    } else {
        out.push_str("Assets:\n");
    }
    for a in &p.assets {
        let mut marks = vec![mark(a.sheet.is_some(), "sheet")];
        marks.push(
            match a.approval {
                Approval::Approved => "approved ✓",
                Approval::Changed => "CHANGED since approved",
                _ if a.sheet.is_some() => "not approved",
                _ => "",
            }
            .to_owned(),
        );
        if let Some(v) = a.voice {
            marks.push(mark(v, "voice"));
        }
        if let Some(v) = a.video_sheet {
            marks.push(mark(v, "video sheet"));
        }
        marks.retain(|m| !m.is_empty());
        let used = match a.used {
            0 => "unused".to_owned(),
            1 => "on 1 canvas".to_owned(),
            n => format!("on {n} canvases"),
        };
        let _ = writeln!(out, "- {} {}: {} · {used}", a.id, quoted(&a.name, 80), marks.join(" · "));
    }
    if p.episodes.is_empty() {
        out.push_str("Episodes: none yet\n");
    } else {
        out.push_str("Episodes and canvases:\n");
    }
    for e in &p.episodes {
        let mut line = format!("- {}", e.canvas);
        if let Some(n) = e.episode {
            let _ = write!(line, " Ep {n}");
        }
        let _ = write!(line, " {}: {}", quoted(&e.title, 80), mark(e.script, "script"));
        let _ = write!(line, " · {} shots · keyframes {}/{} · video {}/{} · sound {}", e.shots, e.keyframes, e.shots, e.videos, e.shots, e.sound);
        match &e.cut {
            Some(cut) => {
                let _ = write!(line, " · cut v{} ({}){}", cut.version, cut.path, if cut.stale { ", older than a picked take" } else { "" });
            }
            None => line.push_str(" · no cut"),
        }
        for (n, what) in [(e.out_of_date, "out of date"), (e.failed, "failed"), (e.running, "running")] {
            if n > 0 {
                let _ = write!(line, " · {n} {what}");
            }
        }
        let _ = writeln!(out, "{line}");
    }
    if !p.models.is_empty() {
        let picks: Vec<String> = p.models.iter().map(|(k, v)| format!("{k} {v}")).collect();
        let _ = writeln!(out, "Model picks ([models]): {}", picks.join(" · "));
    }
    if p.next.is_empty() {
        out.push_str("Next: nothing outstanding\n");
    } else {
        out.push_str("Next:\n");
    }
    for (i, s) in p.next.iter().enumerate() {
        let on = s.entity.as_deref().or(s.canvas.as_deref()).map(|t| format!(" [{t}]")).unwrap_or_default();
        let whose = if s.ask.is_none() { " (the user's to decide)" } else { "" };
        let _ = writeln!(out, "{}. {}{on}{whose}", i + 1, s.text);
    }
    out
}

fn media_name(m: StudioMedia) -> &'static str {
    match m {
        StudioMedia::Image => "image",
        StudioMedia::Video => "video",
        StudioMedia::Audio => "audio",
        StudioMedia::Text => "document",
        StudioMedia::Unknown => "media",
    }
}

fn status_name(s: GenStatus) -> &'static str {
    match s {
        GenStatus::Draft => "draft (never run)",
        GenStatus::Queued => "queued",
        GenStatus::Running => "running",
        GenStatus::Done => "done",
        GenStatus::Failed => "failed",
        GenStatus::Stopped => "stopped",
        GenStatus::Unknown => "unknown",
    }
}

fn role_name(r: EdgeRole) -> String {
    serde_json::to_value(r).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "unknown".into())
}

/// A card's mentions (the tokens of its prompt that resolve) with what each names and whether it is wired:
/// ` · mentions @characters/mara = Mara Venn (wired reference from n_ab), @#n_3 = "dock.png" (not wired)`; a named
/// reference with its file and job: `@characters/mara#coat = "Mara Venn, Coat" (world/…/coat.png, defines "the
/// coat"; wired reference from n_cd)`.
fn mentions_text(canvas: &Canvas, card: &str, sections: &[silo_proto::studio::Section]) -> String {
    let list: Vec<String> = canvas
        .mentions_of(card, sections)
        .into_iter()
        .map(|(target, label)| {
            let wires: Vec<String> = canvas
                .inputs_of(card)
                .into_iter()
                .filter(|e| canvas.mention_names(&target, &e.from, sections))
                .map(|e| format!("{} from {}", role_name(e.role), e.from))
                .collect();
            let mut wired = if wires.is_empty() { "not wired".to_owned() } else { format!("wired {}", wires.join(", ")) };
            if let Some((_, look)) = target.look(sections) {
                let job = look.defines.as_deref().map(|d| format!(", defines {}", quoted(d, 120))).unwrap_or_default();
                wired = format!("{}{job}; {wired}", look.path);
            }
            format!("{} = {} ({wired})", target.token(), quoted(&label, 80))
        })
        .collect();
    if list.is_empty() { String::new() } else { format!(" · mentions {}", list.join(", ")) }
}

/// One node's line (no indent, no newline).
fn node_line(node: &Node, out_of_date: Option<&[String]>) -> String {
    let mut line = format!("{} ", node.id);
    match &node.body {
        NodeBody::Asset { path, media, width, height, seconds, prompt } => {
            let _ = write!(line, "{} {path}", media_name(*media));
            if let (Some(w), Some(h)) = (width, height) {
                let _ = write!(line, " {w}x{h}");
            }
            if let Some(s) = seconds {
                let _ = write!(line, " {s:.1}s");
            }
            if let Some(p) = prompt {
                let _ = write!(line, " · prompt {}", quoted(p, 600));
            }
        }
        NodeBody::Note { text } => {
            let _ = write!(line, "note {}", quoted(text, 600));
        }
        NodeBody::Entity { id } => {
            let _ = write!(line, "entity {id}");
        }
        body @ NodeBody::Gen { makes, title, prompt, model, mode, options, status, progress, job, error, error_code, takes, pick, .. } => {
            let _ = write!(line, "card {}", makes.label().to_lowercase());
            if let Some(t) = title {
                let _ = write!(line, " {}", quoted(t, 120));
            }
            let _ = write!(line, " · {}", model.as_deref().unwrap_or("model automatic"));
            if let Some(m) = mode {
                let _ = write!(line, " ({m})");
            }
            let _ = write!(line, " · {}", status_name(*status));
            if let Some(p) = progress {
                let _ = write!(line, " {}", one_line(p, 60));
            }
            if let Some(why) = out_of_date {
                let why = if why.is_empty() { "its inputs changed since the picked take".to_owned() } else { why.join("; ") };
                let _ = write!(line, " · OUT OF DATE ({why})");
            }
            if let Some(j) = job {
                let _ = write!(line, " · job {j}");
            }
            if let Some(e) = error {
                let code = error_code.as_deref().map(|c| format!(" [{c}]")).unwrap_or_default();
                let _ = write!(line, " · error{code} {}", quoted(e, 300));
            }
            let _ = write!(line, " · prompt {}", if prompt.is_empty() { "(empty)".into() } else { quoted(prompt, 600) });
            if options.as_object().is_some_and(|o| !o.is_empty()) {
                let _ = write!(line, " · options {}", one_line(&options.to_string(), 200));
            }
            if !takes.is_empty() {
                let picked = body.take().map(|t| t.path.as_str());
                let list: Vec<String> = takes
                    .iter()
                    .enumerate()
                    .map(|(i, t)| format!("{i}{} {}", if Some(t.path.as_str()) == picked { "*" } else { "" }, t.path))
                    .collect();
                let _ = write!(
                    line,
                    " · pick {} (0-based, of {} takes): {}",
                    pick.map_or(takes.len() - 1, |p| p as usize),
                    takes.len(),
                    picked.unwrap_or("none")
                );
                let _ = write!(line, " · takes (* picked) {}", list.join(", "));
            }
        }
        NodeBody::Frame { .. } => {}
        NodeBody::Unknown => line.push_str("(a node kind this version doesn't know)"),
    }
    line
}

fn frame_head(node: &Node) -> String {
    let NodeBody::Frame { title, role, order, format } = &node.body else { return node_line(node, None) };
    let role = match role {
        FrameRole::Scene => "scene",
        FrameRole::Shot => "shot",
        FrameRole::Group => "group",
        FrameRole::Unknown => "frame",
    };
    let mut line = format!("{} frame {role}", node.id);
    if let Some(o) = order {
        let _ = write!(line, " {o}");
    }
    let _ = write!(line, " {}", quoted(title, 120));
    if let Some(f) = format {
        let _ = write!(line, " · format {f}");
    }
    line
}

/// How many frames overlap another frame in the same frame (or at the top level).
fn overlapping_frames(canvas: &Canvas) -> usize {
    let frames: Vec<&Node> = canvas.nodes.iter().filter(|n| matches!(n.body, NodeBody::Frame { .. })).collect();
    let hit =
        |a: &Node, b: &Node| a.parent == b.parent && a.x + a.w > b.x + 0.5 && b.x + b.w > a.x + 0.5 && a.y + a.h > b.y + 0.5 && b.y + b.h > a.y + 0.5;
    frames.iter().enumerate().filter(|&(i, a)| frames.iter().enumerate().any(|(j, b)| i != j && hit(a, b))).count()
}

/// Reading order: frames by `order`, then top to bottom and left to right.
fn reading_key(node: &Node) -> (u32, i64, i64) {
    let order = match &node.body {
        NodeBody::Frame { order: Some(o), .. } => *o,
        _ => u32::MAX,
    };
    (order, (node.y / 40.0).floor() as i64, node.x as i64)
}

/// `canvas_read`'s text: the canvas folder's documents `docs` (root-relative; whether each is pinned), frames in
/// order with their nodes nested, nodes outside frames, then the edges; `frame` limits it to that frame.
#[must_use]
pub fn canvas_outline(canvas: &Canvas, project: Option<&Project>, frame: Option<&str>, docs: &[String]) -> String {
    let mut out = String::new();
    let mut head = format!("Canvas {} {}", canvas.name, quoted(&canvas.title, 120));
    if let Some(ep) = canvas.episode {
        let _ = write!(head, " · Ep {ep}");
    }
    match project.and_then(|p| p.format_for(canvas, None)) {
        Some((name, f)) => {
            let _ = write!(head, " · format {name} ({}){}", format_text(&f), if canvas.format.is_none() { " from the project" } else { "" });
        }
        None => {
            if let Some(f) = &canvas.format {
                let _ = write!(head, " · format {f}");
            }
        }
    }
    let _ = write!(
        head,
        " · rev {} · {} nodes, {} edges · assets in canvases/{}/assets/",
        canvas.rev,
        canvas.nodes.len(),
        canvas.edges.len(),
        canvas.name
    );
    let _ = writeln!(out, "{head}");
    let overlaps = overlapping_frames(canvas);
    if overlaps > 0 {
        let _ = writeln!(out, "Layout: {overlaps} frames overlap; run canvas_edit {{op: tidy}}");
    }
    if !docs.is_empty() {
        let _ = writeln!(out, "Documents (canvases/{}/):", canvas.name);
        for d in docs {
            let pinned: Vec<&str> = canvas.nodes.iter().filter(|n| canvas.document_of(&n.id) == Some(d.as_str())).map(|n| n.id.as_str()).collect();
            let on = if pinned.is_empty() { "not on the canvas".to_owned() } else { format!("on the canvas as {}", pinned.join(", ")) };
            let _ = writeln!(out, "- {d} · {on}");
        }
    }
    let ids: HashSet<&str> = canvas.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut children: HashMap<Option<&str>, Vec<&Node>> = HashMap::new();
    for n in &canvas.nodes {
        // A parent that is gone: shown at the top.
        let parent = n.parent.as_deref().filter(|p| ids.contains(p) && *p != n.id);
        children.entry(parent).or_default().push(n);
    }
    for list in children.values_mut() {
        list.sort_by_key(|n| reading_key(n));
    }
    let mut shown: HashSet<String> = HashSet::new();
    let stale: HashMap<&str, Vec<String>> =
        canvas.out_of_date.iter().map(|id| (id.as_str(), canvas.out_of_date_why.get(id).cloned().unwrap_or_default())).collect();
    let sections = project.map_or(&[][..], |p| p.sections.as_slice());
    let walk_cx = Walk { children: &children, stale: &stale, canvas, sections };
    struct Walk<'a, 'b> {
        children: &'b HashMap<Option<&'a str>, Vec<&'a Node>>,
        stale: &'b HashMap<&'b str, Vec<String>>,
        canvas: &'b Canvas,
        sections: &'b [silo_proto::studio::Section],
    }
    fn walk<'a>(out: &mut String, cx: &Walk<'a, '_>, node: &'a Node, depth: usize, shown: &mut HashSet<String>) {
        if !shown.insert(node.id.clone()) {
            return;
        }
        let indent = "  ".repeat(depth);
        if matches!(node.body, NodeBody::Frame { .. }) {
            let _ = writeln!(out, "{indent}{}", frame_head(node));
            for child in cx.children.get(&Some(node.id.as_str())).into_iter().flatten() {
                walk(out, cx, child, depth + 1, shown);
            }
        } else {
            let mut line = node_line(node, cx.stale.get(node.id.as_str()).map(Vec::as_slice));
            if matches!(node.body, NodeBody::Gen { .. }) {
                line.push_str(&mentions_text(cx.canvas, &node.id, cx.sections));
            }
            let _ = writeln!(out, "{indent}{line}");
        }
    }
    match frame.and_then(|f| canvas.nodes.iter().find(|n| n.id == f)) {
        Some(root) => walk(&mut out, &walk_cx, root, 0, &mut shown),
        None => {
            if let Some(f) = frame {
                let _ = writeln!(out, "(no frame {f}; the whole canvas follows)");
            }
            let top = children.get(&None).cloned().unwrap_or_default();
            let (frames, loose): (Vec<&Node>, Vec<&Node>) = top.into_iter().partition(|n| matches!(n.body, NodeBody::Frame { .. }));
            for f in frames {
                walk(&mut out, &walk_cx, f, 0, &mut shown);
            }
            if !loose.is_empty() {
                out.push_str("Outside frames:\n");
                for n in loose {
                    walk(&mut out, &walk_cx, n, 1, &mut shown);
                }
            }
        }
    }
    if canvas.nodes.is_empty() {
        out.push_str("(empty)\n");
    }
    // Wires card by card in their input order (port, then as made), then the rest.
    let mut ordered: Vec<&silo_proto::studio::Edge> = canvas.nodes.iter().flat_map(|n| canvas.inputs_of(&n.id)).collect();
    let wired: HashSet<&str> = ordered.iter().map(|e| e.id.as_str()).collect();
    ordered.extend(canvas.edges.iter().filter(|e| !wired.contains(e.id.as_str())));
    let edges: Vec<_> = ordered.into_iter().filter(|e| frame.is_none() || shown.contains(&e.from) || shown.contains(&e.to)).collect();
    if !edges.is_empty() {
        out.push_str("Wires (source → card port) and other edges:\n");
        for e in edges {
            let label = e.label.as_deref().map(|l| format!(" {}", quoted(l, 80))).unwrap_or_default();
            let _ = writeln!(out, "  {} → {} ({}){label} [{}]", e.from, e.to, role_name(e.role), e.id);
        }
    }
    out
}

/// Each node's kind count, for short results.
#[must_use]
pub fn counts(canvas: &Canvas) -> BTreeMap<&'static str, usize> {
    let mut out = BTreeMap::new();
    for n in &canvas.nodes {
        let kind = match n.body {
            NodeBody::Asset { .. } => "asset",
            NodeBody::Note { .. } => "note",
            NodeBody::Entity { .. } => "entity",
            NodeBody::Gen { .. } => "gen",
            NodeBody::Frame { .. } => "frame",
            NodeBody::Unknown => "unknown",
        };
        *out.entry(kind).or_insert(0) += 1;
    }
    out
}
