//! Studio blocks (docs/STUDIO.md): a project folder on the server's machine
//! holding a world (`world/`) and canvases (`canvases/<name>/`), for making
//! media with an agent. Every path is relative to the block's root
//! ([`StudioState::root`]), `/`-separated, without `.` or `..` components.
//! Feature [`crate::compat::feature::STUDIO`].

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::changes::double_option;
use crate::model::{AgentLink, BlockId};

pub mod mention;
pub mod size;
#[cfg(test)]
mod tests;

/// Server-held state of a studio block (docs/STUDIO.md §3), in
/// [`crate::model::BlockInfo::studio`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StudioState {
    /// Absolute, canonical; the block never reads or writes outside it.
    pub root: String,
    /// What the body shows.
    #[serde(default)]
    pub view: StudioView,
    /// Canvas tabs open, in order (canvas names: folder names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<String>,
    /// The canvas shown when `view` is [`StudioView::Canvas`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// The World view's section (`overview`, `style`, `characters`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_section: Option<String>,
    /// An entity shown in detail (`characters/mara`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_entity: Option<String>,
    /// The agent block it talks to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentLink>,
}

impl StudioState {
    /// Most open tabs.
    pub const OPEN_MAX: usize = 100;

    /// A fresh state on `root`: the canvas view, no tabs.
    #[must_use]
    pub fn new(root: String) -> Self {
        Self { root, view: StudioView::default(), open: Vec::new(), active: None, world_section: None, world_entity: None, agent: None }
    }
}

/// What a studio block's body shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StudioView {
    /// The active canvas (none open: an empty canvas area).
    #[default]
    Canvas,
    World,
    /// The root has no `studio.toml` yet.
    Setup,
    /// Where the production stands and what is next ([`Production`],
    /// docs/STUDIO.md §4.6).
    Production,
    #[serde(other)]
    Unknown,
}

/// `studio.project` and `studio.world`: the block alone.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioProjectParams {
    pub block_id: BlockId,
}

/// A format preset (`studio.toml`'s `[formats.<name>]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Format {
    /// `W:H` (`16:9`, `2.39:1`).
    pub aspect: String,
    /// The short side: `480p`, `720p`, `1080p`, `4k`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

impl Format {
    /// The aspect as `(w, h)`, both positive (`"2.39:1"` is `(2.39, 1.0)`);
    /// `None` when it doesn't parse.
    #[must_use]
    pub fn aspect_ratio(&self) -> Option<(f64, f64)> {
        let (w, h) = self.aspect.trim().split_once(':')?;
        let (w, h) = (w.trim().parse::<f64>().ok()?, h.trim().parse::<f64>().ok()?);
        (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some((w, h))
    }
}

/// `studio.project`'s (and the project writes') result: `studio.toml` with
/// what is on disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub name: String,
    /// The default preset's name.
    pub format: String,
    /// Presets by name.
    #[serde(default)]
    pub formats: BTreeMap<String, Format>,
    /// In tab order: episodes by number, then `[canvases] order`, then the
    /// rest by name.
    #[serde(default)]
    pub canvases: Vec<CanvasInfo>,
    /// The world's sections: overview, style, then the folders by name.
    #[serde(default)]
    pub sections: Vec<Section>,
    /// `[models]`: the project's model pick (a handle) by kind (`image`,
    /// `video`, `speech`, …); a kind not here is Automatic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, String>,
}

impl Project {
    /// The effective format of a generation on `canvas` (docs/STUDIO.md
    /// §2): frame `frame`'s, walking up its `parent` frames (a shot, then
    /// its scene), else the canvas's, else the project's. A name with no
    /// preset in `formats` is skipped. Returns the preset's name and it.
    #[must_use]
    pub fn format_for(&self, canvas: &Canvas, frame: Option<&str>) -> Option<(String, Format)> {
        let preset = |name: &str| self.formats.get(name).map(|f| (name.to_owned(), f.clone()));
        let mut at = frame.map(str::to_owned);
        // Bounded: a cycle of parents ends the walk.
        for _ in 0..canvas.nodes.len() {
            let Some(node) = at.as_deref().and_then(|id| canvas.nodes.iter().find(|n| n.id == id)) else { break };
            let NodeBody::Frame { format, .. } = &node.body else { break };
            if let Some(found) = format.as_deref().and_then(preset) {
                return Some(found);
            }
            at.clone_from(&node.parent);
        }
        canvas.format.as_deref().and_then(preset).or_else(|| preset(&self.format))
    }
}

/// A canvas, as listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CanvasInfo {
    /// Its folder's name under `canvases/`; stable (renames change `title`).
    pub name: String,
    pub title: String,
    /// Marks an episode; the series order is by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    /// The canvas's format override (a preset name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default)]
    pub nodes: u32,
    /// `canvas.json`'s mtime, unix ms.
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub rev: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    /// `world/overview.md`.
    Overview,
    /// `world/style.md`.
    Style,
    /// A folder of entities.
    Entities,
    #[serde(other)]
    Unknown,
}

/// A section of the world.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Section {
    /// `overview`, `style`, or the folder's name under `world/`.
    pub id: String,
    /// For display (`Characters`; a custom section: its folder's name).
    pub title: String,
    pub kind: SectionKind,
    /// [`SectionKind::Entities`]: by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<EntityInfo>,
    /// [`SectionKind::Style`]: its reference images, every image under
    /// `world/style/` (root-relative, by path).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

/// A world entity, as listed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EntityInfo {
    /// Its folder under `world/` (`characters/mara`).
    pub id: String,
    /// The frontmatter's `name`, else the folder's.
    pub name: String,
    /// Its markdown file, root-relative (`world/characters/mara/mara.md`).
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The canonical sheet, root-relative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// Other reference images, root-relative.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    /// Every reference with its id, name and job, in file order: `refs`
    /// (the same paths) as named references (docs/STUDIO.md §1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub looks: Vec<RefInfo>,
    /// The entities that go with it (`props/bhima-mace`): those its
    /// frontmatter's `links` names, then those whose `links` name it (by
    /// id); existing ones only, each once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Whether the user approved its sheet (the frontmatter's `approved`),
    /// and whether the sheet changed since. `None` without a sheet.
    #[serde(default, skip_serializing_if = "Approval::is_none")]
    pub approval: Approval,
    /// The frontmatter's `voice` names a provider and voice
    /// (`elevenlabs/JBFqnCBsd6RMkjVDRZzb`), not prose.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub voice: bool,
}

impl EntityInfo {
    /// Its reference `id` (`crown-off`).
    #[must_use]
    pub fn look(&self, id: &str) -> Option<&RefInfo> {
        self.looks.iter().find(|r| r.id == id)
    }
}

/// A named reference of an entity (docs/STUDIO.md §1): one entry of its
/// frontmatter's `refs`, a path (`refs/face.png`) or a table
/// (`{file, name?, defines?}`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RefInfo {
    /// Unique within the entity: the slug of its file's stem
    /// (`refs/crown-off.png`: `crown-off`), else of its name; a rename
    /// leaves it alone. The later of two equal ones gets `-2`, `-3`…
    /// (`ref_ids`). `@characters/mara#crown-off` mentions it.
    pub id: String,
    /// Root-relative.
    pub path: String,
    /// The entry's `name`, else its file's stem (`face`).
    pub name: String,
    /// What to take from it (`his bare head and hair, without the crown`);
    /// everything else in it stays behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defines: Option<String>,
    /// By its file's name: an image (a look, an angle, a variant), a video,
    /// audio.
    #[serde(default)]
    pub media: StudioMedia,
}

/// The slug a reference id is made of: lowercase ASCII letters and digits,
/// one `-` between words, at most 48 characters; empty when nothing is left.
#[must_use]
pub fn ref_slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let mut out: String = out.trim_matches('-').chars().take(48).collect();
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// A file's stem: its name without the folders and the last extension.
#[must_use]
pub fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// The ids of an entity's references, given each one's `(name, path)` in
/// file order: the slug of the file's stem (`refs/crown-off.png`:
/// `crown-off`), which a rename doesn't change; else of the name, else
/// `ref`; one already taken gets `-2`, `-3`… (the first free).
#[must_use]
pub fn ref_ids(refs: &[(Option<&str>, &str)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(refs.len());
    for (name, path) in refs {
        // The file's, so a rename keeps the id and the tokens written with it.
        let slug = Some(ref_slug(file_stem(path))).filter(|s| !s.is_empty()).unwrap_or_else(|| name.map(ref_slug).unwrap_or_default());
        let base = if slug.is_empty() { "ref".to_owned() } else { slug };
        let id = if out.contains(&base) { (2..).map(|n| format!("{base}-{n}")).find(|i| !out.contains(i)).unwrap_or_default() } else { base };
        out.push(id);
    }
    out
}

/// An entity sheet's approval by the user (docs/STUDIO.md §4.6): advice,
/// never a lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// Not approved, or no sheet.
    #[default]
    None,
    /// Approved, and the sheet file is no newer than the approval.
    Approved,
    /// Approved, but the sheet file changed since: approve again.
    Changed,
    #[serde(other)]
    Unknown,
}

impl Approval {
    #[must_use]
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }
}

/// `studio.world`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StudioWorld {
    pub sections: Vec<Section>,
}

/// `studio.entity`'s result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Entity {
    pub info: EntityInfo,
    /// The whole frontmatter, unknown keys included.
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    /// The markdown after the frontmatter.
    #[serde(default)]
    pub body: String,
    /// Canvas nodes referencing it (Entity nodes, edges from its files).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_in: Vec<EntityUse>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EntityUse {
    pub canvas: String,
    pub node: String,
}

/// `studio.canvas`'s result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Canvas {
    pub name: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default)]
    pub rev: u64,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    /// Cards whose inputs changed since their picked take was made
    /// ([`Canvas::out_of_date`], computed by the server at read time with
    /// the files' mtimes; never saved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_date: Vec<String>,
    /// For each card in `out_of_date`: what changed
    /// ([`Canvas::out_of_date_why`]). Computed like it; never saved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub out_of_date_why: BTreeMap<String, Vec<String>>,
}

/// A node on a canvas; canvas units (1 at 100 % zoom).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Node {
    /// Short and random (`n_3f2a9c`), stable across writes.
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Stacking; higher is drawn above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<i32>,
    /// The frame it is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub body: NodeBody,
}

/// `add_node`'s node: without `id` one is made; without `x`/`y` it goes
/// right of the last node in its frame (or of the content); without
/// `w`/`h` it is sized for its media at the effective aspect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NewNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub body: NodeBody,
}

/// What an asset is. `Unknown` (also the default when an `Asset` omits it):
/// the server infers it from the path ([`StudioMedia::of_path`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StudioMedia {
    Image,
    Video,
    Audio,
    /// A text document (`.md`, `.markdown`, `.txt`): its output is its path;
    /// whoever compiles reads the text.
    Text,
    #[default]
    #[serde(other)]
    Unknown,
}

impl StudioMedia {
    /// The kind of a file by its extension; `Unknown` for anything else.
    #[must_use]
    pub fn of_path(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path);
        let Some((_, ext)) = name.rsplit_once('.') else { return Self::Unknown };
        match ext.to_ascii_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "avif" | "svg" => Self::Image,
            "mp4" | "m4v" | "mov" | "webm" | "mkv" => Self::Video,
            "mp3" | "wav" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "flac" => Self::Audio,
            "md" | "markdown" | "txt" => Self::Text,
            _ => Self::Unknown,
        }
    }
}

/// A canvas's text documents (docs/STUDIO.md §4.2): a file name directly in
/// `canvases/<name>/` that is `.md`, `.markdown` or `.txt`, not hidden.
#[must_use]
pub fn is_document(name: &str) -> bool {
    !name.starts_with('.') && !name.contains('/') && StudioMedia::of_path(name) == StudioMedia::Text
}

/// Documents in the order they are listed: `script.md` first, then by name
/// (case-insensitive).
pub fn sort_documents(names: &mut [String]) {
    names.sort_by_key(|n| (!n.eq_ignore_ascii_case("script.md"), n.to_lowercase(), n.clone()));
}

/// The document paths (root-relative) of canvas `canvas` among `names`
/// (the folder's file names), sorted ([`sort_documents`]).
#[must_use]
pub fn documents_of(canvas: &str, names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut docs: Vec<String> = names.into_iter().filter(|n| is_document(n)).collect();
    sort_documents(&mut docs);
    docs.into_iter().map(|n| format!("canvases/{canvas}/{n}")).collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GenStatus {
    /// Never run: no take yet.
    #[default]
    Draft,
    Queued,
    Running,
    Done,
    Failed,
    Stopped,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FrameRole {
    #[default]
    Group,
    Scene,
    Shot,
    #[serde(other)]
    Unknown,
}

/// What a node is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant, reason = "a wire type, matched by every peer")]
pub enum NodeBody {
    /// A media file, root-relative (usually in the canvas's `assets/`).
    Asset {
        path: String,
        /// Omitted: inferred from the path by the server.
        #[serde(default)]
        media: StudioMedia,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<f64>,
        /// The prompt it was made from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// Markdown: a prompt draft, a beat, a direction.
    Note {
        #[serde(default)]
        text: String,
    },
    /// A world entity placed on the canvas (shows its sheet).
    Entity { id: String },
    /// A card: what it makes, from its prompt and what is wired into its
    /// ports ([`GenKind::ports`]); its takes. A draft has only `makes` and
    /// `prompt`.
    Gen {
        #[serde(default)]
        makes: GenKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prompt: String,
        /// `None`: automatic (the project's pick, else the default).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        /// The card's own settings (aspect, length, voice, …); unset ones
        /// are inherited.
        #[serde(default, skip_serializing_if = "Value::is_null")]
        options: Value,
        #[serde(default)]
        status: GenStatus,
        /// The provider's progress, for display.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<String>,
        /// Its background job, while it runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// What kind of failure `error` is (`no_provider`, `input`,
        /// `model`, `invalid`, …), for the app to act on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        /// Its results, oldest first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        takes: Vec<Take>,
        /// The take it shows and outputs (an index in `takes`); `None`:
        /// the last.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pick: Option<u32>,
        /// The input keys ([`Canvas::input_keys`]) the picked take was
        /// made from; other keys now: [`Canvas::out_of_date`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inputs_used: Vec<String>,
        /// The last request sent (`generate_image`, …, `generate_media`),
        /// as a record.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
    },
    /// A titled region; scenes and shots are ordered by `order`.
    Frame {
        #[serde(default)]
        title: String,
        #[serde(default)]
        role: FrameRole,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        order: Option<u32>,
        /// A format override (a preset name) for what is made in it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// A node kind of a newer peer: kept, drawn as unknown.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRole {
    /// Ports ([`EdgeRole::is_port`]): an edge into a card with one is a
    /// wire, landing on that port.
    FirstFrame,
    MiddleFrame,
    LastFrame,
    /// An image to keep (a character's sheet, a place, the style).
    Reference,
    Video,
    Audio,
    /// Context, or the line a speech card reads.
    Text,
    /// Not a wire: shot order when frames don't give it.
    Next,
    #[serde(other)]
    Unknown,
}

/// A link between two nodes (`from` feeds `to`): a wire when `role` is a
/// port and `to` a card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Edge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub role: EdgeRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Made by the server for a mention in the card's prompt ([`mention`]): removed with the token. A wire
    /// drawn by hand (or added with `add_edge`) has `false` and stays when the token goes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mention: bool,
}

/// What a card makes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GenKind {
    #[default]
    Image,
    Video,
    Speech,
    Sound,
    Music,
    #[serde(other)]
    Unknown,
}

impl GenKind {
    /// Its input ports, top to bottom.
    #[must_use]
    pub fn ports(self) -> &'static [EdgeRole] {
        use EdgeRole as R;
        match self {
            Self::Image => &[R::Reference, R::Text],
            Self::Video => &[R::FirstFrame, R::MiddleFrame, R::LastFrame, R::Reference, R::Video, R::Audio, R::Text],
            Self::Speech | Self::Sound | Self::Music => &[R::Text],
            Self::Unknown => &[],
        }
    }

    /// What its takes are.
    #[must_use]
    pub fn media(self) -> SourceMedia {
        match self {
            Self::Image => SourceMedia::Image,
            Self::Video => SourceMedia::Video,
            Self::Speech | Self::Sound | Self::Music => SourceMedia::Audio,
            Self::Unknown => SourceMedia::Unknown,
        }
    }

    /// For display: `Image`, `Video`, `Speech`, `Sound`, `Music`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Speech => "Speech",
            Self::Sound => "Sound",
            Self::Music => "Music",
            Self::Unknown => "Card",
        }
    }
}

/// What a source outputs, and what a port takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceMedia {
    Image,
    Video,
    Audio,
    Text,
    #[serde(other)]
    Unknown,
}

impl SourceMedia {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Image => "an image",
            Self::Video => "a video",
            Self::Audio => "audio",
            Self::Text => "text",
            Self::Unknown => "something unknown",
        }
    }
}

impl From<StudioMedia> for SourceMedia {
    fn from(m: StudioMedia) -> Self {
        match m {
            StudioMedia::Image => Self::Image,
            StudioMedia::Video => Self::Video,
            StudioMedia::Audio => Self::Audio,
            StudioMedia::Text => Self::Text,
            StudioMedia::Unknown => Self::Unknown,
        }
    }
}

impl EdgeRole {
    /// A port of some card (every role but `next`).
    #[must_use]
    pub fn is_port(self) -> bool {
        !matches!(self, Self::Next | Self::Unknown)
    }

    /// What the port takes: frames and `reference` an image, `video` a
    /// video, `audio` audio, `text` text.
    #[must_use]
    pub fn accepts(self) -> Option<SourceMedia> {
        match self {
            Self::FirstFrame | Self::MiddleFrame | Self::LastFrame | Self::Reference => Some(SourceMedia::Image),
            Self::Video => Some(SourceMedia::Video),
            Self::Audio => Some(SourceMedia::Audio),
            Self::Text => Some(SourceMedia::Text),
            Self::Next | Self::Unknown => None,
        }
    }

    /// Holds one wire (a new one replaces it): the frame ports.
    #[must_use]
    pub fn single(self) -> bool {
        matches!(self, Self::FirstFrame | Self::MiddleFrame | Self::LastFrame)
    }

    /// For display and messages: `first frame`, `reference`, …
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FirstFrame => "first frame",
            Self::MiddleFrame => "middle frame",
            Self::LastFrame => "last frame",
            Self::Reference => "reference",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Text => "text",
            Self::Next => "next",
            Self::Unknown => "unknown",
        }
    }
}

/// The port a `media` source dropped on a `makes` card's body lands on:
/// the first that takes it and is free (`taken`: the ports already wired);
/// the middle and last frames only by choice. An image onto a video card:
/// `first_frame` if free, else `reference`.
#[must_use]
pub fn default_port(makes: GenKind, media: SourceMedia, taken: &[EdgeRole]) -> Option<EdgeRole> {
    makes
        .ports()
        .iter()
        .copied()
        .filter(|p| !matches!(p, EdgeRole::MiddleFrame | EdgeRole::LastFrame))
        .find(|p| p.accepts() == Some(media) && !(p.single() && taken.contains(p)))
}

/// A card's result: a file in the canvas's `assets/`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Take {
    /// Root-relative.
    pub path: String,
    pub media: StudioMedia,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The prompt as sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// When it was made, unix ms.
    #[serde(default)]
    pub at: u64,
}

/// What a node outputs now ([`Canvas::output_of`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceOutput {
    pub media: SourceMedia,
    /// The file (root-relative): an asset, an entity's sheet, a card's
    /// picked take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A note's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// For messages: the file's name, the entity's or card's title.
    pub label: String,
    /// A card still queued or running: wait for it.
    #[serde(default)]
    pub pending: bool,
    /// Why it has nothing to give (`Run Keyframe 3 first`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

impl NodeBody {
    /// A card's picked take (`pick`, else the last).
    #[must_use]
    pub fn take(&self) -> Option<&Take> {
        let Self::Gen { takes, pick, .. } = self else { return None };
        pick.and_then(|i| takes.get(i as usize)).or_else(|| takes.last())
    }
}

/// A short stable hash (FNV-1a, 64 bits) for note keys.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn entity_of<'a>(sections: &'a [Section], id: &str) -> Option<&'a EntityInfo> {
    sections.iter().flat_map(|s| &s.entities).find(|e| e.id == id)
}

impl Canvas {
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// What node `id` outputs, without looking anything up: `None` for a
    /// frame, an unknown kind or a missing id.
    #[must_use]
    pub fn source_media(&self, id: &str) -> Option<SourceMedia> {
        match &self.node(id)?.body {
            NodeBody::Asset { media, .. } => Some((*media).into()),
            NodeBody::Entity { .. } => Some(SourceMedia::Image),
            body @ NodeBody::Gen { makes, .. } => Some(body.take().map_or(makes.media(), |t| t.media.into())),
            NodeBody::Note { .. } => Some(SourceMedia::Text),
            NodeBody::Frame { .. } | NodeBody::Unknown => None,
        }
    }

    /// Node `id`'s name in messages.
    #[must_use]
    pub fn label_of(&self, id: &str, sections: &[Section]) -> String {
        let Some(node) = self.node(id) else { return id.to_owned() };
        match &node.body {
            NodeBody::Asset { path, .. } => path.rsplit('/').next().unwrap_or(path).to_owned(),
            NodeBody::Entity { id } => entity_of(sections, id).map_or_else(|| id.clone(), |e| e.name.clone()),
            NodeBody::Gen { makes, title, .. } => {
                title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| format!("{} {}", makes.label(), node.id))
            }
            NodeBody::Note { text } => {
                let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Note");
                let short: String = first.trim_start_matches('#').trim().chars().take(40).collect();
                if short.is_empty() { "Note".into() } else { short }
            }
            NodeBody::Frame { title, .. } => title.clone(),
            NodeBody::Unknown => node.id.clone(),
        }
    }

    /// Node `id`'s document path, when it is a text asset (a document card).
    #[must_use]
    pub fn document_of(&self, id: &str) -> Option<&str> {
        match &self.node(id)?.body {
            NodeBody::Asset { path, media: StudioMedia::Text, .. } => Some(path),
            _ => None,
        }
    }

    /// What node `id` outputs now: an asset its file (a document: its path,
    /// the text is read by whoever compiles), an entity its sheet
    /// (from `sections`), a card its picked take (`pending` while queued or
    /// running; a `problem` without a take), a note its text. `None` for a
    /// frame, an unknown kind or a missing id.
    #[must_use]
    pub fn output_of(&self, id: &str, sections: &[Section]) -> Option<SourceOutput> {
        let media = self.source_media(id)?;
        let label = self.label_of(id, sections);
        let mut out = SourceOutput { media, path: None, text: None, label, pending: false, problem: None };
        match &self.node(id)?.body {
            NodeBody::Asset { path, .. } => out.path = Some(path.clone()),
            NodeBody::Entity { id } => match entity_of(sections, id).and_then(|e| e.sheet.clone()) {
                Some(sheet) => out.path = Some(sheet),
                None => out.problem = Some(format!("{} has no sheet yet", out.label)),
            },
            body @ NodeBody::Gen { status, .. } => {
                out.pending = matches!(status, GenStatus::Queued | GenStatus::Running);
                out.path = body.take().map(|t| t.path.clone());
                if out.path.is_none() && !out.pending {
                    out.problem = Some(format!("Run {} first", out.label));
                }
            }
            NodeBody::Note { text } => out.text = Some(text.clone()),
            NodeBody::Frame { .. } | NodeBody::Unknown => return None,
        }
        Some(out)
    }

    /// The wires into card `id`: by port (the card's order: frames, then
    /// reference, video, audio, text), and within a port in the order they
    /// were made (their order in `edges`): the first `reference` wire is
    /// image 1.
    #[must_use]
    pub fn inputs_of(&self, id: &str) -> Vec<&Edge> {
        let ports = match self.node(id).map(|n| &n.body) {
            Some(NodeBody::Gen { makes, .. }) => makes.ports(),
            _ => return Vec::new(),
        };
        let mut wires: Vec<&Edge> = self.edges.iter().filter(|e| e.to == id && ports.contains(&e.role)).collect();
        // Stable: edge order within a port.
        wires.sort_by_key(|e| ports.iter().position(|p| *p == e.role).unwrap_or(usize::MAX));
        wires
    }

    /// The key of what node `id` outputs now: `path@mtime` for a file
    /// (`mtime` gives a root-relative file's, in any unit; `None`: the path
    /// alone), a card's picked take's path, `note:<hash>` for a note's text.
    #[must_use]
    pub fn source_key(&self, id: &str, sections: &[Section], mtime: impl Fn(&str) -> Option<u64>) -> Option<String> {
        let file = |path: &str| Some(mtime(path).map_or_else(|| path.to_owned(), |m| format!("{path}@{m}")));
        match &self.node(id)?.body {
            NodeBody::Asset { path, .. } => file(path),
            NodeBody::Entity { id } => file(entity_of(sections, id)?.sheet.as_deref()?),
            body @ NodeBody::Gen { .. } => body.take().map(|t| t.path.clone()),
            NodeBody::Note { text } => Some(format!("note:{:016x}", fnv(text))),
            NodeBody::Frame { .. } | NodeBody::Unknown => None,
        }
    }

    /// The keys of card `id`'s inputs now, in [`Self::inputs_of`]'s order
    /// (sources without one left out): what a run stores as `inputs_used`.
    #[must_use]
    pub fn input_keys(&self, id: &str, sections: &[Section], mtime: impl Fn(&str) -> Option<u64>) -> Vec<String> {
        self.inputs_of(id).into_iter().filter_map(|e| self.source_key(&e.from, sections, &mtime)).collect()
    }

    /// Whether card `id` has a take made from other inputs than
    /// `keys_now` ([`Self::input_keys`]): its `inputs_used` differ, in any
    /// order. Drafts and running cards are not.
    #[must_use]
    pub fn out_of_date(&self, id: &str, keys_now: &[String]) -> bool {
        let Some(node) = self.node(id) else { return false };
        let NodeBody::Gen { takes, status, inputs_used, .. } = &node.body else { return false };
        if takes.is_empty() || matches!(status, GenStatus::Queued | GenStatus::Running) {
            return false;
        }
        let (mut used, mut now) = (inputs_used.clone(), keys_now.to_vec());
        used.sort();
        now.sort();
        used != now
    }

    /// What made card `id` out of date ([`Self::out_of_date`]), for
    /// display: the label of each wired source whose key isn't in its
    /// `inputs_used` (`Mara Venn changed`: its file at another mtime, or
    /// another take of a card; `Keyframe 2 is a new input`),
    /// and how many inputs it used are no longer wired. Empty when it
    /// isn't out of date.
    #[must_use]
    pub fn out_of_date_why(&self, id: &str, sections: &[Section], mtime: impl Fn(&str) -> Option<u64>) -> Vec<String> {
        let keys: Vec<(String, String)> =
            self.inputs_of(id).into_iter().filter_map(|e| Some((e.from.clone(), self.source_key(&e.from, sections, &mtime)?))).collect();
        let now: Vec<String> = keys.iter().map(|(_, k)| k.clone()).collect();
        if !self.out_of_date(id, &now) {
            return Vec::new();
        }
        let Some(NodeBody::Gen { inputs_used, .. }) = self.node(id).map(|n| &n.body) else { return Vec::new() };
        let path_of = |k: &str| k.rsplit_once('@').map_or(k, |(p, _)| p).to_owned();
        // The used key that source `from`'s key now replaces: the same file
        // at another mtime, another take of the same card, or (a note's
        // text edited) an earlier note.
        let earlier = |from: &str, key: &str, replaced: &[&String]| {
            let body = self.node(from).map(|n| &n.body);
            let takes = match body {
                Some(NodeBody::Gen { takes, .. }) => takes.as_slice(),
                _ => &[],
            };
            let note = matches!(body, Some(NodeBody::Note { .. }));
            inputs_used.iter().find(|u| {
                !now.contains(u)
                    && !replaced.contains(u)
                    && ((u.contains('@') && key.contains('@') && path_of(u) == path_of(key))
                        || takes.iter().any(|t| t.path == **u)
                        || (note && u.starts_with("note:")))
            })
        };
        let mut replaced: Vec<&String> = Vec::new();
        let mut out = Vec::new();
        for (from, key) in &keys {
            if inputs_used.contains(key) {
                continue;
            }
            let label = self.label_of(from, sections);
            match earlier(from, key, &replaced) {
                Some(used) => {
                    replaced.push(used);
                    out.push(format!("{label} changed"));
                }
                None => out.push(format!("{label} is a new input")),
            }
        }
        let gone = inputs_used.iter().filter(|u| !now.contains(u) && !replaced.contains(u)).count();
        if gone > 0 {
            out.push(format!("{gone} input{} no longer wired", if gone == 1 { "" } else { "s" }));
        }
        out
    }

    /// Whether an edge `from` → `to` with `role` may be added: a wire (a
    /// port role) goes into a card that has that port, from a source whose
    /// output the port takes, not already there and making no loop. Ok:
    /// the ids of wires it replaces (a frame port holds one). Err: why not.
    ///
    /// # Errors
    /// The reason, for the user.
    pub fn wire_check(&self, from: &str, to: &str, role: EdgeRole) -> Result<Vec<String>, String> {
        if from == to {
            return Err("a node can't be wired to itself".into());
        }
        let source = self.node(from).ok_or_else(|| format!("no node {from}"))?;
        let target = self.node(to).ok_or_else(|| format!("no node {to}"))?;
        match role {
            EdgeRole::Unknown => return Err("unknown edge role".into()),
            EdgeRole::Next => return Ok(Vec::new()),
            _ => {}
        }
        let NodeBody::Gen { makes, .. } = &target.body else {
            return Err(format!("{to} is not a card: wires go into cards (role next orders shots)"));
        };
        if !makes.ports().contains(&role) {
            let ports: Vec<&str> = makes.ports().iter().map(|p| p.name()).collect();
            let kind = makes.label().to_lowercase();
            let article = if kind.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
            return Err(format!("{article} {kind} card has no {} port (its ports: {})", role.name(), ports.join(", ")));
        }
        let media = self.source_media(from).ok_or_else(|| format!("{} has no output to wire", source.id))?;
        if role.accepts() != Some(media) {
            let wants = role.accepts().map_or("nothing", SourceMedia::name);
            return Err(format!("the {} port takes {wants}; {from} outputs {}", role.name(), media.name()));
        }
        if self.edges.iter().any(|e| e.from == from && e.to == to && e.role == role) {
            return Err(format!("{from} is already wired into {to}'s {} port", role.name()));
        }
        // A loop: `from` is fed, through wires, by `to`.
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![to];
        while let Some(at) = stack.pop() {
            if at == from {
                return Err(format!("wiring {from} into {to} would make a loop"));
            }
            if seen.insert(at) {
                stack.extend(self.edges.iter().filter(|e| e.from == at && e.role.is_port()).map(|e| e.to.as_str()));
            }
        }
        Ok(if role.single() { self.edges.iter().filter(|e| e.to == to && e.role == role).map(|e| e.id.clone()).collect() } else { Vec::new() })
    }
}

/// `add_edge`'s edge: without `id` one is made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NewEdge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub from: String,
    pub to: String,
    pub role: EdgeRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// How `layout` places nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Arrange {
    #[default]
    Row,
    Column,
    Grid,
    #[serde(other)]
    Unknown,
}

/// One edit of `studio.canvas_edit`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant, reason = "a wire type, matched by every peer")]
pub enum Op {
    AddNode {
        node: NewNode,
    },
    /// Omitted fields stay.
    UpdateNode {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        h: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        z: Option<i32>,
        /// The frame it moves into; `null`: out of any frame.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        #[schemars(with = "Option<String>")]
        parent: Option<Option<String>>,
        /// Merged into the body as a JSON merge patch (RFC 7396: `null`
        /// removes a field); `kind` can't change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<Value>,
    },
    /// Its edges go too; a frame's children stay (out of it).
    RemoveNode {
        id: String,
    },
    AddEdge {
        edge: NewEdge,
    },
    RemoveEdge {
        id: String,
    },
    /// Place `ids` in a row, column or grid, in `frame` (`in`) or after the
    /// content, `gap` apart.
    Layout {
        ids: Vec<String>,
        #[serde(rename = "as", default)]
        arrange: Arrange,
        #[serde(rename = "in", default, skip_serializing_if = "Option::is_none")]
        frame: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap: Option<f64>,
    },
    /// Re-flow the frames and loose nodes under `frame` (none: the top
    /// level), recursively, so nothing overlaps: scenes in a column, shots
    /// in a row (in `order`, then by position), each frame fitted to its
    /// contents, loose nodes kept together clear of the frames. Only
    /// positions and frames' sizes change.
    Tidy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<String>,
    },
    /// An op of a newer peer: fails alone.
    #[serde(other)]
    Unknown,
}

/// `studio.init`: make the project in the root; nothing is overwritten.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioInitParams {
    pub block_id: BlockId,
    pub name: String,
    /// The default preset; omitted: `landscape`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

/// `studio.set_format`: with `canvas`, its override (`None` clears it);
/// else the project default (`None` is refused).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioSetFormatParams {
    pub block_id: BlockId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<String>,
    /// A preset name.
    #[serde(default)]
    pub format: Option<String>,
}

/// `studio.set_formats`: add or replace presets.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioSetFormatsParams {
    pub block_id: BlockId,
    pub formats: BTreeMap<String, Format>,
}

/// `studio.canvas` and `studio.canvas_delete`: one canvas by name.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioCanvasParams {
    pub block_id: BlockId,
    pub name: String,
}

/// `studio.canvas_create`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioCanvasCreateParams {
    pub block_id: BlockId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The folder's name; omitted: a slug of the title (`ep01-pilot` for
    /// an episode). Taken: the next free one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `studio.canvas_update`: omitted fields stay; `null` clears.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioCanvasUpdateParams {
    pub block_id: BlockId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schemars(with = "Option<u32>")]
    pub episode: Option<Option<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schemars(with = "Option<String>")]
    pub format: Option<Option<String>>,
}

/// `studio.canvas_edit`: apply `ops` in order, each on its own: one that
/// fails (an id gone, say) is reported and the others apply.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioCanvasEditParams {
    pub block_id: BlockId,
    pub name: String,
    pub ops: Vec<Op>,
    /// The rev the ops were made against; behind: they still apply and the
    /// result says `moved`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_rev: Option<u64>,
}

/// `studio.canvas_edit`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CanvasEdited {
    pub rev: u64,
    /// One per `add_node`/`add_edge` op, in op order: the id given or
    /// made; empty when that op failed.
    #[serde(default)]
    pub ids: Vec<String>,
    /// `base_rev` was behind: the canvas had changed.
    #[serde(default)]
    pub moved: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<OpFailed>,
    /// What the mentions in the cards' prompts this edit set or changed did, part of the same write
    /// (docs/STUDIO.md §3, "Mentions"): wires made and removed, Entity nodes added, wires that couldn't be made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<MentionSynced>,
}

/// What a mention did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MentionDid {
    /// A token was written: its source is wired into the card (`edge`, with `mention` set); `node` is an Entity
    /// node added for it.
    Wired,
    /// A token was removed: the wire it had made (`edge`, as it was) is gone.
    Unwired,
    /// A token was written but no wire was made (`why`: a video or audio source, a port the card lacks, a loop).
    NotWired,
    #[serde(other)]
    Unknown,
}

/// One thing a mention did ([`CanvasEdited::mentions`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MentionSynced {
    /// The card whose prompt has (or had) the token.
    pub card: String,
    /// As written: `@characters/mara`, `@#n_3f2a9c`.
    pub token: String,
    pub did: MentionDid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<Edge>,
    /// An Entity node added for the token (it stays when the token goes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// An op that didn't apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpFailed {
    /// Its index in `ops`.
    pub op: u32,
    /// An [`crate::control::RpcError`] code (`not_found`, `bad_request`).
    pub code: String,
    pub message: String,
}

/// `studio.entity`: one entity by id (`characters/mara`).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioEntityParams {
    pub block_id: BlockId,
    pub id: String,
}

/// `studio.entity_create`: a folder in `section` (made when missing) named
/// by a slug of `name`, with its markdown file.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioEntityCreateParams {
    pub block_id: BlockId,
    /// A folder name under `world/` (`characters`).
    pub section: String,
    pub name: String,
    /// More frontmatter (`name` is set from `name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// `studio.entity_update`: `fields` are merged into the frontmatter (`null`
/// removes a key); `body` replaces the markdown.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioEntityUpdateParams {
    pub block_id: BlockId,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// `studio.entity_approve`: stamp the frontmatter's `approved` with now
/// (RFC 3339, UTC), or remove it. The entity needs a sheet to approve.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioEntityApproveParams {
    pub block_id: BlockId,
    pub id: String,
    pub approved: bool,
}

/// `studio.entity_ref`: one change to an entity's named references (its
/// frontmatter's `refs`, docs/STUDIO.md §1); every other line of the file
/// stays as written. Without a `refs` key, the images of its `refs/` folder
/// are written out first.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioEntityRefParams {
    pub block_id: BlockId,
    /// The entity (`characters/mara`).
    pub id: String,
    pub change: RefChange,
}

/// A change to an entity's references. `ref`: a reference's id
/// (`RefInfo::id`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RefChange {
    /// Add a reference: `path` root-relative, a file under the entity's
    /// folder (listed where it is), or anywhere else (root-relative, or
    /// absolute for this Mac's user only) copied into its `refs/` folder as
    /// `studio.import` copies (a taken name gets `-2`, …); with a `name`,
    /// the copy is named after the name's slug when no file in `refs/` has
    /// that stem (`Crown off`: `refs/crown-off.png`), since the file's stem
    /// makes the id. A file listed already is named and described rather
    /// than listed twice.
    Add {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        defines: Option<String>,
        /// Its place in the list (0: first); the end when missing or past it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<usize>,
    },
    /// Name it (its id stays: it is made of the file's stem); `None` or
    /// empty: back to its file's stem.
    Rename {
        #[serde(rename = "ref")]
        ref_id: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Say what to take from it; `None` or empty: nothing said.
    Describe {
        #[serde(rename = "ref")]
        ref_id: String,
        #[serde(default)]
        defines: Option<String>,
    },
    /// Move it to place `to` (0: first; past the end: last).
    Move {
        #[serde(rename = "ref")]
        ref_id: String,
        to: usize,
    },
    /// Take it off the list; the file stays.
    Remove {
        #[serde(rename = "ref")]
        ref_id: String,
    },
}

/// `studio.entity_ref`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EntityRefEdited {
    pub info: EntityInfo,
    /// The id of the reference changed, as it is now (a rename changes it);
    /// `None` after `remove`.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<String>,
}

/// `studio.entity_link`: link entity `id` with entity `other` (written in
/// `id`'s frontmatter `links`), or unlink them (from both files' `links`,
/// wherever it is declared). Linking a pair linked from either end
/// already changes nothing.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioEntityLinkParams {
    pub block_id: BlockId,
    pub id: String,
    pub other: String,
    pub linked: bool,
}

/// `studio.set_models`: merged into `studio.toml`'s `[models]`; `null`
/// removes a kind's pick (Automatic).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioSetModelsParams {
    pub block_id: BlockId,
    /// By kind (`image`, `video`, `speech`, …): a model handle, or `null`.
    pub models: BTreeMap<String, Option<String>>,
}

/// `studio.production`'s result (docs/STUDIO.md §4.6): where the project
/// stands, derived from the files on every call; nothing is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Production {
    /// Every entity of the world's entity sections, in section order, then
    /// by name (as `studio.world`).
    #[serde(default)]
    pub assets: Vec<AssetState>,
    /// The episodes by number, then the other canvases in tab order.
    #[serde(default)]
    pub episodes: Vec<EpisodeState>,
    /// `[models]` ([`Project::models`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, String>,
    /// At most three, in production order.
    #[serde(default)]
    pub next: Vec<Step>,
}

/// An entity's readiness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AssetState {
    /// `characters/mara`.
    pub id: String,
    pub name: String,
    /// Its section's id (`characters`).
    pub section: String,
    /// Root-relative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    #[serde(default)]
    pub approval: Approval,
    /// Characters that speak: whether `voice` names a provider and voice.
    /// Absent for other sections and for a character whose `voice` is
    /// `none` (or `false`, `silent`, `mute`): it has no voice to cast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<bool>,
    /// Characters and props: whether a video is among its refs (the
    /// frontmatter's `refs`, or its `refs/` folder). Absent for others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_sheet: Option<bool>,
    /// How many canvases use it ([`Entity::used_in`]'s rules).
    #[serde(default)]
    pub used: u32,
}

/// A canvas's stages (an episode's, or another canvas's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EpisodeState {
    pub canvas: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    /// `canvases/<canvas>/script.md` exists and isn't blank.
    #[serde(default)]
    pub script: bool,
    /// Shot frames.
    #[serde(default)]
    pub shots: u32,
    /// Shot frames holding an image: an image card with a take, or an image
    /// asset (in the frame or a frame inside it).
    #[serde(default)]
    pub keyframes: u32,
    /// Shot frames holding a video: a video card with a take, or a video
    /// asset.
    #[serde(default)]
    pub videos: u32,
    /// Speech, sound and music cards with a take, and audio assets,
    /// anywhere on the canvas.
    #[serde(default)]
    pub sound: u32,
    /// Cards out of date ([`Canvas::out_of_date`]).
    #[serde(default)]
    pub out_of_date: u32,
    #[serde(default)]
    pub failed: u32,
    /// Queued or running.
    #[serde(default)]
    pub running: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut: Option<Cut>,
}

/// An episode's newest cut: `canvases/<c>/out/<c>-v<N>.mp4` with the
/// highest `N`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Cut {
    /// Root-relative.
    pub path: String,
    pub version: u32,
    /// The other versions' paths, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub older: Vec<String>,
    /// The file's mtime, unix ms.
    #[serde(default)]
    pub at: u64,
    /// A picked take of a video, speech, sound or music card is newer than
    /// it: cut again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

/// What a next step is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// `world/overview.md` is empty: the onboarding interview.
    Onboarding,
    /// An entity without a sheet.
    Sheet,
    /// A sheet to approve (or approve again): the user's step.
    Approve,
    /// A speaking character without a voice.
    Voice,
    /// No episode canvas yet.
    Episode,
    Script,
    Shots,
    Keyframes,
    Video,
    /// No cut yet, or a stale one.
    Cut,
    OutOfDate,
    #[serde(other)]
    Unknown,
}

/// One next step: a button. With `ask` it sends that message to the linked
/// agent; without, it opens `entity` (or `canvas`) for the user to decide.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Step {
    pub kind: StepKind,
    /// The button's label (`Make Mara Venn's sheet`).
    pub text: String,
    /// A whole message for the agent, naming the entity or canvas by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<String>,
    /// The entity it is about (`characters/mara`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    /// The canvas it is about (`ep01-pilot`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<String>,
}

/// `studio.asset`: a chunk of any file under the root.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioAssetParams {
    pub block_id: BlockId,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// At most [`StudioAsset::MAX_CHUNK`] (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

/// `studio.asset`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StudioAsset {
    pub mime: String,
    /// The whole file's.
    pub size: u64,
    pub offset: u64,
    pub data_base64: String,
}

impl StudioAsset {
    pub const MAX_CHUNK: u64 = 4 * 1024 * 1024;
}

/// `studio.import`: copy a file into a canvas's `assets/` (a taken name
/// gets the next free one).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StudioImportParams {
    pub block_id: BlockId,
    pub canvas: String,
    /// Root-relative, or absolute (a file the caller may read).
    pub from: String,
}

/// `studio.import`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StudioImported {
    /// Root-relative.
    pub path: String,
}

/// `studio.set`: change the block's view state; omitted fields stay.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct StudioSetParams {
    pub block_id: BlockId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<StudioView>,
    /// Open tabs, in order (replaces the list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<Vec<String>>,
    /// The canvas shown; one not open is opened (appended). `null`: none.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schemars(with = "Option<String>")]
    pub active: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schemars(with = "Option<String>")]
    pub world_section: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schemars(with = "Option<String>")]
    pub world_entity: Option<Option<String>>,
    /// The agent it talks to; `null` unlinks.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schemars(with = "Option<AgentLink>")]
    pub agent: Option<Option<AgentLink>>,
}

/// What `studio.changed` is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StudioChange {
    /// `studio.toml` or the canvas list: re-read `studio.project`.
    Project,
    /// Something under `world/`: re-read `studio.world` (and the entity shown).
    World,
    /// Canvas `name`: re-read it.
    Canvas,
    #[serde(other)]
    Unknown,
}
