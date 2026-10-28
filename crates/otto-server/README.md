# otto-server

Unix-socket daemon for `otto-core`. No provider or network client is constructed
here. The embedding CLI supplies a recovered runtime, its model resolver, and an
`AuthService` implementation.

```rust,ignore
let handle = Server::start(ServerConfig {
    home,
    runtime: Arc::new(runtime),
    models,
    auth,
    idle_exit: Some(Duration::from_secs(60)),
    build: String::new(),
}).await?;
handle.serve_forever().await?;
```

`Server::start` returns only after binding the private socket and writing
`runtime.json`. `ServerHandle::shutdown` stops accepting, joins connection tasks,
shuts down the runtime, and removes the socket and runtime metadata. Dropping the
handle requests the same cleanup asynchronously; await shutdown when cleanup
must be complete before returning. `Server::serve_forever(config)` combines start
and wait. The embedding application owns OS signal handling.

`init_logging(home) -> io::Result<File>` returns an append-only, mode-0600 log
sink for the CLI's tracing subscriber. It does not install a global subscriber.

## Authentication interface

`AuthFuture<'a, T>` is a boxed `Send` future returning `Result<T, RpcError>`.
`AuthService: Send + Sync` has these object-safe methods:

- `status(&self) -> AuthFuture<'_, Vec<AuthStatus>>`
- `login(&self, provider: String, mode: AuthMode, api_key: Option<String>, device: bool) -> AuthFuture<'_, LoginResult>`
- `login_complete(&self, login_id: String, code: Option<String>) -> AuthFuture<'_, AuthStatus>`
- `login_cancel(&self, login_id: String) -> AuthFuture<'_, ()>` (drops a pending login and aborts an in-flight `login_complete`)
- `logout(&self, provider: String) -> AuthFuture<'_, ()>`
- `list_models(&self, provider: Option<String>) -> AuthFuture<'_, Vec<ModelInfo>>`

The auth implementation filters model results by signed-in auth mode and owns
credential storage and login state. Credentials are never written to daemon
config or tracing events. `auth.login` accepts `device: bool` (defaults to false),
valid only for subscription mode, and `paste: bool` (defaults to false; subscription mode, not with
`device`), which starts the provider's paste redirect: `Pending` with `needs_code`. `LoginResult::Pending` retains `url` and `login_id`
and adds optional `user_code: String`, `verification_uri: String`, and
`interval_secs: u64`. Missing values deserialize as None and None values are omitted.
`needs_code: bool` (omitted when false) says the provider shows a code to paste into
`auth.login_complete`; otherwise `login_complete` without a code waits for the browser
redirect or device approval. `accepts_code: bool` (omitted when false) says a browser
login also completes with the pasted callback URL or code, for when its loopback
callback can't be reached. `AuthStatus.modes` lists the sign-in modes a provider
supports (also when signed out); `AuthStatus.default_model` is set on the provider new
sessions default to.
The CLI implementation must pass the device flag to its provider login flow and
populate these fields for device authorization; browser logins use None.

## Streaming and lifecycle

RPC work is capped at 64 in-flight requests per connection and 256 server-wide.
The connection acquires its permit before reading the next frame, so a saturated
client is backpressured at the socket. Global saturation returns `busy` rather
than spawning waiting tasks. `ping` is answered inline and remains available on
other connections even when all work permits are occupied. Disconnect/shutdown
cancels tasks and releases both permits.

Each connection has one writer and a 256-frame outbox. Producers use `try_send`;
overflow cancels only that connection. Journal replay and live events share one
subscription task, and core's `LiveEvents` suppresses overlap. Durable broadcast lag
closes the connection instead of losing events silently. Replacing/unsubscribing
a subscription aborts and joins its previous task before acknowledging. Session
index notifications go to every connected client with a daemon-global sequence.

Durable session events, including coalesced deltas, retain journal sequences.
Uncoalesced display deltas additionally use a separate protocol-v1 envelope:

```json
{"live":true,"id":0,"session_id":"s","after_seq":4,"event":{"event":"text.delta","data":{"item_id":"i","text":"hello"}}}
```

There is no `seq`. `after_seq` is the last durable sequence sent before the live
message on this subscription. `event` is restricted to text, reasoning, tool-input,
and tool-progress deltas. The reserved unsolicited-response `id: 0` is necessary:
old clients' no-sequence decoder expects a response ID and ignores responses without
a pending request. They do not have a generic unknown-unsequenced-event path.

One subscription task orders both streams. It drains queued display deltas before
forwarding a coalesced delta or authoritative item completion. Ephemeral broadcast
lag is tolerated, and live output is dropped once half the writer queue is occupied,
reserving capacity for durable output. Reconnect always resumes the durable stream;
live messages are never replayed. `SessionMirror::apply_live` overlays the durable
projection without advancing its cursor. Persisted deltas consume matching overlay
prefixes, and `item.done` replaces the item authoritatively.

The lifetime lock uses `File::try_lock`. Only its owner removes a stale socket.
The home is 0700, the socket is 0600, and peer effective UID must match the server.
Idle exit requires no connected clients and no busy, queued, or approval-waiting
sessions for the configured duration, using the runtime's nonblocking busy query.

## Runtime integration

- `project.list` and cross-project session listing read the durable runtime index.
  `project.list` orders projects by their latest session `updated_at`
  (`ProjectInfo.updated_at`, else `created_at`) and omits roots that no longer exist.
  No `projects.json` is read or written; a legacy file can be removed by the user.
- `tools.list` requires `session_id` and returns that session's effective tools.
  Omitting it returns `bad_request`, because effective grants depend on the session.
- `agents.list` uses `Runtime::agents(None)` for built-ins and the runtime's user
  configuration directory. The current RPC has no project-root parameter.
- `skills.list {project_root?}` and `commands.list {project_root?}` list the user's
  and, with a root, the project's skills and slash commands.
- `session.queue_remove {session_id, input_id}` withdraws a pending input;
  `session.queue_update {session_id, input_id, delivery}` moves it between `steer`,
  `queue` and `interrupt`. Both journal `queue.changed` and fail with `bad_request`
  once the input was handed to the model or is not pending.
- `auth.login {base_url?}` stores the endpoint of a local or custom provider
  (`<state>/providers.json`) before signing in; `AuthStatus.base_url` reports it.
  Non-HTTP(S) URLs and providers with a fixed endpoint are `bad_request`.
- `approval.answer` forwards optional notes to the runtime for persistence.
- `session.create.approval_mode` optionally overrides defaults for that session.
  `session.set_mode {session_id, approval_mode}` journals a SessionChanged event
  and applies the mode at subsequent tool preparation. Wire values are `auto`,
  `ask_on_effects`, `ask_all`, and `plan`; SessionInfo includes the current mode.
  Plan agents reject non-plan mode changes with `bad_request`.
- `session.create {effort?}` sets the session's reasoning effort (`SessionInfo.effort`).
  `session.update {session_id, provider?, model?, agent?, effort?, reset_effort?}` changes only
  the given fields (`reset_effort`: back to the agent's effort, else the model's),
  journals `session.changed`, emits `sessions.changed` and returns the
  `SessionInfo`. It applies at the next model request (a running turn's next step);
  unknown agents or models and signed-out providers are `bad_request`.
- `AuthStatus.plan` is the subscription plan label when the sign-in reports one.
- Fork replay uses core's contiguous journal stream, including `ItemDone` for fork
  records and sequenced error events for undecodable records.
- Startup loads `config.json`; `config.set` updates runtime defaults for new sessions
  and saves the file. Existing sessions retain their configuration. Recognized keys
  are `agent`, `provider`, `model` (nonempty strings or null), `approval_mode`
  (the core approval-mode enum or null) and `native_compaction` (boolean or null).
  Other JSON keys remain stored but do not affect runtime defaults. Invalid
  defaults are rejected before persistence.


Tests run entirely in-process with temporary homes, Unix sockets, scripted models,
and no network/provider access. They exercise real `Connection` and
`SessionFollower` clients, replay after disconnect, tool and approval turns,
protocol compatibility, locking, permissions, idle exit, cleanup, and overflow.
