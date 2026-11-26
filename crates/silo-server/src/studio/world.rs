//! The world (docs/STUDIO.md §1): sections are folders under `world/`
//! (with Overview and Style from `overview.md` and `style.md`), entities
//! folders in them with a markdown file. An entity's frontmatter lists its
//! named references (`refs`: paths or `{file, name, defines}` tables) and
//! the entities that go with it (`links`).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use silo_proto::{
    control::RpcError,
    studio::{Entity, EntityInfo, EntityRefEdited, EntityUse, NodeBody, RefChange, RefInfo, Section, SectionKind, StudioMedia, file_stem, ref_ids},
};

use super::{approval, canvas, disk, frontmatter::Doc};
use crate::files::paths;

pub const OVERVIEW: &str = "# Overview\n\nPremise, genre, tone, audience, the rules of the world, lore, timeline.\n";
pub const STYLE: &str = "# Style\n\nVisual language: palette, lenses, lighting, texture, era, reference notes.\n\n## Style line\n\n";

const IMAGES: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "heic", "avif", "tif", "tiff", "bmp"];

fn is_image(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| IMAGES.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The folders in `dir` (not hidden, resolving under canonical `root`),
/// by name.
fn folders(root: &Path, dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            let path = e.path().canonicalize().ok()?;
            (!name.starts_with('.') && path.is_dir() && paths::inside(root, &path)).then_some(name)
        })
        .collect();
    out.sort_by_key(|n| n.to_lowercase());
    out
}

/// The files in `dir` by name.
fn files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            (!name.starts_with('.') && e.path().is_file()).then_some(name)
        })
        .collect();
    out.sort();
    out
}

fn title_of(section: &str) -> String {
    match section {
        "characters" => "Characters".into(),
        "places" => "Places".into(),
        "props" => "Props".into(),
        other => other.to_owned(),
    }
}

/// `studio.world`: Overview, Style, then the folders under `world/` (but
/// `world/style/`, Style's reference images) by name.
pub fn sections(root: &Path) -> Vec<Section> {
    sections_with_fields(root).into_iter().map(|(s, _)| s).collect()
}

/// [`sections`] with each entity's frontmatter, in the same order.
pub fn sections_with_fields(root: &Path) -> Vec<(Section, Vec<Map<String, Value>>)> {
    let canonical = paths::canonical_root(root).ok();
    let refs = canonical.as_deref().map(style_refs).unwrap_or_default();
    let mut out = vec![
        (
            Section { id: "overview".into(), title: "Overview".into(), kind: SectionKind::Overview, entities: Vec::new(), refs: Vec::new() },
            Vec::new(),
        ),
        (Section { id: "style".into(), title: "Style".into(), kind: SectionKind::Style, entities: Vec::new(), refs }, Vec::new()),
    ];
    let Some(canonical) = canonical else { return out };
    let world = canonical.join("world");
    for section in folders(&canonical, &world) {
        if section == "style" {
            continue;
        }
        let mut entities: Vec<(EntityInfo, Map<String, Value>)> = folders(&canonical, &world.join(&section))
            .iter()
            .map(|e| {
                let (info, fields, _) = info(&canonical, &section, e);
                (info, fields)
            })
            .collect();
        entities.sort_by_key(|(e, _)| e.name.to_lowercase());
        let (entities, fields): (Vec<EntityInfo>, Vec<Map<String, Value>>) = entities.into_iter().unzip();
        let listed = Section { id: section.clone(), title: title_of(&section), kind: SectionKind::Entities, entities, refs: Vec::new() };
        out.push((listed, fields));
    }
    // Links read from both ends.
    let declared: Vec<(String, Vec<String>)> = out.iter().flat_map(|(s, _)| &s.entities).map(|e| (e.id.clone(), e.links.clone())).collect();
    for e in out.iter_mut().flat_map(|(s, _)| &mut s.entities) {
        e.links = both_ends(&e.id, &e.links, &declared);
    }
    out
}

/// The links of entity `id` declaring `own`: those, then the entities
/// whose declared links (`declared`, every entity's) name it, by id.
fn both_ends(id: &str, own: &[String], declared: &[(String, Vec<String>)]) -> Vec<String> {
    let mut back: Vec<&String> =
        declared.iter().filter(|(other, links)| other != id && links.iter().any(|l| l == id)).map(|(other, _)| other).collect();
    back.sort();
    let mut out: Vec<String> = own.to_vec();
    for other in back {
        if !out.contains(other) {
            out.push(other.clone());
        }
    }
    out
}

/// Every entity's declared links (canonical `root`), read from the files.
fn declared_links(root: &Path) -> Vec<(String, Vec<String>)> {
    let world = root.join("world");
    let mut out = Vec::new();
    for section in folders(root, &world).into_iter().filter(|s| s != "style") {
        for name in folders(root, &world.join(&section)) {
            let file = file_of(root, &section, &name);
            let doc = std::fs::read_to_string(root.join(&file)).map(|t| Doc::parse(&t)).unwrap_or_default();
            let id = format!("{section}/{name}");
            let links = links_of(root, &id, doc.fields().get("links"));
            out.push((id, links));
        }
    }
    out
}

/// A written link (`props/mace`, `@props/mace`, `world/props/mace/`) as an
/// entity id; `None` when it isn't one.
fn link_id(text: &str) -> Option<String> {
    let id = text.trim().trim_start_matches('@').trim_start_matches("world/").trim_matches('/');
    let (section, name) = id.split_once('/')?;
    let plain = |s: &str| !s.is_empty() && !s.starts_with('.') && !s.contains(['/', '\\']) && s.trim() == s;
    (plain(section) && plain(name)).then(|| id.to_owned())
}

/// The entities a `links` value names that exist (canonical `root`), but
/// `id` itself, each once, in order.
fn links_of(root: &Path, id: &str, value: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for link in strings(value).iter().filter_map(|l| link_id(l)) {
        let dir = root.join("world").join(&link);
        let exists = dir.canonicalize().is_ok_and(|d| d.is_dir() && paths::inside(root, &d));
        if link != id && exists && !out.contains(&link) {
            out.push(link);
        }
    }
    out
}

/// Every image under `world/style/` (canonical `root`), root-relative, by
/// path; folders that leave the root are skipped.
fn style_refs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec!["world/style".to_owned()];
    while let Some(dir) = todo.pop() {
        if out.len() > 2000 {
            break;
        }
        for name in folders(root, &root.join(&dir)) {
            todo.push(format!("{dir}/{name}"));
        }
        out.extend(files(&root.join(&dir)).into_iter().filter(|f| is_image(f)).map(|f| format!("{dir}/{f}")));
    }
    out.sort();
    out
}

/// The markdown file of entity folder `world/<section>/<name>`: `<name>.md`,
/// else the first `.md`, else `<name>.md` to be.
fn file_of(root: &Path, section: &str, name: &str) -> String {
    let dir = format!("world/{section}/{name}");
    if root.join(&dir).join(format!("{name}.md")).is_file() {
        return format!("{dir}/{name}.md");
    }
    files(&root.join(&dir)).into_iter().find(|f| f.ends_with(".md")).map_or_else(|| format!("{dir}/{name}.md"), |f| format!("{dir}/{f}"))
}

/// `rel` (relative to root-relative folder `dir`) as a root-relative path;
/// `None` when it leaves the root or is absolute.
fn under(dir: &str, rel: &str) -> Option<String> {
    let rel = rel.trim();
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('~') {
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

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(items)) => items.iter().filter_map(|i| i.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)).collect(),
        Some(Value::String(s)) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// One `refs` entry: its file as written (entity-relative), its name and
/// what it defines.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RefEntry {
    file: String,
    name: Option<String>,
    defines: Option<String>,
}

fn text_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// An entry of `refs` (a path, or a table with `file`, `name`, `defines`);
/// `None` without a file.
fn ref_entry(v: &Value) -> Option<RefEntry> {
    match v {
        Value::Object(map) => Some(RefEntry {
            file: text_of(map.get("file").or_else(|| map.get("path")))?,
            name: text_of(map.get("name")),
            defines: text_of(map.get("defines")),
        }),
        other => Some(RefEntry { file: text_of(Some(other))?, name: None, defines: None }),
    }
}

/// The entries of a `refs` value, in order (a string: comma-separated paths).
fn ref_entries(v: Option<&Value>) -> Vec<RefEntry> {
    match v {
        Some(Value::Array(items)) => items.iter().filter_map(ref_entry).collect(),
        Some(Value::String(_)) => strings(v).into_iter().map(|file| RefEntry { file, name: None, defines: None }).collect(),
        _ => Vec::new(),
    }
}

/// The named references of the entity in root-relative folder `dir`:
/// `entries` that stay under the root, with their ids.
fn looks_of(dir: &str, entries: &[RefEntry]) -> Vec<RefInfo> {
    let found: Vec<(&RefEntry, String)> = entries.iter().filter_map(|e| Some((e, under(dir, &e.file)?))).collect();
    let ids = ref_ids(&found.iter().map(|(e, path)| (e.name.as_deref(), path.as_str())).collect::<Vec<_>>());
    found
        .into_iter()
        .zip(ids)
        .map(|((e, path), id)| RefInfo {
            id,
            name: e.name.clone().unwrap_or_else(|| file_stem(&path).to_owned()),
            defines: e.defines.clone(),
            media: StudioMedia::of_path(&path),
            path,
        })
        .collect()
}

/// The `refs` entries of the entity in folder `dir` (root-relative,
/// canonical `root`) given its frontmatter: as written, else the images of
/// its `refs/` folder.
fn entries_of(root: &Path, dir: &str, fields: &Map<String, Value>) -> Vec<RefEntry> {
    if fields.contains_key("refs") {
        ref_entries(fields.get("refs"))
    } else {
        files(&root.join(dir).join("refs"))
            .into_iter()
            .filter(|f| is_image(f))
            .map(|f| RefEntry { file: format!("refs/{f}"), name: None, defines: None })
            .collect()
    }
}

/// An entity's listing, its frontmatter and its body (canonical `root`);
/// its links only those it declares ([`both_ends`] adds the others).
fn info(root: &Path, section: &str, name: &str) -> (EntityInfo, Map<String, Value>, String) {
    let file = file_of(root, section, name);
    let dir = format!("world/{section}/{name}");
    let doc = std::fs::read_to_string(root.join(&file)).map(|t| Doc::parse(&t)).unwrap_or_default();
    let fields = doc.fields();
    let text = |k: &str| fields.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let sheet = match text("sheet") {
        Some(sheet) => under(&dir, &sheet),
        // Unnamed: a `sheet.*` image in the folder.
        None => files(&root.join(&dir)).into_iter().find(|f| f.starts_with("sheet.") && is_image(f)).map(|f| format!("{dir}/{f}")),
    };
    let looks = looks_of(&dir, &entries_of(root, &dir, &fields));
    let refs = looks.iter().map(|l| l.path.clone()).collect();
    let id = format!("{section}/{name}");
    let links = links_of(root, &id, fields.get("links"));
    let sheet_mtime = sheet.as_deref().and_then(|s| root.join(s).metadata().ok()).map(|m| disk::mtime_ms(&m));
    let info = EntityInfo {
        id,
        name: text("name").unwrap_or_else(|| name.to_owned()),
        file,
        summary: text("summary"),
        approval: approval::of(fields.get(approval::KEY), sheet_mtime),
        voice: approval::names_voice(fields.get("voice")),
        sheet,
        refs,
        looks,
        links,
        tags: strings(fields.get("tags")),
    };
    (info, fields, doc.body)
}

/// Whether entity `info` (canonical `root`) has a video among its refs, or
/// in its `refs/` folder: its video sheet.
pub fn has_video_ref(root: &Path, info: &EntityInfo) -> bool {
    let video = |p: &str| StudioMedia::of_path(p) == StudioMedia::Video;
    info.refs.iter().any(|r| video(r)) || files(&root.join("world").join(&info.id).join("refs")).iter().any(|f| video(f))
}

/// `characters/mara` as `(section, name)`.
fn split_id(id: &str) -> Result<(String, String), RpcError> {
    let id = paths::normalize(id.trim_start_matches("world/"))?;
    let (section, name) = id.split_once('/').ok_or_else(|| RpcError::bad_request(format!("entity id {id} must be <section>/<name>")))?;
    Ok((disk::component("section", section)?, disk::component("entity", name)?))
}

fn entity_dir(root: &Path, section: &str, name: &str) -> Result<PathBuf, RpcError> {
    let rel = format!("world/{section}/{name}");
    let dir =
        paths::resolve(root, &rel).map_err(|e| if e.code == "not_found" { RpcError::not_found(format!("entity {section}/{name}")) } else { e })?;
    if !dir.is_dir() {
        return Err(RpcError::not_found(format!("entity {section}/{name}")));
    }
    Ok(dir)
}

/// `studio.entity`.
///
/// # Errors
/// `not_found` for a missing entity; path errors.
pub fn entity(root: &Path, id: &str) -> Result<Entity, RpcError> {
    let (section, name) = split_id(id)?;
    entity_dir(root, &section, &name)?;
    let canonical = paths::canonical_root(root)?;
    let (mut info, fields, body) = info(&canonical, &section, &name);
    info.links = both_ends(&info.id, &info.links, &declared_links(&canonical));
    let used_in = used_in(&canonical, &info.id);
    Ok(Entity { info, fields: fields.into_iter().collect(), body, used_in })
}

/// Canvas nodes using entity `id`: its Entity nodes and Asset nodes of its
/// files, the cards wired from those, and cards whose last request
/// (`input`) names one of its files.
fn used_in(root: &Path, id: &str) -> Vec<EntityUse> {
    let dir = format!("world/{id}/");
    let names_file = |text: &str| text.contains(&dir);
    let mut out: Vec<EntityUse> = Vec::new();
    let push = |out: &mut Vec<EntityUse>, canvas: &str, node: &str| {
        if !out.iter().any(|u| u.canvas == canvas && u.node == node) {
            out.push(EntityUse { canvas: canvas.to_owned(), node: node.to_owned() });
        }
    };
    for name in canvas::names(root) {
        let Ok(doc) = canvas::load(root, &name) else { continue };
        let c = &doc.canvas;
        for node in &c.nodes {
            let source = match &node.body {
                NodeBody::Entity { id: e } => e == id,
                NodeBody::Asset { path, .. } => path.starts_with(&dir),
                NodeBody::Gen { input: Some(input), .. } => {
                    if names_file(&input.to_string()) {
                        push(&mut out, &name, &node.id);
                    }
                    false
                }
                _ => false,
            };
            if !source {
                continue;
            }
            push(&mut out, &name, &node.id);
            for edge in c.edges.iter().filter(|e| e.from == node.id && e.role.is_port()) {
                push(&mut out, &name, &edge.to);
            }
        }
    }
    out
}

/// The canonical-root info of entity `section/name`, its links from both
/// ends.
fn listed(root: &Path, section: &str, name: &str) -> Result<EntityInfo, RpcError> {
    let canonical = paths::canonical_root(root)?;
    let mut info = info(&canonical, section, name).0;
    info.links = both_ends(&info.id, &info.links, &declared_links(&canonical));
    Ok(info)
}

fn check_fields(fields: &std::collections::BTreeMap<String, Value>) -> Result<(), RpcError> {
    if let Some(bad) = fields.keys().find(|k| !super::frontmatter::valid_key(k)) {
        return Err(RpcError::bad_request(format!("frontmatter key {bad:?} can't be written")));
    }
    Ok(())
}

/// `studio.entity_create`.
///
/// # Errors
/// `bad_request` for a bad section, name or field; path errors.
pub fn create(
    root: &Path,
    section: &str,
    name: &str,
    fields: Option<&std::collections::BTreeMap<String, Value>>,
    body: Option<&str>,
) -> Result<EntityInfo, RpcError> {
    let section = disk::component("section", section)?;
    let title = name.trim();
    if title.is_empty() || title.len() > 200 || title.chars().any(char::is_control) {
        return Err(RpcError::bad_request("an entity needs a name (1 to 200 printable bytes)"));
    }
    if let Some(fields) = fields {
        check_fields(fields)?;
    }
    let section_dir = disk::mkdirs(root, &format!("world/{section}"))?;
    let folder = disk::locked(root, &format!("world/{section}"), || {
        let folder = disk::free_name(&section_dir, &disk::slug(title, "entity"), false);
        disk::mkdirs(root, &format!("world/{section}/{folder}"))?;
        Ok::<_, RpcError>(folder)
    })?;
    let mut doc = Doc::parse("");
    doc.set("name", &Value::String(title.to_owned()));
    for (k, v) in fields.into_iter().flatten().filter(|(k, _)| *k != "name") {
        doc.set(k, v);
    }
    doc.body = body.map_or_else(|| format!("\n# {title}\n\n"), |b| if b.starts_with('\n') { b.to_owned() } else { format!("\n{b}") });
    let rel = format!("world/{section}/{folder}/{folder}.md");
    disk::atomic_write(&disk::target(root, &rel)?, doc.render().as_bytes(), &rel)?;
    listed(root, &section, &folder)
}

/// `studio.entity_approve`: the frontmatter's `approved` set to `now`
/// (RFC 3339, UTC), or removed; every other line stays as written. An
/// unchanged file isn't written.
///
/// # Errors
/// `not_found` for a missing entity; `bad_request` approving one with no
/// sheet.
pub fn approve(root: &Path, id: &str, approved: bool, now_ms: u64) -> Result<EntityInfo, RpcError> {
    let (section, name) = split_id(id)?;
    entity_dir(root, &section, &name)?;
    let canonical = paths::canonical_root(root)?;
    if approved && info(&canonical, &section, &name).0.sheet.is_none() {
        return Err(RpcError::bad_request(format!("{section}/{name} has no sheet to approve")));
    }
    let rel = file_of(&canonical, &section, &name);
    disk::locked(root, &rel, || {
        let path = disk::target(root, &rel)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(disk::io_error(&rel, &e)),
        };
        let mut doc = Doc::parse(&text);
        let had = doc.fields().contains_key(approval::KEY);
        if !approved && !had {
            return Ok(());
        }
        let value = if approved { Value::String(approval::rfc3339(now_ms)) } else { Value::Null };
        doc.set(approval::KEY, &value);
        disk::atomic_write(&path, doc.render().as_bytes(), &rel)
    })?;
    listed(root, &section, &name)
}

/// `studio.entity_update`: `fields` merged (`null` removes), `body`
/// replaced.
///
/// # Errors
/// `not_found` for a missing entity; `bad_request` for a bad field.
pub fn update(root: &Path, id: &str, fields: Option<&std::collections::BTreeMap<String, Value>>, body: Option<&str>) -> Result<EntityInfo, RpcError> {
    let (section, name) = split_id(id)?;
    entity_dir(root, &section, &name)?;
    if let Some(fields) = fields {
        check_fields(fields)?;
    }
    let rel = file_of(&paths::canonical_root(root)?, &section, &name);
    disk::locked(root, &rel, || {
        let path = disk::target(root, &rel)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(disk::io_error(&rel, &e)),
        };
        let mut doc = Doc::parse(&text);
        for (k, v) in fields.into_iter().flatten() {
            doc.set(k, v);
        }
        if let Some(body) = body {
            doc.body = if body.starts_with('\n') || doc.render() == doc.body { body.to_owned() } else { format!("\n{body}") };
        }
        disk::atomic_write(&path, doc.render().as_bytes(), &rel)
    })?;
    listed(root, &section, &name)
}

/// Rewrite markdown file `rel` (root-relative) under its write lock: `f`
/// edits its parsed form and says whether anything changed (an unchanged
/// file isn't written).
fn rewrite<T>(root: &Path, rel: &str, f: impl FnOnce(&mut Doc) -> Result<(T, bool), RpcError>) -> Result<T, RpcError> {
    disk::locked(root, rel, || {
        let path = disk::target(root, rel)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(disk::io_error(rel, &e)),
        };
        let mut doc = Doc::parse(&text);
        let (out, changed) = f(&mut doc)?;
        if changed {
            disk::atomic_write(&path, doc.render().as_bytes(), rel)?;
        }
        Ok(out)
    })
}

/// `refs` entry `raw` with `key` set to `value` (`None`: removed): a plain
/// path becomes a table when it gets a name or a job, and a table left with
/// its file alone a plain path again; a table's other keys stay.
fn with_key(raw: &Value, key: &str, value: Option<&str>) -> Value {
    let mut map = match raw {
        Value::Object(map) => map.clone(),
        other => Map::from_iter([("file".to_owned(), other.clone())]),
    };
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => map.insert(key.to_owned(), Value::String(v.to_owned())),
        None => map.remove(key),
    };
    match (map.len(), map.get("file").or_else(|| map.get("path"))) {
        (1, Some(file)) => file.clone(),
        _ => Value::Object(map),
    }
}

/// Where a reference added to the entity in root-relative folder `dir`
/// comes from (`from`: root-relative, or absolute and `~/…` for the owner):
/// a file under that folder stays where it is; one from elsewhere is copied
/// into its `refs/` folder, named after `label`'s slug when no file there
/// has that stem (the stem makes the id: `Crown off`, `refs/crown-off.png`),
/// else as it is. Its entity-relative path.
fn ref_file(root: &Path, dir: &str, from: &str, label: Option<&str>, owner: bool) -> Result<String, RpcError> {
    let canonical = paths::canonical_root(root)?;
    let src = super::media::source(root, from, owner)?;
    if !src.is_file() {
        return Err(RpcError::bad_request(format!("{from} is not a file")));
    }
    let folder = canonical.join(dir).canonicalize().map_err(|e| disk::io_error(dir, &e))?;
    if let Some(rel) = paths::relative(&folder, &src).filter(|r| !r.is_empty()) {
        return Ok(rel);
    }
    let refs_rel = format!("{dir}/refs");
    let refs = disk::mkdirs(root, &refs_rel)?.canonicalize().map_err(|e| disk::io_error(&refs_rel, &e))?;
    let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| RpcError::bad_request("no file name"))?;
    let name = named_after(&refs, &name, label).unwrap_or(name);
    let name = super::media::copy_into(root, &src, &refs, &refs_rel, &name, from)?;
    Ok(format!("refs/{name}"))
}

/// File `name` renamed after `label`'s slug, its extension kept
/// (`Crown off`, `IMG_1.PNG`: `crown-off.PNG`), when that slug isn't empty and
/// no file in folder `refs` has a stem of that slug; `None` otherwise.
fn named_after(refs: &Path, name: &str, label: Option<&str>) -> Option<String> {
    let slug = silo_proto::studio::ref_slug(label?);
    if slug.is_empty() {
        return None;
    }
    let taken = std::fs::read_dir(refs)
        .map(|entries| entries.flatten().any(|e| silo_proto::studio::ref_slug(file_stem(&e.file_name().to_string_lossy())) == slug))
        .unwrap_or(false);
    (!taken).then(|| format!("{slug}{}", crate::trash::split_ext(name).1))
}

/// `studio.entity_ref`: one change to entity `id`'s named references (its
/// frontmatter's `refs`), written in place; every other line stays. A list
/// not written yet (the `refs/` folder's images) is written out first.
/// `owner`: the caller may add a file from outside the root.
///
/// # Errors
/// `not_found` for a missing entity or reference; path errors adding one.
pub fn edit_ref(root: &Path, id: &str, change: &RefChange, owner: bool) -> Result<EntityRefEdited, RpcError> {
    let (section, name) = split_id(id)?;
    entity_dir(root, &section, &name)?;
    let canonical = paths::canonical_root(root)?;
    let dir = format!("world/{section}/{name}");
    // A file from elsewhere is copied in before the entity's file is locked.
    let added = match change {
        RefChange::Add { path, name: label, .. } => Some(ref_file(root, &dir, path, label.as_deref(), owner)?),
        _ => None,
    };
    let rel = file_of(&canonical, &section, &name);
    let path = rewrite(root, &rel, |doc| {
        let fields = doc.fields();
        let mut raw: Vec<Value> = match fields.get("refs") {
            Some(Value::Array(items)) => items.clone(),
            Some(v @ Value::String(_)) => strings(Some(v)).into_iter().map(Value::String).collect(),
            Some(_) => Vec::new(),
            None => entries_of(&canonical, &dir, &fields).into_iter().map(|e| Value::String(e.file)).collect(),
        };
        // Each entry's root-relative path and id, as `info` lists them.
        let listed = |raw: &[Value]| -> Vec<Option<(String, String)>> {
            let paths: Vec<Option<(Option<String>, String)>> =
                raw.iter().map(|v| ref_entry(v).and_then(|e| Some((e.name, under(&dir, &e.file)?)))).collect();
            let found: Vec<(Option<&str>, &str)> = paths.iter().flatten().map(|(n, p)| (n.as_deref(), p.as_str())).collect();
            let mut ids = ref_ids(&found).into_iter();
            paths.iter().map(|p| p.as_ref().and_then(|(_, path)| Some((path.clone(), ids.next()?)))).collect()
        };
        let before = listed(&raw);
        let find = |ref_id: &str| {
            before
                .iter()
                .position(|e| e.as_ref().is_some_and(|(_, i)| i == ref_id))
                .ok_or_else(|| RpcError::not_found(format!("reference {ref_id} of {section}/{name}")))
        };
        // The raw index of the `to`th listed entry (the end past the last).
        let slot = |raw: &[Value], to: usize| listed(raw).iter().enumerate().filter(|(_, e)| e.is_some()).nth(to).map_or(raw.len(), |(i, _)| i);
        let old = raw.clone();
        let path = match change {
            RefChange::Add { name: label, defines, at, .. } => {
                let file = added.clone().unwrap_or_default();
                let full = under(&dir, &file).unwrap_or_default();
                let listed_at = before.iter().position(|e| e.as_ref().is_some_and(|(p, _)| *p == full));
                let mut entry = match listed_at {
                    // Listed already: named and described where it is.
                    Some(i) => raw.remove(i),
                    None => Value::String(file),
                };
                if label.is_some() {
                    entry = with_key(&entry, "name", label.as_deref());
                }
                if defines.is_some() {
                    entry = with_key(&entry, "defines", defines.as_deref());
                }
                let to = match (at, listed_at) {
                    (Some(to), _) => slot(&raw, *to),
                    (None, Some(i)) => i,
                    (None, None) => raw.len(),
                };
                raw.insert(to, entry);
                Some(full)
            }
            RefChange::Rename { ref_id, name: label } => {
                let i = find(ref_id)?;
                raw[i] = with_key(&raw[i], "name", label.as_deref());
                before[i].as_ref().map(|(p, _)| p.clone())
            }
            RefChange::Describe { ref_id, defines } => {
                let i = find(ref_id)?;
                raw[i] = with_key(&raw[i], "defines", defines.as_deref());
                before[i].as_ref().map(|(p, _)| p.clone())
            }
            RefChange::Move { ref_id, to } => {
                let i = find(ref_id)?;
                let entry = raw.remove(i);
                let to = slot(&raw, *to);
                raw.insert(to, entry);
                before[i].as_ref().map(|(p, _)| p.clone())
            }
            RefChange::Remove { ref_id } => {
                raw.remove(find(ref_id)?);
                None
            }
        };
        let changed = raw != old || !fields.contains_key("refs");
        if changed {
            doc.set("refs", &Value::Array(raw));
        }
        Ok((path, changed))
    })?;
    let info = listed(root, &section, &name)?;
    let ref_id = path.and_then(|p| info.looks.iter().find(|l| l.path == p).map(|l| l.id.clone()));
    Ok(EntityRefEdited { info, ref_id })
}

/// `studio.entity_link`: link entity `id` with `other` (added to `id`'s
/// `links` unless either names the other already), or unlink them (taken
/// out of both files' `links`, wherever it is written; a list left empty
/// goes).
///
/// # Errors
/// `not_found` for a missing entity; `bad_request` linking one to itself.
pub fn link(root: &Path, id: &str, other: &str, linked: bool) -> Result<EntityInfo, RpcError> {
    let (section, name) = split_id(id)?;
    let (other_section, other_name) = split_id(other)?;
    entity_dir(root, &section, &name)?;
    entity_dir(root, &other_section, &other_name)?;
    let (id, other) = (format!("{section}/{name}"), format!("{other_section}/{other_name}"));
    if id == other {
        return Err(RpcError::bad_request(format!("{id} can't be linked to itself")));
    }
    let canonical = paths::canonical_root(root)?;
    let names = |doc: &Doc, whom: &str| strings(doc.fields().get("links")).iter().any(|l| link_id(l).as_deref() == Some(whom));
    let read = |section: &str, name: &str| {
        std::fs::read_to_string(canonical.join(file_of(&canonical, section, name))).map(|t| Doc::parse(&t)).unwrap_or_default()
    };
    if linked {
        if names(&read(&other_section, &other_name), &id) {
            return listed(root, &section, &name);
        }
        rewrite(root, &file_of(&canonical, &section, &name), |doc| {
            if names(doc, &other) {
                return Ok(((), false));
            }
            let mut list: Vec<Value> = strings(doc.fields().get("links")).into_iter().map(Value::String).collect();
            list.push(Value::String(other.clone()));
            doc.set("links", &Value::Array(list));
            Ok(((), true))
        })?;
    } else {
        for ((s, n), whom) in [((&section, &name), &other), ((&other_section, &other_name), &id)] {
            rewrite(root, &file_of(&canonical, s, n), |doc| {
                if !names(doc, whom) {
                    return Ok(((), false));
                }
                let list: Vec<Value> = strings(doc.fields().get("links"))
                    .into_iter()
                    .filter(|l| link_id(l).as_deref() != Some(whom.as_str()))
                    .map(Value::String)
                    .collect();
                doc.set("links", &if list.is_empty() { Value::Null } else { Value::Array(list) });
                Ok(((), true))
            })?;
        }
    }
    listed(root, &section, &name)
}
