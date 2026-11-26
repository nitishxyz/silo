//! Canvases (docs/STUDIO.md §1.2, §3, §5): `canvases/<name>/canvas.json`,
//! written by the server only (atomically, under a per-canvas lock, `rev`
//! bumped per write), and `studio.canvas_edit`'s ops.
//!
//! Reading is lenient: a node whose body doesn't parse (a newer peer's
//! kind, a hand edit) is kept as it is and written back unchanged; one
//! without an id gets one. A file edited outside the server (an agent's
//! file tools) reads with a rev above the last one the server saw.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{LazyLock, Mutex},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use silo_proto::{
    control::RpcError,
    studio::{
        Arrange, Canvas, CanvasEdited, CanvasInfo, Edge, FrameRole, GenKind, GenStatus, NewEdge, NewNode, Node, NodeBody, Op, OpFailed, Project,
        Section, StudioMedia,
        size::{self, CARD_GAP, GAP, OUTPUT_GUTTER, PORT_GUTTER},
    },
};

use super::{disk, media, project, world};
use crate::files::paths;

mod mention;

pub const FILE: &str = "canvas.json";
/// Most nodes, edges and ops per call.
const NODES_MAX: usize = 20_000;
const OPS_MAX: usize = 2_000;

/// Placement, canvas units (cards' sizes and the gaps: [`size`]): room inside a frame, its title.
const PAD: f64 = 24.0;
const HEAD: f64 = 44.0;
/// The long side of an asset's face.
const MEDIA: f64 = 320.0;

#[derive(Serialize, Deserialize)]
struct File {
    #[serde(default = "one")]
    version: u32,
    #[serde(default)]
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    episode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    #[serde(default)]
    rev: u64,
    #[serde(default)]
    nodes: Vec<Value>,
    #[serde(default)]
    edges: Vec<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

fn one() -> u32 {
    1
}

/// A node as read before its body is checked.
#[derive(Deserialize)]
struct LooseNode {
    #[serde(default)]
    id: String,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
    #[serde(default)]
    w: f64,
    #[serde(default)]
    h: f64,
    #[serde(default)]
    z: Option<i32>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    body: Value,
}

/// A canvas in memory.
pub struct Doc {
    pub canvas: Canvas,
    /// The bodies of [`NodeBody::Unknown`] nodes, as read, by node id.
    raw: HashMap<String, Value>,
    extra: Map<String, Value>,
    version: u32,
}

/// The last rev seen of each `canvas.json`, with its mtime and size, so
/// an outside edit reads as a newer rev.
/// Rev, mtime (ns) and size by canonical path.
type Seen = HashMap<std::path::PathBuf, (u64, u128, u64)>;
static REVS: LazyLock<Mutex<Seen>> = LazyLock::new(Default::default);

fn rel_of(name: &str) -> String {
    format!("canvases/{name}/{FILE}")
}

/// Canvas `name`'s folder name, checked.
///
/// # Errors
/// `bad_request` for a name that is no folder name.
pub fn check_name(name: &str) -> Result<String, RpcError> {
    disk::component("canvas", name)
}

fn new_id(prefix: &str, taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("{prefix}{:06x}", rand::random::<u32>() & 0xff_ffff);
        if !taken(&id) {
            return id;
        }
    }
}

/// The id given to the `i`th node or edge of a file that has none (or a
/// bad or repeated one): the same on every read until a write saves it, so
/// a client may use it in its next edit.
fn fallback_id(prefix: &str, i: usize, taken: impl Fn(&str) -> bool) -> String {
    (0..).map(|n| if n == 0 { format!("{prefix}x{i}") } else { format!("{prefix}x{i}-{n}") }).find(|id| !taken(id)).expect("a free id")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The canvas folders (with a `canvas.json`), by name.
pub fn names(root: &Path) -> Vec<String> {
    let Ok(canonical) = paths::canonical_root(root) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(canonical.join("canvases")) else { return Vec::new() };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            let file = e.path().join(FILE).canonicalize().ok()?;
            (!name.starts_with('.') && file.is_file() && paths::inside(&canonical, &file)).then_some(name)
        })
        .collect();
    out.sort();
    out
}

/// Every canvas's listing (any order).
pub fn list(root: &Path) -> Vec<CanvasInfo> {
    names(root).iter().filter_map(|n| load(root, n).ok().map(|d| info_of(root, &d))).collect()
}

/// The listing of a loaded canvas.
pub fn info_of(root: &Path, doc: &Doc) -> CanvasInfo {
    let c = &doc.canvas;
    let updated = root.join(rel_of(&c.name)).metadata().map_or(0, |m| disk::mtime_ms(&m));
    CanvasInfo {
        name: c.name.clone(),
        title: c.title.clone(),
        episode: c.episode,
        format: c.format.clone(),
        nodes: u32::try_from(c.nodes.len()).unwrap_or(u32::MAX),
        updated,
        rev: c.rev,
    }
}

/// Read canvas `name`.
///
/// # Errors
/// `not_found` for a missing canvas; `bad_request` when it isn't JSON.
pub fn load(root: &Path, name: &str) -> Result<Doc, RpcError> {
    let name = check_name(name)?;
    let rel = rel_of(&name);
    let path = paths::resolve(root, &rel).map_err(|e| if e.code == "not_found" { RpcError::not_found(format!("canvas {name}")) } else { e })?;
    let bytes = std::fs::read(&path).map_err(|e| disk::io_error(&rel, &e))?;
    let meta = path.metadata().map_err(|e| disk::io_error(&rel, &e))?;
    let file: File = serde_json::from_slice(&bytes).map_err(|e| RpcError::bad_request(format!("{rel} doesn't parse: {e}")))?;
    let mut raw = HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();
    for (at, value) in file.nodes.into_iter().enumerate() {
        let Ok(loose) = serde_json::from_value::<LooseNode>(value) else { continue };
        let mut id = loose.id;
        if !valid_id(&id) || nodes.iter().any(|n| n.id == id) {
            id = fallback_id("n_", at, |i| nodes.iter().any(|n| n.id == i));
        }
        let body = match serde_json::from_value::<NodeBody>(loose.body.clone()) {
            Ok(NodeBody::Unknown) | Err(_) => {
                raw.insert(id.clone(), loose.body);
                NodeBody::Unknown
            }
            Ok(body) => body,
        };
        let finite = |v: f64| if v.is_finite() { v } else { 0.0 };
        nodes.push(Node {
            id,
            x: finite(loose.x),
            y: finite(loose.y),
            w: finite(loose.w),
            h: finite(loose.h),
            z: loose.z,
            parent: loose.parent,
            body,
        });
    }
    let mut edges: Vec<Edge> = Vec::new();
    for (at, value) in file.edges.into_iter().enumerate() {
        let Ok(mut map) = serde_json::from_value::<Map<String, Value>>(value) else { continue };
        let id_ok = map.get("id").and_then(Value::as_str).is_some_and(|i| valid_id(i) && !edges.iter().any(|e| e.id == i));
        if !id_ok {
            map.insert("id".into(), Value::String(fallback_id("e_", at, |i| edges.iter().any(|e| e.id == i))));
        }
        if let Ok(edge) = serde_json::from_value::<Edge>(Value::Object(map)) {
            edges.push(edge);
        }
    }
    let (mtime, size) = (disk::mtime_ns(&meta), meta.len());
    let rev = {
        let mut revs = REVS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let rev = match revs.get(&path) {
            Some(&(seen, m, s)) if (m, s) != (mtime, size) && file.rev <= seen => seen + 1,
            Some(&(seen, ..)) => seen.max(file.rev),
            None => file.rev,
        };
        revs.insert(path, (rev, mtime, size));
        rev
    };
    let title = if file.title.trim().is_empty() { name.clone() } else { file.title };
    let canvas = Canvas {
        name,
        title,
        episode: file.episode,
        format: file.format,
        rev,
        nodes,
        edges,
        out_of_date: Vec::new(),
        out_of_date_why: BTreeMap::new(),
    };
    Ok(Doc { canvas, raw, extra: file.extra, version: file.version })
}

/// Write `doc` with its `rev` (the caller bumps it).
///
/// # Errors
/// An I/O failure.
pub fn save(root: &Path, doc: &Doc) -> Result<(), RpcError> {
    let c = &doc.canvas;
    let rel = rel_of(&c.name);
    let nodes = c
        .nodes
        .iter()
        .map(|n| {
            let mut v = serde_json::to_value(n).unwrap_or(Value::Null);
            if let (NodeBody::Unknown, Some(raw)) = (&n.body, doc.raw.get(&n.id)) {
                v["body"] = raw.clone();
            }
            v
        })
        .collect();
    let edges = c.edges.iter().filter_map(|e| serde_json::to_value(e).ok()).collect();
    let file = File {
        version: doc.version.max(1),
        title: c.title.clone(),
        episode: c.episode,
        format: c.format.clone(),
        rev: c.rev,
        nodes,
        edges,
        extra: doc.extra.clone(),
    };
    let mut bytes = serde_json::to_vec_pretty(&file).map_err(RpcError::internal)?;
    bytes.push(b'\n');
    let path = disk::target(root, &rel)?;
    disk::atomic_write(&path, &bytes, &rel)?;
    if let Ok(meta) = path.metadata() {
        let path = path.canonicalize().unwrap_or(path);
        REVS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(path, (c.rev, disk::mtime_ns(&meta), meta.len()));
    }
    Ok(())
}

/// Canvas `name`'s rev now (for `studio.changed`); `None` when it's gone.
pub fn current_rev(root: &Path, name: &str) -> Option<u64> {
    load(root, name).ok().map(|d| d.canvas.rev)
}

/// Load, change and save canvas `name` under its lock; `f` says whether
/// it changed anything (then the rev is bumped and it is written).
fn modify<T>(root: &Path, name: &str, f: impl FnOnce(&mut Doc) -> Result<(T, bool), RpcError>) -> Result<(T, Doc), RpcError> {
    let name = check_name(name)?;
    disk::locked(root, &rel_of(&name), || {
        let mut doc = load(root, &name)?;
        let (out, changed) = f(&mut doc)?;
        if changed {
            doc.canvas.rev += 1;
            save(root, &doc)?;
        }
        Ok((out, doc))
    })
}

/// `studio.canvas`: the canvas with its out-of-date cards (keys from the
/// files' mtimes in unix ms, as [`Canvas::input_keys`] takes them).
///
/// # Errors
/// As [`load`].
pub fn read(root: &Path, name: &str) -> Result<Canvas, RpcError> {
    let mut canvas = load(root, name)?.canvas;
    mark_out_of_date(root, &mut canvas, None);
    Ok(canvas)
}

/// Fill `canvas.out_of_date` and `out_of_date_why`. `sections`: the world
/// already read (`None`: read when an Entity node needs it).
pub fn mark_out_of_date(root: &Path, canvas: &mut Canvas, sections: Option<&[Section]>) {
    let cards: Vec<String> =
        canvas.nodes.iter().filter(|n| matches!(&n.body, NodeBody::Gen { takes, .. } if !takes.is_empty())).map(|n| n.id.clone()).collect();
    if cards.is_empty() {
        return;
    }
    let read;
    let sections = match sections {
        Some(s) => s,
        None => {
            read = if canvas.nodes.iter().any(|n| matches!(n.body, NodeBody::Entity { .. })) { world::sections(root) } else { Vec::new() };
            &read
        }
    };
    // As otto computes them (`otto-silo-tools`): the file under the root, links followed, unix ms.
    let mtime = |rel: &str| paths::normalize(rel).ok().and_then(|rel| root.join(rel).metadata().ok()).map(|m| disk::mtime_ms(&m));
    for id in cards {
        let why = canvas.out_of_date_why(&id, sections, mtime);
        if !why.is_empty() {
            canvas.out_of_date.push(id.clone());
            canvas.out_of_date_why.insert(id, why);
        }
    }
}

/// `studio.canvas_create`: a folder named by `name` or a slug of the title
/// (`ep01-pilot` for an episode), the next free one when taken.
///
/// # Errors
/// `bad_request` for an empty title or a format with no preset.
pub fn create(root: &Path, title: &str, episode: Option<u32>, format: Option<&str>, name: Option<&str>) -> Result<CanvasInfo, RpcError> {
    let title = title.trim();
    if title.is_empty() || title.len() > 200 || title.chars().any(char::is_control) {
        return Err(RpcError::bad_request("a canvas needs a title (1 to 200 printable bytes)"));
    }
    if let Some(format) = format
        && !project::has_preset(root, format)
    {
        return Err(RpcError::bad_request(format!("no format preset {format}")));
    }
    let base = match name {
        Some(name) => check_name(name)?,
        None => match episode {
            Some(ep) => format!("ep{ep:02}-{}", disk::slug(title, "episode")),
            None => disk::slug(title, "canvas"),
        },
    };
    let canvases = disk::mkdirs(root, "canvases")?;
    let name = disk::locked(root, "canvases", || {
        let name = disk::free_name(&canvases, &base, false);
        disk::mkdirs(root, &format!("canvases/{name}/assets"))?;
        let canvas = Canvas {
            name: name.clone(),
            title: title.to_owned(),
            episode,
            format: format.map(str::to_owned),
            rev: 1,
            nodes: Vec::new(),
            edges: Vec::new(),
            out_of_date: Vec::new(),
            out_of_date_why: BTreeMap::new(),
        };
        save(root, &Doc { canvas, raw: HashMap::new(), extra: Map::new(), version: 1 })?;
        Ok::<_, RpcError>(name)
    })?;
    project::order(root, &name, true)?;
    Ok(info_of(root, &load(root, &name)?))
}

/// `studio.canvas_update`.
///
/// # Errors
/// As [`load`]; `bad_request` for an empty title or an unknown format.
pub fn update(
    root: &Path,
    name: &str,
    title: Option<&str>,
    episode: Option<Option<u32>>,
    format: Option<Option<&str>>,
) -> Result<CanvasInfo, RpcError> {
    if let Some(Some(format)) = format
        && !project::has_preset(root, format)
    {
        return Err(RpcError::bad_request(format!("no format preset {format}")));
    }
    let ((), doc) = modify(root, name, |doc| {
        let c = &mut doc.canvas;
        let before = (c.title.clone(), c.episode, c.format.clone());
        if let Some(title) = title {
            let title = title.trim();
            if title.is_empty() || title.len() > 200 {
                return Err(RpcError::bad_request("a canvas needs a title (1 to 200 bytes)"));
            }
            c.title = title.to_owned();
        }
        if let Some(episode) = episode {
            c.episode = episode;
        }
        if let Some(format) = format {
            c.format = format.map(str::to_owned);
        }
        Ok(((), before != (c.title.clone(), c.episode, c.format.clone())))
    })?;
    Ok(info_of(root, &doc))
}

/// `studio.canvas_delete`: the folder to the Trash (else deleted).
///
/// # Errors
/// `not_found` for a missing canvas.
pub fn delete(root: &Path, name: &str, trash: Option<&Path>) -> Result<(), RpcError> {
    let name = check_name(name)?;
    let rel = format!("canvases/{name}");
    let dir = paths::resolve(root, &rel).map_err(|e| if e.code == "not_found" { RpcError::not_found(format!("canvas {name}")) } else { e })?;
    if !dir.is_dir() {
        return Err(RpcError::not_found(format!("canvas {name}")));
    }
    disk::locked(root, &rel_of(&name), || {
        if let Err(error) = crate::trash::trash(&dir, trash) {
            tracing::info!(canvas = %dir.display(), "studio: no trash ({error}); deleting");
            std::fs::remove_dir_all(&dir).map_err(|e| disk::io_error(&rel, &e))?;
        }
        Ok::<(), RpcError>(())
    })?;
    project::order(root, &name, false)
}

// ---- canvas_edit ------------------------------------------------------

fn fail(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, message.into())
}

fn missing(what: &str, id: &str) -> RpcError {
    RpcError::not_found(format!("{what} {id}"))
}

/// `(w, h)` for media of aspect `w:h` fitting [`MEDIA`].
fn fit(aspect: (f64, f64)) -> (f64, f64) {
    let (a, b) = aspect;
    if a >= b { (MEDIA, (MEDIA * b / a).round()) } else { ((MEDIA * a / b).round(), MEDIA) }
}

/// The effective aspect for something in frame `parent`.
fn aspect_in(project: &Project, canvas: &Canvas, parent: Option<&str>) -> (f64, f64) {
    project.format_for(canvas, parent).and_then(|(_, f)| f.aspect_ratio()).unwrap_or((16.0, 9.0))
}

fn dims(width: Option<u32>, height: Option<u32>) -> Option<(f64, f64)> {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((f64::from(w), f64::from(h))),
        _ => None,
    }
}

/// A card's face aspect (`w / h`): its picked take's, else its own `aspect_ratio`, else `aspect`.
fn card_aspect(body: &NodeBody, aspect: (f64, f64)) -> f64 {
    let own = || match body {
        NodeBody::Gen { options, .. } => options.get("aspect_ratio").and_then(Value::as_str).and_then(size::parse_aspect),
        _ => None,
    };
    body.take().and_then(|t| dims(t.width, t.height)).map(|(w, h)| w / h).or_else(own).unwrap_or(aspect.0 / aspect.1)
}

/// The size a node gets without `w`/`h`: cards as the app draws them (collapsed).
fn size_for(body: &NodeBody, aspect: (f64, f64)) -> (f64, f64) {
    match body {
        NodeBody::Asset { media: StudioMedia::Audio, .. } => (MEDIA, 96.0),
        NodeBody::Asset { media: StudioMedia::Text, .. } => (size::DOC_W, size::DOC_H),
        NodeBody::Asset { width, height, .. } => fit(dims(*width, *height).unwrap_or(aspect)),
        NodeBody::Gen { makes, prompt, .. } => {
            let a = card_aspect(body, aspect);
            let w = size::card_width(*makes, a);
            (w, size::card_height(*makes, w, a, size::estimate_lines(prompt, w)).round())
        }
        NodeBody::Note { .. } => (240.0, 160.0),
        NodeBody::Entity { .. } => (200.0, 260.0),
        NodeBody::Frame { role: FrameRole::Shot, .. } => {
            // One card with its ports and output.
            let a = aspect.0 / aspect.1;
            let w = size::card_width(GenKind::Video, a);
            let h = size::card_height(GenKind::Video, w, a, 1);
            (w + PORT_GUTTER + OUTPUT_GUTTER + 2.0 * PAD, (HEAD + h + PAD).round())
        }
        NodeBody::Frame { .. } => (640.0, 400.0),
        NodeBody::Unknown => (240.0, 160.0),
    }
}

fn is_card(body: &NodeBody) -> bool {
    matches!(body, NodeBody::Gen { .. })
}

/// What floats right of a node: its output dot (frames have none).
fn right_gutter(body: &NodeBody) -> f64 {
    if matches!(body, NodeBody::Frame { .. } | NodeBody::Unknown) { 0.0 } else { OUTPUT_GUTTER }
}

/// Between `left` and a node right of it: [`CARD_GAP`] when either is a card (ports, output), else [`GAP`].
fn gap_between(left: &NodeBody, card: bool) -> f64 {
    if card || is_card(left) { CARD_GAP } else { GAP }
}

/// Where a node without coordinates goes: right of the last node in its
/// frame (the frame's corner when empty, a card's ports inside it); at the
/// top level, right of the content, or for a frame below it.
fn place(canvas: &Canvas, parent: Option<&str>, body: &NodeBody) -> (f64, f64) {
    let card = is_card(body);
    if let Some(p) = parent.and_then(|p| canvas.node(p)) {
        return match canvas.nodes.iter().rev().find(|n| n.parent.as_deref() == Some(p.id.as_str())) {
            Some(last) => (last.x + last.w + gap_between(&last.body, card), last.y),
            None => (p.x + PAD + if card { PORT_GUTTER } else { 0.0 }, p.y + HEAD),
        };
    }
    if canvas.nodes.is_empty() {
        return (0.0, 0.0);
    }
    let left = canvas.nodes.iter().map(|n| n.x).fold(f64::INFINITY, f64::min);
    let bottom = canvas.nodes.iter().map(|n| n.y + n.h).fold(f64::NEG_INFINITY, f64::max);
    if matches!(body, NodeBody::Frame { .. }) {
        return (left, bottom + 2.0 * GAP);
    }
    let Some(last) = canvas.nodes.iter().max_by(|a, b| (a.x + a.w).total_cmp(&(b.x + b.w))) else { return (0.0, 0.0) };
    let y = canvas.nodes.iter().rev().find(|n| n.parent.is_none()).map_or(0.0, |n| n.y);
    (last.x + last.w + gap_between(&last.body, card), y)
}

/// Slack in layout comparisons, canvas units.
const EPS: f64 = 0.5;

/// What node `n` takes up: its rect with a card's ports (left) and an output dot (right): `[x0, y0, x1, y1]`.
fn extent(n: &Node) -> [f64; 4] {
    let left = if is_card(&n.body) { PORT_GUTTER } else { 0.0 };
    [n.x - left, n.y, n.x + n.w + right_gutter(&n.body), n.y + n.h]
}

/// `n`'s frame when it exists (`ids`: every node id), else `None` (the top level).
fn parent_in<'a>(n: &'a Node, ids: &HashSet<&str>) -> Option<&'a str> {
    n.parent.as_deref().filter(|p| *p != n.id && ids.contains(p))
}

/// Move `id` by `(dx, dy)` with everything in it.
fn shift(canvas: &mut Canvas, id: &str, dx: f64, dy: f64) {
    let mut todo = vec![id.to_owned()];
    let mut seen = HashSet::new();
    while let Some(at) = todo.pop() {
        if !seen.insert(at.clone()) {
            continue;
        }
        for n in &mut canvas.nodes {
            if n.id == at {
                n.x += dx;
                n.y += dy;
            }
            if n.parent.as_deref() == Some(at.as_str()) {
                todo.push(n.id.clone());
            }
        }
    }
}

/// The siblings (indices into `sibs`, sorted) in the way of something growing along an axis whose far edge
/// was `far` and which spans `cross` on the other axis: those at or beyond `far` overlapping `cross`, and
/// transitively those at or beyond theirs, so all move together and keep their gaps. `x`: the axis is x.
fn in_the_way(sibs: &[[f64; 4]], far: f64, cross: (f64, f64), x: bool) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut todo = vec![(far, cross)];
    while let Some((far, (lo, hi))) = todo.pop() {
        for (i, e) in sibs.iter().enumerate() {
            let (near, end, clo, chi) = if x { (e[0], e[2], e[1], e[3]) } else { (e[1], e[3], e[0], e[2]) };
            if !out.contains(&i) && near >= far - EPS && clo < hi - EPS && chi > lo + EPS {
                out.push(i);
                todo.push((end, (clo, chi)));
            }
        }
    }
    out.sort_unstable();
    out
}

/// Frame `id` grew from `ow` × `oh` to its size now: its siblings in the way (right of its old right edge
/// and beside it, then below its old bottom and under it) move by the growth with their contents. The ids
/// moved.
fn push(canvas: &mut Canvas, id: &str, ow: f64, oh: f64) -> Vec<String> {
    let Some(p) = canvas.node(id).cloned() else { return Vec::new() };
    let (dw, dh) = (p.w - ow, p.h - oh);
    if dw <= EPS && dh <= EPS {
        return Vec::new();
    }
    let sibs: Vec<String> = {
        let ids: HashSet<&str> = canvas.nodes.iter().map(|n| n.id.as_str()).collect();
        let parent = parent_in(&p, &ids);
        canvas.nodes.iter().filter(|n| n.id != p.id && parent_in(n, &ids) == parent).map(|n| n.id.clone()).collect()
    };
    let extents = |canvas: &Canvas| sibs.iter().map(|s| canvas.node(s).map_or([0.0; 4], extent)).collect::<Vec<_>>();
    let mut moved: Vec<String> = Vec::new();
    for (x, d) in [(true, dw), (false, dh)] {
        if d <= EPS {
            continue;
        }
        let far = if x { p.x + ow } else { p.y + oh };
        let cross = if x { (p.y, p.y + p.h) } else { (p.x, p.x + p.w) };
        for i in in_the_way(&extents(canvas), far, cross, x) {
            let (dx, dy) = if x { (d, 0.0) } else { (0.0, d) };
            shift(canvas, &sibs[i], dx, dy);
            if !moved.contains(&sibs[i]) {
                moved.push(sibs[i].clone());
            }
        }
    }
    moved
}

/// Grow node `id`'s frames (up the chain) to hold it, its output dot included; a frame that grows pushes
/// what is in its way ([`push`]), and its frame then holds those too.
fn grow(canvas: &mut Canvas, id: &str) {
    grow_holding(canvas, vec![id.to_owned()]);
}

/// [`grow`] for `held`, nodes in one frame.
fn grow_holding(canvas: &mut Canvas, mut held: Vec<String>) {
    for _ in 0..64 {
        let Some(parent) = held.first().and_then(|c| canvas.node(c)).and_then(|c| c.parent.clone()) else { return };
        let Some(p) = canvas.node(&parent) else { return };
        let (px, py, ow, oh) = (p.x, p.y, p.w, p.h);
        let (mut w, mut h) = (ow, oh);
        for c in held.iter().filter_map(|c| canvas.node(c)) {
            let [_, _, x1, y1] = extent(c);
            w = w.max(x1 + PAD - px);
            h = h.max(y1 + PAD - py);
        }
        if let Some(p) = canvas.nodes.iter_mut().find(|n| n.id == parent) {
            (p.w, p.h) = (w, h);
        }
        let moved = push(canvas, &parent, ow, oh);
        held = std::iter::once(parent).chain(moved).collect();
    }
}

/// Re-flow what is in `frame` (none: the top level), frames first, depth first: frames in a column (all
/// shots: a row) in `order`, then by position, from the frame's content corner (the top level: the
/// content's top left), `2·GAP` apart in a column and `GAP` in a row; the loose nodes as one block kept
/// as arranged, before the frames when they started before them, else after; then `frame` fitted to it all
/// with its padding. Only positions and frames' sizes change.
fn tidy(canvas: &mut Canvas, frame: Option<&str>, depth: usize) {
    if depth > 64 {
        return;
    }
    let (kids, frames, row): (Vec<String>, Vec<String>, bool) = {
        let ids: HashSet<&str> = canvas.nodes.iter().map(|n| n.id.as_str()).collect();
        let kids: Vec<&Node> = canvas.nodes.iter().filter(|n| parent_in(n, &ids) == frame && Some(n.id.as_str()) != frame).collect();
        let mut frames: Vec<&Node> = kids.iter().copied().filter(|n| matches!(n.body, NodeBody::Frame { .. })).collect();
        let row = !frames.is_empty() && frames.iter().all(|n| matches!(n.body, NodeBody::Frame { role: FrameRole::Shot, .. }));
        let order = |n: &Node| match &n.body {
            NodeBody::Frame { order: Some(o), .. } => *o,
            _ => u32::MAX,
        };
        let along = |n: &Node| if row { (n.x, n.y) } else { (n.y, n.x) };
        frames.sort_by(|a, b| order(a).cmp(&order(b)).then(along(a).0.total_cmp(&along(b).0)).then(along(a).1.total_cmp(&along(b).1)));
        (kids.iter().map(|n| n.id.clone()).collect(), frames.iter().map(|n| n.id.clone()).collect(), row)
    };
    if kids.is_empty() {
        return;
    }
    for f in &frames {
        tidy(canvas, Some(f), depth + 1);
    }
    let loose: Vec<String> = kids.iter().filter(|k| !frames.contains(k)).cloned().collect();
    let ext = |canvas: &Canvas, id: &str| canvas.node(id).map_or([0.0; 4], extent);
    let union = |canvas: &Canvas, ids: &[String]| {
        ids.iter().map(|i| ext(canvas, i)).fold([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY], |a, e| {
            [a[0].min(e[0]), a[1].min(e[1]), a[2].max(e[2]), a[3].max(e[3])]
        })
    };
    let (ox, oy) = match frame.and_then(|f| canvas.node(f)) {
        Some(f) => (f.x + PAD, f.y + HEAD),
        None => {
            let all = union(canvas, &kids);
            (all[0], all[1])
        }
    };
    let gap = if row { GAP } else { 2.0 * GAP };
    // Where something starting at `e` goes with the cursor at `at`, and how far the cursor then moves.
    let target = |e: [f64; 4], at: f64| if row { ((at - e[0], oy - e[1]), e[2] - e[0]) } else { ((ox - e[0], at - e[1]), e[3] - e[1]) };
    let block = (!loose.is_empty()).then(|| union(canvas, &loose));
    let start = frames.iter().map(|f| ext(canvas, f)).map(|e| if row { e[0] } else { e[1] }).fold(f64::INFINITY, f64::min);
    let first = block.is_some_and(|b| (if row { b[0] } else { b[1] }) < start);
    let mut at = if row { ox } else { oy };
    let place_loose = |canvas: &mut Canvas, at: &mut f64| {
        let Some(b) = block else { return };
        let ((dx, dy), size) = target(b, *at);
        for id in &loose {
            shift(canvas, id, dx, dy);
        }
        *at += size + gap;
    };
    if first {
        place_loose(canvas, &mut at);
    }
    for f in &frames {
        let ((dx, dy), size) = target(ext(canvas, f), at);
        shift(canvas, f, dx, dy);
        at += size + gap;
    }
    if !first {
        place_loose(canvas, &mut at);
    }
    if let Some(f) = frame {
        let all = union(canvas, &kids);
        if let Some(n) = canvas.nodes.iter_mut().find(|n| n.id == f) {
            n.w = all[2] + PAD - n.x;
            n.h = all[3] + PAD - n.y;
        }
    }
}

fn is_frame(canvas: &Canvas, id: &str) -> bool {
    matches!(canvas.node(id).map(|n| &n.body), Some(NodeBody::Frame { .. }))
}

/// Whether making `parent` the frame of `id` keeps frames a tree.
fn no_cycle(canvas: &Canvas, id: &str, parent: &str) -> bool {
    let mut at = Some(parent.to_owned());
    for _ in 0..=canvas.nodes.len() {
        match at {
            None => return true,
            Some(p) if p == id => return false,
            Some(p) => at = canvas.node(&p).and_then(|n| n.parent.clone()),
        }
    }
    false
}

/// A body as stored: paths normalized, image sizes filled when cheap.
fn check_body(root: &Path, body: NodeBody) -> Result<NodeBody, RpcError> {
    let path = |p: &str| -> Result<String, RpcError> {
        let p = paths::normalize(p)?;
        if p.is_empty() { Err(RpcError::bad_request("a media path is needed")) } else { Ok(p) }
    };
    let size = |p: &str, media: StudioMedia, w: Option<u32>, h: Option<u32>| match (media, w, h) {
        (StudioMedia::Image, None, _) | (StudioMedia::Image, _, None) => media::image_size(root, p).map_or((w, h), |(w, h)| (Some(w), Some(h))),
        _ => (w, h),
    };
    Ok(match body {
        NodeBody::Asset { path: p, media, width, height, seconds, prompt } => {
            let p = path(&p)?;
            let media = if media == StudioMedia::Unknown { media::media_of(&p).unwrap_or(media) } else { media };
            let (width, height) = size(&p, media, width, height);
            NodeBody::Asset { path: p, media, width, height, seconds, prompt }
        }
        NodeBody::Entity { id } => {
            let id = paths::normalize(id.trim_start_matches("world/"))?;
            if !id.contains('/') {
                return Err(RpcError::bad_request(format!("entity id {id} must be <section>/<name>")));
            }
            NodeBody::Entity { id }
        }
        NodeBody::Gen {
            makes,
            title,
            prompt,
            model,
            mode,
            options,
            status,
            progress,
            job,
            error,
            error_code,
            takes,
            pick,
            inputs_used,
            tool,
            input,
        } => {
            if makes == GenKind::Unknown {
                return Err(RpcError::bad_request("a card's makes must be image, video, speech, sound or music"));
            }
            let mut checked = Vec::with_capacity(takes.len());
            for mut t in takes {
                t.path = path(&t.path)?;
                (t.width, t.height) = size(&t.path, t.media, t.width, t.height);
                checked.push(t);
            }
            if pick.is_some_and(|p| p as usize >= checked.len()) {
                return Err(RpcError::bad_request(format!("pick {} is no take (it has {})", pick.unwrap_or(0), checked.len())));
            }
            NodeBody::Gen {
                makes,
                title,
                prompt,
                model,
                mode,
                options,
                status,
                progress,
                job,
                error,
                error_code,
                takes: checked,
                pick,
                inputs_used,
                tool,
                input,
            }
        }
        NodeBody::Unknown => return Err(RpcError::bad_request("unknown node kind")),
        other => other,
    })
}

/// RFC 7396.
fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(map) = target else { return };
    for (k, v) in patch {
        if v.is_null() {
            map.remove(k);
        } else {
            merge_patch(map.entry(k.clone()).or_insert(Value::Null), v);
        }
    }
}

struct Edit<'a> {
    root: &'a Path,
    project: &'a Project,
    doc: &'a mut Doc,
}

impl Edit<'_> {
    fn canvas(&mut self) -> &mut Canvas {
        &mut self.doc.canvas
    }

    fn add_node(&mut self, new: NewNode) -> Result<String, RpcError> {
        if self.doc.canvas.nodes.len() >= NODES_MAX {
            return Err(fail("too_large", format!("a canvas holds at most {NODES_MAX} nodes")));
        }
        let id = match new.id {
            Some(id) if !valid_id(&id) => return Err(RpcError::bad_request(format!("node id {id:?}: letters, digits, _ and - only"))),
            Some(id) if self.doc.canvas.node(&id).is_some() => return Err(fail(RpcError::CONFLICT, format!("node {id} exists"))),
            Some(id) => id,
            None => new_id("n_", |i| self.doc.canvas.nodes.iter().any(|n| n.id == i) || self.doc.canvas.edges.iter().any(|e| e.id == i)),
        };
        if let Some(p) = &new.parent
            && !is_frame(&self.doc.canvas, p)
        {
            return Err(missing("frame", p));
        }
        let mut body = check_body(self.root, new.body)?;
        if let NodeBody::Gen { status, takes, .. } = &mut body
            && takes.is_empty()
            && *status == GenStatus::Done
        {
            *status = GenStatus::Draft;
        }
        let aspect = aspect_in(self.project, &self.doc.canvas, new.parent.as_deref());
        let (dw, dh) = size_for(&body, aspect);
        let (w, h) = (new.w.filter(|w| w.is_finite() && *w > 0.0).unwrap_or(dw), new.h.filter(|h| h.is_finite() && *h > 0.0).unwrap_or(dh));
        let auto = new.x.is_none() || new.y.is_none();
        let (px, py) = if auto { place(&self.doc.canvas, new.parent.as_deref(), &body) } else { (0.0, 0.0) };
        let x = new.x.filter(|x| x.is_finite()).unwrap_or(px);
        let y = new.y.filter(|y| y.is_finite()).unwrap_or(py);
        self.canvas().nodes.push(Node { id: id.clone(), x, y, w, h, z: new.z, parent: new.parent, body });
        if auto {
            grow(self.canvas(), &id);
        }
        Ok(id)
    }

    #[allow(clippy::too_many_arguments, reason = "the op's fields")]
    fn update_node(
        &mut self,
        id: &str,
        xywh: [Option<f64>; 4],
        z: Option<i32>,
        parent: Option<Option<String>>,
        patch: Option<Value>,
    ) -> Result<(), RpcError> {
        let canvas = &self.doc.canvas;
        let node = canvas.node(id).ok_or_else(|| missing("node", id))?;
        if let Some(Some(p)) = &parent {
            if !is_frame(canvas, p) {
                return Err(missing("frame", p));
            }
            if !no_cycle(canvas, id, p) {
                return Err(RpcError::bad_request(format!("frame {p} is inside {id}")));
            }
        }
        // The new body first: nothing changes when it's refused.
        let body = match &patch {
            None => None,
            Some(patch) => {
                let mut value = match (&node.body, self.doc.raw.get(id)) {
                    (NodeBody::Unknown, Some(raw)) => raw.clone(),
                    (body, _) => serde_json::to_value(body).map_err(RpcError::internal)?,
                };
                let kind = value.get("kind").cloned();
                if patch.get("kind").is_some_and(|k| Some(k) != kind.as_ref()) {
                    return Err(RpcError::bad_request("a node's kind can't change"));
                }
                merge_patch(&mut value, patch);
                if matches!(node.body, NodeBody::Unknown) {
                    Some((NodeBody::Unknown, Some(value)))
                } else {
                    let body: NodeBody = serde_json::from_value(value).map_err(|e| RpcError::bad_request(format!("body: {e}")))?;
                    Some((check_body(self.root, body)?, None))
                }
            }
        };
        let node = self.doc.canvas.nodes.iter_mut().find(|n| n.id == id).ok_or_else(|| missing("node", id))?;
        for (field, v) in [&mut node.x, &mut node.y].into_iter().zip([xywh[0], xywh[1]]) {
            if let Some(v) = v.filter(|v| v.is_finite()) {
                *field = v;
            }
        }
        for (field, v) in [&mut node.w, &mut node.h].into_iter().zip([xywh[2], xywh[3]]) {
            if let Some(v) = v.filter(|v| v.is_finite() && *v > 0.0) {
                *field = v;
            }
        }
        if z.is_some() {
            node.z = z;
        }
        if let Some(parent) = parent {
            node.parent = parent;
        }
        match body {
            Some((NodeBody::Unknown, Some(raw))) => drop(self.doc.raw.insert(id.to_owned(), raw)),
            Some((body, _)) => node.body = body,
            None => {}
        }
        Ok(())
    }

    fn remove_node(&mut self, id: &str) -> Result<(), RpcError> {
        let at = self.doc.canvas.nodes.iter().position(|n| n.id == id).ok_or_else(|| missing("node", id))?;
        let gone = self.canvas().nodes.remove(at);
        self.doc.raw.remove(id);
        let canvas = self.canvas();
        canvas.edges.retain(|e| e.from != id && e.to != id);
        // A frame's children stay, out of it.
        for n in canvas.nodes.iter_mut().filter(|n| n.parent.as_deref() == Some(id)) {
            n.parent.clone_from(&gone.parent);
        }
        Ok(())
    }

    fn add_edge(&mut self, new: NewEdge) -> Result<String, RpcError> {
        let canvas = &self.doc.canvas;
        let id = match new.id {
            Some(id) if !valid_id(&id) => return Err(RpcError::bad_request(format!("edge id {id:?}: letters, digits, _ and - only"))),
            Some(id) if canvas.edges.iter().any(|e| e.id == id) => return Err(fail(RpcError::CONFLICT, format!("edge {id} exists"))),
            Some(id) => id,
            None => new_id("e_", |i| canvas.edges.iter().any(|e| e.id == i) || canvas.nodes.iter().any(|n| n.id == i)),
        };
        for end in [&new.from, &new.to] {
            if canvas.node(end).is_none() {
                return Err(missing("node", end));
            }
        }
        let replaced = canvas.wire_check(&new.from, &new.to, new.role).map_err(RpcError::bad_request)?;
        let edges = &mut self.canvas().edges;
        // A frame port's new wire takes its old one's place.
        let at = edges.iter().position(|e| replaced.contains(&e.id));
        edges.retain(|e| !replaced.contains(&e.id));
        let edge = Edge { id: id.clone(), from: new.from, to: new.to, role: new.role, label: new.label, mention: false };
        match at {
            Some(at) => edges.insert(at.min(edges.len()), edge),
            None => edges.push(edge),
        }
        Ok(id)
    }

    fn remove_edge(&mut self, id: &str) -> Result<(), RpcError> {
        let at = self.doc.canvas.edges.iter().position(|e| e.id == id).ok_or_else(|| missing("edge", id))?;
        self.canvas().edges.remove(at);
        Ok(())
    }

    /// `tidy`: re-flow `frame` (none: the top level); a frame that grew pushes what is in its way.
    fn tidy(&mut self, frame: Option<&str>) -> Result<(), RpcError> {
        let canvas = self.canvas();
        let Some(f) = frame else {
            tidy(canvas, None, 0);
            return Ok(());
        };
        let (ow, oh) = match canvas.node(f) {
            Some(n) if matches!(n.body, NodeBody::Frame { .. }) => (n.w, n.h),
            _ => return Err(missing("frame", f)),
        };
        tidy(canvas, Some(f), 0);
        let moved = push(canvas, f, ow, oh);
        grow_holding(canvas, std::iter::once(f.to_owned()).chain(moved).collect());
        Ok(())
    }

    fn layout(&mut self, ids: &[String], arrange: Arrange, frame: Option<&str>, gap: Option<f64>) -> Result<(), RpcError> {
        let canvas = &self.doc.canvas;
        if ids.is_empty() {
            return Err(RpcError::bad_request("layout needs ids"));
        }
        if arrange == Arrange::Unknown {
            return Err(RpcError::bad_request("as must be row, column or grid"));
        }
        for id in ids {
            if canvas.node(id).is_none() {
                return Err(missing("node", id));
            }
        }
        if let Some(f) = frame {
            if !is_frame(canvas, f) {
                return Err(missing("frame", f));
            }
            if ids.iter().any(|id| !no_cycle(canvas, id, f)) {
                return Err(RpcError::bad_request(format!("frame {f} is one of the nodes or inside one")));
            }
        }
        // Rows of cards never overlap a card's ports or the output before them.
        let cards = ids.iter().any(|id| canvas.node(id).is_some_and(|n| is_card(&n.body)));
        let given = gap.filter(|g| g.is_finite() && *g >= 0.0);
        let (gx, gy) = given.map_or((if cards { CARD_GAP } else { GAP }, GAP), |g| (g, g));
        let (ox, oy) = match frame.and_then(|f| canvas.node(f)) {
            Some(f) => (f.x + PAD + if cards { PORT_GUTTER } else { 0.0 }, f.y + HEAD),
            None => canvas.node(&ids[0]).map_or((0.0, 0.0), |n| (n.x, n.y)),
        };
        let sizes: Vec<(f64, f64)> = ids.iter().filter_map(|id| canvas.node(id).map(|n| (n.w, n.h))).collect();
        let (cw, ch) = sizes.iter().fold((0.0f64, 0.0f64), |(w, h), (a, b)| (w.max(*a), h.max(*b)));
        let cols = match arrange {
            Arrange::Grid => (ids.len() as f64).sqrt().ceil().max(1.0) as usize,
            _ => usize::MAX,
        };
        let (mut x, mut y) = (ox, oy);
        for (i, id) in ids.iter().enumerate() {
            let (w, h) = sizes[i];
            let (tx, ty) = match arrange {
                Arrange::Grid => (ox + (i % cols) as f64 * (cw + gx), oy + (i / cols) as f64 * (ch + gy)),
                _ => (x, y),
            };
            let (nx, ny) = self.doc.canvas.node(id).map_or((tx, ty), |n| (n.x, n.y));
            shift(self.canvas(), id, tx - nx, ty - ny);
            if let Some(f) = frame
                && let Some(n) = self.doc.canvas.nodes.iter_mut().find(|n| n.id == *id)
            {
                n.parent = Some(f.to_owned());
            }
            match arrange {
                Arrange::Row => x += w + gx,
                Arrange::Column => y += h + gy,
                _ => {}
            }
        }
        if frame.is_some() {
            for id in ids {
                grow(self.canvas(), id);
            }
        }
        Ok(())
    }

    /// Apply `op`; the id it added, if any.
    fn apply(&mut self, op: Op) -> Result<Option<String>, RpcError> {
        match op {
            Op::AddNode { node } => self.add_node(node).map(Some),
            Op::UpdateNode { id, x, y, w, h, z, parent, body } => self.update_node(&id, [x, y, w, h], z, parent, body).map(|()| None),
            Op::RemoveNode { id } => self.remove_node(&id).map(|()| None),
            Op::AddEdge { edge } => self.add_edge(edge).map(Some),
            Op::RemoveEdge { id } => self.remove_edge(&id).map(|()| None),
            Op::Layout { ids, arrange, frame, gap } => self.layout(&ids, arrange, frame.as_deref(), gap).map(|()| None),
            Op::Tidy { frame } => self.tidy(frame.as_deref()).map(|()| None),
            Op::Unknown => Err(RpcError::bad_request("unknown op")),
        }
    }
}

/// `studio.canvas_edit`: each op on its own; the canvas is written once,
/// when any applied.
///
/// # Errors
/// As [`load`]; `bad_request` for too many ops.
pub fn edit(root: &Path, name: &str, ops: Vec<Op>, base_rev: Option<u64>) -> Result<CanvasEdited, RpcError> {
    if ops.len() > OPS_MAX {
        return Err(RpcError::bad_request(format!("at most {OPS_MAX} ops at once")));
    }
    let project = project::formats_project(root);
    let (edited, _) = modify(root, name, |doc| {
        let moved = base_rev.is_some_and(|b| b != doc.canvas.rev);
        let mut out = CanvasEdited { rev: doc.canvas.rev, ids: Vec::new(), moved, failed: Vec::new(), mentions: Vec::new() };
        let mut applied = false;
        let prompts = mention::prompts(&doc.canvas);
        let mut cx = Edit { root, project: &project, doc };
        for (i, op) in ops.into_iter().enumerate() {
            let adds = matches!(op, Op::AddNode { .. } | Op::AddEdge { .. });
            // A tidy that moves nothing is no change.
            let before = matches!(op, Op::Tidy { .. }).then(|| cx.doc.canvas.nodes.clone());
            match cx.apply(op) {
                Ok(id) => {
                    applied |= before.is_none_or(|b| b != cx.doc.canvas.nodes);
                    if adds {
                        out.ids.push(id.unwrap_or_default());
                    }
                }
                Err(error) => {
                    if adds {
                        out.ids.push(String::new());
                    }
                    out.failed.push(OpFailed { op: u32::try_from(i).unwrap_or(u32::MAX), code: error.code, message: error.message });
                }
            }
        }
        // Mentions as wires: after every op, so a batch that adds a card's sources and wires itself is left as made.
        out.mentions = cx.sync_mentions(&prompts);
        applied |= !out.mentions.is_empty();
        if applied {
            out.rev += 1;
        }
        Ok((out, applied))
    })?;
    Ok(edited)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: Option<&str>, [x, y, w, h]: [f64; 4], body: NodeBody) -> Node {
        Node { id: id.into(), x, y, w, h, z: None, parent: parent.map(Into::into), body }
    }

    fn scene(order: u32) -> NodeBody {
        NodeBody::Frame { title: String::new(), role: FrameRole::Scene, order: Some(order), format: None }
    }

    fn note() -> NodeBody {
        NodeBody::Note { text: String::new() }
    }

    fn canvas(nodes: Vec<Node>) -> Canvas {
        Canvas {
            name: "c".into(),
            title: "c".into(),
            episode: None,
            format: None,
            rev: 1,
            nodes,
            edges: Vec::new(),
            out_of_date: Vec::new(),
            out_of_date_why: BTreeMap::new(),
        }
    }

    fn at(c: &Canvas, id: &str) -> (f64, f64) {
        c.node(id).map(|n| (n.x, n.y)).unwrap()
    }

    #[test]
    fn in_the_way_is_beyond_the_edge_overlapping_and_transitive() {
        // Growing right from x 100 over y 0..100: a (beyond, beside), b (beyond a, beside a only), c (below).
        let sibs = [[150.0, 0.0, 200.0, 50.0], [250.0, 40.0, 300.0, 140.0], [0.0, 200.0, 50.0, 250.0], [90.0, 0.0, 120.0, 20.0]];
        assert_eq!(in_the_way(&sibs, 100.0, (0.0, 100.0), true), [0, 1]);
        assert_eq!(in_the_way(&sibs, 100.0, (0.0, 100.0), false), [2]);
    }

    #[test]
    fn a_growing_scene_pushes_the_scenes_below_with_their_contents() {
        let mut c = canvas(vec![
            node("s1", None, [0.0, 240.0, 640.0, 400.0], scene(1)),
            node("s2", None, [0.0, 720.0, 640.0, 400.0], scene(2)),
            node("in2", Some("s2"), [24.0, 764.0, 240.0, 160.0], note()),
            node("s3", None, [0.0, 1200.0, 640.0, 400.0], scene(3)),
            node("left", None, [-900.0, 900.0, 100.0, 100.0], note()),
            node("tall", Some("s1"), [24.0, 284.0, 240.0, 700.0], note()),
        ]);
        grow(&mut c, "tall");
        let dh = 284.0 + 700.0 + PAD - 240.0 - 400.0;
        assert_eq!(c.node("s1").unwrap().h, 400.0 + dh);
        assert_eq!([at(&c, "s2"), at(&c, "in2"), at(&c, "s3")], [(0.0, 720.0 + dh), (24.0, 764.0 + dh), (0.0, 1200.0 + dh)]);
        assert_eq!(at(&c, "left"), (-900.0, 900.0), "not beside the scene: stays");
    }

    #[test]
    fn growth_goes_up_the_chain() {
        let shot = || NodeBody::Frame { title: String::new(), role: FrameRole::Shot, order: None, format: None };
        let mut c = canvas(vec![
            node("s1", None, [0.0, 0.0, 1000.0, 400.0], scene(1)),
            node("b1", Some("s1"), [24.0, 44.0, 400.0, 300.0], shot()),
            node("b2", Some("s1"), [464.0, 44.0, 400.0, 300.0], shot()),
            node("s2", None, [0.0, 480.0, 640.0, 400.0], scene(2)),
            node("wide", Some("b1"), [48.0, 88.0, 600.0, 100.0], note()),
        ]);
        grow(&mut c, "wide");
        let b1 = c.node("b1").unwrap().clone();
        let dw = b1.w - 400.0;
        assert_eq!(at(&c, "b2"), (464.0 + dw, 44.0), "the next shot moved right");
        let s1 = c.node("s1").unwrap();
        assert_eq!(s1.w, 464.0 + dw + 400.0 + PAD, "the scene holds it");
        assert_eq!(at(&c, "s2"), (0.0, 480.0), "the scene grew sideways only");
    }

    #[test]
    fn tidy_stacks_fits_and_is_idempotent() {
        let mut c = canvas(vec![
            node("brief", None, [0.0, 0.0, 240.0, 160.0], note()),
            node("s2", None, [0.0, 240.0, 640.0, 765.0], scene(2)),
            node("s1", None, [0.0, 720.0, 640.0, 765.0], scene(1)),
            node("a", Some("s1"), [500.0, 1000.0, 240.0, 160.0], note()),
        ]);
        tidy(&mut c, None, 0);
        assert_eq!(at(&c, "brief"), (0.0, 0.0));
        assert_eq!(at(&c, "s1"), (0.0, 240.0), "order first");
        let s1 = c.node("s1").unwrap();
        assert_eq!((s1.w, s1.h), (PAD + 240.0 + OUTPUT_GUTTER + PAD, HEAD + 160.0 + PAD), "fitted");
        assert_eq!(at(&c, "a"), (PAD, 240.0 + HEAD), "moved with its scene to its corner");
        assert_eq!(at(&c, "s2"), (0.0, 240.0 + s1.h + 2.0 * GAP));
        let once = c.nodes.clone();
        tidy(&mut c, None, 0);
        assert_eq!(c.nodes, once);
    }
}
