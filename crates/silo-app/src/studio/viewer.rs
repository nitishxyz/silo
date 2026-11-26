//! The Studio block's host of the shared media viewer (`crate::media_viewer`, docs/STUDIO.md §4.2): a canvas's
//! viewable nodes in reading order (what `[`/`]` walk: images and videos of assets, entities' sheets, cards'
//! picked takes), their media through the store's asset cache, the footer's caption from the take (its model,
//! prompt, size) or the node. Documents open in the document panel (`documents.rs`) and audio plays on its node:
//! neither is an item. How the viewer layers over the canvas is [`layers`]. The World view's entity page hosts one
//! too ([`EntityHost`]): the entity's sheet, then its refs; and the Production view's ▶ Play ([`CutHost`]): an
//! episode's newest cut, then its older versions.

use std::rc::Rc;

use gpui::{App, Entity};
use silo_proto::studio::{Canvas, Cut, EntityInfo, NodeBody, Section, SourceMedia, StudioMedia, StudioView, Take};

use super::{
    model,
    store::{Asset, AssetKind, Store},
};
use crate::{
    agent::media::{Entry, cache_file},
    media_viewer::{Item, Kind, Source},
};

/// What draws over a canvas's board, bottom to top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// The canvas's own: toolbar, selection actions, minimap, card settings, the canvas prompt, pickers,
    /// notices, its menu.
    Overlays,
    /// The Documents button and its list.
    Documents,
    DocumentPanel,
    /// The media viewer, over the whole body.
    Viewer,
}

/// The layers drawn over the board: `viewer` is `None` without one, else whether it is open (not fading out).
/// Open, it alone draws (opaque, above everything, taking all input); fading out, it goes on over the rest.
#[must_use]
pub fn layers(viewer: Option<bool>) -> &'static [Layer] {
    match viewer {
        None => &[Layer::Overlays, Layer::Documents, Layer::DocumentPanel],
        Some(true) => &[Layer::Viewer],
        Some(false) => &[Layer::Overlays, Layer::Documents, Layer::DocumentPanel, Layer::Viewer],
    }
}

/// `image/png`, `video/mp4` for a file (the footer's format), by its extension.
fn mime(path: &str, kind: Kind) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let ext = match ext.as_str() {
        "jpg" => "jpeg",
        "svg" => "svg+xml",
        "tif" => "tiff",
        e => e,
    };
    let top = if kind == Kind::Video { "video" } else { "image" };
    if ext.is_empty() { String::new() } else { format!("{top}/{ext}") }
}

/// The viewer's items for `canvas`: its nodes in reading order that output an image or video file (an asset, an
/// entity's sheet, a card's picked take), captioned from the take (or the asset's own fields); `scratch` is
/// where copies named as made go (by file).
#[must_use]
pub fn items(canvas: &Canvas, sections: &[Section], scratch: &dyn Fn(&str) -> Option<std::path::PathBuf>) -> Vec<Item> {
    model::walk(canvas)
        .into_iter()
        .filter_map(|id| {
            let out = canvas.output_of(&id, sections)?;
            let path = out.path?;
            let kind = match out.media {
                SourceMedia::Image => Kind::Image,
                SourceMedia::Video => Kind::Video,
                _ => return None,
            };
            let node = canvas.node(&id)?;
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            let mut item = Item {
                id: id.clone(),
                key: path.clone(),
                title: None,
                mime: mime(&path, kind),
                kind,
                size: None,
                dims: None,
                ms: None,
                credit: None,
                prompt: None,
                scratch: scratch(&path),
                name,
            };
            match &node.body {
                NodeBody::Asset { width, height, seconds, prompt, .. } => {
                    item.dims = width.zip(*height);
                    item.ms = seconds.map(ms);
                    item.prompt.clone_from(prompt);
                }
                body @ NodeBody::Gen { .. } => {
                    item.title = Some(out.label);
                    if let Some(take) = body.take() {
                        caption_from(&mut item, take);
                    }
                }
                _ => item.title = Some(out.label),
            }
            Some(item)
        })
        .collect()
}

/// A take's caption: its size, length, model and prompt; its file is the item's media (`at` too: a take made
/// again in place is new media).
pub fn caption_from(item: &mut Item, take: &Take) {
    item.key = format!("{}#{}", take.path, take.at);
    item.dims = take.width.zip(take.height);
    item.ms = take.seconds.map(ms);
    item.credit = take.model.clone().filter(|m| !m.trim().is_empty());
    item.prompt = take.prompt.clone().filter(|p| !p.trim().is_empty());
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ms(secs: f64) -> u64 {
    (secs.max(0.0) * 1000.0) as u64
}

/// The viewer's view of a canvas: its items, and their media from the store.
pub struct Host {
    pub store: Entity<Store>,
    pub canvas: String,
}

impl Host {
    pub fn source(store: Entity<Store>, canvas: String) -> Rc<dyn Source> {
        Rc::new(Self { store, canvas })
    }

    /// Node `id`'s file now, and how it is read.
    fn file(&self, id: &str, cx: &App) -> Option<(String, AssetKind)> {
        let s = self.store.read(cx);
        let out = s.canvas(&self.canvas)?.output_of(id, s.sections.as_deref().unwrap_or(&[]))?;
        let kind = if out.media == SourceMedia::Video { AssetKind::Video } else { AssetKind::Image };
        Some((out.path?, kind))
    }
}

impl Source for Host {
    fn items(&self, cx: &App) -> Vec<Item> {
        let s = self.store.read(cx);
        let Some(canvas) = s.canvas(&self.canvas) else { return Vec::new() };
        let mut items = items(canvas, s.sections.as_deref().unwrap_or(&[]), &scratch_of(&s.block_id));
        // What reading the file told: its size, and the pixels and length the canvas didn't say.
        for item in &mut items {
            let Some((path, _)) = self.file(&item.id, cx) else { continue };
            let Some((asset, size)) = s.cached(&path) else { continue };
            item.size = size.map(crate::files::model::human_size);
            match asset {
                Asset::Image(l) => item.dims = item.dims.or(Some(l.dims)),
                Asset::Clip(c) => {
                    item.ms = item.ms.or(c.ms);
                    item.dims = item.dims.or(c.poster.as_ref().map(|p| p.1));
                }
                _ => {}
            }
        }
        items
    }

    fn media(&self, item: &Item, cx: &mut App) -> Entry {
        let Some((path, kind)) = self.file(&item.id, cx) else { return Entry::Failed };
        match self.store.update(cx, |s, cx| s.asset(&path, kind, cx)) {
            Asset::Loading => Entry::Loading,
            Asset::Image(l) => Entry::Ready(l),
            Asset::Clip(c) => Entry::Clip(c),
            Asset::Text(_) | Asset::Failed => Entry::Failed,
        }
    }
}

/// The viewer's items for an entity's page: its sheet, then its refs (one that is the sheet once; audio, which
/// plays on its tile, left out), by file ([`model::RefMedia`]: a video or an image); the sheet titled by the
/// entity's name.
#[must_use]
pub fn entity_items(info: &EntityInfo, scratch: &dyn Fn(&str) -> Option<std::path::PathBuf>) -> Vec<Item> {
    let sheet = info.sheet.iter().map(|p| (p, Some(info.name.clone())));
    let refs = info.refs.iter().map(|p| (p, None));
    let mut seen = std::collections::HashSet::new();
    sheet
        .chain(refs)
        .filter(|(p, _)| model::RefMedia::of(p) != model::RefMedia::Audio && seen.insert(p.as_str()))
        .map(|(path, title)| {
            let kind = if model::RefMedia::of(path) == model::RefMedia::Video { Kind::Video } else { Kind::Image };
            Item {
                id: path.clone(),
                key: path.clone(),
                name: path.rsplit('/').next().unwrap_or(path).to_owned(),
                title,
                kind,
                mime: mime(path, kind),
                size: None,
                dims: None,
                ms: None,
                credit: None,
                prompt: None,
                scratch: scratch(path),
            }
        })
        .collect()
}

/// Whether the World view's viewer, open on entity `viewing`, still has its page: the World view shows that
/// entity (`world`: the World view is what the body shows). Else it closes.
#[must_use]
pub fn entity_viewer_kept(world: bool, view: StudioView, shown: Option<&str>, viewing: &str) -> bool {
    world && view == StudioView::World && shown == Some(viewing)
}

/// Where copies named as made go for files of block `block_id`, by file.
fn scratch_of(block_id: &str) -> impl Fn(&str) -> Option<std::path::PathBuf> {
    let block = format!("studio-{block_id}");
    let root = crate::cache::root();
    move |path: &str| {
        let name = path.rsplit('/').next().unwrap_or(path);
        Some(cache_file(&root, &block, path, name).with_extension(""))
    }
}

/// The viewer's view of an entity's page: its sheet and refs, their media from the store.
pub struct EntityHost {
    pub store: Entity<Store>,
    pub entity: String,
}

impl EntityHost {
    pub fn source(store: Entity<Store>, entity: String) -> Rc<dyn Source> {
        Rc::new(Self { store, entity })
    }
}

impl Source for EntityHost {
    fn items(&self, cx: &App) -> Vec<Item> {
        let s = self.store.read(cx);
        let Some(info) = s.entity_info(&self.entity) else { return Vec::new() };
        let mut items = entity_items(info, &scratch_of(&s.block_id));
        for item in &mut items {
            let Some((asset, size)) = s.cached(&item.id) else { continue };
            item.size = size.map(crate::files::model::human_size);
            match asset {
                Asset::Image(l) => item.dims = Some(l.dims),
                Asset::Clip(c) => {
                    item.ms = c.ms;
                    item.dims = c.poster.as_ref().map(|p| p.1);
                }
                _ => {}
            }
        }
        items
    }

    fn media(&self, item: &Item, cx: &mut App) -> Entry {
        let kind = if item.kind == Kind::Video { AssetKind::Video } else { AssetKind::Image };
        match self.store.update(cx, |s, cx| s.asset(&item.id, kind, cx)) {
            Asset::Loading => Entry::Loading,
            Asset::Image(l) => Entry::Ready(l),
            Asset::Clip(c) => Entry::Clip(c),
            Asset::Text(_) | Asset::Failed => Entry::Failed,
        }
    }
}

/// The version a cut's file names (`canvases/ep01/out/ep01-v12.mp4` → 12).
#[must_use]
pub fn cut_version(path: &str) -> Option<u32> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.rsplit_once("-v")?.1.parse().ok()
}

/// The viewer's items for an episode's cuts: the newest, then the older versions newest first (so `]` steps back
/// through them), each titled `<episode> · v<N>`; the newest says when it is stale.
#[must_use]
pub fn cut_items(cut: &Cut, title: &str, scratch: &dyn Fn(&str) -> Option<std::path::PathBuf>) -> Vec<Item> {
    let newest = std::iter::once((cut.path.as_str(), Some(cut.version), cut.stale));
    let older = cut.older.iter().map(|p| (p.as_str(), cut_version(p), false));
    newest
        .chain(older)
        .map(|(path, version, stale)| {
            let name = path.rsplit('/').next().unwrap_or(path).to_owned();
            let mut label = version.map_or_else(|| format!("{title} \u{b7} {name}"), |v| format!("{title} \u{b7} v{v}"));
            if stale {
                label.push_str(" (stale: takes changed since)");
            }
            Item {
                id: path.to_owned(),
                key: path.to_owned(),
                name,
                title: Some(label),
                kind: Kind::Video,
                mime: mime(path, Kind::Video),
                size: None,
                dims: None,
                ms: None,
                credit: None,
                prompt: None,
                scratch: scratch(path),
            }
        })
        .collect()
}

/// Whether the cut viewer still has its page: the body shows the Production view and the episode has a cut.
#[must_use]
pub fn cut_viewer_kept(production_shown: bool, has_cut: bool) -> bool {
    production_shown && has_cut
}

/// The viewer's view of an episode's cuts (the Production view's ▶ Play), their media from the store.
pub struct CutHost {
    pub store: Entity<Store>,
    pub canvas: String,
}

impl CutHost {
    pub fn source(store: Entity<Store>, canvas: String) -> Rc<dyn Source> {
        Rc::new(Self { store, canvas })
    }
}

impl Source for CutHost {
    fn items(&self, cx: &App) -> Vec<Item> {
        let s = self.store.read(cx);
        let Some(e) = s.production().and_then(|p| p.episodes.iter().find(|e| e.canvas == self.canvas)) else { return Vec::new() };
        let Some(cut) = &e.cut else { return Vec::new() };
        let mut items = cut_items(cut, &super::production::episode_title(e), &scratch_of(&s.block_id));
        for item in &mut items {
            let Some((asset, size)) = s.cached(&item.id) else { continue };
            item.size = size.map(crate::files::model::human_size);
            if let Asset::Clip(c) = asset {
                item.ms = c.ms;
                item.dims = c.poster.as_ref().map(|p| p.1);
            }
        }
        items
    }

    fn media(&self, item: &Item, cx: &mut App) -> Entry {
        match self.store.update(cx, |s, cx| s.asset(&item.id, AssetKind::Video, cx)) {
            Asset::Loading => Entry::Loading,
            Asset::Clip(c) => Entry::Clip(c),
            Asset::Image(_) | Asset::Text(_) | Asset::Failed => Entry::Failed,
        }
    }
}

/// A text asset node: it opens in the document panel, not the viewer.
#[must_use]
pub fn document(canvas: &Canvas, id: &str) -> bool {
    matches!(canvas.node(id).map(|n| &n.body), Some(NodeBody::Asset { media: StudioMedia::Text, .. }))
}
