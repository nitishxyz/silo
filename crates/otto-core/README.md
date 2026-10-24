# otto-core

The CLI and daemon share `Runtime`. All public runtime operations are async except
`events()`. The caller supplies a Tokio runtime, a store, and a provider resolver.
No provider credentials or network clients are constructed by this crate.

```rust,ignore
let runtime = Runtime::new(RuntimeConfig {
    store: Arc::new(std::sync::Mutex::new(Store::open(database_path)?)),
    models: resolver,
    state_dir,
    user_config_dir,
}).await?;
let project = runtime.open_project(cwd).await?;
let session = runtime.create_session(CreateSession {
    project_id: project.id,
    cwd: cwd.into(),
    agent: "build".into(),
    ..Default::default()
}).await?;
let (replay, mut live) = runtime.subscribe(&session.id, 0).await?;
session.prompt(vec![InputPart::Text { text: task }], Delivery::Queue).await?;
```

## Public API

- `ModelResolver: Send + Sync`: `resolve(&self, provider, model) -> Result<Arc<dyn Model>>`,
  `default_model(&self) -> Option<(String, String)>`, `list(&self) -> Vec<ModelInfo>`.
- `Runtime::new(RuntimeConfig) -> Result<Runtime>` performs recovery before returning.
- `open_project(root) -> Result<ProjectInfo>` canonicalizes the root.
- `create_session(CreateSession) -> Result<SessionHandle>`; zero `max_steps` selects 999.
  `approval_mode: Option<otto_proto::ApprovalMode>` overrides new-session defaults.
- `set_approval_mode(session_id: &str, mode: otto_proto::ApprovalMode) -> Result<()>`
  durably changes policy for subsequent tool preparation. SessionChanged and
  SessionInfo expose the mode. Existing prepared/gated calls retain their decision.
  A stored mode otto no longer has (`plan`) reads as Auto.
- `session(id) -> Result<SessionHandle>` starts a persisted session lazily.
- `list_sessions(project, include_archived) -> Result<Vec<SessionInfo>>`.
- `list_projects() -> Result<Vec<ProjectInfo>>` lists durable project records.
- `tools(session_id) -> Result<Vec<ToolInfo>>` lists the exact model-facing tool
  definitions allowed by the session grant, including policy-gated tools.
- Synchronous `busy() -> bool` conservatively reports active/queued session work
  without database IO; it returns true on contention.
- Synchronous `agents(project_root: Option<&Path>) -> Vec<AgentInfo>` applies the
  configured user directory and optional project overrides; `skills(..)` and
  `commands(..)` list skills and slash commands the same way.
- `agent(name, project_root)`, `save_agent(&AgentDef, previous_name, project_root)`
  and `delete_agent(name, scope, project_root)` read and edit definitions
  (`agents.get/save/delete`); `tool_catalog(project_root)` lists every grantable tool.
- `add_source(Arc<dyn otto_tools::ProjectSources>)` adds project-dependent tools
  (MCP via `otto-mcp`), listed again before every model request and granted by
  pattern (`mcp__*` by default). `shutdown` stops them.
- `set_terminals(Arc<dyn otto_tools::builtin::shell::Terminals>)` runs `shell` commands in a terminal host
  (silo's parked command blocks, `otto_silo_tools::silo`); short read-only ones and calls it does not serve use
  pipes. Shell jobs are runtime-wide; a job's result (background, or a command that waited for input) is delivered
  to its session as a system item through the outbox (`{notice}` from the session to itself). Archiving a session
  stops its jobs.
- `deliver_pending() -> Result<usize>` wakes sessions with undelivered outbox
  entries; call it once after registering tools and sources.
- `queue_remove(id, input_id)` and `queue_update(id, input_id, Delivery)` (also on
  `SessionHandle`) withdraw or move a pending input until the model has it.
- `CreateSession.task` records a delegated child's task; `SessionDefaults.native_compaction`
  opts new sessions into provider-native compaction.
- Synchronous `set_config(SessionDefaults) -> Result<()>` replaces new-session
  defaults (`agent`, `provider`, `model`, `approval_mode`, all optional). Explicit
  creation options and agent model overrides take priority.
  Existing sessions retain their settings. None restores Auto ("yolo"):
  effect approvals do not prompt, but deny rules still block tools. Explicit
  AskOnEffects and AskAll remain available. User questions still require
  an answer; Auto is not permission to invent answers.
- `answer_approval_with_note(id, approval_id, decision, Option<String>)` persists
  the note as an ItemDone system item before resolving the approval. The handle
  exposes the same method without the session ID. The old method remains valid.
- `view(id) -> Result<otto_proto::SessionView>` is a reconnect snapshot.
- `subscribe(id, after_seq) -> Result<(Vec<SequencedEvent>, LiveEvents)>` subscribes
  before replay; `LiveEvents::recv()` discards the overlap. A lag error requires
  resnapshot/replay rather than silently skipping events. Legacy `item.forked`
  records map to ItemDone; undecodable records become Error events with class
  `journal_decode` at their original sequence, never silently dropped.
- `prompt(id, Vec<InputPart>, Delivery) -> Result<String>` returns an input ID.
- `cancel(id)`, `compact(id)`, `answer_approval(id, approval_id, decision)`,
  `answer_question(id, question_id, answer)`, `rename(id, title)`, and
  `archive(id, archived)` return `Result<()>`.
- `configure(id, Configure { provider, model, agent, effort, reset_effort }) -> Result<SessionInfo>`
  (`session.update`): each step reads one snapshot of the session's model, agent and
  meta, so a change applies at the next model request. A new provider/model resolves
  a new model instance (dropping its provider-side state) and regrants tools for the
  model's family; a new agent brings its model (unless one is given), effort and
  approval mode, regrants tools and reassembles the prompt. `CreateSession.effort` / meta `effort` reach requests as
  `Reasoning::Effort` only for models with `caps.reasoning`.
- `set_auto_title(true)` (the daemon enables it) names untitled sessions after their
  first finished turn with one call to the provider's cheapest priced model, else the
  session's model; it never replaces a title set meanwhile, and a failure is logged.
- `recent_projects()` lists projects most recently used first, without missing roots.
- `fork(id, at_seq) -> Result<SessionHandle>` creates a new provider lane.
- `events() -> broadcast::Receiver<Event>` carries global `SessionsChanged`.
- `index_events() -> broadcast::Receiver<SequencedEvent>` is the additive sequenced
  equivalent. Its index resets with each Runtime; it is not a durable replay log.
- `SessionHandle` exposes `id` and the session-specific prompt/view/cancel/answer/
  compact methods without a session-ID argument.
- `SessionHandle::deltas()` returns ephemeral uncoalesced display deltas. Clients
  using this path must not also render durable delta events; `ItemDone` always
  contains authoritative complete parts. Durable subscriptions carry coalesced
  deltas, including on replay.

All database operations use `spawn_blocking`. Tools execute only after their
intent is committed; outcomes precede result items. Unknown recovered calls are
not rerun. Pending approvals and questions are memory-only. Queue waits for the
current turn; steer injects at a step boundary; interrupt cancels the active turn
before starting the replacement input. Cancellation preserves queued inputs only
for interrupt, not explicit cancel.

## Prepared tools and output retention

Inputs are validated with JSON Schema (external HTTP/file schema resolution is
 disabled), then prepared once before approval. PreToolUse hooks receive resolved
input with private guards removed; a modified input is validated and prepared
again before the final policy gate. Execution receives the exact final prepared
input and path guards, never the model's unprepared input. A prepare/validation
failure produces an error ToolResult without invoking the tool.
ToolCall and ApprovalRequested input payloads contain `original_input` (redacted),
`approved_input` (redacted effective input), and `resolved_paths` (path/location).
The private `__otto_paths` execution guards are not included in these payloads or
persisted assistant call inputs. Approvals authorize these pinned targets; a
symlink changed while waiting for approval causes a tool error rather than a write
to its new target.

Every call receives `<state>/outputs/<session-id>` as spill_dir. Shell's
`metadata.spill_path` is retained in tool results. Archiving an idle session
removes its entire output directory; busy sessions must be cancelled and finished
before archiving. Archived sessions do not start new prompt/outbox turns until
unarchived. Startup removes leftover outputs of archived sessions whose last
metadata update is more than seven days old (a conservative proxy for archive
age). Recent archived and all active-session outputs are retained. Cleanup refuses
a symlinked output root and never follows a session-directory symlink. Historical
spill links can therefore refer to deleted files after archive cleanup.

## Ownership and recovery

The actor owns pending approvals/questions, steering inputs, policy, todos, and
journal writes. Workers send acknowledged messages; parallel tools remain parallel.
Status transitions and metadata changes commit with their events in one store
transaction. `list_sessions` reads those status projections. Runtime startup scans
all sessions for missing tool results, including already-unknown calls, calls
without intents, and terminal outcomes whose result items were not committed.
Nothing recovered is executed. Project instructions, agents, skills, hooks, and
policy resolve from the opened project's root; tool execution uses session cwd.

## Hooks, commands, skills, agents

Hooks use Claude Code's `hooks` object from user `hooks.json`, project
`.claude/settings.json`, `.claude/settings.local.json` and `.otto/hooks.json` (all
run). Events: `SessionStart` (source `startup`/`resume`), `SessionEnd` (archive),
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification` (approvals and
questions), `Stop`, `SubagentStop`, `PreCompact` (`manual`/`auto`), plus snake-case
aliases. Matchers are anchored regexes (`""`/`*` match all) and also match Claude
Code's names for built-ins (`Bash` = `shell`). Handlers are `type: "command"` with
`timeout` seconds (default 60, max 600) and get `CLAUDE_PROJECT_DIR`,
`OTTO_PROJECT_DIR`, `OTTO_SESSION_ID`. Exit 2 blocks with stderr as the reason;
other non-zero exits are reported as `error` events of class `hook` and ignored.
Stdout (64 KiB max) JSON: `continue: false` + `stopReason` stops the turn after the
step; `decision: "block"` + `reason` blocks a prompt or tool, sends a PostToolUse
reason to the model, and makes Stop continue (3 times per turn);
`hookSpecificOutput.permissionDecision` `deny`/`ask` (an `allow` never overrides
policy), `updatedInput`, `additionalContext`; `systemMessage` becomes a `hook`
error event. Plain stdout is context for SessionStart and UserPromptSubmit.
Context produced while tools run is committed after the tool results. Hooks
execute trusted local shell configuration: opening an untrusted project with
hooks permits its commands to execute.

`/name arguments` expands the first matching template from `.otto/commands`,
`.claude/commands`, then user `commands` (subfolders: `folder:name`), with
`$ARGUMENTS`, `$1`…`$9`, or the arguments appended. Skills come from user
`skills/`, `.claude/skills`, `.agents/skills`, `.otto/skills`. Agents come from user
`agents/`, `.claude/agents`, `.otto/agents` with `tools` (shown), `loadable` (behind
`load_tools`), `disallowedTools` (Claude Code names mapped, `prefix*` patterns),
`model` (`inherit`), `effort`, and `approval_mode`/`permissionMode`. The only
built-in is `build`.

## Subagents and outbox

A finished child turn (`done`, `error`, or `interrupted` by a restart) is pushed
once to the parent's outbox, versioned by the child's turn, then the parent is
woken; its actor is opened when it is not running. Delivery commits the system
item and its acknowledgement in one transaction (`Store::append_delivering`), so
an entry is never delivered twice. Forks are not subagents. Limits: depth 2, four
running children per parent.

## Hardening status

- Single owner: actor-owned state and acknowledged worker messages; actor-only
  journal writes once running. Startup recovery and creation/fork initialization
  operate before the relevant actor starts. Parallel tool execution is retained.
- Atomic status: turn/gating status and metadata updates commit with their events;
  session lists preserve waiting status while any parallel gate remains pending.
- Startup recovery: every stored session is scanned, including already-unknown
  calls, missing intents, and terminal outcomes without result items. Terminal
  output reconstruction preserves text, JSON, images, and file references.
- Crash tests: file databases are stopped at each persisted boundary (assistant
  only, unknown intent, terminal outcome), reopened, and checked without opening
  an actor or invoking the model. These simulate crash state directly rather
  than killing a process or adding production fault-injection hooks.
- Bounded compaction: per-item head/tail, context-window-derived total input
  budget, max_output, and a defensive accumulated-output cap; huge-history test.
- Root/cwd: project configuration uses the opened root; tool processes and the
  prompt's environment cwd use the session cwd, including nested directories.
- Hooks/templates: SessionStart, PreToolUse, PostToolUse, Stop, PreCompact command
  handlers and slash templates are implemented with the subset documented above.
- Verification: offline core/store tests, formatting, clippy, and server compile
  check pass. Existing public API signatures remain unchanged.

## Current limitations

Native compaction is opt-in (threshold `Auto` at 75% of the window on ordinary
requests, `Now` for manual/overflow). Every provider gets otto's own tool specs
(no provider-predefined tools); old sessions' calls to Claude `bash` /
`str_replace_based_edit_tool` or OpenAI `shell_call` / `apply_patch_call` are read
as calls to ours (`legacy.rs`) for requests and views. Summary input and output are bounded; oversized user instructions retain
verbatim head/tail excerpts, not their complete text. Stable prompts are rebuilt on
reopen. Hooks have no prompt/HTTP handler types or async handlers; `transcript_path`
is null. SessionStart runs at the first turn of each actor, including resumed
sessions. Hook process timeouts kill the shell but do not guarantee termination of
detached descendants. Hooks are trusted local commands and are not sandboxed;
approval policy does not gate hook commands. `load_tools` writes `loaded_tools`
outside the actor journal.
Call `runtime.shutdown().await` before dropping it to cancel/join actors and
release store references; do not submit commands concurrently with shutdown.
