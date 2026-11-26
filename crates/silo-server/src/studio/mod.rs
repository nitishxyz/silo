//! Studio blocks (docs/STUDIO.md): a media project in a folder (a world and
//! canvases), with no process behind it. The server holds the view state
//! ([`StudioState`]), answers `studio.*` from the files under the block's
//! root and watches it, sending `studio.changed`. The server never runs a
//! card: otto does, writing its status and takes through `canvas_edit`.

pub mod approval;
pub mod canvas;
pub mod disk;
pub mod frontmatter;
pub mod media;
pub mod production;
pub mod project;
pub mod watch;
pub mod world;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use silo_proto::{
    access::Rights,
    control::{Event, Method, RpcError},
    model::{AgentLink, BlockInfo, BlockKind, TermSize},
    studio::{StudioChange, StudioSetParams, StudioState, StudioView, StudioWorld},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    Server,
    access::{On, Principal, rights_on},
    block::Note,
    browser::Block,
    files::paths,
};

/// One `studio.changed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    pub what: StudioChange,
    pub name: Option<String>,
    pub rev: Option<u64>,
}

impl Changed {
    pub fn project() -> Self {
        Self { what: StudioChange::Project, name: None, rev: None }
    }

    pub fn world() -> Self {
        Self { what: StudioChange::World, name: None, rev: None }
    }

    pub fn canvas(name: &str, rev: Option<u64>) -> Self {
        Self { what: StudioChange::Canvas, name: Some(name.to_owned()), rev }
    }
}

pub struct StudioBlock {
    /// Block metadata; `studio` is `None` here and filled from `state`.
    base: BlockInfo,
    state: StudioState,
    watch: Option<watch::Watch>,
}

/// The view a fresh block on canonical `root` starts with: Setup without
/// `studio.toml`, else the first canvas in tab order (open and active), else
/// Production.
pub fn initial(root: &Path) -> StudioState {
    let mut state = StudioState::new(root.to_string_lossy().into_owned());
    state.view = StudioView::Setup;
    settle(&mut state, root.join(project::TOML).is_file(), project::canvas_order(root).into_iter().next());
    state
}

/// Follow `studio.toml` appearing (out of Setup: `first` canvas, else
/// Production) or going (Setup). True if the state changed.
fn settle(state: &mut StudioState, toml: bool, first: Option<String>) -> bool {
    match (toml, state.view) {
        (true, StudioView::Setup | StudioView::Unknown) => {}
        (false, StudioView::Setup) | (true, _) => return false,
        (false, _) => {
            state.view = StudioView::Setup;
            return true;
        }
    }
    match first {
        Some(first) if state.open.is_empty() => {
            state.view = StudioView::Canvas;
            state.open = vec![first.clone()];
            state.active = Some(first);
        }
        _ if !state.open.is_empty() => state.view = StudioView::Canvas,
        _ => state.view = StudioView::Production,
    }
    true
}

impl StudioBlock {
    /// A studio block showing `state`; it watches nothing until
    /// [`Self::watch`].
    pub fn new(mut base: BlockInfo, state: StudioState) -> Self {
        base.kind = BlockKind::Studio;
        (base.browser, base.agent, base.files, base.changes, base.studio) = (None, None, None, None, None);
        base.cwd = Some(state.root.clone());
        base.size = TermSize::default();
        Self { base, state, watch: None }
    }

    /// A studio block rebuilt from saved or transferred metadata.
    ///
    /// # Errors
    /// `info` carries no studio state.
    pub fn restore(mut info: BlockInfo) -> anyhow::Result<Self> {
        let state = *info.studio.take().ok_or_else(|| anyhow::anyhow!("studio block {} has no state", info.id))?;
        info.owner = None;
        info.owner_name = None;
        Ok(Self::new(info, state))
    }

    pub fn info(&self) -> BlockInfo {
        let root = Path::new(&self.state.root);
        let title = root.file_name().map_or_else(|| self.state.root.clone(), |n| n.to_string_lossy().into_owned());
        BlockInfo { title, studio: Some(Box::new(self.state.clone())), ..self.base.clone() }
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.state.root)
    }

    pub fn state(&self) -> &StudioState {
        &self.state
    }

    /// (Re)start watching the root, reporting to `notes`.
    pub fn watch(&mut self, notes: UnboundedSender<Note>) {
        self.watch = None;
        match watch::Watch::start(self.base.id.clone(), self.root(), notes) {
            Ok(w) => self.watch = Some(w),
            Err(error) => tracing::warn!(block = %self.base.id, "studio watch: {error}"),
        }
    }

    /// Link the agent it talks to (a block split off it).
    pub fn link(&mut self, agent: AgentLink) {
        self.state.agent = Some(agent);
    }

    /// `studio.toml` appeared or went; true if the view changed.
    pub fn settle(&mut self, toml: bool, first: Option<String>) -> bool {
        settle(&mut self.state, toml, first)
    }

    /// Canvas `name` is gone: close its tab. True if the state changed.
    pub fn forget(&mut self, name: &str) -> bool {
        let before = self.state.clone();
        self.state.open.retain(|n| n != name);
        if self.state.active.as_deref() == Some(name) {
            self.state.active = self.state.open.last().cloned();
        }
        before != self.state
    }

    /// Apply `studio.set`; true if the state changed.
    ///
    /// # Errors
    /// `bad_request` for a bad name, view or link, or too many tabs.
    pub fn set(&mut self, params: StudioSetParams) -> Result<bool, RpcError> {
        let mut state = self.state.clone();
        if let Some(view) = params.view {
            if view == StudioView::Unknown {
                return Err(RpcError::bad_request("view must be canvas, world, setup or production"));
            }
            state.view = view;
        }
        if let Some(open) = params.open {
            let mut out: Vec<String> = Vec::new();
            for name in open {
                let name = canvas::check_name(&name)?;
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            state.open = out;
        }
        if let Some(active) = params.active {
            state.active = match active {
                Some(name) => {
                    let name = canvas::check_name(&name)?;
                    if !state.open.contains(&name) {
                        state.open.push(name.clone());
                    }
                    Some(name)
                }
                None => None,
            };
        }
        if state.open.len() > StudioState::OPEN_MAX {
            return Err(RpcError::bad_request(format!("at most {} open canvases", StudioState::OPEN_MAX)));
        }
        if state.active.as_ref().is_some_and(|a| !state.open.contains(a)) {
            state.active = state.open.last().cloned();
        }
        if let Some(section) = params.world_section {
            state.world_section = section.map(|s| disk::component("section", &s)).transpose()?;
        }
        if let Some(entity) = params.world_entity {
            state.world_entity = match entity {
                Some(id) => {
                    let id = paths::normalize(id.trim_start_matches("world/"))?;
                    if !id.contains('/') {
                        return Err(RpcError::bad_request(format!("entity id {id} must be <section>/<name>")));
                    }
                    Some(id)
                }
                None => None,
            };
        }
        if let Some(agent) = params.agent {
            if let Some(a) = &agent
                && (a.otto_session.is_empty() || a.otto_session.len() > 256 || a.otto_session.chars().any(char::is_control))
            {
                return Err(RpcError::bad_request("agent.otto_session must be 1 to 256 printable bytes"));
            }
            state.agent = agent;
        }
        let changed = state != self.state;
        self.state = state;
        Ok(changed)
    }
}

/// The root of studio block `block_id`.
fn root_of(server: &Server, block_id: &str) -> Result<PathBuf, RpcError> {
    match server.registry.lock().unwrap().blocks.get(block_id) {
        Some(Block::Studio(s)) => Ok(s.root()),
        Some(_) => Err(RpcError::bad_request(format!("block {block_id} is not a studio block"))),
        None => Err(RpcError::not_found(format!("block {block_id}"))),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, RpcError> + Send + 'static) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(f).await.map_err(RpcError::internal)?
}

/// A reply that fits in a frame.
fn reply(value: &impl serde::Serialize) -> Result<Value, RpcError> {
    let value = serde_json::to_value(value).map_err(RpcError::internal)?;
    if value.to_string().len() > silo_proto::MAX_FRAME - 64 * 1024 {
        return Err(RpcError::new("too_large", "the reply would not fit in a frame"));
    }
    Ok(value)
}

/// `studio.changed` for block `block_id` after the server's own write (its
/// watcher, and other blocks' on the root, follow).
fn emit(server: &Server, block_id: &str, changes: &[Changed]) {
    let mut reg = server.registry.lock().unwrap();
    for c in changes {
        reg.emit(Event::StudioChanged { block_id: block_id.to_owned(), what: c.what, name: c.name.clone(), rev: c.rev });
    }
}

/// The `studio.*` methods. `principal`: the caller, for `studio.import`
/// from outside the root (the owner only).
///
/// # Errors
/// Per method: path errors, `not_found` for what isn't there, `bad_request`.
pub async fn handle(server: &Server, principal: &Principal, method: Method) -> Result<Value, RpcError> {
    match method {
        Method::StudioProject(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || project::project(&root)).await?)
        }
        Method::StudioInit(p) => {
            let root = root_of(server, &p.block_id)?;
            let (project, first) = blocking(move || {
                project::init(&root, &p.name, p.format.as_deref())?;
                let project = project::project(&root)?;
                let first = project.canvases.first().map(|c| c.name.clone());
                Ok((project, first))
            })
            .await?;
            server.registry.lock().unwrap().studio_settle(&p.block_id, true, first);
            emit(server, &p.block_id, &[Changed::project(), Changed::world()]);
            reply(&project)
        }
        Method::StudioSetFormat(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let (project, rev) = blocking(move || {
                let rev = match (&p.canvas, &p.format) {
                    (Some(c), format) => Some((c.clone(), canvas::update(&root, c, None, None, Some(format.as_deref()))?.rev)),
                    (None, Some(format)) => {
                        project::set_default(&root, format)?;
                        None
                    }
                    (None, None) => return Err(RpcError::bad_request("the project default can't be cleared: give a format")),
                };
                Ok((project::project(&root)?, rev))
            })
            .await?;
            let mut changes = vec![Changed::project()];
            if let Some((name, rev)) = rev {
                changes.push(Changed::canvas(&name, Some(rev)));
            }
            emit(server, &block_id, &changes);
            reply(&project)
        }
        Method::StudioSetFormats(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let project = blocking(move || {
                project::set_formats(&root, &p.formats)?;
                project::project(&root)
            })
            .await?;
            emit(server, &block_id, &[Changed::project()]);
            reply(&project)
        }
        Method::StudioCanvas(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || canvas::read(&root, &p.name)).await?)
        }
        Method::StudioCanvasCreate(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let info = blocking(move || canvas::create(&root, &p.title, p.episode, p.format.as_deref(), p.name.as_deref())).await?;
            emit(server, &block_id, &[Changed::project(), Changed::canvas(&info.name, Some(info.rev))]);
            reply(&info)
        }
        Method::StudioCanvasUpdate(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let info = blocking(move || {
                let format = p.format.as_ref().map(|f| f.as_deref());
                canvas::update(&root, &p.name, p.title.as_deref(), p.episode, format)
            })
            .await?;
            emit(server, &block_id, &[Changed::project(), Changed::canvas(&info.name, Some(info.rev))]);
            reply(&info)
        }
        Method::StudioCanvasDelete(p) => {
            let root = root_of(server, &p.block_id)?;
            let trash = server.trash.clone();
            let name = p.name.clone();
            blocking(move || canvas::delete(&root, &name, trash.as_deref())).await?;
            server.registry.lock().unwrap().studio_forget(&p.block_id, &p.name);
            emit(server, &p.block_id, &[Changed::project()]);
            Ok(json!({}))
        }
        Method::StudioCanvasEdit(p) => {
            let root = root_of(server, &p.block_id)?;
            let (block_id, name, ops) = (p.block_id.clone(), p.name.clone(), p.ops.len());
            let edited = blocking(move || canvas::edit(&root, &p.name, p.ops, p.base_rev)).await?;
            if edited.failed.len() < ops {
                emit(server, &block_id, &[Changed::canvas(&name, Some(edited.rev))]);
            }
            reply(&edited)
        }
        Method::StudioWorld(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || Ok(StudioWorld { sections: world::sections(&root) })).await?)
        }
        Method::StudioEntity(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || world::entity(&root, &p.id)).await?)
        }
        Method::StudioEntityCreate(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let info = blocking(move || world::create(&root, &p.section, &p.name, p.fields.as_ref(), p.body.as_deref())).await?;
            emit(server, &block_id, &[Changed::world()]);
            reply(&info)
        }
        Method::StudioEntityUpdate(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let info = blocking(move || world::update(&root, &p.id, p.fields.as_ref(), p.body.as_deref())).await?;
            emit(server, &block_id, &[Changed::world()]);
            reply(&info)
        }
        Method::StudioEntityApprove(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
            let info = blocking(move || world::approve(&root, &p.id, p.approved, now)).await?;
            emit(server, &block_id, &[Changed::world()]);
            reply(&info)
        }
        Method::StudioEntityRef(p) => {
            let root = root_of(server, &p.block_id)?;
            let owner = rights_on(principal, On::Server) == Some(Rights::Owner);
            let block_id = p.block_id.clone();
            let edited = blocking(move || world::edit_ref(&root, &p.id, &p.change, owner)).await?;
            emit(server, &block_id, &[Changed::world()]);
            reply(&edited)
        }
        Method::StudioEntityLink(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let info = blocking(move || world::link(&root, &p.id, &p.other, p.linked)).await?;
            emit(server, &block_id, &[Changed::world()]);
            reply(&info)
        }
        Method::StudioProduction(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || production::production(&root)).await?)
        }
        Method::StudioSetModels(p) => {
            let root = root_of(server, &p.block_id)?;
            let block_id = p.block_id.clone();
            let project = blocking(move || {
                project::set_models(&root, &p.models)?;
                project::project(&root)
            })
            .await?;
            emit(server, &block_id, &[Changed::project()]);
            reply(&project)
        }
        Method::StudioAsset(p) => {
            let root = root_of(server, &p.block_id)?;
            reply(&blocking(move || media::asset(&root, &p.path, p.offset, p.length)).await?)
        }
        Method::StudioImport(p) => {
            let root = root_of(server, &p.block_id)?;
            let owner = rights_on(principal, On::Server) == Some(Rights::Owner);
            let (block_id, name) = (p.block_id.clone(), p.canvas.clone());
            let imported = blocking(move || media::import(&root, &p.canvas, &p.from, owner)).await?;
            emit(server, &block_id, &[Changed::canvas(&name, None)]);
            reply(&imported)
        }
        Method::StudioSet(p) => reply(&server.registry.lock().unwrap().studio_set(p)?),
        other => Err(RpcError::method_not_found(&silo_proto::api::method_name(&other))),
    }
}
