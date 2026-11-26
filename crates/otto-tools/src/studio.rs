//! Studio canvases (docs/STUDIO.md §6.2) as the media tools and session-less card runs reach them: the [`Studio`]
//! bridge (Silo's, in `otto-silo-tools`) and the plain data a run needs. The bridge resolves a card with
//! `silo_proto::studio`'s helpers (wires by port, each source's output and key, the effective format), so the
//! crates running cards don't depend on Silo's.
use crate::ToolError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// What a card makes (`NodeBody::Gen.makes`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Makes {
    #[default]
    Image,
    Video,
    Speech,
    Sound,
    Music,
    #[serde(other)]
    Unknown,
}

impl Makes {
    /// `Image`, `Video`, `Speech`, …
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
    /// `image`, `video`, `speech`, …: its name in `studio.toml`'s `[models]`.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Speech => "speech",
            Self::Sound => "sound",
            Self::Music => "music",
            Self::Unknown => "unknown",
        }
    }
}

/// A card's input port (`EdgeRole`, the wire roles only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Port {
    FirstFrame,
    MiddleFrame,
    LastFrame,
    Reference,
    Video,
    Audio,
    Text,
    #[serde(other)]
    Unknown,
}

impl Port {
    /// `first_frame`, `reference`, …: as on the wire.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::FirstFrame => "first_frame",
            Self::MiddleFrame => "middle_frame",
            Self::LastFrame => "last_frame",
            Self::Reference => "reference",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Text => "text",
            Self::Unknown => "unknown",
        }
    }
    /// `first frame`, `reference`, …: in messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FirstFrame => "first frame",
            Self::MiddleFrame => "middle frame",
            Self::LastFrame => "last frame",
            other => other.id(),
        }
    }
    /// What it takes.
    #[must_use]
    pub fn accepts(self) -> SourceMedia {
        match self {
            Self::FirstFrame | Self::MiddleFrame | Self::LastFrame | Self::Reference => SourceMedia::Image,
            Self::Video => SourceMedia::Video,
            Self::Audio => SourceMedia::Audio,
            Self::Text => SourceMedia::Text,
            Self::Unknown => SourceMedia::Unknown,
        }
    }
}

/// What a source outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// `an image`, `a video`, `audio`, `text`.
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

/// A card's state (`GenStatus`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
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

/// One wire into a card, as its source outputs now (`Canvas::output_of`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CardInput {
    pub port: Port,
    /// The source node.
    pub from: String,
    pub media: SourceMedia,
    /// Its file, root-relative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A note's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// For messages (`Keyframe 3`, `Mara Venn`), and what a mention of the source expands to.
    pub label: String,
    /// The world entity (`characters/mara`) when the source is its Entity node: what `@characters/mara` matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    /// A card still queued or running.
    #[serde(default)]
    pub pending: bool,
    /// Why it gives nothing (`Run Keyframe 3 first`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// A format preset with its name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Format {
    pub name: String,
    /// `W:H`.
    pub aspect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

/// A card as a run sees it: its fields, its wires resolved, its inputs' keys and its effective format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Card {
    /// The studio's root, absolute.
    pub root: PathBuf,
    pub canvas: String,
    pub node: String,
    /// For messages: its title, else `Image n_3f2a9c`.
    pub label: String,
    /// The frame it is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    pub makes: Makes,
    #[serde(default)]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default)]
    pub options: Value,
    #[serde(default)]
    pub status: Status,
    /// Its takes as stored (`Take`s), oldest first.
    #[serde(default)]
    pub takes: Vec<Value>,
    /// In `Canvas::inputs_of`'s order.
    #[serde(default)]
    pub inputs: Vec<CardInput>,
    /// `Canvas::input_keys`: what a run stores as `inputs_used`.
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    /// Every token in `prompt` that resolves (`silo_proto::studio::mention`), once each, wired or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<CardMention>,
}

/// A token of a card's prompt that names something: an entity of the world or a node of the canvas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardMention {
    /// As written: `@characters/mara`, `@#n_3f2a9c`.
    pub token: String,
    /// The entity's name, a reference's with the entity's (`Mara Venn, Coat`) or the node's label: what the token
    /// expands to when nothing is wired from it.
    pub label: String,
    /// A named reference's file, root-relative (`@characters/mara#coat`): the wires from it are its.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What a named reference defines: its job, said where it is numbered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defines: Option<String>,
}

impl CardMention {
    /// A mention of an entity or a node: no reference's file or job.
    #[must_use]
    pub fn new(token: String, label: String) -> Self {
        Self { token, label, path: None, defines: None }
    }
}

/// What a root-relative file is on a canvas: a node outputting it, else a world entity whose sheet or ref it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The entity id (`characters/mara`) when no node outputs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
}

/// `studio.canvas_edit`'s answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edited {
    pub rev: u64,
    /// One per `add_node`/`add_edge` op, in op order; empty for one that failed.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Each failed op's message.
    #[serde(default)]
    pub failed: Vec<String>,
}

/// Silo's Studio blocks: projects, canvases and the world, read and edited through the block holding a studio.
/// Raw reads are the server's JSON (`Project`, `Canvas`, `StudioWorld`); ops are `studio.canvas_edit` `Op`s.
#[async_trait::async_trait]
pub trait Studio: Send + Sync {
    /// The Studio block for the studio holding `cwd`: one linked to the agent block showing `sessions`, else any on
    /// that root; with `open`, one is opened beside that agent block when there is none. `None`: none (not opened).
    async fn block(&self, cwd: &Path, sessions: &[String], open: bool) -> Result<Option<String>, ToolError>;
    async fn project(&self, block: &str) -> Result<Value, ToolError>;
    async fn canvas(&self, block: &str, canvas: &str) -> Result<Value, ToolError>;
    async fn world(&self, block: &str) -> Result<Value, ToolError>;
    /// Apply `ops` (each on its own).
    async fn edit(&self, block: &str, canvas: &str, ops: Vec<Value>) -> Result<Edited, ToolError>;
    /// Card `node` with its wires resolved.
    async fn card(&self, block: &str, canvas: &str, node: &str) -> Result<Card, ToolError>;
    /// Each root-relative path's [`Source`] on `canvas`, `None` when it is neither a node's output nor an entity's file.
    async fn sources(&self, block: &str, canvas: &str, paths: &[String]) -> Result<Vec<Option<Source>>, ToolError>;
}

/// The studio holding `dir`: the nearest folder up from it with a `studio.toml`.
#[must_use]
pub fn root_for(dir: &Path) -> Option<PathBuf> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir.ancestors().find(|a| a.join("studio.toml").is_file()).map(Path::to_path_buf)
}

/// A world entity as read from disk (docs/STUDIO.md §1); paths root-relative.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldEntity {
    /// `characters/mara`.
    pub id: String,
    pub name: String,
    /// Its section's folder (`characters`).
    pub section: String,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// Its references' files, in order (those of [`Self::looks`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    /// Its named references, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub looks: Vec<WorldRef>,
    /// The entities its frontmatter's `links` names (existing ones), and with [`entities`] or [`entity`] those
    /// whose `links` name it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// A named reference of an entity (a `refs` entry, `silo_proto::studio::RefInfo` as read here).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldRef {
    /// Unique within the entity: the slug of its name, else of its file's stem (`crown-off`).
    pub id: String,
    /// Root-relative.
    pub path: String,
    /// Its `name`, else its file's stem.
    pub name: String,
    /// What to take from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defines: Option<String>,
}

impl WorldRef {
    /// `video`, `audio`, else `look` (an image: a look, an angle, a variant), by the file's extension.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        let ext = self.path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "mp4" | "m4v" | "mov" | "webm" | "mkv" => "video",
            "mp3" | "wav" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "flac" => "audio",
            _ => "look",
        }
    }
}

impl WorldEntity {
    /// `character` for `characters`, the folder's name otherwise without a trailing `s`.
    #[must_use]
    pub fn kind(&self) -> String {
        self.section.strip_suffix('s').unwrap_or(&self.section).replace(['-', '_'], " ")
    }

    /// Its reference `id`.
    #[must_use]
    pub fn look(&self, id: &str) -> Option<&WorldRef> {
        self.looks.iter().find(|r| r.id == id)
    }
}

/// The slug of a reference id: lowercase ASCII letters and digits, one `-` between words, at most 48 characters
/// (as `silo_proto::studio::ref_slug`).
fn ref_slug(text: &str) -> String {
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

/// A file's stem (as `silo_proto::studio::file_stem`).
fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// The ids of references `(name, path)` in order (as `silo_proto::studio::ref_ids`): the slug of the file's stem,
/// else of the name, else `ref`; one taken already gets `-2`, `-3`…
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

/// A scalar of the frontmatter: unquoted (`"…"` as JSON), a trailing ` #` comment dropped.
fn unquote(text: &str) -> String {
    let text = text.trim();
    if text.starts_with('"')
        && let Ok(s) = serde_json::from_str::<String>(text.split_once(" #").map_or(text, |(a, _)| a).trim())
    {
        return s;
    }
    if let Some(inner) = text.strip_prefix('\'') {
        return inner[..inner.rfind('\'').unwrap_or(inner.len())].replace("''", "'");
    }
    text.split_once(" #").map_or(text, |(a, _)| a).trim().to_owned()
}

/// `key: value` in a table entry; `None` for a plain item.
fn pair(text: &str) -> Option<(&str, &str)> {
    if text.starts_with(['"', '\'', '[', '{', '#']) {
        return None;
    }
    let (key, value) = text.split_once(": ").or_else(|| Some((text.strip_suffix(':')?, "")))?;
    let key = key.trim();
    (!key.is_empty() && !key.contains(['"', '\'', '[', '{', '#'])).then_some((key, value.trim()))
}

/// The inside of a flow list or table split at its top-level commas.
fn split_flow(inner: &str) -> Vec<&str> {
    let (mut out, mut start, mut depth, mut quote) = (Vec::new(), 0, 0i32, None::<char>);
    for (i, c) in inner.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&inner[start..]);
    out.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// A `refs` entry: its file, name and job.
type RefEntry = (String, Option<String>, Option<String>);

/// A table's entry from its `k: v` texts.
fn table<'a>(pairs: impl Iterator<Item = &'a str>) -> Option<RefEntry> {
    let (mut file, mut name, mut defines) = (None, None, None);
    for (k, v) in pairs.filter_map(pair) {
        let v = Some(unquote(v)).filter(|v| !v.is_empty());
        match k {
            "file" | "path" => file = v,
            "name" => name = v,
            "defines" => defines = v,
            _ => {}
        }
    }
    Some((file?, name, defines))
}

/// A flow item: a path, or `{file: …, name: …}` (or JSON).
fn flow_item(text: &str) -> Option<RefEntry> {
    if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
            let s = |k: &str| map.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
            return Some((s("file").or_else(|| s("path"))?, s("name"), s("defines")));
        }
        return table(split_flow(inner).into_iter());
    }
    Some(unquote(text)).filter(|t| !t.is_empty()).map(|t| (t, None, None))
}

/// The entries of a frontmatter's `refs`: inline (`[a, {file: b, name: B}]`, or `a, b`) or one a line, each a
/// path or a table (`- file: …` with more `k: v` lines under it). `None` without a `refs` key.
fn ref_entries(text: &str) -> Option<Vec<RefEntry>> {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return None;
    }
    let mut lines = lines.take_while(|l| l.trim_end() != "---");
    let rest = lines.by_ref().find_map(|l| l.strip_prefix("refs:"))?.trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        let inner = rest.strip_prefix('[').map_or(rest, |r| r.rsplit_once(']').map_or(r, |(a, _)| a));
        return Some(split_flow(inner).into_iter().filter_map(flow_item).collect());
    }
    let block: Vec<&str> = lines
        .take_while(|l| l.starts_with([' ', '\t', '-']) || l.trim().is_empty())
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect();
    let is_item = |l: &str| l == "-" || l.starts_with("- ") || l.starts_with("-\t");
    let mut items: Vec<Vec<&str>> = Vec::new();
    let base = block.first().map_or(0, |l| l.len() - l.trim_start().len());
    for line in block {
        if line.len() - line.trim_start().len() <= base && is_item(line.trim_start()) {
            items.push(vec![line.trim_start()[1..].trim()]);
        } else if let Some(item) = items.last_mut() {
            item.push(line.trim());
        }
    }
    Some(
        items
            .into_iter()
            .filter_map(|item| {
                if pair(item[0]).is_some() || (item[0].is_empty() && item[1..].iter().any(|l| pair(l).is_some())) {
                    table(item.into_iter())
                } else {
                    flow_item(&item.into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" "))
                }
            })
            .collect(),
    )
}

/// `rel` relative to root-relative folder `dir`, as a root-relative path; `None` when it leaves the root or is
/// absolute.
fn under(dir: &str, rel: &str) -> Option<String> {
    let rel = rel.trim();
    if rel.is_empty() || rel.starts_with(['/', '~']) {
        return None;
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// A written link (`props/mace`, `@props/mace`, `world/props/mace/`) as an entity id.
fn link_id(text: &str) -> Option<String> {
    let id = text.trim().trim_matches(['"', '\'']).trim_start_matches('@').trim_start_matches("world/").trim_matches('/');
    let (section, name) = id.split_once('/')?;
    let plain = |s: &str| !s.is_empty() && !s.starts_with('.') && !s.contains(['/', '\\']);
    (plain(section) && plain(name)).then(|| id.to_owned())
}

/// Lenient frontmatter: `key: value` lines, inline `[a, b]` and `- item` lists (joined with `, `).
fn frontmatter(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut fields = std::collections::BTreeMap::new();
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return fields;
    }
    let mut last: Option<String> = None;
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if let (Some(item), Some(key)) = (line.trim_start().strip_prefix("- "), last.as_ref())
            && line.starts_with([' ', '-', '\t'])
        {
            let entry: &mut String = fields.entry(key.clone()).or_default();
            if !entry.is_empty() {
                entry.push_str(", ");
            }
            entry.push_str(item.trim().trim_matches(['"', '\'']));
            continue;
        }
        let Some((key, value)) = line.split_once(':').filter(|_| !line.starts_with([' ', '\t', '#'])) else { continue };
        let value = value.split(" #").next().unwrap_or_default().trim().trim_matches(['"', '\'']);
        let value = value.trim_start_matches('[').trim_end_matches(']');
        let joined: Vec<&str> = value.split(',').map(|v| v.trim().trim_matches(['"', '\''])).filter(|v| !v.is_empty()).collect();
        fields.insert(key.trim().to_owned(), joined.join(", "));
        last = Some(key.trim().to_owned());
    }
    fields
}

fn is_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif"].iter().any(|e| lower.ends_with(e))
}

fn sorted_dir(dir: &Path) -> Vec<std::fs::DirEntry> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    entries
}

/// Entity `id` (`characters/mara`) of the studio at `root`, from its folder under `world/`.
#[must_use]
pub fn entity(root: &Path, id: &str) -> Option<WorldEntity> {
    let (section, folder) = id.trim_matches('/').split_once('/')?;
    let plain = |s: &str| !s.is_empty() && !s.starts_with('.') && s != ".." && !s.contains(['/', '\\']);
    if !plain(section) || !plain(folder) {
        return None;
    }
    let file = entity_file(&root.join("world").join(section).join(folder), folder)?;
    let mut found = declared(root, section, folder, &file)?;
    // Links read from both ends.
    for other in own_entities(root) {
        if other.id != found.id && other.links.contains(&found.id) && !found.links.contains(&other.id) {
            found.links.push(other.id);
        }
    }
    Some(found)
}

/// Entity `section/folder` from its markdown `file`, its links those it declares.
fn declared(root: &Path, section: &str, folder: &str, file: &Path) -> Option<WorldEntity> {
    let dir = root.join("world").join(section).join(folder);
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let fields = frontmatter(&text);
    let base = format!("world/{section}/{folder}");
    let entries = ref_entries(&text).unwrap_or_else(|| {
        sorted_dir(&dir.join("refs"))
            .into_iter()
            .filter_map(|e| e.file_name().to_str().filter(|n| is_image(n)).map(|n| (format!("refs/{n}"), None, None)))
            .collect()
    });
    let placed: Vec<(&RefEntry, String)> = entries.iter().filter_map(|e| Some((e, under(&base, &e.0)?))).collect();
    let ids = ref_ids(&placed.iter().map(|((_, name, _), path)| (name.as_deref(), path.as_str())).collect::<Vec<_>>());
    let looks: Vec<WorldRef> = placed
        .into_iter()
        .zip(ids)
        .map(|(((_, name, defines), path), id)| WorldRef {
            id,
            name: name.clone().unwrap_or_else(|| file_stem(&path).to_owned()),
            defines: defines.clone(),
            path,
        })
        .collect();
    let id = format!("{section}/{folder}");
    let mut links: Vec<String> = Vec::new();
    for link in fields.get("links").map(|l| l.split(", ").filter_map(link_id).collect::<Vec<_>>()).unwrap_or_default() {
        if link != id && root.join("world").join(&link).is_dir() && !links.contains(&link) {
            links.push(link);
        }
    }
    Some(WorldEntity {
        name: fields.get("name").filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| folder.to_owned()),
        section: section.to_owned(),
        file: format!("{base}/{}", file.file_name()?.to_str()?),
        sheet: fields.get("sheet").filter(|s| !s.is_empty()).and_then(|s| under(&base, s)),
        refs: looks.iter().map(|l| l.path.clone()).collect(),
        looks,
        links,
        summary: fields.get("summary").filter(|s| !s.is_empty()).cloned(),
        id,
    })
}

/// The markdown file of entity folder `dir`: `<folder>.md`, else the first `.md` file.
fn entity_file(dir: &Path, folder: &str) -> Option<std::path::PathBuf> {
    let own = dir.join(format!("{folder}.md"));
    if own.is_file() {
        return Some(own);
    }
    sorted_dir(dir).into_iter().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "md") && p.is_file())
}

/// Every entity of the studio at `root` by section then folder, each with the links it declares.
fn own_entities(root: &Path) -> Vec<WorldEntity> {
    let mut out = Vec::new();
    for section in sorted_dir(&root.join("world")).into_iter().filter(|e| e.path().is_dir()) {
        let Some(section) = section.file_name().to_str().filter(|s| !s.starts_with('.')).map(str::to_owned) else { continue };
        for folder in sorted_dir(&root.join("world").join(&section)).into_iter().filter(|e| e.path().is_dir()) {
            let Some(folder) = folder.file_name().to_str().map(str::to_owned) else { continue };
            let Some(file) = entity_file(&root.join("world").join(&section).join(&folder), &folder) else { continue };
            if !folder.starts_with('.')
                && let Some(found) = declared(root, &section, &folder, &file)
            {
                out.push(found);
            }
        }
    }
    out
}

/// Every entity of the studio at `root`, by section then folder; links from both ends.
#[must_use]
pub fn entities(root: &Path) -> Vec<WorldEntity> {
    let mut out = own_entities(root);
    let declared: Vec<(String, Vec<String>)> = out.iter().map(|e| (e.id.clone(), e.links.clone())).collect();
    for e in &mut out {
        let mut back: Vec<&String> = declared.iter().filter(|(other, links)| *other != e.id && links.contains(&e.id)).map(|(o, _)| o).collect();
        back.sort();
        for other in back {
            if !e.links.contains(other) {
                e.links.push(other.clone());
            }
        }
    }
    out
}

/// What an entity mention's target names (`characters/mara`, or `characters/mara#coat` for one of its
/// references): the entity, and the reference when one is named. `None` when either isn't there.
#[must_use]
pub fn mention_target(root: &Path, target: &str) -> Option<(WorldEntity, Option<WorldRef>)> {
    let (id, look) = target.split_once('#').map_or((target, None), |(e, r)| (e, Some(r)));
    let e = entity(root, id)?;
    match look {
        None => Some((e, None)),
        Some(r) => {
            let found = e.look(r)?.clone();
            Some((e, Some(found)))
        }
    }
}

/// `world/style.md`'s text under `## Style line` (to the next heading), on one line: how a frame looks, added to
/// image and video cards when they run.
#[must_use]
pub fn style_line(root: &Path) -> Option<String> {
    style_section(root, "style line")
}

/// `world/style.md`'s text under `## Motion line` (to the next heading), on one line: how things move, added to
/// video cards after the style line when they run.
#[must_use]
pub fn motion_line(root: &Path) -> Option<String> {
    style_section(root, "motion line")
}

/// `world/style.md`'s text under the heading `title` (any level, matched case-insensitively) to the next heading,
/// its non-empty lines joined on one line; `None` without the file, the heading or any text under it.
fn style_section(root: &Path, title: &str) -> Option<String> {
    let text = std::fs::read_to_string(root.join("world/style.md")).ok()?;
    let heading = |l: &str| l.trim_start().starts_with('#') && l.trim_start().trim_start_matches('#').trim().eq_ignore_ascii_case(title);
    let mut lines = text.lines().skip_while(|l| !heading(l));
    lines.next()?;
    let body: Vec<&str> = lines.take_while(|l| !l.trim_start().starts_with('#')).map(str::trim).filter(|l| !l.is_empty()).collect();
    (!body.is_empty()).then(|| body.join(" "))
}

/// `studio.toml`'s `[models]` picks (`video = "kling-3.0-pro"`), read leniently.
#[must_use]
pub fn model_picks(root: &Path) -> std::collections::BTreeMap<String, String> {
    let text = std::fs::read_to_string(root.join("studio.toml")).unwrap_or_default();
    let mut picks = std::collections::BTreeMap::new();
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == "[models]";
        } else if inside && let Some((key, value)) = line.split_once('=') {
            let value = value.split(" #").next().unwrap_or_default().trim().trim_matches(['"', '\'']);
            if !value.is_empty() {
                picks.insert(key.trim().trim_matches('"').to_owned(), value.to_owned());
            }
        }
    }
    picks
}

/// The effective format of canvas `name` read from disk (no frames): the canvas's `format`, else `studio.toml`'s,
/// with its `[formats.<name>]` preset (read leniently, as [`model_picks`]); `None` without a preset.
#[must_use]
pub fn format_of(root: &Path, canvas: &str) -> Option<Format> {
    let text = std::fs::read_to_string(root.join("studio.toml")).ok()?;
    let value = |v: &str| v.split(" #").next().unwrap_or_default().trim().trim_matches(['"', '\'']).to_owned();
    let (mut project, mut presets) = (None, std::collections::BTreeMap::<String, Format>::new());
    let mut section: Option<String> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if let Some(head) = line.strip_prefix('[') {
            let head = head.split(']').next().unwrap_or_default().trim();
            section = head.strip_prefix("formats.").map(|n| n.trim_matches('"').to_owned());
            if section.is_none() && head != "formats" {
                section = Some(String::new());
            }
            continue;
        }
        let Some((key, v)) = line.split_once('=') else { continue };
        let (key, v) = (key.trim(), value(v));
        match &section {
            None if key == "format" => project = Some(v),
            Some(name) if !name.is_empty() => {
                let f =
                    presets.entry(name.clone()).or_insert_with(|| Format { name: name.clone(), aspect: String::new(), resolution: None, fps: None });
                match key {
                    "aspect" => f.aspect = v,
                    "resolution" => f.resolution = Some(v),
                    "fps" => f.fps = v.parse().ok(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let own = canvas_file(root, canvas).and_then(|c| c["format"].as_str().map(str::to_owned));
    [own, project].into_iter().flatten().find_map(|name| presets.remove(&name)).filter(|f| !f.aspect.is_empty())
}

/// `canvases/<name>/canvas.json` of the studio at `root`, read only.
#[must_use]
pub fn canvas_file(root: &Path, name: &str) -> Option<Value> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(root.join("canvases").join(name).join("canvas.json")).ok()?).ok()
}

/// The canvases' folder names of the studio at `root`, with their titles and episode numbers.
#[must_use]
pub fn canvases(root: &Path) -> Vec<(String, String, Option<u64>)> {
    sorted_dir(&root.join("canvases"))
        .into_iter()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            let canvas = canvas_file(root, &name)?;
            let title = canvas["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(&name).to_owned();
            Some((name, title, canvas["episode"].as_u64()))
        })
        .collect()
}

/// An `@` candidate of a studio (`studio.mentions`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// A world entity; else a canvas.
    pub entity: bool,
    /// `characters/mara`, `ep01-pilot`.
    pub target: String,
    /// `Mara Venn`, `Pilot`.
    pub name: String,
    /// `Character`, `Canvas · Ep 1`, `Canvas`.
    pub detail: String,
}

/// The entities and canvases of the studio at `root` matching `query` as the `@` popup matches files
/// (`files.complete`): a fuzzy subsequence of the name or the target, prefix and word-start matches first; at
/// most `limit`. An empty query lists them all (entities by section, then canvases), keeping room for up to half
/// of `limit` canvases however many entities there are. A `canvas#text` query lists that canvas's nodes instead
/// ([`nodes`]); an `entity#text` one that entity's named references ([`looks`]): the entity by id
/// (`characters/mara#`), else, when no canvas has the name, by its folder or name (`mara#`, `Mara Venn#`).
/// A plain query also lists the named references whose name or id contains it (case aside; not fuzzy, to keep
/// noise down), each right after its entity's row when that is listed, else after the entities.
#[must_use]
pub fn mentions(root: &Path, query: &str, limit: usize) -> Vec<Candidate> {
    if let Some((head, text)) = query.split_once('#') {
        let head = head.trim();
        let named = if head.contains('/') {
            entity(root, head)
        } else if canvas_file(root, head).is_some() {
            None
        } else {
            entities(root).into_iter().find(|e| e.id.rsplit('/').next() == Some(head) || e.name.eq_ignore_ascii_case(head))
        };
        return match named {
            Some(e) => looks(&e, text, limit),
            None => nodes(root, head, text, limit),
        };
    }
    let world = entities(root);
    let entities: Vec<Candidate> = world
        .iter()
        .map(|e| {
            let kind = e.kind();
            let detail = kind.chars().next().map(|c| c.to_uppercase().chain(kind.chars().skip(1)).collect()).unwrap_or_default();
            Candidate { entity: true, target: e.id.clone(), name: e.name.clone(), detail }
        })
        .collect();
    let canvases: Vec<Candidate> = canvases(root)
        .into_iter()
        .map(|(name, title, episode)| Candidate {
            entity: false,
            target: name,
            name: title,
            detail: episode.map_or_else(|| "Canvas".to_owned(), |ep| format!("Canvas · Ep {ep}")),
        })
        .collect();
    let needle: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        let room = limit.saturating_sub(canvases.len().min(limit / 2));
        let shown = entities.len().min(room);
        return entities.into_iter().take(shown).chain(canvases).take(limit).collect();
    }
    let lower = query.trim().to_lowercase();
    let mut found: Vec<(i32, usize, Candidate)> = entities
        .into_iter()
        .chain(canvases)
        .enumerate()
        .filter_map(|(i, c)| {
            // The name whole (word starts count), the target by its last segment first.
            let best = [crate::complete::score(&needle, &c.name.replace('/', " "), 0), crate::complete::score(&needle, &c.target, 0)]
                .into_iter()
                .flatten()
                .max()?;
            let leaf = c.target.rsplit('/').next().unwrap_or(&c.target);
            let prefix = [c.name.to_lowercase(), c.target.to_lowercase(), leaf.to_lowercase()].iter().any(|s| s.starts_with(&lower));
            Some((best + if prefix { 1000 } else { 0 }, i, c))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    // References containing the query, by entity (in the world's order).
    let mut refs: Vec<(&str, Option<Candidate>)> = world
        .iter()
        .flat_map(|e| {
            e.looks
                .iter()
                .filter(|l| l.name.to_lowercase().contains(&lower) || l.id.contains(&lower))
                .map(move |l| (e.id.as_str(), Some(look_candidate(e, l))))
        })
        .collect();
    let mut out = Vec::new();
    let mut after_entities = 0;
    for (.., c) in found.into_iter().take(limit) {
        let owner = c.entity.then(|| c.target.clone());
        out.push(c);
        if let Some(owner) = owner {
            out.extend(refs.iter_mut().filter(|(e, _)| *e == owner).filter_map(|(_, r)| r.take()));
            after_entities = out.len();
        }
    }
    let rest: Vec<Candidate> = refs.into_iter().filter_map(|(_, r)| r).collect();
    out.splice(after_entities..after_entities, rest);
    out.truncate(limit);
    out
}

/// Reference `l` of entity `e` as an `@` candidate: `characters/mara#coat`, its name, `Look · Mara Venn`.
fn look_candidate(e: &WorldEntity, l: &WorldRef) -> Candidate {
    let kind = match l.kind() {
        "video" => "Video",
        "audio" => "Audio",
        _ => "Look",
    };
    Candidate { entity: true, target: format!("{}#{}", e.id, l.id), name: l.name.clone(), detail: format!("{kind} · {}", e.name) }
}

/// Entity `e`'s named references as `@` candidates (`characters/mara#coat`, its name, `Look · Mara Venn`), in
/// order for an empty `text`, else matching it (the name or the id) as [`mentions`] does. At most `limit`.
#[must_use]
pub fn looks(e: &WorldEntity, text: &str, limit: usize) -> Vec<Candidate> {
    let all = e.looks.iter().map(|l| look_candidate(e, l));
    let needle: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return all.take(limit).collect();
    }
    let lower = text.trim().to_lowercase();
    let mut scored: Vec<(i32, usize, Candidate)> = all
        .enumerate()
        .filter_map(|(i, c)| {
            let id = c.target.rsplit('#').next().unwrap_or_default().to_owned();
            let best = [crate::complete::score(&needle, &c.name, 0), crate::complete::score(&needle, &id, 0)].into_iter().flatten().max()?;
            let prefix = c.name.to_lowercase().starts_with(&lower) || id.starts_with(&lower);
            Some((best + if prefix { 1000 } else { 0 }, i, c))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(.., c)| c).collect()
}

/// Most characters of a node's name drawn from its text (a note's first line, an untitled card's prompt).
const NODE_NAME: usize = 60;

/// The nodes of canvas `canvas` (its folder name, exactly) worth naming in an `@` popup, as `canvas#node_id`
/// candidates: frames by title (`Scene`, `Shot`, `Frame`), cards by title else their prompt's first line
/// (`Video card`), notes by their first line, documents and other assets by file name; each detail names the
/// frame it sits in (`Video card · Pass 3A`). Untitled frames, empty notes and drafts, and entity nodes (mention
/// the entity) are left out. An empty `text` lists them by kind in that order, canvas order within; else they
/// match `text` as [`mentions`] does. At most `limit`.
#[must_use]
pub fn nodes(root: &Path, canvas: &str, text: &str, limit: usize) -> Vec<Candidate> {
    let Some(file) = canvas_file(root, canvas) else { return Vec::new() };
    let list = file["nodes"].as_array().cloned().unwrap_or_default();
    type Rect = (f64, f64, f64, f64);
    let rect = |n: &Value| -> Rect {
        let f = |k: &str| n[k].as_f64().unwrap_or(0.0);
        (f("x"), f("y"), f("w").max(0.0), f("h").max(0.0))
    };
    let frames: Vec<(&Value, Rect)> = list.iter().filter(|n| n["body"]["kind"] == "frame").map(|n| (n, rect(n))).collect();
    // The smallest titled frame larger than a node holding its centre.
    let holder = |node: &Value| {
        let (x, y, w, h) = rect(node);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        frames
            .iter()
            .filter(|(f, (fx, fy, fw, fh))| f["id"] != node["id"] && fw * fh > w * h && cx >= *fx && cx <= fx + fw && cy >= *fy && cy <= fy + fh)
            .filter_map(|(f, (_, _, fw, fh))| Some((fw * fh, f["body"]["title"].as_str().map(str::trim).filter(|t| !t.is_empty())?)))
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, title)| title.to_owned())
    };
    let line = |text: &str| -> Option<String> {
        let first = text.lines().map(|l| l.trim().trim_start_matches(['#', '>', '-', '*', ' ']).trim()).find(|l| !l.is_empty())?;
        Some(if first.chars().count() > NODE_NAME {
            format!("{}…", first.chars().take(NODE_NAME).collect::<String>().trim_end())
        } else {
            first.to_owned()
        })
    };
    let upper = |s: &str| s.chars().next().map(|c| c.to_uppercase().chain(s.chars().skip(1)).collect::<String>()).unwrap_or_default();
    let mut found: Vec<(usize, usize, Candidate)> = Vec::new();
    for (i, node) in list.iter().enumerate() {
        let (Some(id), body) = (node["id"].as_str(), &node["body"]) else { continue };
        let s = |k: &str| body[k].as_str().map(str::trim).filter(|v| !v.is_empty());
        let (group, name, kind) = match s("kind").unwrap_or_default() {
            "frame" => {
                let kind = match s("role") {
                    Some("scene") => "Scene",
                    Some("shot") => "Shot",
                    _ => "Frame",
                };
                (0, s("title").map(str::to_owned), kind.to_owned())
            }
            "gen" => (1, s("title").map(str::to_owned).or_else(|| line(s("prompt")?)), format!("{} card", upper(s("makes").unwrap_or("image")))),
            "note" => (2, s("text").and_then(line), "Note".to_owned()),
            "asset" => {
                let path = s("path").unwrap_or_default();
                let lower = path.to_ascii_lowercase();
                let text = s("media") == Some("text") || [".md", ".markdown", ".txt"].iter().any(|e| lower.ends_with(e));
                let kind = if text { "Document".to_owned() } else { s("media").filter(|m| *m != "unknown").map_or_else(|| "File".to_owned(), upper) };
                (if text { 3 } else { 4 }, path.rsplit('/').next().filter(|n| !n.is_empty()).map(str::to_owned), kind)
            }
            _ => continue,
        };
        let Some(name) = name else { continue };
        let detail = holder(node).map_or_else(|| kind.clone(), |frame| format!("{kind} · {frame}"));
        found.push((group, i, Candidate { entity: false, target: format!("{canvas}#{id}"), name, detail }));
    }
    let needle: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        found.sort_by_key(|(group, i, _)| (*group, *i));
        return found.into_iter().take(limit).map(|(.., c)| c).collect();
    }
    let lower = text.trim().to_lowercase();
    let mut scored: Vec<(i32, usize, usize, Candidate)> = found
        .into_iter()
        .filter_map(|(group, i, c)| {
            let score = crate::complete::score(&needle, &c.name.replace('/', " "), 0)?;
            Some((score + if c.name.to_lowercase().starts_with(&lower) { 1000 } else { 0 }, group, i, c))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    scored.into_iter().take(limit).map(|(.., c)| c).collect()
}
