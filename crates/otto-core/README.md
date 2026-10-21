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
- `create_session(CreateSession) -> Result<SessionHandle>`; zero `max_steps` selects 200.
  `approval_mode: Option<otto_proto::ApprovalMode>` overrides new-session defaults.
- `set_approval_mode(session_id: &str, mode: otto_proto::ApprovalMode) -> Result<()>`
  durably changes policy for subsequent tool preparation. SessionChanged and
  SessionInfo expose the mode. Existing prepared/gated calls retain their decision.
  Plan agents always report Plan and reject attempts to leave it.
- `session(id) -> Result<SessionHandle>` starts a persisted session lazily.
- `list_sessions(project, include_archived) -> Result<Vec<SessionInfo>>`.
- `list_projects() -> Result<Vec<ProjectInfo>>` lists durable project records.
- `tools(session_id) -> Result<Vec<ToolInfo>>` lists the exact model-facing tool
  definitions allowed by the session grant, including policy-gated tools.
- Synchronous `busy() -> bool` conservatively reports active/queued session work
  without database IO; it returns true on contention.
- Synchronous `agents(project_root: Option<&Path>) -> Vec<AgentInfo>` applies the
  configured user directory and optional project overrides.
- Synchronous `set_config(SessionDefaults) -> Result<()>` replaces new-session
  defaults (`agent`, `provider`, `model`, `approval_mode`, all optional). Explicit
  creation options and agent model overrides take priority; plan agents remain
  read-only. Existing sessions retain their settings. None restores Auto ("yolo"):
  effect approvals do not prompt, but deny rules still block tools. Explicit
  AskOnEffects, AskAll, and Plan remain available. User questions still require
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

## Hooks and commands

Project `.otto/hooks.json` and user `hooks.json` accept Claude Code-style `hooks`
objects. Supported events are `SessionStart`, `PreToolUse`, `PostToolUse`, `Stop`,
and `PreCompact`, with snake-case aliases. A group has a `matcher` and `hooks`
array; handlers have `type: "command"`, `command`, and optional `timeout` seconds.
Matchers support exact names, `|` alternatives, and `*` (not full regex).
Commands receive JSON on stdin. `hookSpecificOutput.permissionDecision: "deny"`,
`updatedInput`, and `additionalContext` are understood; `decision: "block"`,
`continue: false`, and `systemMessage` are also recognized. Exit 2 blocks. A hook
allow does not override tool policy. Timeout defaults to 30 seconds (maximum 300),
and stdout is capped at 64 KiB. Hooks execute trusted local shell configuration:
opening an untrusted project with hooks permits its commands to execute.
`/name arguments` expands project `.otto/commands/name.md`, falling back to user
`commands/name.md`, replacing `$ARGUMENTS` and stripping frontmatter.

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

Native provider compaction is not used. Summary input and output are bounded;
oversized user instructions retain verbatim head/tail excerpts, not their complete
text. Stable prompts are rebuilt on reopen. Hook support is a command-only subset
of Claude Code (no regex matcher engine, HTTP/prompt hooks, async handlers, or
Stop continuation decisions). SessionStart runs at the first turn of each actor,
including resumed sessions. Hook process timeouts kill the shell but do
not guarantee termination of detached descendants. Hooks are trusted local
commands and are not sandboxed; approval policy does not gate hook commands.
Outbox delivery acknowledgment follows the item commit and is idempotent by item
ID; it is not yet a single transaction. Child completion requires its parent actor
to be open; restarting a child alone does not wake an unopened parent.
Call `runtime.shutdown().await` before dropping it to cancel/join actors and
release store references; do not submit commands concurrently with shutdown.
