//! The Studio block's data, shared by its views (the pane, the World view, [`super::canvas::CanvasView`]): the
//! project, the world's sections, canvases by name (kept by `rev`), entities read for their detail, and the
//! asset cache (files read through `studio.asset` in chunks: images decoded off the main thread, video and
//! audio written to a cache file for AVFoundation, as the agent block's attachments). `studio.changed`
//! re-reads only what it names, and `studio.production` on every one (docs/STUDIO.md §4.6). Also otto's media
//! models by kind (the card settings' model list, the Models menu). Every `studio.*` call goes through
//! [`Store::call`].

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use gpui::{Context, EventEmitter};
use otto_proto::{MediaKind, MediaModelInfo};
use silo_proto::{
    control::{Method, RpcError},
    studio::{
        Canvas, Entity, EntityInfo, EntityRefEdited, NodeBody, Production, Project, RefChange, Section, StudioAsset, StudioAssetParams,
        StudioCanvasParams, StudioChange, StudioEntityApproveParams, StudioEntityLinkParams, StudioEntityParams, StudioEntityRefParams,
        StudioProjectParams, StudioWorld,
    },
};

use crate::agent::media::{self, Clip, Loaded};

/// Something read, or why not.
#[derive(Clone, Debug)]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(t) => Some(t),
            _ => None,
        }
    }
}

/// What changed in the store (views re-render; the pane also refreshes its header).
#[derive(Clone, Debug)]
pub enum StoreEvent {
    Project,
    World,
    Canvas(String),
    /// A canvas's documents were listed.
    #[allow(dead_code, reason = "views re-render on any event; the name is for those that care")]
    Documents(String),
    #[allow(dead_code, reason = "views re-render on any event; the id is for those that care")]
    Entity(String),
    /// `studio.production` was read.
    Production,
    /// otto's models of a kind were listed.
    Models,
    Assets,
    /// A call failed: say so.
    Failed(String),
}

/// A canvas read, by name.
#[derive(Default)]
pub struct CanvasSlot {
    pub canvas: Option<Arc<Canvas>>,
    pub error: Option<String>,
    loading: bool,
    /// A newer rev announced while a read was out: read again after it.
    again: bool,
}

/// What an asset is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    Image,
    Video,
    Audio,
    Text,
}

#[derive(Clone)]
pub enum Asset {
    /// Placeholders shimmer meanwhile.
    Loading,
    Image(Arc<Loaded>),
    Clip(Arc<Clip>),
    Text(Arc<str>),
    /// Not readable: a quiet placeholder, not asked again until the file changes.
    Failed,
}

struct AssetSlot {
    asset: Asset,
    /// The file may have changed: read again, showing the old meanwhile.
    stale: bool,
    fetching: bool,
    /// The file's bytes, as last read.
    size: Option<u64>,
}

/// The audio file playing (or paused where it was), by path.
pub struct Playing {
    pub path: String,
    pub player: crate::agent::av::Player,
    pub drag: bool,
}

pub struct Store {
    pub block_id: String,
    rt: tokio::runtime::Handle,
    control: Option<silo_client::Control>,
    /// This host's otto (cards run there: `studio.run_card`, docs/STUDIO.md §6.2); `None` where it has none.
    pub otto: Option<crate::agent::client::Otto>,
    /// The block's root (absolute, on its host): the cwd of otto calls.
    pub root: String,
    /// `None`: not read yet.
    pub project: Option<Load<Project>>,
    /// `studio.project` said `not_found`: no `studio.toml` yet (Setup).
    pub missing: bool,
    pub sections: Option<Vec<Section>>,
    pub world_error: Option<String>,
    canvases: HashMap<String, CanvasSlot>,
    /// Each canvas's documents (root-relative, `script.md` first), by canvas, listed on first ask.
    documents: HashMap<String, Load<Vec<String>>>,
    entities: HashMap<String, Load<Entity>>,
    assets: HashMap<String, AssetSlot>,
    /// When cards were first seen running, by `canvas#node` (the elapsed time; the server keeps none).
    running: HashMap<String, Instant>,
    /// `studio.production`, read with the project and again on every `studio.changed`.
    pub production: Option<Load<Production>>,
    production_loading: bool,
    production_again: bool,
    /// otto's media models by kind, listed on first ask.
    models: HashMap<MediaKind, Load<Vec<MediaModelInfo>>>,
    /// Entities whose approval is being written.
    pub approving: HashSet<String>,
    pub audio: Option<Playing>,
    /// Shimmer phase origin.
    pub epoch: Instant,
}

impl EventEmitter<StoreEvent> for Store {}

/// A failed call's code (`not_found`, `method_not_found`).
fn code(error: &anyhow::Error) -> Option<&str> {
    error.downcast_ref::<RpcError>().map(|e| e.code.as_str())
}

/// A failed call, for people; a server without the call says so plainly.
#[must_use]
pub fn describe(error: &anyhow::Error) -> String {
    match code(error) {
        Some(RpcError::METHOD_NOT_FOUND) => "This server can't open studios yet".to_owned(),
        _ => crate::files::describe(error),
    }
}

impl Store {
    pub fn new(
        block_id: String,
        root: String,
        rt: tokio::runtime::Handle,
        control: Option<silo_client::Control>,
        otto: Option<crate::agent::client::Otto>,
    ) -> Self {
        Self {
            block_id,
            rt,
            control,
            otto,
            root,
            project: None,
            missing: false,
            sections: None,
            world_error: None,
            canvases: HashMap::new(),
            documents: HashMap::new(),
            entities: HashMap::new(),
            assets: HashMap::new(),
            running: HashMap::new(),
            production: None,
            production_loading: false,
            production_again: false,
            models: HashMap::new(),
            approving: HashSet::new(),
            audio: None,
            epoch: Instant::now(),
        }
    }

    /// The connection is back: read everything again.
    pub fn set_control(&mut self, control: silo_client::Control, cx: &mut Context<Self>) {
        self.control = Some(control);
        self.reload(cx);
    }

    pub fn set_root(&mut self, root: String, cx: &mut Context<Self>) {
        if root != self.root {
            self.root = root;
            self.canvases.clear();
            self.documents.clear();
            self.entities.clear();
            self.assets.clear();
            self.production = None;
            self.reload(cx);
        }
    }

    /// Read the project and world, and the canvases already read.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_project(cx);
        self.load_world(cx);
        self.load_production(cx);
        let names: Vec<String> = self.canvases.keys().cloned().collect();
        for name in names {
            self.load_canvas(&name, cx);
        }
        let listed: Vec<String> = self.documents.keys().cloned().collect();
        for name in listed {
            self.load_documents(&name, cx);
        }
    }

    /// A handle making calls on the block's host from any view.
    pub fn caller(&self) -> Caller {
        Caller { rt: self.rt.clone(), control: self.control.clone(), otto: self.otto.clone(), block_id: self.block_id.clone() }
    }

    /// Call `method` on the block's host; `done` runs on `V` (any view holding the store) with the result.
    pub fn call<T, V>(&self, method: Method, cx: &mut Context<V>, done: impl FnOnce(&mut V, anyhow::Result<T>, &mut Context<V>) + 'static)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        V: 'static,
    {
        self.caller().call(method, cx, done);
    }

    /// Call `method`, only telling of a failure (the change comes back as `studio.changed` / `block.changed`).
    pub fn send(&self, method: Method, cx: &mut Context<Self>) {
        self.call(method, cx, |_, r: anyhow::Result<serde_json::Value>, cx| {
            if let Err(e) = r {
                cx.emit(StoreEvent::Failed(describe(&e)));
            }
        });
    }

    // ------------------------------------------------------------ project, world

    pub fn project(&self) -> Option<&Project> {
        self.project.as_ref()?.ready()
    }

    pub fn load_project(&mut self, cx: &mut Context<Self>) {
        if self.project.is_none() {
            self.project = Some(Load::Loading);
        }
        let method = Method::StudioProject(StudioProjectParams { block_id: self.block_id.clone() });
        self.call(method, cx, |this: &mut Self, r: anyhow::Result<Project>, cx| {
            match r {
                Ok(p) => {
                    this.missing = false;
                    this.project = Some(Load::Ready(p));
                }
                Err(e) if code(&e) == Some("not_found") => {
                    this.missing = true;
                    this.project = Some(Load::Failed(describe(&e)));
                }
                Err(e) => this.project = Some(Load::Failed(describe(&e))),
            }
            cx.emit(StoreEvent::Project);
        });
    }

    /// A project write's answer (`studio.init`, `set_format`, …) taken at once.
    pub fn took_project(&mut self, project: Project, cx: &mut Context<Self>) {
        self.missing = false;
        self.project = Some(Load::Ready(project));
        cx.emit(StoreEvent::Project);
        cx.notify();
    }

    pub fn load_world(&mut self, cx: &mut Context<Self>) {
        let method = Method::StudioWorld(StudioProjectParams { block_id: self.block_id.clone() });
        self.call(method, cx, |this: &mut Self, r: anyhow::Result<StudioWorld>, cx| {
            match r {
                Ok(w) => {
                    this.sections = Some(w.sections);
                    this.world_error = None;
                }
                Err(e) => this.world_error = Some(describe(&e)),
            }
            cx.emit(StoreEvent::World);
        });
    }

    // ------------------------------------------------------------ production

    /// Where the production stands, as last read.
    pub fn production(&self) -> Option<&Production> {
        self.production.as_ref()?.ready()
    }

    /// `studio.production` (about 10 ms on the server): one read at a time, one more after it when asked meanwhile.
    pub fn load_production(&mut self, cx: &mut Context<Self>) {
        if self.production_loading {
            self.production_again = true;
            return;
        }
        self.production_loading = true;
        if self.production.is_none() {
            self.production = Some(Load::Loading);
        }
        let method = Method::StudioProduction(StudioProjectParams { block_id: self.block_id.clone() });
        self.call(method, cx, |this: &mut Self, r: anyhow::Result<Production>, cx| {
            this.production_loading = false;
            match r {
                Ok(p) => this.production = Some(Load::Ready(p)),
                // A read that failed keeps what was shown; with nothing shown, it says why.
                Err(e) if this.production().is_none() => this.production = Some(Load::Failed(describe(&e))),
                Err(_) => {}
            }
            cx.emit(StoreEvent::Production);
            if std::mem::take(&mut this.production_again) {
                this.load_production(cx);
            }
        });
    }

    /// An entity's info as a write answered it (`studio.entity_approve`): shown at once in its section, its
    /// detail and its production row; the `studio.changed` that follows reads them all again.
    pub fn took_entity_info(&mut self, info: EntityInfo, cx: &mut Context<Self>) {
        if let Some(Load::Ready(e)) = self.entities.get_mut(&info.id) {
            e.info = info.clone();
        }
        for e in self.sections.iter_mut().flatten().flat_map(|s| &mut s.entities).filter(|e| e.id == info.id) {
            *e = info.clone();
        }
        if let Some(Load::Ready(p)) = &mut self.production {
            for a in p.assets.iter_mut().filter(|a| a.id == info.id) {
                a.approval = info.approval;
                a.sheet.clone_from(&info.sheet);
            }
        }
        cx.emit(StoreEvent::Entity(info.id));
        cx.notify();
    }

    /// Approve entity `id`'s sheet (`approved`), or withdraw it: shown when the server answers.
    pub fn approve(&mut self, id: &str, approved: bool, cx: &mut Context<Self>) {
        if !self.approving.insert(id.to_owned()) {
            return;
        }
        let method = Method::StudioEntityApprove(StudioEntityApproveParams { block_id: self.block_id.clone(), id: id.to_owned(), approved });
        let key = id.to_owned();
        self.call(method, cx, move |this: &mut Self, r: anyhow::Result<EntityInfo>, cx| {
            this.approving.remove(&key);
            match r {
                Ok(info) => this.took_entity_info(info, cx),
                Err(e) => cx.emit(StoreEvent::Failed(describe(&e))),
            }
        });
        cx.notify();
    }

    /// One change to entity `id`'s named references (`studio.entity_ref`): shown when the server answers, as an
    /// approval is.
    pub fn edit_ref(&mut self, id: &str, change: RefChange, cx: &mut Context<Self>) {
        let method = Method::StudioEntityRef(StudioEntityRefParams { block_id: self.block_id.clone(), id: id.to_owned(), change });
        self.call(method, cx, |this: &mut Self, r: anyhow::Result<EntityRefEdited>, cx| match r {
            Ok(done) => this.took_entity_info(done.info, cx),
            Err(e) => cx.emit(StoreEvent::Failed(describe(&e))),
        });
    }

    /// Link entity `id` with `other` (`linked`), or unlink them (`studio.entity_link`): `id`'s answer shown at
    /// once, and `other`'s links as they now read (a link reads from both ends) until the world is read again.
    pub fn link(&mut self, id: &str, other: &str, linked: bool, cx: &mut Context<Self>) {
        let method =
            Method::StudioEntityLink(StudioEntityLinkParams { block_id: self.block_id.clone(), id: id.to_owned(), other: other.to_owned(), linked });
        let other = other.to_owned();
        self.call(method, cx, move |this: &mut Self, r: anyhow::Result<EntityInfo>, cx| match r {
            Ok(info) => {
                let id = info.id.clone();
                let fix = |e: &mut EntityInfo| {
                    e.links.retain(|l| *l != id);
                    if linked {
                        e.links.push(id.clone());
                    }
                };
                if let Some(Load::Ready(e)) = this.entities.get_mut(&other) {
                    fix(&mut e.info);
                }
                for e in this.sections.iter_mut().flatten().flat_map(|s| &mut s.entities).filter(|e| e.id == other) {
                    fix(e);
                }
                this.took_entity_info(info, cx);
            }
            Err(e) => cx.emit(StoreEvent::Failed(describe(&e))),
        });
    }

    /// The project's pick for `kind` (`image`, `video`, `speech`): `model` a handle, `None` Automatic.
    pub fn set_model(&mut self, kind: &str, model: Option<String>, cx: &mut Context<Self>) {
        let method = Method::StudioSetModels(super::production::set_models_params(&self.block_id, kind, model));
        self.call(method, cx, |this: &mut Self, r: anyhow::Result<Project>, cx| match r {
            Ok(p) => {
                if let Some(Load::Ready(prod)) = &mut this.production {
                    prod.models.clone_from(&p.models);
                }
                this.took_project(p, cx);
                cx.emit(StoreEvent::Production);
            }
            Err(e) => cx.emit(StoreEvent::Failed(describe(&e))),
        });
    }

    // ------------------------------------------------------------ otto's models

    /// otto's models of `kind` as last listed (`None`: not asked yet).
    pub fn models(&self, kind: MediaKind) -> Option<&Load<Vec<MediaModelInfo>>> {
        self.models.get(&kind)
    }

    /// otto's models of `kind` (`media.models`), listed on first ask.
    pub fn want_models(&mut self, kind: MediaKind, cx: &mut Context<Self>) {
        if self.models.contains_key(&kind) {
            return;
        }
        self.models.insert(kind, Load::Loading);
        self.caller().otto(
            cx,
            move |c| async move { c.media_models(Some(kind), None, None).await },
            move |this: &mut Self, r, cx| {
                this.models.insert(kind, r.map_or_else(|e| Load::Failed(crate::agent::describe(&e)), Load::Ready));
                cx.emit(StoreEvent::Models);
            },
        );
    }

    /// Providers changed (one connected): list the models again when next asked.
    pub fn forget_models(&mut self) {
        self.models.clear();
    }

    /// Entity `id` (`characters/mara`), read on first ask.
    pub fn entity(&mut self, id: &str, cx: &mut Context<Self>) -> Load<Entity> {
        if let Some(e) = self.entities.get(id) {
            return e.clone();
        }
        self.load_entity(id, cx);
        Load::Loading
    }

    /// Entity `id`'s info as last read, nothing fetched: its detail's, else its section's.
    pub fn entity_info(&self, id: &str) -> Option<&silo_proto::studio::EntityInfo> {
        match self.entities.get(id) {
            Some(Load::Ready(e)) => Some(&e.info),
            _ => self.sections.iter().flatten().flat_map(|s| &s.entities).find(|e| e.id == id),
        }
    }

    fn load_entity(&mut self, id: &str, cx: &mut Context<Self>) {
        self.entities.entry(id.to_owned()).or_insert(Load::Loading);
        let method = Method::StudioEntity(StudioEntityParams { block_id: self.block_id.clone(), id: id.to_owned() });
        let key = id.to_owned();
        self.call(method, cx, move |this: &mut Self, r: anyhow::Result<Entity>, cx| {
            this.entities.insert(key.clone(), r.map_or_else(|e| Load::Failed(describe(&e)), Load::Ready));
            cx.emit(StoreEvent::Entity(key));
        });
    }

    // ------------------------------------------------------------ canvases

    /// Canvas `name` as last read.
    pub fn canvas(&self, name: &str) -> Option<&Arc<Canvas>> {
        self.canvases.get(name)?.canvas.as_ref()
    }

    pub fn canvas_error(&self, name: &str) -> Option<&str> {
        self.canvases.get(name)?.error.as_deref()
    }

    /// Canvas `name`, read on first ask.
    pub fn want_canvas(&mut self, name: &str, cx: &mut Context<Self>) -> Option<Arc<Canvas>> {
        if !self.canvases.contains_key(name) {
            self.load_canvas(name, cx);
        }
        self.canvas(name).cloned()
    }

    fn load_canvas(&mut self, name: &str, cx: &mut Context<Self>) {
        let slot = self.canvases.entry(name.to_owned()).or_default();
        if slot.loading {
            slot.again = true;
            return;
        }
        slot.loading = true;
        let method = Method::StudioCanvas(StudioCanvasParams { block_id: self.block_id.clone(), name: name.to_owned() });
        let key = name.to_owned();
        self.call(method, cx, move |this: &mut Self, r: anyhow::Result<Canvas>, cx| {
            let slot = this.canvases.entry(key.clone()).or_default();
            slot.loading = false;
            match r {
                // An older answer (a write raced the read) never replaces a newer one.
                Ok(c) if slot.canvas.as_ref().is_none_or(|old| c.rev >= old.rev) => {
                    slot.canvas = Some(Arc::new(c));
                    slot.error = None;
                }
                Ok(_) => {}
                Err(e) => slot.error = Some(describe(&e)),
            }
            let again = std::mem::take(&mut slot.again);
            this.track_running(&key);
            cx.emit(StoreEvent::Canvas(key.clone()));
            if again {
                this.load_canvas(&key, cx);
            }
        });
    }

    /// Note when cards started running (and forget those that stopped).
    fn track_running(&mut self, name: &str) {
        let Some(canvas) = self.canvas(name).cloned() else { return };
        let prefix = format!("{name}#");
        let now: Vec<String> = canvas
            .nodes
            .iter()
            .filter(|n| matches!(&n.body, NodeBody::Gen { status, .. } if super::model::running(*status)))
            .map(|n| format!("{prefix}{}", n.id))
            .collect();
        self.running.retain(|k, _| !k.starts_with(&prefix) || now.contains(k));
        for k in now {
            self.running.entry(k).or_insert_with(Instant::now);
        }
    }

    /// How long card `node` of `canvas` has been running here (as far as this pane saw).
    pub fn running_since(&self, canvas: &str, node: &str) -> Option<Instant> {
        self.running.get(&format!("{canvas}#{node}")).copied()
    }

    /// `studio.changed`: re-read what it names (a canvas only when read here and behind `rev`), and where the
    /// production stands (any change can move it).
    pub fn changed(&mut self, what: StudioChange, name: Option<String>, rev: Option<u64>, cx: &mut Context<Self>) {
        if what != StudioChange::Unknown {
            self.load_production(cx);
        }
        match what {
            StudioChange::Project => self.load_project(cx),
            StudioChange::World => {
                self.load_world(cx);
                let ids: Vec<String> = self.entities.keys().cloned().collect();
                for id in ids {
                    self.load_entity(&id, cx);
                }
                self.stale("world/", cx);
                // A sheet changed on disk (fixed by hand, made again): the cards wired from its entity go out of
                // date, which the server says when the canvas is read (its files' mtimes), so canvases showing
                // entities are read again.
                let wired: Vec<String> = self
                    .canvases
                    .iter()
                    .filter(|(_, s)| s.canvas.as_deref().is_some_and(super::model::shows_entities))
                    .map(|(n, _)| n.clone())
                    .collect();
                for name in wired {
                    self.load_canvas(&name, cx);
                }
            }
            StudioChange::Canvas => {
                let Some(name) = name else { return };
                let loaded = self.canvases.get(&name).and_then(|s| s.canvas.clone());
                // Not ahead of what was read: a file in its folder changed (a document rewritten, added or removed;
                // an asset; an echo of a write). Its documents are listed and read again, and a canvas showing
                // documents is read again for its Out of date.
                let mut again = false;
                if loaded.as_ref().is_none_or(|c| rev.is_none_or(|r| r <= c.rev)) {
                    self.stale_documents(&name, cx);
                    again = loaded.as_ref().is_some_and(|c| c.nodes.iter().any(|n| c.document_of(&n.id).is_some()));
                }
                let behind = self.canvases.get(&name).is_some_and(|s| s.canvas.as_ref().is_none_or(|c| rev.is_none_or(|r| r > c.rev)) || s.loading);
                if behind || again {
                    self.load_canvas(&name, cx);
                }
            }
            StudioChange::Unknown => self.reload(cx),
        }
    }

    // ------------------------------------------------------------ documents

    /// Canvas `name`'s documents (`canvases/<name>/*.md`, `.txt`; `script.md` first), listed on first ask.
    pub fn documents(&mut self, name: &str, cx: &mut Context<Self>) -> Option<Vec<String>> {
        match self.documents.get(name) {
            Some(l) => l.ready().cloned(),
            None => {
                self.load_documents(name, cx);
                None
            }
        }
    }

    fn load_documents(&mut self, name: &str, cx: &mut Context<Self>) {
        self.documents.entry(name.to_owned()).or_insert(Load::Loading);
        let method = Method::FilesList(silo_proto::files::FilesListParams { block_id: self.block_id.clone(), path: format!("canvases/{name}") });
        let key = name.to_owned();
        self.call(method, cx, move |this: &mut Self, r: anyhow::Result<silo_proto::files::FileList>, cx| {
            let listed = r.map(|l| {
                let files = l.entries.into_iter().filter(|e| e.kind != silo_proto::files::FileKind::Dir).map(|e| e.name);
                silo_proto::studio::documents_of(&key, files)
            });
            this.documents.insert(key.clone(), listed.map_or_else(|e| Load::Failed(describe(&e)), Load::Ready));
            cx.emit(StoreEvent::Documents(key));
        });
    }

    /// A file in canvas `name`'s folder changed: list its documents again and read the ones shown again.
    fn stale_documents(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.documents.contains_key(name) {
            self.load_documents(name, cx);
        }
        let prefix = format!("canvases/{name}/");
        let mut any = false;
        for (path, slot) in &mut self.assets {
            if path.strip_prefix(&prefix).is_some_and(silo_proto::studio::is_document) {
                slot.stale = true;
                any = true;
            }
        }
        if any {
            cx.emit(StoreEvent::Assets);
        }
    }

    // ------------------------------------------------------------ assets

    /// Assets under `prefix` may have changed: read again when next shown (the old shows meanwhile).
    pub fn stale(&mut self, prefix: &str, cx: &mut Context<Self>) {
        for (path, slot) in &mut self.assets {
            if path.starts_with(prefix) {
                slot.stale = true;
            }
        }
        cx.emit(StoreEvent::Assets);
    }

    /// The file at `path` (root-relative) as `kind`, read on first ask (once; again when stale).
    pub fn asset(&mut self, path: &str, kind: AssetKind, cx: &mut Context<Self>) -> Asset {
        if let Some(slot) = self.assets.get_mut(path) {
            if !slot.stale || slot.fetching {
                return slot.asset.clone();
            }
            slot.stale = false;
        }
        let shown = self.assets.get(path).map_or(Asset::Loading, |s| s.asset.clone());
        let size = self.assets.get(path).and_then(|s| s.size);
        self.assets.insert(path.to_owned(), AssetSlot { asset: shown.clone(), stale: false, fetching: true, size });
        self.fetch(path.to_owned(), kind, cx);
        shown
    }

    /// The file at `path` as already read (or being read), and its size in bytes: nothing is fetched.
    pub fn cached(&self, path: &str) -> Option<(&Asset, Option<u64>)> {
        self.assets.get(path).map(|s| (&s.asset, s.size))
    }

    fn fetch(&self, path: String, kind: AssetKind, cx: &mut Context<Self>) {
        let Some(control) = self.control.clone() else { return };
        let block = self.block_id.clone();
        let wanted = path.clone();
        let task = self.rt.spawn(async move { read_all(control, block, wanted).await });
        let file = matches!(kind, AssetKind::Video | AssetKind::Audio).then(|| {
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            media::cache_file(&crate::cache::root(), &format!("studio-{}", self.block_id), &path, &name)
        });
        cx.spawn(async move |this, cx| {
            let read = task.await.ok().and_then(Result::ok);
            let done = cx.background_executor().spawn(async move {
                let (mime, bytes) = read?;
                let size = bytes.len() as u64;
                let asset = match (kind, file) {
                    (AssetKind::Video | AssetKind::Audio, Some(file)) => {
                        // The cached copy is replaced (a take re-made in place).
                        let _ = std::fs::remove_file(&file);
                        media::store(file, Some(bytes), kind == AssetKind::Video).map(|c| Asset::Clip(Arc::new(c)))
                    }
                    (AssetKind::Text, _) => Some(Asset::Text(String::from_utf8_lossy(&bytes).into())),
                    _ => media::decode(bytes, &mime).map(|l| Asset::Image(Arc::new(l))),
                };
                Some((asset?, size))
            });
            let (asset, size) = done.await.map_or((Asset::Failed, None), |(a, n)| (a, Some(n)));
            let _ = this.update(cx, |this, cx| {
                if let Some(slot) = this.assets.get_mut(&path) {
                    slot.fetching = false;
                    slot.asset = asset;
                    slot.size = size.or(slot.size);
                }
                cx.emit(StoreEvent::Assets);
                cx.notify();
            });
        })
        .detach();
    }

    // ------------------------------------------------------------ audio

    /// The player on `path`'s clip, made current (another one stops).
    pub fn audio_toggle(&mut self, path: &str, clip: &Clip, cx: &mut Context<Self>) {
        if self.audio.as_ref().is_none_or(|a| a.path != path) {
            self.audio = crate::agent::av::Player::new(&clip.file).map(|player| Playing { path: path.to_owned(), player, drag: false });
        }
        if let Some(a) = &self.audio {
            if a.player.playing() { a.player.pause() } else { a.player.play() }
        }
        cx.notify();
    }

    /// Seek `path`'s clip to `at` (0–1).
    pub fn audio_seek(&mut self, path: &str, clip: &Clip, at: f64, cx: &mut Context<Self>) {
        if self.audio.as_ref().is_none_or(|a| a.path != path) {
            self.audio = crate::agent::av::Player::new(&clip.file).map(|player| Playing { path: path.to_owned(), player, drag: false });
        }
        #[allow(clippy::cast_precision_loss)]
        let known = clip.ms.map(|ms| ms as f64 / 1000.0);
        if let Some(a) = &self.audio
            && let Some(d) = a.player.duration().or(known)
        {
            a.player.seek(at * d);
        }
        cx.notify();
    }

    /// Audio plays (or its bar is dragged): views redraw meanwhile.
    pub fn audio_playing(&self) -> bool {
        self.audio.as_ref().is_some_and(|a| a.drag || a.player.playing())
    }
}

/// Makes `studio.*` (and other) calls on the block's host.
#[derive(Clone)]
pub struct Caller {
    rt: tokio::runtime::Handle,
    control: Option<silo_client::Control>,
    otto: Option<crate::agent::client::Otto>,
    block_id: String,
}

impl Caller {
    /// Run `work` on this host's otto; `done` runs on `V` with its result (`Closed` where there is no otto).
    pub fn otto<T, V, F, Fut>(
        &self,
        cx: &mut Context<V>,
        work: F,
        done: impl FnOnce(&mut V, Result<T, otto_client::ClientError>, &mut Context<V>) + 'static,
    ) where
        T: Send + 'static,
        V: 'static,
        F: FnOnce(otto_client::Connection) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, otto_client::ClientError>> + Send + 'static,
    {
        let otto = self.otto.clone();
        let task = self.rt.spawn(async move {
            let otto = otto.ok_or(otto_client::ClientError::Closed)?;
            work(crate::agent::client::connection(&otto).await?).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or(Err(otto_client::ClientError::Closed));
            let _ = this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// All of file `path`'s bytes (root-relative), for `done` on `V`.
    pub fn read_file<V: 'static>(
        &self,
        path: &str,
        cx: &mut Context<V>,
        done: impl FnOnce(&mut V, anyhow::Result<Vec<u8>>, &mut Context<V>) + 'static,
    ) {
        let (control, block, path) = (self.control.clone(), self.block_id.clone(), path.to_owned());
        let task = self.rt.spawn(async move {
            let control = control.ok_or_else(|| anyhow::anyhow!("Not connected"))?;
            read_all(control, block, path).await.map(|(_, bytes)| bytes)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| done(this, result, cx));
        })
        .detach();
    }

    /// Call `method`; `done` runs on `V` with the result (`Not connected` without a connection).
    pub fn call<T, V>(&self, method: Method, cx: &mut Context<V>, done: impl FnOnce(&mut V, anyhow::Result<T>, &mut Context<V>) + 'static)
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        V: 'static,
    {
        let control = self.control.clone();
        let task = self.rt.spawn(async move {
            match control {
                Some(control) => control.call::<T>(method).await,
                None => Err(anyhow::anyhow!("Not connected")),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                done(this, result, cx);
                cx.notify();
            });
        })
        .detach();
    }
}

/// All of `path`'s bytes, a chunk (`StudioAsset::MAX_CHUNK`) per call, and its mime.
async fn read_all(control: silo_client::Control, block: String, path: String) -> anyhow::Result<(String, Vec<u8>)> {
    let mut bytes = Vec::new();
    loop {
        let offset = bytes.len() as u64;
        let method = Method::StudioAsset(StudioAssetParams { block_id: block.clone(), path: path.clone(), offset: Some(offset), length: None });
        let part: StudioAsset = control.call(method).await?;
        let chunk = data_encoding::BASE64.decode(part.data_base64.as_bytes())?;
        if chunk.is_empty() && offset < part.size {
            anyhow::bail!("{path}: short read");
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 >= part.size {
            return Ok((part.mime, bytes));
        }
    }
}
