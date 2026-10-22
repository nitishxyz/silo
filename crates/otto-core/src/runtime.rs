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
}
pub struct RuntimeConfig {
    pub store: Arc<Mutex<Store>>,
    pub models: Arc<dyn ModelResolver>,
    pub state_dir: PathBuf,
    pub user_config_dir: PathBuf,
}
/// Reloadable defaults, applied only to sessions created after set_config.
#[derive(Clone, Debug, Default)]
pub struct SessionDefaults {
    pub agent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// None selects Auto; plan agents always use Plan.
    pub approval_mode: Option<otto_tools::policy::ApprovalMode>,
    /// Let new sessions use the provider's native compaction when their model
    /// has it and the history is its own; otto's summarizer otherwise.
    pub native_compaction: bool,
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
    /// Set by shutdown: no actor starts afterwards.
    closing: std::sync::atomic::AtomicBool,
    /// Name untitled sessions after their first turn (`set_auto_title`).
    auto_title: std::sync::atomic::AtomicBool,
}
impl Runtime {
    /// Recover interrupted tool calls before accepting any new work.
    pub async fn new(config: RuntimeConfig) -> Result<Self> {
        let (global, _) = broadcast::channel(256);
        let (index, _) = broadcast::channel(256);
        let runtime = Self(Arc::new(Inner {
            config,
            actors: tokio::sync::Mutex::new(HashMap::new()),
            global,
            index: Mutex::new((0, index)),
            defaults: Mutex::new(SessionDefaults::default()),
            extra: Mutex::new(Vec::new()),
            sources: Mutex::new(Vec::new()),
            closing: std::sync::atomic::AtomicBool::new(false),
            auto_title: std::sync::atomic::AtomicBool::new(false),
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
        Ok(runtime)
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
                otto_proto::AgentInfo { name: a.name, description: a.description, model, provider }
            })
            .collect()
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
        let (grant, denied) = self.grants(&agent, &family, options.parent_id.clone()).await?;
        let effort = options.effort.map(serde_json::to_value).transpose()?.or_else(|| agent.effort.clone().map(serde_json::Value::String));
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
                    meta: serde_json::json!({"max_steps":options.max_steps,"tools":grant,"denied_tools":denied,"task":task,"effort":effort,"native_compaction":config.native_compaction,"approval_mode":approval_mode.or_else(||config.approval_mode.map(|m|serde_json::to_value(m).expect("mode enum")))}),
                })?)
            })
            .await?;
        let handle = self.session(&row.id).await?;
        self.changed();
        Ok(handle)
    }
    /// The tools `agent` is granted on a model of `family`, narrowed by the parent's grant.
    pub(crate) async fn grants(&self, agent: &crate::agents::Agent, family: &str, parent: Option<String>) -> Result<(Vec<String>, Vec<String>)> {
        let mut grant: Vec<String> = if agent.tools.is_empty() {
            otto_tools::registry::tools_for_family(family)
                .iter()
                .map(|t| t.spec().name.clone())
                .chain(["skill".into(), "subagent".into(), "mcp__*".into()])
                .chain(self.extra_tools().iter().map(|t| t.spec().name.clone()))
                .collect()
        } else {
            agent.tools.clone()
        };
        let mut denied = agent.disallowed.clone();
        if let Some(parent) = parent {
            let parent = self.db(move |s| Ok(s.session(&parent)?.ok_or(Error::Closed)?.meta)).await?;
            if let Some(allowed) = parent["tools"].as_array() {
                grant.retain(|tool| allowed.iter().filter_map(|v| v.as_str()).any(|p| crate::tools::pattern(p, tool)));
            }
            denied.extend(parent["denied_tools"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)));
        }
        grant.retain(|tool| !denied.iter().any(|d| crate::tools::pattern(d, tool)));
        if otto_tools::registry::tools_for_family(family)
            .into_iter()
            .chain(self.extra_tools())
            .any(|tool| tool.spec().loading == otto_tools::Loading::Deferred && grant.contains(&tool.spec().name))
            && !grant.iter().any(|name| name == "load_tools")
        {
            grant.push("load_tools".into());
        }
        Ok((grant, denied))
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
        let row = self
            .db(move |s| {
                let row = s.session(&owned)?.ok_or_else(|| Error::Invalid(format!("unknown session {owned}")))?;
                Ok(row)
            })
            .await?;
        let handle = crate::session::spawn(self.clone(), row).await?;
        actors.insert(id.into(), handle.clone());
        Ok(handle)
    }
    pub async fn list_sessions(&self, project: &str, include_archived: bool) -> Result<Vec<SessionInfo>> {
        let project = project.to_owned();
        self.db(move |s| s.list_sessions(&project, include_archived)?.into_iter().map(|row| info(s, row)).collect()).await
    }
    pub async fn view(&self, id: &str) -> Result<SessionView> {
        self.session(id).await?.view().await
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
    pub async fn prompt(&self, id: &str, input: Vec<InputPart>, delivery: Delivery) -> Result<String> {
        self.session(id).await?.prompt(input, delivery).await
    }
    pub async fn cancel(&self, id: &str) -> Result<()> {
        self.session(id).await?.cancel().await
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
    pub async fn fork(&self, id: &str, at_seq: i64) -> Result<SessionHandle> {
        let id = id.to_owned();
        let fork = self.db(move |s| Ok(s.fork_session(&id, at_seq)?)).await?;
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
        self.update(id, SessionUpdate { archived: Some(archived), ..Default::default() }, false).await.map(drop)
    }
    /// Change the session's provider, model, agent or effort (`session.update`).
    pub async fn configure(&self, id: &str, change: Configure) -> Result<SessionInfo> {
        let handle = self.session(id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.tx.send(crate::session::Command::Configure(change, tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
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
pub(crate) fn info(s: &Store, row: otto_store::Session) -> Result<SessionInfo> {
    let usage = s.usage_totals(&row.id)?;
    Ok(SessionInfo {
        id: row.id,
        project_id: row.project_id,
        parent_id: row.parent_id,
        approval_mode: if row.agent == "plan" {
            otto_proto::ApprovalMode::Plan
        } else {
            serde_json::from_value(row.meta["approval_mode"].clone()).unwrap_or_default()
        },
        agent: row.agent,
        provider: row.provider,
        model: row.model,
        effort: serde_json::from_value(row.meta["effort"].clone()).ok(),
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
