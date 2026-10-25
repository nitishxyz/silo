use crate::{Error, Result, session::SessionHandle};
use otto_llm::{Model, ModelInfo};
use otto_proto::{ApprovalDecision, Delivery, Event, InputPart, ProjectInfo, SessionInfo, SessionStatus, SessionView, UsageTotals};
use otto_store::{EventRecord, NewSession, SessionUpdate, Store};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

/// Provider registry supplied by the embedding application.
pub trait ModelResolver: Send + Sync {
    fn resolve(&self, provider: &str, model: &str) -> Result<Arc<dyn Model>>;
    fn default_model(&self) -> Option<(String, String)>;
    fn list(&self) -> Vec<ModelInfo>;
    /// Refresh the model lists signed-in providers report, where due; true when one changed.
    fn refresh_models(&self) -> futures::future::BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
}
/// How often [`ModelResolver::refresh_models`] is asked whether a list is due.
pub const MODEL_REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
pub struct RuntimeConfig {
    pub store: Arc<Mutex<Store>>,
    pub models: Arc<dyn ModelResolver>,
    pub state_dir: PathBuf,
    pub user_config_dir: PathBuf,
}
/// Reloadable defaults, applied only to sessions created after set_config.
#[derive(Clone, Debug)]
pub struct SessionDefaults {
    pub agent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// None selects Auto.
    pub approval_mode: Option<otto_tools::policy::ApprovalMode>,
    /// Let new sessions use the provider's native compaction when their model
    /// has it and the history is its own; otto's summarizer otherwise.
    pub native_compaction: bool,
    /// The results page `web_search` reads (`search_engine` in config).
    pub search_engine: otto_tools::builtin::web_search::Engine,
    /// Reasoning effort for sessions whose agent sets none.
    pub effort: Option<otto_llm::Effort>,
    /// Snapshot a git work tree at each turn's start and end for review (`turn_snapshots` in config, on by
    /// default; read at each turn, so a change applies to every session's next turn).
    pub turn_snapshots: bool,
}
impl Default for SessionDefaults {
    fn default() -> Self {
        Self {
            agent: None,
            provider: None,
            model: None,
            approval_mode: None,
            native_compaction: false,
            search_engine: Default::default(),
            effort: None,
            turn_snapshots: true,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct CreateSession {
    pub project_id: String,
    pub cwd: PathBuf,
    pub parent_id: Option<String>,
    pub agent: String,
    pub approval_mode: Option<otto_proto::ApprovalMode>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub title: String,
    /// Zero selects the default limit of 200.
    pub max_steps: usize,
    /// A delegated child's task, kept for `subagent` status and retry.
    pub task: Option<String>,
    /// Reasoning effort; `None` keeps the agent's, else the model's default.
    pub effort: Option<otto_llm::Effort>,
    /// A delegated child's tools: a subset of its agent's grant (O5 per-subagent
    /// grants); a name outside it is an error listing the available ones.
    pub tools: Option<Vec<String>>,
}
/// `session.update`: given fields only, from the session's next model request.
#[derive(Clone, Debug, Default)]
pub struct Configure {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub effort: Option<otto_llm::Effort>,
    /// Back to the agent's effort, else the model's default.
    pub reset_effort: bool,
}
/// Each provider's last plan usage, in the state folder.
const PLANS: &str = "plan_usage.json";
/// A committed event. Live and replay streams use identical sequence numbers.
pub type SequencedEvent = (i64, Event);
/// Receiver that suppresses events already included in replay.
pub struct LiveEvents {
    receiver: broadcast::Receiver<SequencedEvent>,
    after: i64,
}
impl LiveEvents {
    pub async fn recv(&mut self) -> std::result::Result<SequencedEvent, broadcast::error::RecvError> {
        loop {
            let event = self.receiver.recv().await?;
            if event.0 > self.after {
                self.after = event.0;
                return Ok(event);
            }
        }
    }
}
#[derive(Clone)]
pub struct Runtime(pub(crate) Arc<Inner>);
pub(crate) struct Inner {
    pub config: RuntimeConfig,
    pub actors: tokio::sync::Mutex<HashMap<String, SessionHandle>>,
    pub global: broadcast::Sender<Event>,
    index: Mutex<(i64, broadcast::Sender<SequencedEvent>)>,
    defaults: Mutex<SessionDefaults>,
    /// Tools beyond the built-ins (silo), offered to every session.
    extra: Mutex<Vec<Arc<dyn otto_tools::Tool>>>,
    /// Project-dependent tools (MCP), listed again before each model request.
    sources: Mutex<Vec<Arc<dyn otto_tools::ProjectSources>>>,
    /// What every session's shells share: background jobs and the terminal
    /// host (silo) commands run in.
    shell: Mutex<otto_tools::builtin::shell::Host>,
    /// Loads pages for `web_fetch` and `web_search` (silo's off-screen browser).
    renderer: Mutex<Option<Arc<dyn otto_tools::builtin::render::Renderer>>>,
    /// Set by shutdown: no actor starts afterwards.
    closing: std::sync::atomic::AtomicBool,
    /// Name untitled sessions after their first turn (`set_auto_title`).
    auto_title: std::sync::atomic::AtomicBool,
    /// Each provider's subscription limits as last reported (`<state>/plan_usage.json`).
    plans: Mutex<std::collections::BTreeMap<String, otto_llm::PlanUsage>>,
}
impl Runtime {
    /// Recover interrupted tool calls before accepting any new work.
    pub async fn new(config: RuntimeConfig) -> Result<Self> {
        let (global, _) = broadcast::channel(256);
        let (index, _) = broadcast::channel(256);
        let plans = std::fs::read(config.state_dir.join(PLANS)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let runtime = Self(Arc::new(Inner {
            config,
            actors: tokio::sync::Mutex::new(HashMap::new()),
            global,
            index: Mutex::new((0, index)),
            defaults: Mutex::new(SessionDefaults::default()),
            extra: Mutex::new(Vec::new()),
            sources: Mutex::new(Vec::new()),
            shell: Mutex::new(otto_tools::builtin::shell::Host::default()),
            renderer: Mutex::new(None),
            closing: std::sync::atomic::AtomicBool::new(false),
            auto_title: std::sync::atomic::AtomicBool::new(false),
            plans: Mutex::new(plans),
        }));
        runtime.db(crate::recovery::recover).await?;
        let state = runtime.0.config.state_dir.clone();
        runtime
            .db(move |s| {
                let now =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64;
                for session in s.all_sessions()? {
                    crate::spills::expire(&state, &session.id, session.archived, session.updated_at, now)?;
                }
                Ok(())
            })
            .await?;
        runtime.spawn_model_refresh();
        Ok(runtime)
    }
    /// Hourly (first right away), refresh due model lists and announce a change (`auth.changed`,
    /// which makes clients reread their model pickers). Ends with the runtime.
    fn spawn_model_refresh(&self) {
        let inner = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(MODEL_REFRESH_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(runtime) = inner.upgrade().map(Runtime) else { return };
                if runtime.0.closing.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let models = runtime.0.config.models.clone();
                drop(runtime);
                if models.refresh_models().await
                    && let Some(runtime) = inner.upgrade().map(Runtime)
                {
                    runtime.broadcast_global(Event::AuthChanged {});
                }
            }
        });
    }
    fn broadcast_global(&self, event: Event) {
        let mut index = self.0.index.lock().expect("index mutex");
        index.0 += 1;
        let _ = index.1.send((index.0, event.clone()));
        let _ = self.0.global.send(event);
    }
    pub(crate) async fn db<T: Send + 'static>(&self, f: impl FnOnce(&mut Store) -> Result<T> + Send + 'static) -> Result<T> {
        let store = self.0.config.store.clone();
        tokio::task::spawn_blocking(move || f(&mut *store.lock().map_err(|_| Error::Invalid("store mutex poisoned".into()))?))
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
    }
    pub async fn open_project(&self, root: impl AsRef<Path>) -> Result<ProjectInfo> {
        let root = std::fs::canonicalize(root)?.to_string_lossy().into_owned();
        self.db(move |s| {
            let p = s.create_project(&root)?;
            Ok(ProjectInfo { id: p.id, root: p.root, updated_at: p.created_at, created_at: p.created_at })
        })
        .await
    }
    /// Name sessions whose title is still empty after their first finished
    /// turn, with one short call to a small model of the session's provider.
    pub fn set_auto_title(&self, on: bool) {
        self.0.auto_title.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub(crate) fn auto_title(&self) -> bool {
        self.0.auto_title.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub(crate) fn defaults(&self) -> SessionDefaults {
        self.0.defaults.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
    /// Replace defaults atomically; existing sessions keep their configuration.
    pub fn set_config(&self, config: SessionDefaults) -> Result<()> {
        if let Some(agent) = &config.agent
            && agent.trim().is_empty()
        {
            return Err(Error::Invalid("empty default agent".into()));
        }
        *self.0.defaults.lock().map_err(|_| Error::Invalid("config mutex poisoned".into()))? = config;
        Ok(())
    }
    /// Offer `tools` to sessions created from now on (and to existing
    /// sessions whose agent lists them); a name already present is replaced.
    pub fn add_tools(&self, tools: Vec<Arc<dyn otto_tools::Tool>>) {
        let mut extra = self.0.extra.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for tool in tools {
            extra.retain(|t| t.spec().name != tool.spec().name);
            extra.push(tool);
        }
    }
    pub(crate) fn extra_tools(&self) -> Vec<Arc<dyn otto_tools::Tool>> {
        self.0.extra.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
    /// Run `shell` commands in `terminals` (silo's parked command blocks)
    /// when it serves the call; pipes otherwise.
    pub fn set_terminals(&self, terminals: Arc<dyn otto_tools::builtin::shell::Terminals>) {
        self.0.shell.lock().unwrap_or_else(std::sync::PoisonError::into_inner).terminals = Some(terminals);
    }
    /// Render pages for `web_fetch` and `web_search` in `renderer` (silo's
    /// off-screen browser); plain HTTP otherwise.
    pub fn set_renderer(&self, renderer: Arc<dyn otto_tools::builtin::render::Renderer>) {
        *self.0.renderer.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(renderer);
    }
    /// The built-ins for `family`, sharing this runtime's jobs, terminals and renderer.
    pub(crate) fn builtins(&self, family: &str) -> Vec<Arc<dyn otto_tools::Tool>> {
        let web = otto_tools::builtin::web_fetch::Web {
            renderer: self.0.renderer.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
            engine: self.defaults().search_engine,
        };
        otto_tools::registry::tools_with(family, &self.0.shell.lock().unwrap_or_else(std::sync::PoisonError::into_inner), &web)
    }
    /// Add project-dependent tools (MCP servers). Their tools are granted by
    /// pattern (`mcp__*` by default) and listed again before every request,
    /// so list changes reach the next step. Stopped by `shutdown`.
    pub fn add_source(&self, source: Arc<dyn otto_tools::ProjectSources>) {
        self.0.sources.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(source);
    }
    pub(crate) async fn source_tools(&self, root: &Path) -> Vec<Arc<dyn otto_tools::Tool>> {
        let sources = self.0.sources.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let mut tools = Vec::new();
        for source in sources {
            tools.extend(source.tools(root).await);
        }
        tools
    }
    pub fn skills(&self, project_root: Option<&Path>) -> Vec<otto_proto::SkillInfo> {
        crate::skills::discover(project_root.unwrap_or(&self.0.config.user_config_dir.join("__no_project__")), &self.0.config.user_config_dir)
            .into_iter()
            .map(|s| otto_proto::SkillInfo { name: s.name, description: s.description })
            .collect()
    }
    pub fn commands(&self, project_root: Option<&Path>) -> Vec<otto_proto::CommandInfo> {
        crate::commands::discover(project_root.unwrap_or(&self.0.config.user_config_dir.join("__no_project__")), &self.0.config.user_config_dir)
            .into_iter()
            .map(|c| otto_proto::CommandInfo { name: c.name, description: c.description, argument_hint: c.argument_hint })
            .collect()
    }
    pub async fn list_projects(&self) -> Result<Vec<ProjectInfo>> {
        self.db(|s| {
            Ok(s.recent_projects()?
                .into_iter()
                .map(|(p, updated_at)| ProjectInfo { id: p.id, root: p.root, created_at: p.created_at, updated_at })
                .collect())
        })
        .await
    }
    /// Projects most recently used first, without those whose root is gone.
    pub async fn recent_projects(&self) -> Result<Vec<ProjectInfo>> {
        Ok(self.list_projects().await?.into_iter().filter(|p| Path::new(&p.root).is_dir()).collect())
    }
    /// Nonblocking conservative idle-exit check, including queued commands.
    pub fn busy(&self) -> bool {
        self.0.actors.try_lock().map_or(true, |actors| {
            actors.values().any(|h| !h.tx.is_closed() && (h.tx.capacity() < h.tx.max_capacity() || h.busy.try_lock().map_or(true, |busy| *busy)))
        })
    }
    pub fn agents(&self, project_root: Option<&Path>) -> Vec<otto_proto::AgentInfo> {
        crate::agents::discover(project_root.unwrap_or(&self.0.config.user_config_dir.join("__no_project__")), &self.0.config.user_config_dir)
            .into_iter()
            .map(|a| {
                let (provider, model) = a.model.map_or((None, None), |m| match m.split_once('/') {
                    Some((p, m)) => (Some(p.into()), Some(m.into())),
                    None => (None, Some(m)),
                });
                otto_proto::AgentInfo { name: a.name, description: a.description, model, provider, scope: a.scope }
            })
            .collect()
    }
    /// Paths under `cwd` for an `@` mention (`files.complete`), best match first.
    pub async fn complete_files(&self, cwd: String, query: String, limit: usize) -> Result<Vec<otto_proto::FileMatch>> {
        if !Path::new(&cwd).is_dir() {
            return Err(Error::Invalid("cwd is not a folder".into()));
        }
        let hits = tokio::task::spawn_blocking(move || otto_tools::complete::complete(Path::new(&cwd), &query, limit.min(200)))
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(hits.into_iter().map(|(path, dir)| otto_proto::FileMatch { path, dir }).collect())
    }
    /// `name` as discovery resolves it for `project_root` (`agents.get`).
    pub fn agent(&self, name: &str, project_root: Option<&Path>) -> Result<otto_proto::AgentDef> {
        crate::agents::discover(project_root.unwrap_or(&self.0.config.user_config_dir.join("__no_project__")), &self.0.config.user_config_dir)
            .into_iter()
            .find(|a| a.name == name)
            .map(crate::agents::Agent::def)
            .ok_or_else(|| otto_store::Error::NotFound(format!("agent {name}")).into())
    }
    /// Write a user or project definition and read it back (`agents.save`); a
    /// different `previous_name` removes that scope's old definition.
    pub fn save_agent(&self, agent: &otto_proto::AgentDef, previous_name: Option<&str>, project_root: Option<&Path>) -> Result<otto_proto::AgentDef> {
        crate::agents::save(project_root, &self.0.config.user_config_dir, agent, previous_name)?;
        self.agent(&agent.name, project_root)
    }
    /// Remove a user or project definition (`agents.delete`).
    pub fn delete_agent(&self, name: &str, scope: otto_proto::AgentScope, project_root: Option<&Path>) -> Result<()> {
        crate::agents::delete(project_root, &self.0.config.user_config_dir, name, scope)
    }
    /// Every tool an agent can list (`tools.catalog`): the built-ins for the
    /// default model's family, `skill` and `subagent`, extras, and `project_root`'s sources.
    pub async fn tool_catalog(&self, project_root: Option<&Path>) -> Vec<otto_proto::ToolCatalogEntry> {
        let config = self.defaults();
        let mut defaults = self.0.config.models.default_model().unwrap_or_default();
        defaults.0 = config.provider.unwrap_or(defaults.0);
        defaults.1 = config.model.unwrap_or(defaults.1);
        let family = self.0.config.models.resolve(&defaults.0, &defaults.1).map_or_else(|_| "generic".into(), |m| crate::session::family(m.as_ref()));
        let mut tools = self.builtins(&family);
        tools.extend(self.extra_tools());
        if let Some(root) = project_root {
            tools.extend(self.source_tools(root).await);
        }
        let mut specs: Vec<otto_tools::ToolSpec> = tools.iter().map(|t| t.spec().clone()).collect();
        specs.extend(crate::tools::session_specs());
        specs.iter().map(crate::tools::catalog_entry).collect()
    }
    pub async fn tools(&self, session_id: &str) -> Result<Vec<otto_proto::ToolInfo>> {
        let handle = self.session(session_id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.tx.send(crate::session::Command::Tools(tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
    pub async fn create_session(&self, mut options: CreateSession) -> Result<SessionHandle> {
        let config = self.0.defaults.lock().map_err(|_| Error::Invalid("config mutex poisoned".into()))?.clone();
        if options.agent.is_empty() {
            options.agent = config.agent.clone().unwrap_or_else(|| "build".into());
        }
        let project = options.project_id.clone();
        let root = self.db(move |s| Ok(s.project(&project)?.ok_or(Error::Closed)?.root)).await?;
        let agent_name = if options.agent.is_empty() { "build" } else { &options.agent };
        let agent = crate::agents::discover(Path::new(&root), &self.0.config.user_config_dir)
            .into_iter()
            .find(|a| a.name == agent_name)
            .ok_or_else(|| Error::Invalid(format!("unknown agent {agent_name}")))?;
        let mut defaults = self.0.config.models.default_model().unwrap_or_default();
        if let Some(provider) = config.provider {
            defaults.0 = provider;
        }
        if let Some(model) = config.model {
            defaults.1 = model;
        }
        let agent_model = agent.model.as_deref().map(|m| m.split_once('/').map_or((None, m), |(p, m)| (Some(p), m)));
        let provider = options.provider.unwrap_or_else(|| agent_model.and_then(|m| m.0).unwrap_or(&defaults.0).to_owned());
        let model = options.model.unwrap_or_else(|| agent_model.map(|m| m.1).unwrap_or(&defaults.1).to_owned());
        let resolved = self.0.config.models.resolve(&provider, &model)?;
        let family = crate::session::family(resolved.as_ref());
        let window = resolved.info().caps.context_window;
        if let Some(names) = &options.tools {
            let (grant, _, denied) = self.grants(&agent, &family, options.parent_id.clone(), None).await?;
            let granted = |n: &String| grant.iter().any(|p| crate::tools::pattern(p, n)) && !denied.iter().any(|d| crate::tools::pattern(d, n));
            let mut known: Vec<String> = self
                .builtins(&family)
                .into_iter()
                .chain(self.extra_tools())
                .chain(self.source_tools(Path::new(&root)).await)
                .map(|t| t.spec().name.clone())
                .chain(crate::tools::session_specs().into_iter().map(|s| s.name))
                .filter(granted)
                .collect();
            known.sort_unstable();
            known.dedup();
            let unknown: Vec<&str> = names.iter().filter(|n| !known.contains(n)).map(String::as_str).collect();
            if !unknown.is_empty() {
                return Err(Error::Invalid(format!(
                    "tools not available to agent {agent_name}: {}; available: {}",
                    unknown.join(", "),
                    known.join(", ")
                )));
            }
        }
        let (grant, loadable, denied) = self.grants(&agent, &family, options.parent_id.clone(), options.tools.as_deref()).await?;
        let effort = options
            .effort
            .map(serde_json::to_value)
            .transpose()?
            .or_else(|| agent.effort.clone().map(serde_json::Value::String))
            .or_else(|| config.effort.map(|e| serde_json::to_value(e).expect("effort enum")));
        let approval_mode = options.approval_mode.or(agent.approval_mode).map(|m| serde_json::to_value(m).expect("mode enum"));
        let task = options.task;
        let row = self
            .db(move |s| {
                Ok(s.create_session(NewSession {
                    project_id: options.project_id,
                    parent_id: options.parent_id,
                    agent: if options.agent.is_empty() { "build".into() } else { options.agent },
                    provider,
                    model,
                    title: options.title,
                    cwd: options.cwd.to_string_lossy().into_owned(),
                    meta: serde_json::json!({"max_steps":options.max_steps,"tools":grant,"loadable":loadable,"denied_tools":denied,"task":task,"grant":options.tools,"effort":effort,"native_compaction":config.native_compaction,"approval_mode":approval_mode.or_else(||config.approval_mode.map(|m|serde_json::to_value(m).expect("mode enum"))),"context":{"used":0,"window":window}}),
                })?)
            })
            .await?;
        let handle = self.session(&row.id).await?;
        self.changed();
        Ok(handle)
    }
    /// The tools `agent` is granted on a model of `family`, narrowed by the
    /// parent's grant: the whole grant, the part behind `load_tools` (`None`:
    /// each tool's own default), and the denied patterns. `narrow` (a delegated
    /// child's own `tools`) keeps just those names of the grant.
    pub(crate) async fn grants(
        &self,
        agent: &crate::agents::Agent,
        family: &str,
        parent: Option<String>,
        narrow: Option<&[String]>,
    ) -> Result<(Vec<String>, Option<Vec<String>>, Vec<String>)> {
        let listed = !agent.tools.is_empty() || !agent.loadable.is_empty();
        let mut grant: Vec<String> = if !listed {
            otto_tools::registry::tools_for_family(family)
                .iter()
                .map(|t| t.spec().name.clone())
                .chain(["skill".into(), "subagent".into(), "mcp__*".into()])
                .chain(self.extra_tools().iter().map(|t| t.spec().name.clone()))
                .collect()
        } else {
            agent.tools.iter().chain(&agent.loadable).cloned().collect()
        };
        let mut loadable = listed.then(|| agent.loadable.clone());
        let mut denied = agent.disallowed.clone();
        if let Some(parent) = parent {
            let parent = self.db(move |s| Ok(s.session(&parent)?.ok_or(Error::Closed)?.meta)).await?;
            if let Some(allowed) = parent["tools"].as_array() {
                let allowed = |tool: &String| allowed.iter().filter_map(|v| v.as_str()).any(|p| crate::tools::pattern(p, tool));
                grant.retain(allowed);
                loadable.iter_mut().for_each(|l| l.retain(allowed));
            }
            denied.extend(parent["denied_tools"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)));
        }
        let kept = |tool: &String| !denied.iter().any(|d| crate::tools::pattern(d, tool));
        grant.retain(kept);
        loadable.iter_mut().for_each(|l| l.retain(kept));
        if let Some(narrow) = narrow {
            let within =
                |patterns: &[String]| narrow.iter().filter(|n| patterns.iter().any(|p| crate::tools::pattern(p, n))).cloned().collect::<Vec<_>>();
            grant = within(&grant);
            loadable = loadable.map(|l| within(&l));
        }
        if (loadable.as_ref().is_some_and(|l| !l.is_empty())
            || otto_tools::registry::tools_for_family(family)
                .into_iter()
                .chain(self.extra_tools())
                .any(|tool| tool.spec().loading == otto_tools::Loading::Deferred && grant.contains(&tool.spec().name)))
            && !grant.iter().any(|name| name == "load_tools")
        {
            grant.push("load_tools".into());
        }
        Ok((grant, loadable, denied))
    }
    pub async fn session(&self, id: &str) -> Result<SessionHandle> {
        let mut actors = self.0.actors.lock().await;
        if let Some(handle) = actors.get(id) {
            return Ok(handle.clone());
        }
        if self.0.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        let owned = id.to_owned();
        let models = self.0.config.models.clone();
        let row = self
            .db(move |s| {
                let row = s.session(&owned)?.ok_or_else(|| Error::Invalid(format!("unknown session {owned}")))?;
                with_context(s, models.as_ref(), row)
            })
            .await?;
        let handle = crate::session::spawn(self.clone(), row).await?;
        actors.insert(id.into(), handle.clone());
        Ok(handle)
    }
    pub async fn list_sessions(&self, project: &str, include_archived: bool) -> Result<Vec<SessionInfo>> {
        let project = project.to_owned();
        let models = self.0.config.models.clone();
        self.db(move |s| {
            s.list_sessions(&project, include_archived)?
                .into_iter()
                .map(|row| {
                    let row = with_context(s, models.as_ref(), row)?;
                    info(s, row)
                })
                .collect()
        })
        .await
    }
    /// Every item, unclipped: otto's own use.
    pub async fn view(&self, id: &str) -> Result<SessionView> {
        let mut view = self.session(id).await?.view().await?;
        view.info = self.with_plan(view.info);
        Ok(view)
    }
    /// What a client is sent (`session.view`): the items `window` selects, clipped.
    pub async fn view_window(&self, id: &str, window: ViewWindow) -> Result<SessionView> {
        let mut view = self.session(id).await?.view_window(window).await?;
        view.info = self.with_plan(view.info);
        Ok(view)
    }
    /// Subscription limits per provider as last reported (`plan.usage`): `provider`'s, or all.
    pub fn plan_usage(&self, provider: Option<&str>) -> Vec<otto_proto::ProviderPlan> {
        let plans = self.0.plans.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        plans
            .iter()
            .filter(|(p, _)| provider.is_none_or(|want| want == p.as_str()))
            .map(|(provider, usage)| otto_proto::ProviderPlan { provider: provider.clone(), usage: usage.clone() })
            .collect()
    }
    /// `info` with its provider's plan usage (`SessionInfo.plan`).
    #[must_use]
    pub fn with_plan(&self, mut info: SessionInfo) -> SessionInfo {
        info.plan = self.0.plans.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&info.provider).cloned();
        info
    }
    /// A response reported `provider`'s limits: kept (merged by window),
    /// saved, and announced (`plan.usage`).
    pub(crate) fn record_plan(&self, provider: &str, usage: otto_llm::PlanUsage) {
        let (merged, bytes) = {
            let mut plans = self.0.plans.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = plans.entry(provider.to_owned()).or_default();
            entry.merge(usage);
            (entry.clone(), serde_json::to_vec_pretty(&*plans))
        };
        let dir = &self.0.config.state_dir;
        let temporary = dir.join(format!("{PLANS}.tmp"));
        let saved = bytes.map_err(std::io::Error::other).and_then(|bytes| {
            std::fs::create_dir_all(dir)?;
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(&temporary, dir.join(PLANS))
        });
        if let Err(error) = saved {
            tracing::warn!(%error, "saving plan usage");
        }
        self.broadcast_global(Event::PlanUsage { provider: provider.to_owned(), usage: merged });
    }
    /// Subscribe first, read the journal second, then discard overlapping live sequences.
    pub async fn subscribe(&self, id: &str, after_seq: i64) -> Result<(Vec<SequencedEvent>, LiveEvents)> {
        let handle = self.session(id).await?;
        let receiver = handle.events.subscribe();
        let id = id.to_owned();
        let replay = self.db(move |s| Ok(s.events_since(&id, after_seq, usize::MAX)?.into_iter().map(decode).collect::<Vec<_>>())).await?;
        let after = replay.last().map_or(after_seq, |e| e.0);
        Ok((replay, LiveEvents { receiver, after }))
    }
    /// Cancel and join all actors. Stop submitting commands before calling this.
    /// Call before dropping the last runtime to release actor-held store references.
    pub async fn shutdown(&self) -> Result<()> {
        self.0.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        let actors = std::mem::take(&mut *self.0.actors.lock().await);
        let mut replies = Vec::new();
        for handle in actors.into_values() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if handle.tx.send(crate::session::Command::Shutdown(tx)).await.is_ok() {
                replies.push(rx);
            }
        }
        for reply in replies {
            reply.await.map_err(|_| Error::Closed)?;
        }
        let sources = std::mem::take(&mut *self.0.sources.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for source in sources {
            source.shutdown().await;
        }
        Ok(())
    }
    /// Nudge a session that has undelivered outbox entries, opening its actor
    /// if needed. Entries are durable; a failed wake only delays delivery.
    /// Boxed with a named type: it is reached from inside session actors, whose
    /// future types would otherwise be defined in terms of themselves.
    pub(crate) fn wake(&self, id: String) -> futures::future::BoxFuture<'static, Result<()>> {
        let runtime = self.clone();
        Box::pin(async move {
            let handle = runtime.session(&id).await?;
            handle.tx.send(crate::session::Command::Wake).await.map_err(|_| Error::Closed)
        })
    }
    /// Wake every session whose children finished while it was not running
    /// (including before a restart). Call once after registering tools and
    /// sources; returns how many sessions were woken.
    pub async fn deliver_pending(&self) -> Result<usize> {
        let ids = self.db(|s| Ok(s.outbox_pending_sessions()?)).await?;
        for id in &ids {
            self.wake(id.clone()).await?;
        }
        Ok(ids.len())
    }
    /// Withdraw a pending input; see `SessionHandle::queue_remove`.
    pub async fn queue_remove(&self, id: &str, input_id: &str) -> Result<()> {
        self.session(id).await?.queue_remove(input_id).await
    }
    /// Change a pending input's delivery; see `SessionHandle::queue_update`.
    pub async fn queue_update(&self, id: &str, input_id: &str, delivery: Delivery) -> Result<()> {
        self.session(id).await?.queue_update(input_id, delivery).await
    }
    /// Sequenced global notifications; the sequence is scoped to this runtime lifetime.
    pub fn index_events(&self) -> broadcast::Receiver<SequencedEvent> {
        self.0.index.lock().expect("index mutex").1.subscribe()
    }
    pub(crate) fn changed(&self) {
        let mut index = self.0.index.lock().expect("index mutex");
        index.0 += 1;
        let _ = index.1.send((index.0, Event::SessionsChanged {}));
        let _ = self.0.global.send(Event::SessionsChanged {});
    }
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.0.global.subscribe()
    }
    /// User input; to a subagent it counts as steering or taking it over (§7.4).
    pub async fn prompt(&self, id: &str, input: Vec<InputPart>, delivery: Delivery) -> Result<String> {
        let dir = otto_tools::attachments::dir(&self.attachments_root(), id);
        for part in &input {
            if let InputPart::Attachment { id } = part {
                otto_tools::attachments::get(&dir, id).map_err(|_| Error::Invalid(format!("unknown attachment {id}")))?;
            }
        }
        self.session(id).await?.prompt(input, delivery).await
    }
    /// `<state>/attachments`: a folder per session.
    pub fn attachments_root(&self) -> PathBuf {
        self.0.config.state_dir.join("attachments")
    }
    /// Store a file for the session's prompts (`attachment.put`), or append a
    /// chunk to attachment `append_to`; at most `otto_proto::MAX_ATTACHMENT` bytes.
    pub async fn attachment_put(
        &self,
        id: &str,
        name: &str,
        mime: &str,
        bytes: Vec<u8>,
        append_to: Option<&str>,
    ) -> Result<otto_proto::AttachmentInfo> {
        let max = otto_proto::MAX_ATTACHMENT as u64;
        if bytes.len() as u64 > max {
            return Err(Error::Invalid(format!("attachment exceeds {}", otto_tools::attachments::size(max))));
        }
        if self.session(id).await?.view().await?.info.status == otto_proto::SessionStatus::Archived {
            return Err(Error::Invalid("Unarchive the session before attaching files".into()));
        }
        let dir = otto_tools::attachments::dir(&self.attachments_root(), id);
        let (name, mime, append_to) = (name.to_owned(), mime.to_owned(), append_to.map(str::to_owned));
        let stored = tokio::task::spawn_blocking(move || match append_to {
            Some(to) => otto_tools::attachments::append(&dir, &to, &bytes, max),
            None => otto_tools::attachments::put(&dir, &name, &mime, &bytes),
        })
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
        .map_err(|e| if e.kind() == std::io::ErrorKind::InvalidInput { Error::Invalid(e.to_string()) } else { e.into() })?;
        Ok(otto_proto::AttachmentInfo { id: stored.id, name: stored.name, mime: stored.mime, size: stored.size })
    }
    /// Input from a parent session to its child.
    pub(crate) async fn prompt_child(&self, id: &str, input: Vec<InputPart>, delivery: Delivery) -> Result<String> {
        self.session(id).await?.submit(input, delivery, false).await
    }
    /// The user stops the turn; a subagent then holds its report (§7.4).
    pub async fn cancel(&self, id: &str) -> Result<()> {
        self.session(id).await?.cancel().await
    }
    pub(crate) async fn cancel_child(&self, id: &str) -> Result<()> {
        self.session(id).await?.stop(false).await
    }
    /// The user hands a stopped sub-agent back: its held report goes to the
    /// parent now, once (§7.4). Refused when nothing is held or it runs again.
    pub async fn hand_back(&self, id: &str) -> Result<()> {
        self.session(id).await?.hand_back().await
    }
    pub async fn answer_approval(&self, id: &str, approval: &str, decision: ApprovalDecision) -> Result<()> {
        self.session(id).await?.answer_approval(approval, decision).await
    }
    pub async fn answer_approval_with_note(&self, id: &str, approval: &str, decision: ApprovalDecision, note: Option<String>) -> Result<()> {
        self.session(id).await?.answer_approval_with_note(approval, decision, note).await
    }
    pub async fn answer_question(&self, id: &str, question: &str, answer: String) -> Result<()> {
        self.session(id).await?.answer_question(question, answer).await
    }
    pub async fn compact(&self, id: &str) -> Result<()> {
        self.session(id).await?.compact().await
    }
    fn jobs(&self) -> Arc<otto_tools::builtin::shell::Jobs> {
        self.0.shell.lock().unwrap_or_else(std::sync::PoisonError::into_inner).jobs.clone()
    }
    /// The user detaches session `id`'s running shell call `call_id` (`shell.detach`).
    pub async fn detach_shell(&self, id: &str, call_id: &str) -> Result<()> {
        self.session(id).await?;
        if self.jobs().detach(id, call_id) { Ok(()) } else { Err(Error::Invalid(format!("no running shell command {call_id}"))) }
    }
    /// Session `id`'s background shell jobs (`job.list`).
    pub async fn shell_jobs(&self, id: &str) -> Result<Vec<otto_proto::JobInfo>> {
        self.session(id).await?;
        Ok(self
            .jobs()
            .list(id)
            .await
            .into_iter()
            .map(|j| otto_proto::JobInfo {
                job_id: j.job_id,
                command: j.command,
                call_id: j.call_id,
                block_id: j.block_id,
                started_at: j.started_at,
                completed: j.completed,
            })
            .collect())
    }
    /// Stop session `id`'s job `job_id` (`job.kill`).
    pub async fn kill_job(&self, id: &str, job_id: &str) -> Result<()> {
        self.session(id).await?;
        if self.jobs().kill(id, job_id).await { Ok(()) } else { Err(Error::Invalid(format!("no job {job_id} in this session"))) }
    }
    pub async fn fork(&self, id: &str, at_seq: i64) -> Result<SessionHandle> {
        let id = id.to_owned();
        let from = id.clone();
        let fork = self.db(move |s| Ok(s.fork_session(&id, at_seq)?)).await?;
        let root = self.attachments_root();
        otto_tools::attachments::copy_all(&otto_tools::attachments::dir(&root, &from), &otto_tools::attachments::dir(&root, &fork))?;
        self.session(&fork).await
    }
    /// Apply a durable mode change before subsequent tool policy checks.
    pub async fn set_approval_mode(&self, id: &str, mode: otto_proto::ApprovalMode) -> Result<()> {
        let handle = self.session(id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.tx.send(crate::session::Command::SetMode(mode, tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
    pub async fn rename(&self, id: &str, title: String) -> Result<()> {
        self.update(id, SessionUpdate { title: Some(title), ..Default::default() }, false).await.map(drop)
    }
    /// Set an automatic title unless the session got one meanwhile; `true` when applied.
    /// Boxed like `wake`: it is reached from inside session actors.
    pub(crate) fn auto_rename(&self, id: String, title: String) -> futures::future::BoxFuture<'static, Result<bool>> {
        let runtime = self.clone();
        Box::pin(async move { runtime.update(&id, SessionUpdate { title: Some(title), ..Default::default() }, true).await })
    }
    pub async fn archive(&self, id: &str, archived: bool) -> Result<()> {
        self.update(id, SessionUpdate { archived: Some(archived), ..Default::default() }, false).await?;
        // A closed session's background commands stop with it.
        if archived {
            let jobs = self.0.shell.lock().unwrap_or_else(std::sync::PoisonError::into_inner).jobs.clone();
            jobs.stop_session(id).await;
        }
        Ok(())
    }
    /// Change the session's provider, model, agent or effort (`session.update`).
    pub async fn configure(&self, id: &str, change: Configure) -> Result<SessionInfo> {
        let handle = self.session(id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.tx.send(crate::session::Command::Configure(change, tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?.map(|info| self.with_plan(info))
    }
    async fn update(&self, id: &str, update: SessionUpdate, untitled: bool) -> Result<bool> {
        let handle = self.session(id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.tx.send(crate::session::Command::Update(update, untitled, tx)).await.map_err(|_| Error::Closed)?;
        let applied = rx.await.map_err(|_| Error::Closed)??;
        if applied {
            self.changed();
        }
        Ok(applied)
    }
}
/// `row` with its context use: a session stored before otto kept one gets it
/// once, persisted: its last step's prompt out of its model's window.
fn with_context(s: &mut Store, models: &dyn ModelResolver, mut row: otto_store::Session) -> Result<otto_store::Session> {
    if !row.meta["context"]["used"].is_null() {
        return Ok(row);
    }
    let used = s.last_usage(&row.id)?.map_or(0, |usage| usage.total_input());
    let window = models.resolve(&row.provider, &row.model).map_or(0, |model| model.info().caps.context_window);
    if !row.meta.is_object() {
        row.meta = serde_json::json!({});
    }
    row.meta["context"] = serde_json::json!(otto_proto::ContextUse { used, window });
    s.set_meta(&row.id, &row.meta)?;
    Ok(row)
}
pub(crate) fn info(s: &Store, row: otto_store::Session) -> Result<SessionInfo> {
    let usage = s.usage_totals(&row.id)?;
    Ok(SessionInfo {
        id: row.id,
        project_id: row.project_id,
        parent_id: row.parent_id,
        approval_mode: approval_mode(&row.meta),
        agent: row.agent,
        provider: row.provider,
        model: row.model,
        effort: serde_json::from_value(row.meta["effort"].clone()).ok(),
        report_held: crate::subagents::held(&row.meta),
        plan: None,
        context: serde_json::from_value(row.meta["context"].clone()).unwrap_or_default(),
        title: row.title,
        cwd: row.cwd,
        status: if row.archived {
            SessionStatus::Archived
        } else {
            serde_json::from_value(serde_json::Value::String(row.status)).unwrap_or_default()
        },
        created_at: row.created_at,
        updated_at: row.updated_at,
        last_seq: row.last_seq,
        usage: UsageTotals { usage: usage.usage, cost_usd: usage.cost_usd },
    })
}
/// The session's approval mode; a value otto no longer has (`plan`) reads as the default, `auto`.
pub(crate) fn approval_mode(meta: &serde_json::Value) -> otto_proto::ApprovalMode {
    serde_json::from_value(meta["approval_mode"].clone()).unwrap_or_default()
}
pub(crate) fn policy_mode(mode: otto_proto::ApprovalMode) -> otto_tools::policy::ApprovalMode {
    match mode {
        otto_proto::ApprovalMode::Auto => otto_tools::policy::ApprovalMode::Auto,
        otto_proto::ApprovalMode::AskOnEffects => otto_tools::policy::ApprovalMode::AskOnEffects,
        otto_proto::ApprovalMode::AskAll => otto_tools::policy::ApprovalMode::AskAll,
    }
}
/// Which items a client's view holds (`session.view`): the newest stored
/// before `before_seq` (all when `None`) that fit `budget` serialized bytes
/// once clipped, at most `limit`. Items stored at one sequence (a fork's) are
/// never split, and at least one sequence's items are always sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewWindow {
    pub limit: Option<usize>,
    pub before_seq: Option<i64>,
    pub budget: usize,
}
impl Default for ViewWindow {
    fn default() -> Self {
        Self { limit: None, before_seq: None, budget: otto_proto::clip::VIEW_BUDGET }
    }
}
/// The session actor's side of a view.
pub(crate) struct ViewState {
    pub busy: bool,
    pub waiting: bool,
    pub approvals: Vec<otto_proto::PendingApproval>,
    pub questions: Vec<otto_proto::PendingQuestion>,
    pub queued: Vec<otto_proto::QueuedInput>,
}
/// The session's view: every item as stored (`window: None`), or a client's window of them, clipped.
pub(crate) fn build_view(s: &mut Store, id: &str, window: Option<ViewWindow>, state: ViewState) -> Result<SessionView> {
    use otto_proto::clip;
    let ViewState { busy, waiting, mut approvals, questions, queued } = state;
    let row = s.session(id)?.ok_or(Error::Closed)?;
    let mut info = info(s, row)?;
    info.status = if waiting {
        SessionStatus::WaitingApproval
    } else if busy {
        SessionStatus::Busy
    } else {
        info.status
    };
    let (mut items, mut has_older, mut oldest_seq) = (Vec::new(), false, None);
    match window {
        None => items = crate::legacy::items(s.items(id)?),
        Some(w) => {
            let (limit, mut used) = (w.limit.unwrap_or(usize::MAX).max(1), 0);
            s.items_newest(id, w.before_seq, |seq, item| {
                let mut item = crate::legacy::items(vec![item]).remove(0);
                clip::clip_item(&mut item);
                let size = clip::json_len(&item);
                let full = items.len() >= limit || used + size > w.budget;
                if full && !items.is_empty() && oldest_seq != Some(seq) {
                    has_older = true;
                    return false;
                }
                used += size;
                oldest_seq = Some(seq);
                items.push(item);
                true
            })?;
            items.reverse();
        }
    }
    let newest = window.is_none_or(|w| w.before_seq.is_none());
    if busy && newest {
        let mut open: Vec<_> = open_items(s, id, info.last_seq)?.into_iter().filter(|o| !items.iter().any(|i| i.id == o.id)).collect();
        if window.is_some() {
            open.iter_mut().for_each(|i| {
                clip::clip_item(i);
            });
        }
        items.extend(open);
    }
    let part_times = part_times(s, id, &items)?;
    let turns = s
        .turn_changes(id)?
        .into_iter()
        .filter(|(_, item, _)| item.as_ref().is_some_and(|item| items.iter().any(|i| i.id == *item)))
        .filter_map(|(turn, item_id, changes)| Some(otto_proto::TurnSummary { turn, item_id, changes: serde_json::from_value(changes).ok()? }))
        .collect();
    let streaming_tool_calls = if busy && newest { open_inputs(s, id, info.last_seq)? } else { Vec::new() };
    let mut running_tool_calls: Vec<_> = s
        .tool_calls(id)?
        .into_iter()
        .filter(|t| matches!(t.status, otto_store::ToolStatus::Intent | otto_store::ToolStatus::Running))
        .map(|t| otto_proto::RunningToolCall { call_id: t.call_id, name: t.name, input: t.input, started_at: Some(t.started_at) })
        .collect();
    let mut todos = None;
    if window.is_some() {
        for value in running_tool_calls.iter_mut().map(|c| &mut c.input).chain(approvals.iter_mut().map(|a| &mut a.input)) {
            clip::clip_large_value(value);
        }
        // The todo list is the latest `update_todos` call's: complete from the newest window.
        if has_older
            && let Some(oldest) = oldest_seq
            && !items.iter().any(|i| i.tool_calls().any(|(_, name, _)| name == "update_todos"))
        {
            todos = s.latest_call_before(id, oldest, "update_todos")?.map(|mut input| {
                clip::clip_large_value(&mut input);
                input
            });
        }
    }
    Ok(SessionView {
        seq: info.last_seq,
        info,
        items,
        part_times,
        streaming_tool_calls,
        pending_approvals: approvals,
        pending_questions: questions,
        queue: queued,
        running_tool_calls,
        turn_started_at: if busy { s.turn_started(id)? } else { None },
        has_older,
        oldest_seq,
        todos,
        turns,
    })
}
/// When the items' timed parts ran, from the journal (`SessionView.part_times`).
pub(crate) fn part_times(s: &Store, id: &str, items: &[otto_llm::Item]) -> Result<Vec<otto_proto::PartTime>> {
    let marks: Vec<_> = s
        .marks(id, otto_proto::timing::MARKS)?
        .into_iter()
        .map(|(kind, at, item_id, call_id)| otto_proto::timing::Mark { kind, at, item_id, call_id })
        .collect();
    Ok(otto_proto::timing::part_times(items, &marks))
}
/// Items started and not yet done (a reply being written), with their deltas journaled up to `upto`:
/// a view taken mid-reply shows the text already streamed.
pub(crate) fn open_items(s: &Store, id: &str, upto: i64) -> Result<Vec<otto_llm::Item>> {
    let mut open: Vec<otto_llm::Item> = Vec::new();
    for (seq, event) in s.events_after_last(id, &["item.done", "turn.started", "turn.finished"])?.into_iter().map(decode) {
        if seq > upto {
            break;
        }
        match &event {
            Event::ItemStarted { item_id, role } => open.push(otto_llm::Item::new(item_id, *role, vec![])),
            Event::TextDelta { item_id, .. } | Event::ReasoningDelta { item_id, .. } => {
                if let Some(item) = open.iter_mut().find(|i| i.id == *item_id) {
                    event.append_delta(&mut item.parts);
                }
            }
            _ => {}
        }
    }
    Ok(open)
}
/// Tool calls the model is still writing (named, input streaming, not yet `tool.call`) up to `upto`.
pub(crate) fn open_inputs(s: &Store, id: &str, upto: i64) -> Result<Vec<otto_proto::StreamingToolCall>> {
    let mut open: Vec<otto_proto::StreamingToolCall> = Vec::new();
    for (seq, event) in s.events_after_last(id, &["item.done", "turn.started", "turn.finished"])?.into_iter().map(decode) {
        if seq > upto {
            break;
        }
        match event {
            Event::ToolInputStart { call_id, name } => {
                open.push(otto_proto::StreamingToolCall { call_id, name, input_delta: String::new() });
            }
            Event::ToolInputDelta { call_id, delta } => {
                if let Some(call) = open.iter_mut().find(|c| c.call_id == call_id) {
                    call.input_delta.push_str(&delta);
                }
            }
            Event::ToolCall { call_id, .. } => open.retain(|c| c.call_id != call_id),
            _ => {}
        }
    }
    Ok(open)
}
pub(crate) fn record(event: &Event, turn: i64, step: i64) -> Result<EventRecord> {
    let value = serde_json::to_value(event)?;
    Ok(EventRecord { turn, step, kind: value["event"].as_str().unwrap_or("error").into(), payload: value["data"].clone() })
}
#[cfg(test)]
#[test]
fn unknown_journal_event_preserves_sequence() {
    let (seq, event) = decode(otto_store::Event {
        session_id: "session".into(),
        seq: 17,
        turn: 0,
        step: 0,
        kind: "future.event".into(),
        payload: serde_json::Value::Null,
        at: 0,
    });
    assert_eq!(seq, 17);
    assert!(matches!(event,otto_proto::Event::Error{class,..} if class=="journal_decode"));
}
fn decode(e: otto_store::Event) -> SequencedEvent {
    let decoded = if e.kind == "item.forked" {
        serde_json::from_value(e.payload.clone()).map(|item| Event::ItemDone { item })
    } else {
        serde_json::from_value(serde_json::json!({"event":e.kind,"data":e.payload}))
    };
    (
        e.seq,
        decoded.unwrap_or_else(|error| Event::Error {
            class: "journal_decode".into(),
            message: format!("Journal event {} at sequence {} cannot be represented: {error}; fetch session.view", e.kind, e.seq),
            retryable: false,
        }),
    )
}
