//! `studio.toml` (docs/STUDIO.md §1.1): read, and edited in place with
//! `toml_edit`, so comments and keys the server doesn't know stay.

use std::{collections::BTreeMap, path::Path};

use silo_proto::{
    control::RpcError,
    studio::{CanvasInfo, Format, Project},
};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::{canvas, disk, world};

pub const TOML: &str = "studio.toml";

/// The presets `studio.init` writes.
const PRESETS: &str = r#"name = ""
format = "landscape"                         # the project default: a preset below

[formats.landscape]
aspect = "16:9"
resolution = "1080p"
fps = 24

[formats.vertical]                           # reels, shorts, stories
aspect = "9:16"
resolution = "1080p"
fps = 30

[formats.square]
aspect = "1:1"
resolution = "1080p"

[formats.portrait]                           # feed posts
aspect = "4:5"
resolution = "1080p"

[canvases]
order = []                                   # tab order; missing ones go last by name
"#;

/// The four presets, as `studio.init` writes them (also the formats of a
/// root without `studio.toml`).
pub fn presets() -> BTreeMap<String, Format> {
    let doc: DocumentMut = PRESETS.parse().expect("the presets parse");
    formats_of(&doc)
}

/// `studio.toml`, if there is one.
///
/// # Errors
/// `bad_request` when it doesn't parse; an I/O failure.
pub fn read_doc(root: &Path) -> Result<Option<DocumentMut>, RpcError> {
    let path = root.join(TOML);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(disk::io_error(TOML, &e)),
    };
    if !crate::files::paths::inside(&crate::files::paths::canonical_root(root)?, &path.canonicalize().map_err(|e| disk::io_error(TOML, &e))?) {
        return Err(RpcError::denied("studio.toml is a link out of the block's root"));
    }
    text.parse::<DocumentMut>().map(Some).map_err(|e| RpcError::bad_request(format!("studio.toml doesn't parse: {e}")))
}

fn write_doc(root: &Path, doc: &DocumentMut) -> Result<(), RpcError> {
    disk::atomic_write(&disk::target(root, TOML)?, doc.to_string().as_bytes(), TOML)
}

/// Edit `studio.toml` (which must exist) under its write lock.
fn edit(root: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<(), RpcError>) -> Result<(), RpcError> {
    disk::locked(root, TOML, || {
        let mut doc = read_doc(root)?.ok_or_else(|| RpcError::not_found("studio.toml (studio.init makes it)"))?;
        f(&mut doc)?;
        write_doc(root, &doc)
    })
}

fn text_of(item: &Item) -> Option<String> {
    let v = item.as_value()?;
    v.as_str().map(str::to_owned).or_else(|| v.as_integer().map(|i| i.to_string())).or_else(|| v.as_float().map(|f| f.to_string()))
}

/// The presets of a parsed `studio.toml`; ones without an `aspect` are
/// left out.
pub fn formats_of(doc: &DocumentMut) -> BTreeMap<String, Format> {
    let mut out = BTreeMap::new();
    let Some(formats) = doc.get("formats").and_then(Item::as_table_like) else { return out };
    for (name, item) in formats.iter() {
        let Some(t) = item.as_table_like() else { continue };
        let Some(aspect) = t.get("aspect").and_then(text_of) else { continue };
        let resolution = t.get("resolution").and_then(|r| {
            let v = r.as_value()?;
            v.as_str().map(str::to_owned).or_else(|| v.as_integer().map(|i| format!("{i}p")))
        });
        let fps = t.get("fps").and_then(|f| f.as_integer()).and_then(|f| u32::try_from(f).ok());
        out.insert(name.to_owned(), Format { aspect, resolution, fps });
    }
    out
}

/// The default preset's name: `format`, else `landscape`.
fn default_of(doc: &DocumentMut) -> String {
    doc.get("format").and_then(Item::as_str).map_or_else(|| "landscape".to_owned(), str::to_owned)
}

/// `[canvases] order`.
pub fn order_of(doc: &DocumentMut) -> Vec<String> {
    doc.get("canvases")
        .and_then(|c| c.get("order"))
        .and_then(Item::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// The formats and the default, for sizing: the presets when there is no
/// `studio.toml` (or it doesn't parse).
pub fn formats(root: &Path) -> (BTreeMap<String, Format>, String) {
    match read_doc(root) {
        Ok(Some(doc)) => (formats_of(&doc), default_of(&doc)),
        _ => (presets(), "landscape".into()),
    }
}

/// A project with only the formats, for [`Project::format_for`].
pub fn formats_project(root: &Path) -> Project {
    let (formats, format) = formats(root);
    Project { name: String::new(), format, formats, canvases: Vec::new(), sections: Vec::new(), models: BTreeMap::new() }
}

/// `[models]`: the picks by kind (string values only).
pub fn models_of(doc: &DocumentMut) -> BTreeMap<String, String> {
    let Some(models) = doc.get("models").and_then(Item::as_table_like) else { return BTreeMap::new() };
    models
        .iter()
        .filter_map(|(kind, item)| Some((kind.to_owned(), item.as_str()?.trim().to_owned())))
        .filter(|(_, handle)| !handle.is_empty())
        .collect()
}

/// The picks of the project at `root`; none without `studio.toml`.
pub fn models(root: &Path) -> BTreeMap<String, String> {
    read_doc(root).ok().flatten().map(|d| models_of(&d)).unwrap_or_default()
}

/// Canvases in tab order: episodes by number, then `order`, then the rest
/// by name.
pub fn ordered(mut list: Vec<CanvasInfo>, order: &[String]) -> Vec<CanvasInfo> {
    let rank = |c: &CanvasInfo| match (c.episode, order.iter().position(|o| *o == c.name)) {
        (Some(ep), _) => (0, ep as usize, String::new()),
        (None, Some(at)) => (1, at, String::new()),
        (None, None) => (2, 0, c.name.to_lowercase()),
    };
    list.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name)));
    list
}

/// The canvas names in tab order (for opening the first).
pub fn canvas_order(root: &Path) -> Vec<String> {
    let order = read_doc(root).ok().flatten().map(|d| order_of(&d)).unwrap_or_default();
    ordered(canvas::list(root), &order).into_iter().map(|c| c.name).collect()
}

/// `studio.project`.
///
/// # Errors
/// `not_found` without `studio.toml`; `bad_request` when it doesn't parse.
pub fn project(root: &Path) -> Result<Project, RpcError> {
    let doc = read_doc(root)?.ok_or_else(|| RpcError::not_found("studio.toml (the block shows Setup)"))?;
    let name = doc.get("name").and_then(Item::as_str).map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned);
    let name = name.unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    Ok(Project {
        name,
        format: default_of(&doc),
        formats: formats_of(&doc),
        canvases: ordered(canvas::list(root), &order_of(&doc)),
        sections: world::sections(root),
        models: models_of(&doc),
    })
}

/// `studio.init`: `studio.toml` with the presets (unless there is one) and
/// the world's skeleton (what is missing).
///
/// # Errors
/// `bad_request` for an empty name or a format that is no preset.
pub fn init(root: &Path, name: &str, format: Option<&str>) -> Result<(), RpcError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
        return Err(RpcError::bad_request("the studio needs a name (1 to 200 printable bytes)"));
    }
    let format = format.unwrap_or("landscape");
    let presets = presets();
    if !presets.contains_key(format) {
        return Err(RpcError::bad_request(format!("format must be one of {}", presets.keys().cloned().collect::<Vec<_>>().join(", "))));
    }
    disk::locked(root, TOML, || {
        if read_doc(root)?.is_none() && !root.join(TOML).exists() {
            let mut doc: DocumentMut = PRESETS.parse().expect("the presets parse");
            set_keep_decor(&mut doc, "name", name);
            set_keep_decor(&mut doc, "format", format);
            let path = disk::target(root, TOML)?;
            // Never over a file that appeared meanwhile.
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| disk::io_error(TOML, &e))?;
            std::io::Write::write_all(&mut file, doc.to_string().as_bytes()).map_err(|e| disk::io_error(TOML, &e))?;
        }
        Ok::<(), RpcError>(())
    })?;
    for dir in ["world/characters", "world/places", "world/props", "canvases"] {
        disk::mkdirs(root, dir)?;
    }
    for (file, text) in [("world/overview.md", world::OVERVIEW), ("world/style.md", world::STYLE)] {
        let path = disk::target(root, file)?;
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            std::io::Write::write_all(&mut f, text.as_bytes()).map_err(|e| disk::io_error(file, &e))?;
        }
    }
    Ok(())
}

/// Set top-level string `key`, keeping the line's comment.
fn set_keep_decor(doc: &mut DocumentMut, key: &str, text: &str) {
    let decor = doc.get(key).and_then(Item::as_value).map(|v| v.decor().clone());
    let mut new = toml_edit::Value::from(text);
    if let Some(decor) = decor {
        *new.decor_mut() = decor;
    }
    doc[key] = Item::Value(new);
}

/// `studio.set_format` without a canvas: the project default.
///
/// # Errors
/// `bad_request` for a name with no preset.
pub fn set_default(root: &Path, format: &str) -> Result<(), RpcError> {
    edit(root, |doc| {
        if !formats_of(doc).contains_key(format) {
            return Err(RpcError::bad_request(format!("no format preset {format} in studio.toml")));
        }
        set_keep_decor(doc, "format", format);
        Ok(())
    })
}

/// Whether `format` names a preset of the project at `root`.
pub fn has_preset(root: &Path, format: &str) -> bool {
    formats(root).0.contains_key(format)
}

/// `studio.set_formats`: add or replace presets.
///
/// # Errors
/// `bad_request` for a bad name or aspect.
pub fn set_formats(root: &Path, formats: &BTreeMap<String, Format>) -> Result<(), RpcError> {
    for (name, f) in formats {
        let ok = !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return Err(RpcError::bad_request(format!("format name {name:?}: letters, digits, - and _ only")));
        }
        if f.aspect_ratio().is_none() {
            return Err(RpcError::bad_request(format!("format {name}: aspect must be W:H (16:9, 2.39:1), not {:?}", f.aspect)));
        }
    }
    edit(root, |doc| {
        if !doc.get("formats").is_some_and(Item::is_table_like) {
            let mut table = Table::new();
            table.set_implicit(true);
            doc["formats"] = Item::Table(table);
        }
        for (name, f) in formats {
            let table = &mut doc["formats"][name.as_str()];
            if !table.is_table_like() {
                *table = Item::Table(Table::new());
            }
            table["aspect"] = value(f.aspect.clone());
            match &f.resolution {
                Some(r) => table["resolution"] = value(r.clone()),
                None => drop(table.as_table_like_mut().and_then(|t| t.remove("resolution"))),
            }
            match f.fps {
                Some(fps) => table["fps"] = value(i64::from(fps)),
                None => drop(table.as_table_like_mut().and_then(|t| t.remove("fps"))),
            }
        }
        Ok(())
    })
}

/// `studio.set_models`: merge `models` into `[models]` (`None` removes a
/// kind), keeping the file's other text, the table's comments and a value's
/// trailing comment.
///
/// # Errors
/// `bad_request` for a bad kind or handle; `not_found` without `studio.toml`.
pub fn set_models(root: &Path, models: &BTreeMap<String, Option<String>>) -> Result<(), RpcError> {
    for (kind, handle) in models {
        let ok = !kind.is_empty() && kind.len() <= 64 && kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return Err(RpcError::bad_request(format!("model kind {kind:?}: letters, digits, - and _ only")));
        }
        if let Some(h) = handle
            && (h.trim().is_empty() || h.len() > 200 || h.chars().any(char::is_control))
        {
            return Err(RpcError::bad_request(format!("model for {kind}: a handle (1 to 200 printable bytes), or null for Automatic")));
        }
    }
    edit(root, |doc| {
        if !doc.get("models").is_some_and(Item::is_table_like) {
            if models.values().all(Option::is_none) {
                return Ok(());
            }
            doc["models"] = Item::Table(Table::new());
        }
        let Some(table) = doc["models"].as_table_like_mut() else { return Ok(()) };
        for (kind, handle) in models {
            match handle {
                Some(handle) => {
                    let decor = table.get(kind).and_then(Item::as_value).map(|v| v.decor().clone());
                    let mut new = toml_edit::Value::from(handle.trim());
                    if let Some(decor) = decor {
                        *new.decor_mut() = decor;
                    }
                    match table.get_mut(kind) {
                        Some(item) => *item = Item::Value(new),
                        None => drop(table.insert(kind, Item::Value(new))),
                    }
                }
                None => drop(table.remove(kind)),
            }
        }
        Ok(())
    })
}

/// Add canvas `name` to `[canvases] order` (once), or remove it; nothing
/// without `studio.toml`.
pub fn order(root: &Path, name: &str, add: bool) -> Result<(), RpcError> {
    if !root.join(TOML).exists() {
        return Ok(());
    }
    edit(root, |doc| {
        if !doc.get("canvases").is_some_and(Item::is_table_like) {
            doc["canvases"] = Item::Table(Table::new());
        }
        let canvases = &mut doc["canvases"];
        if !canvases.get("order").is_some_and(Item::is_array) {
            canvases["order"] = value(Array::new());
        }
        let Some(order) = canvases["order"].as_array_mut() else { return Ok(()) };
        order.retain(|v| v.as_str() != Some(name));
        if add {
            order.push(name);
        }
        Ok(())
    })
}
