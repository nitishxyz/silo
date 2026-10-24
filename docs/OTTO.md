# Otto: an agent harness for silo (research + plan)

Status: direction decided (§11.1). **O0–O2 implemented on branch `otto`, verified offline** (§0). Still open:
cross-machine conflicts (§12).

## 0. Implementation status

| Phase | State | Where |
|---|---|---|
| O0 providers | Anthropic Messages + OpenAI Responses (SSE), each with API key and subscription sign-in (Claude Pro/Max code paste; ChatGPT/Codex browser loopback + device code). Recorded-fixture tests. models.dev catalog with per-model auth availability | `otto-llm` |
| O1 loop + tools | Turn engine (one actor per session), approvals (default `auto`, per-session modes), `Policy::prepare` path binding, tool journal + crash recovery, compaction, subagents, agents/skills/hooks/commands, built-in tools | `otto-core`, `otto-tools`, `otto-store` |
| O2 server | Daemon with gap-free subscriptions, live deltas, bounded queues, idle exit; client mirror + follower; `otto-silo` CLI | `otto-proto`, `otto-server`, `otto-client`, `otto-cli` |
| O3 | OpenAI WebSocket with steering and `previous_response_id` continuation, Codex over WebSocket, OpenAI and Anthropic native compaction (Anthropic threshold compaction pauses after its checkpoint, which drops everything before it, and the turn's prompt is re-sent after it; OpenAI's carries the context and continues in the same response), async tools, Anthropic server tools and context editing; Gemini (Interactions API), xAI (Responses), the compat family (OpenRouter, DeepSeek, Kimi, Z.ai, MiniMax, Ollama, LM Studio, custom) with local model discovery; cross-provider scripted test (§4.7, §4.8). Every provider gets otto's own tools (name, description, JSON schema; GPT's `apply_patch` as our freeform patch tool); provider-predefined client tools (Claude `bash`/`text_editor`, OpenAI `shell`/`apply_patch` types) are not used, and old sessions' calls to them replay as calls to ours (§5) | `otto-llm`, `otto-cli/src/{auth,services}.rs` |
| O4 harness depth | Durable versioned outbox (delivered once, wakes a closed or restarted parent, interrupted children report), `subagent` delegate/reuse/list/status/message/stop/retry with depth 2 and 4 running children; Claude Code-compatible hooks (9 events, regex matchers, exit 2, `decision`/`permissionDecision`/`continue`), skills, slash commands and agents (`.claude/` dirs read too); MCP client (stdio + streamable HTTP, `.mcp.json`); `load_tools` ranked search and prefix loading; context editing; opt-in native compaction; queue edits (`session.queue_remove`/`queue_update`, pending steers listed in the queue); `auth login` for all 14 providers with `--base-url` (keyless for local and custom endpoints) | `otto-core`, `otto-mcp`, `otto-store`, `otto-proto` |
| Sessions | `session.update` (provider, model, agent, reasoning effort; from the next model request, a new model drops provider-side state), per-session effort (`Reasoning::Effort` for models with `caps.reasoning`), automatic titles after the first turn (cheapest priced model of the session's provider, never over a set title), plan labels in `auth.status` (Claude profile `organization_type`, ChatGPT `chatgpt_plan_type`), `project.list` most recent first without missing roots | `otto-core`, `otto-llm`, `otto-server`, `otto-proto` |
| O5 silo toolset | Partly built: `terminal_*` and `browser_*` tools (`silo-tools`), offered to every otto session through `otto-silo-tools`, and to other harnesses through `silo mcp`; parked blocks (`block.spawn`/`park`/`reveal`) and `shell` in parked command blocks with background results delivered to the session (§6.2). Per-subagent grants: `delegate {tools}` narrows a child to a subset of its agent's tools, silo's included (§7.4). Not built: the `service` tunnel target | `silo-proto`, `silo-server`, `silo-tools`, `otto-silo-tools`, `otto-tools`, `silo-cli` |
| O7 agent block | Built: otto sessions as a native chat block in the app (docs/AGENT_BLOCK.md) | `silo-app/src/agent` |
| O6, O8 | Not started beyond the `browser_*` tools above | – |

**Verified (2026-10-22):** workspace fmt, clippy `-D warnings`, `cargo nextest run --workspace` on macOS (1026 tests),
the otto and silo server/CLI crates on Linux (`rust:1-bookworm`), `otto-proto` on wasm32, the silo protocol pin.
Live with the Anthropic subscription (`claude-haiku-4-5`, `otto-silo run --json`): plain chat; ls/search/read/write/
`bash` (then Claude's predefined tool, since removed) with `update_todos`; an MCP tool from a stdio server in the project's `.mcp.json`; a `subagent`
delegation whose completion reached the parent; a follow-up turn in the same session. Through the app
(`claude-sonnet-5-5`): a turn steered mid-run with ⏎, the steer applied at the next step.

**Not yet verified live:** OpenAI (API key and subscription), Gemini, xAI and the compat providers (no credentials
here); provider-native compaction and context editing (offline tests only; `otto-silo` has no compact command);
subscription refresh; the models.dev catalog refresh.

**Known gaps:**
- Hooks run `type: "command"` handlers only (no prompt/HTTP hooks, no async handlers); `SessionEnd` fires on archive.
- The MCP client is hand-written (stdio, streamable HTTP without the GET notification stream or OAuth); `rmcp` was
  not used because builds here are offline. Resources and prompts are not exposed.
- Path guards are revalidation, not a sandbox (§5.2 "later").
- Global index event seqs reset when the daemon restarts.
- Native tool search (`defer_loading`) is not used.
- A runtime that was shut down stays closed (outbox wakes cannot reopen actors); a restarted server needs a new one.
- An agent change in `session.update` keeps the session's model (the agent's `model` applies only at creation).
- Automatic titles are tried once per session actor; a restarted daemon may try a still-untitled session once more.
  The Claude plan comes from `/api/oauth/profile` at login and refresh (best effort, not verified live); older
  stored credentials report none until their next refresh.
- O5 shell: every model gets the full `shell` (`command`, `cwd`, `timeout_ms`, `background`). The user can detach a
  running foreground call (`shell.detach`), which then reports like a background one; `job.list` / `job.kill` list and
  stop a session's jobs. Jobs live in the daemon: after a restart, a job that never reported gets a `was stopped (the otto server
  restarted)` system item at startup (no turn starts). `waiting_for_input` is a heuristic (`line_prompt`: canonical
  tty, cursor ending a non-empty line, 2 s quiet); a prompt that ends with a newline, or a raw-mode one, isn't seen
  and the call runs to its timeout. The agent block recognises job reports by their system text
  (`Background shell <id> …`), since system items carry no metadata.

Each crate's `README.md` lists its own limitations.

Sources: this workspace at release 0.1.4 plus the uncommitted Phase 7 work; the TypeScript otto at
`~/dev/nitishxyz/otto` (read for lessons, not ported); provider docs as of October 2026 (Anthropic Messages,
OpenAI Responses + WebSocket mode, Gemini Interactions API).

---

## 1. Goals

- **A harness server in Rust**, its own crates, server/client like silo. The GPUI client becomes a silo block
  later (not designed here).
- **First-party providers.** Our own clients for Anthropic, OpenAI, Google, xAI and the OpenAI-compatible family.
  Each one covers everything its API supports, behind one generic model interface the harness uses.
- **A harness-agnostic toolset in silo.** Silo features (terminal, browser, layout, notifications) become agent
  tools any harness can use. Otto loads them as first-class tools, not through a lossy bridge.
- **Everything a modern harness does:** streaming, parallel tools, approvals, reasoning, caching, compaction,
  steering, subagents, skills, MCP, hooks, durable sessions, multiple clients.

**Non-goals for v1:** porting the TS code, a web UI, sandboxed untrusted plugins, remote execution.

---

## 2. Findings

### 2.1 What silo already gives us

| Thing | Where | Why it matters to otto |
|---|---|---|
| Framing `u32 len \| u8 kind \| payload`, JSON control, seq'd events, 8 MiB overflow closes | `silo-proto/src/{frame,lib}.rs`, PLAN §3.2 | The same wire works for otto: snapshot + gap-free events is how clients mirror a session |
| Unix socket + iroh QUIC transport, pairing, identity | `silo-transport` | Otto's server can listen with it unchanged |
| Self-describing API: JSON Schema params/results, examples, access needs | `silo-proto/src/api.rs` (`describe`, `examples`) | Almost a tool registry already. PHASE7 §2 defers "agent skill / MCP server: a thin generator over `api.describe`" |
| Grants, rights, approvals, scoped tokens, `SILO_AGENT` attribution | PHASE7 §3, §9 (in progress) | Agents get least-privilege tokens per session/subagent; owner actions already go to an approval card |
| Terminal: `terminal.send/capture`, `block.commands`, `terminal.command_output`, `blocks.busy`, OSC 133 records | PHASE7 §5, §7 | Enough to build `terminal.run` (send, wait for D mark, return exit + output) |
| Browser blocks: the **engine runs in the client** (CEF); the server only holds URL/history/zoom | `silo-server/src/browser.rs`, PLAN §6 | Agent browser actions (snapshot, click, screenshot, eval) must go to the owning app, like `app.*` forwarding (PHASE7 §4.2). PLAN §6 already lists a server-side headless Chromium for agents as "later" |
| `app.request` / `app.reply` forwarding to one app client | PHASE7 §4.2 | Same mechanism carries browser tool calls to the app that runs CEF |
| Sealed state files with a per-host key | `silo-store` | Credentials at rest (API keys, OAuth tokens) |
| Tunnels `silo/tunnel/1` to server-private TCP addresses | `silo-server/src/tunnel.rs` | A remote GPUI client can reach a local otto daemon with no new transport |

### 2.2 Lessons from the TS otto

What worked and should carry over as design, not code:
- **Async-only delegation**: subagents are child sessions; the parent keeps working; results go through an outbox
  that wakes an idle parent (`docs/plans/otto.md`, `runtime/subagents/`). `delegate / list / message(queue|interrupt)
  / stop / retry / reuseSessionId` covered every real use.
- **Tools keyed by call id, durable finalization**: journal intent before running, outcome before commit; after a
  crash, report "outcome unknown" and never re-run a mutation (`harness-reliability-roadmap.md`).
- **One checkpoint compaction**: a bounded summary that replaces history, reserved space for user instructions,
  head+tail of long evidence, `summarizedThrough` separate from the summary message (`compaction-continuity.md`).
- **Tool metadata drives policy**: effects (`workspace-read/write`, `process`, `network`, `secrets`,
  `external-write`), sensitive input fields redacted everywhere, canonical path resolution before guards.
- **Loadable tools**: extension tools cost no schema space until `load_tools` brings them in.
- **Model-family tool shapes**: models edit best with the tool shape they were trained on (`apply_patch` for GPT,
  `str_replace` editing for Claude). The TS runner picked edit tools per family.

What went wrong, and why a fresh design:
- Vercel AI SDK in the middle: provider-native features (encrypted reasoning, server tools, WebSocket mode,
  compaction blocks) were reached through `providerOptions` side doors and patched fetches
  (`openai-oauth-websocket.ts`, `anthropic-thinking-binding.ts`, `text-guard.ts`).
- The runner and tool adapter grew to ~800 lines each mixing streaming, persistence, events and policy
  (`runner-provider-tool-refactor.md`). Persistence ordering, duplicate terminal transitions and outbox races took a
  whole reliability phase to fix.
- Messages + parts rows written from many places. An append-only event log with projections avoids most of those races.
- Strict-mode schema surprises (`openai-oauth-tool-streaming.md`): provider quirks need to live in the provider,
  covered by recorded-stream tests.

### 2.3 Provider landscape (Oct 2026)

| | Anthropic Messages | OpenAI Responses | Google Interactions | OpenAI-compatible chat |
|---|---|---|---|---|
| Transport | HTTP + SSE | HTTP + SSE, **WebSocket mode** (`stream_id` multiplexing, forks, `generate:false` warmup) | HTTP + SSE, background | HTTP + SSE |
| Server state | none (stateless) | `previous_response_id`, `store=false`/ZDR works with WS cache | `previous_interaction_id`, `store=false` | none |
| Reasoning | thinking blocks + signatures, effort, task budgets | reasoning items (+ `encrypted_content`), effort | thinking + thought signatures (must be returned) | `reasoning_content` (varies) |
| Tools | client, Anthropic-schema client (bash, text_editor, memory, computer, browser), server (web_search/fetch, code_execution, tool_search, advisor, MCP connector), strict, programmatic calling, fine-grained streaming | function, custom (freeform), hosted (web/file search, code interpreter, tool search, image gen, MCP), shell, apply_patch, computer use, **async tools**, programmatic calling | function, Google Search, Maps, code execution, URL context, computer use, file search | function |
| Mid-run control | **mid-conversation system messages, tool_addition/removal** (beta) keep the cache | **mid-turn steering** (`response.steer`, WS, GPT-6) | – | – |
| Context | prompt caching (breakpoints), context editing, **compaction** (on demand / threshold, beta) | prompt cache key, `context_management` compaction, `/responses/compact` | implicit + explicit caching | provider-specific |
| Other | Files API, PDFs, citations, batch, fast mode, structured outputs | background mode, webhooks, batch, flex/priority, structured outputs | background, Files, batch, flex/priority | – |

What this means for the design:
- The common denominator is far too small. The generic interface must be **lossless per provider**: opaque items
  (signatures, encrypted reasoning, compaction blocks, server tool results) are kept and replayed byte-exact to
  the same provider, and dropped or converted when switching providers.
- Three providers have server-side conversation state, and OpenAI's is fastest over a held WebSocket. History
  stays ours (replayable, ZDR-friendly); provider state is a **cache** we use when it is still valid.
- Steering, async tools, deferred tools and compaction are native on some providers and must be **emulated** on
  the rest, behind the same harness feature.

---

## 3. Architecture

```
            GPUI agent block ─┐                    ┌──────────── otto server (daemon) ────────────┐
            otto CLI ─────────┼─ unix / tunnel ────┤ sessions · turn engine · event log (seq)    │
            other harnesses ──┘   (silo framing)   │ approvals · subagents · scheduler            │
                                                   │   ├─ otto-llm: anthropic/openai/google/…    │──► provider APIs
                                                   │   └─ otto-tools: builtin · silo · mcp · ext │
                                                   └───────────────┬──────────────────────────────┘
                                                                   │ silo control (role: agent, scoped grant)
                                                   ┌───────────────▼──────────────┐
                                                   │ silo server: tools.list/call │──► app (CEF browser actions)
                                                   └──────────────────────────────┘
   silo mcp (stdio) ── same silo toolset for Claude Code, Codex, … ─────────┘
```

### 3.1 Crates

| Crate | Role | Depends on silo? |
|---|---|---|
| `otto-llm` | Provider clients + generic model interface, catalog, auth | no |
| `otto-tools` | `Tool` trait, schemas, effects, results, tool sources (builtin, MCP, ext) | no |
| `otto-core` | Agent runtime: sessions, turn engine, compaction, subagents, approvals, event log | no |
| `otto-store` | SQLite event log, projections, tool journal | no |
| `otto-proto` | Wire types (wasm-safe: no tokio/rustix, the P7-8 lesson), `protocol.json` pin | no |
| `otto-server` | Daemon: transport, auth, methods, event fan-out | `silo-transport` only |
| `otto-silo-tools` | `ToolSource` over a silo control connection | `silo-client` |
| `otto-client` | Connection + session mirror reducer for the GPUI block and CLI | no |
| `otto-cli` | The `otto-silo` binary: server + CLI in one, like `silo`; also a library (`otto_cli::main_from`) embedded in `silo` as the hidden `silo otto …` | – |

The harness library stays usable without silo, and silo integration lives in one adapter crate. The silo side
gets `silo-proto/src/tools.rs` plus `tools.*` methods (§6).

**Naming for this first version:** the crates live in this workspace under `crates/otto-*`. Only the user-visible
names differ from the final ones, so nothing clashes with the TS `otto` already installed on users' machines:

| Thing | Now | Later |
|---|---|---|
| Binary | `otto-silo` | `otto` |
| State dir | `~/Library/Application Support/otto-silo`, `$XDG_STATE_HOME/otto-silo`; override `OTTO_SILO_HOME` | `otto` |
| User config | `$XDG_CONFIG_HOME/otto-silo`, else `~/.config/otto-silo` (agents, skills, commands, hooks, `mcp.json`, `rules.json`, `AGENTS.md`); override `OTTO_SILO_CONFIG_DIR`. Its own, not shared with the TS otto's `~/.config/otto` | `otto` |
| Project dir | `.otto/` read-only for agents, skills and commands (shared with the TS otto); otto-silo state in `.otto-silo/` | `.otto/` |
| Socket | `<state>/server.sock` | – |

The rename is one switch: the binary name, the path constants in `otto-proto` and the env var prefix.

---

## 4. `otto-llm`: providers

### 4.1 Two layers

1. **Native clients** (`otto_llm::anthropic`, `::openai`, `::google`, `::xai`, `::compat`): typed request/response/stream
   types that mirror each API in full, including betas behind typed flags. These can be used directly and are
   where "supports everything the API supports" lives.
2. **`Model` trait**: one generic interface the harness calls, implemented by each provider on top of its native
   client.

```rust
#[async_trait]
pub trait Model: Send + Sync {
    fn info(&self) -> &ModelInfo;                       // id, provider, limits, capabilities, pricing
    async fn stream(&self, req: Request, cx: CallCx) -> Result<EventStream>;
    async fn count_tokens(&self, req: &Request) -> Result<Option<u64>>;
    fn session(&self) -> Option<&dyn ModelSession> { None } // WS / previous_response_id state, steering
}
```

### 4.2 Canonical transcript

```rust
pub struct Item { pub id: ItemId, pub role: Role, pub parts: Vec<Part>, pub origin: Option<Origin> }
pub enum Part {
    Text { text: String, citations: Vec<Citation> },
    Image(Media), Document(Media), Audio(Media),
    Reasoning { summary: Option<String>, text: Option<String>, opaque: Option<Opaque> },
    ToolCall { call_id: String, name: String, input: Value, kind: CallKind /* function|custom|hosted */, opaque: Option<Opaque> },
    ToolResult { call_id: String, content: Vec<ResultPart>, is_error: bool },
    HostedResult { call_id: String, tool: String, payload: Value },   // web_search, code_execution, …
    Compaction { summary: Option<String>, opaque: Option<Opaque> },
    System { text: String },                                          // mid-conversation instructions
    ToolsChanged { added: Vec<String>, removed: Vec<String> },
}
pub struct Opaque { pub provider: ProviderId, pub model: String, pub data: Value } // signatures, encrypted_content, thought signatures
```

- `Origin` records which provider/model produced an item. The request builder replays `Opaque` only when the
  target matches; otherwise it converts (reasoning becomes plain text or is dropped, hosted results become text).
- One stream event enum for all providers: `ItemStart`, `TextDelta`, `ReasoningDelta`, `ToolInputDelta`,
  `ToolCall` (complete, ready to run), `HostedProgress`, `Usage`, `Finish{reason}`, `Error{class, retry_after}`.
  Providers own their quirks, e.g. Gemini thought signatures and OpenAI strict-schema normalization.

### 4.3 Request options

- Common: model, system, items, tools, tool_choice, parallel, max_output, temperature/top_p, stop,
  `reasoning: Effort { none, minimal, low, medium, high, xhigh, max } | Budget(tokens)`, structured output schema,
  cache hints, metadata/user id.
- Per provider: `ProviderOptions::{Anthropic(..), OpenAi(..), Google(..)}` as typed structs, plus
  `extra: Value` merged last as a raw escape hatch.

### 4.4 Capabilities the harness branches on

`ModelInfo.caps`: `reasoning`, `reasoning_replay`, `vision`, `pdf`, `audio_in`, `parallel_tools`, `strict_tools`,
`custom_tools`, `async_tools`, `hosted_tools[]`, `deferred_tools`, `inline_tool_changes`, `mid_system`, `steering`,
`native_compaction`, `server_state`, `prompt_cache`, `context_window`, `max_output`, `tool_family`
(`anthropic`, `openai`, `gemini`, `generic`: the trained edit and shell shapes).

### 4.5 Provider set

| Provider | API | Phase |
|---|---|---|
| Anthropic | Messages (SSE); caching, thinking, compaction, inline tools, server tools, Files | O0 |
| OpenAI | Responses (SSE, then WebSocket with `stream_id`, steering, async tools, compaction) | O0 / O3 |
| Google | Interactions API (`POST /v1beta/interactions`, SSE, `store=false`) | O3 |
| Google Antigravity (subscription) | Cloud Code Assist `v1internal:streamGenerateContent?alt=sse` (`google/codeassist`): agent envelope, daily → sandbox → prod hosts, project via `loadCodeAssist`/`onboardUser`, models via `fetchAvailableModels`, usage via `retrieveUserQuotaSummary` (`Model::plan_usage`, once a minute); provider `antigravity`, subscription only, default `gemini-3.1-pro-low` | Phase 2 |
| Claude subscription (Pro/Max) | Same Messages client, OAuth bearer auth plus the request shaping the subscription endpoint expects | O0 |
| ChatGPT subscription (Codex) | Same Responses client against `chatgpt.com/backend-api/codex/responses` (HTTP and `wss://`) | O0 (HTTP), O3 (WS) |
| xAI | Same Responses client, `https://api.x.ai/v1`; Grok Build subscription (`openai/xai.rs`, `xai::SPEC`) with `grok-build`/`grok-composer-2.5-fast` through the Grok CLI proxy | O3 / Phase 2 |
| Compat family | Chat Completions dialect table: OpenRouter, DeepSeek, Kimi, Z.ai, MiniMax (Anthropic dialect), Ollama, LM Studio, custom base URLs | O3 |
| Bedrock / Vertex / Azure | auth + endpoint variants of the above | later |

### 4.6 Transport and auth

- `reqwest` + rustls, an SSE parser, `tokio-websockets` (already in the tree via iroh) for OpenAI WS. Retries with jitter, honoring
  `retry-after` and rate-limit headers, an idle timeout between events (not the total time), overload handling, and
  cancellation that closes the stream.
- Credentials: env vars, sealed store (`silo-store`-style), macOS Keychain. OAuth: device and loopback flows, a
  refresh coordinator with a file lock (a TS lesson: concurrent refresh rotation), request-scoped credentials.
- Sign-in specs (`auth/flows.rs`): one `SubscriptionSpec` per provider in `SPECS` (client, URLs, scopes, params,
  nonce, callbacks `Loopback{host, Fixed|Any, path}` / `Paste{redirect}`, `Device::Rfc8628` or a custom device
  flow, per-call headers from the state dir, refresh encoding/rotation/rejection, credential mapping, `finish`
  hook). `providers.rs` login, refresh, plan labels and login modes read the spec. A loopback login also completes
  with a pasted callback URL or `code#state` (`LoginResult.accepts_code`). `auth.login {paste: true}` starts the
  spec's `Paste` callback instead (the remote/SSH flow): `Pending` with `needs_code`, finished by
  `auth.login_complete` with the pasted code or URL; the CLI picks it when no browser is available.
- Google Antigravity (`google/codeassist/auth.rs`): Google PKCE with the Antigravity CLI's installed-app client
  (its public secret as embedded in pi-antigravity), `access_type=offline`, `prompt=consent`, scopes `cloud-platform`,
  `userinfo.email`, `userinfo.profile`, `cclog`, `experimentsandconfigs`, `aicode`; loopback
  `http://localhost:51121/oauth-callback` or paste via `https://antigravity.google/oauth-callback` (bare code
  accepted). The `finish` hook runs `loadCodeAssist`/`onboardUser` and stores the project as `extra.project`;
  refreshes keep it, and the model, usage and model-list calls reuse it. Refresh: `invalid_grant` = sign in again.
  The CLI shows the terms warning with a y/N confirm before starting.
- **Subscriptions are an auth mode of a provider, not a separate provider.** `Auth::{ApiKey, Subscription}`
  picks the endpoint, headers and request shaping inside the provider crate. The model list, tools and transcript
  stay the same. Every subscription-specific rule (required instructions, header set, Codex prompt mode, no-strict
  tool default, the TS text guard) lives in one module per provider, so it can be fixed or removed in one place
  (decision 2). The TS flows are the reference for behavior: `packages/sdk/src/auth/src/{oauth,openai-oauth}.ts`,
  `packages/sdk/src/providers/src/{anthropic-oauth-client,openai-oauth-client,openai-oauth-websocket}.ts`.
  The Claude subscription takes otto's snake_case tool names as they are (checked live 2026-10-23 on
  `claude-sonnet-5-5`: `shell`/`read` accepted, 200 with a `shell` call, also with a history call to a `Bash` not in
  the list), so its shaping is only the `SYSTEM_PREFIX` block and the headers; every provider gets the same names.
- Catalog: generated at build from a models.dev-style source plus a manual overlay, refreshed at runtime with a
  cache. Pricing feeds usage and cost events.
- Remote model lists (`catalog/remote.rs`): subscription providers' own lists (Kimi Code `/models`, Antigravity
  `fetchAvailableModels`) are saved to `<state>/catalog/remote-<provider>.json` and refreshed after login and when
  older than a jittered 4–6 h. Catalog metadata wins on matching ids, list-only ids use the list's fields, and with no
  list yet the overlay's `subscription_ids` stand in. `Providers` holds the store for `providers::REMOTE`
  (`kimi-code`, `antigravity`): `catalog_entries`, `model_info` and `resolve_model` read it; `complete_login`
  refreshes the provider's list after a subscription sign-in; otto-core asks `ModelResolver::refresh_models` at start
  and hourly (`Providers::refresh_remote`, only signed-in providers whose list is due) and announces a change as
  `auth.changed`.
- Tests: **recorded stream fixtures** (raw SSE/WS bytes in, canonical events out) per provider feature, with
  `OTTO_RECORD=1` to refresh them against live APIs.

### 4.7 OpenAI WebSocket mode and steering (checked 2026-10-22)

**What the API offers.**
- Public API (API key): `wss://api.openai.com/v1/responses`, `Authorization: Bearer`. Client events are
  `response.create` (the HTTP body without `stream`/`background`, plus an optional WS-only `stream_id`) and
  `response.steer`. Server events are the usual Responses stream events plus `error` (with `status`, and
  `stream_id` on named lanes) and `response.steer.accepted|pending|failed`. Limits: 60 minutes per connection,
  16 in-flight responses and 32 named lanes per connection, FIFO within a lane. `previous_response_id`
  continues from a connection-local cache; with `store=false` an uncached id fails with
  `previous_response_not_found` (no persisted fallback).
  ([WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode),
  [events reference](https://developers.openai.com/api/reference/resources/responses/websocket-events))
- **Steering:** `{"type":"response.steer","previous_response_id":<active response>,"input":...}` after that
  response's `response.created`. Only user messages with text/image/file content. GPT-6 family only ("GPT-5.6 and
  earlier models do not support steering"); not for responses bound to a `conversation` or using automatic
  compaction. `accepted` only queues the input; the commit point is the successor's `response.created`. The active
  response may end with `response.incomplete` (`reason: "steered"`), or complete normally, and is followed by an
  automatic successor that inherits its settings. If it stops for client tool output, `response.steer.pending`
  follows `response.completed` and the input stays queued *on that connection* until a `response.create` with
  `previous_response_id` and the tool results. `failed` returns the input (`response_not_found`,
  `steering_not_supported`, `too_many_pending_steers`, `response_already_completed`, …).
  ([Mid-turn steering](https://developers.openai.com/api/docs/guides/steering))
- **ChatGPT/Codex subscription** (`chatgpt.com/backend-api/codex/responses`): the Codex CLI does use WebSockets
  there (`wss://…/codex/responses`, header `OpenAI-Beta: responses_websockets=2026-02-06`,
  `codex-rs/core/src/client.rs`, `codex-api/src/endpoint/responses_websocket.rs` in
  [openai/codex](https://github.com/openai/codex)), but only sends `response.create`. Its steering
  (`start_or_steer_turn`) is client-side, at the next sampling request. No documentation says the backend
  accepts `response.steer`. The documented "Sign in with ChatGPT" flow
  ([preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations))
  targets `api.openai.com/v1` instead, not the endpoint otto uses.

**What otto does.**
- `caps.steering` is true only for GPT-6 family ids with API-key auth (`openai::supports_steering`, catalog).
  Those models and every ChatGPT/Codex subscription model stream over WebSocket (Codex: `wss://…/codex/responses`
  with the subscription headers plus `OpenAI-Beta: responses_websockets=2026-02-06`, no steering). Other API-key
  models stay on HTTP + SSE with step-boundary steering.
- One connection per session (a model is resolved per session), on the default lane; named lanes are not used
  (one session never runs two responses at once). It is parked after a clean call and reused, reconnecting once
  if a parked connection turns out closed, and it is dropped on cancel, error or a `steer.pending`.
- **Continuation** (the Codex CLI's `get_incremental_items` rule): the parked connection remembers its last
  response id, the request without `input`, and that input plus the response's output as we would resend it.
  The next call sends `previous_response_id` and only the new items when every other field is equal and the
  input extends that baseline; otherwise, after steering, after a reconnect, or on `previous_response_not_found`
  it sends the full input (`store=false` stays; our log is the truth).
- `ModelSession::steer` hands input to the live stream, which sends `response.steer` once the response id is
  known. On the successor's `response.created` the stream emits `StreamEvent::Steered{ids}`. The turn engine then
  closes the current assistant item, records the steered user item, and opens a new assistant item, so the log
  reads exactly as the model saw it. Input that is not committed (failed, pending behind tool results, or sent
  after the response ended) goes back to the step-boundary queue. Pending input is delivered by us with the tool
  results, and the connection that held it is discarded so it cannot be applied twice.

### 4.8 O3 provider depth and breadth (checked 2026-10-22)

**OpenAI.**
- Compaction: `context_management: [{type:"compaction", compact_threshold}]` on create emits an opaque
  `compaction` output item (replayed as-is); `POST /responses/compact` returns a compacted window (`output`) that
  replaces the input and starts a new chain. Not compatible with steering. Exposed as `caps.native_compaction`
  (API key) and `Request.compaction` (`Auto{threshold}` → `context_management`, `Now` → `/responses/compact`, the
  window becomes one `Part::Compaction` with `{type:"compacted_window", output}`).
  ([Compaction](https://developers.openai.com/api/docs/guides/compaction),
  [compact](https://developers.openai.com/api/reference/resources/responses/methods/compact))
- Async tools: `async: true` on function/custom tools, calls carry `async: true`, results arrive in a later request
  on the original `call_id`; GPT-6 Astra and later. `caps.async_tools` + `openai::Options.async_tools`; async call
  items are kept raw so the flag replays. ([Async tool calling](https://developers.openai.com/api/docs/guides/async-tool-calling))
- Native client tools: `{type:"apply_patch"}` → `apply_patch_call{call_id, operation}` /
  `apply_patch_call_output{status, output}`; `{type:"shell", environment:{type:"local"}}` → `shell_call{action}` /
  `shell_call_output{output:[{stdout, stderr, outcome}]}`. Calls become ordinary tool calls (name `apply_patch` /
  `shell`); a shell result that is the documented JSON array is sent as-is, plain text is wrapped.
  ([Apply patch](https://developers.openai.com/api/docs/guides/tools-apply-patch),
  [Shell](https://developers.openai.com/api/docs/guides/tools-shell))

**Anthropic.**
- Compaction on demand: beta `compact-2026-09-04`, `compaction:{type:"summarize", instructions?}`, the response is
  one signed `compaction` block (`stop_reason:"compaction"`, whole in `content_block_start`) that goes first in
  later requests, with the beta on each; no `stop_sequences`, `output_config.format` or forced `tool_choice`, and
  not together with `context_management`. Threshold: beta `compact-2026-01-12`,
  `context_management.edits:[{type:"compact_20260112", trigger:{type:"input_tokens", value≥50000}}]`, summary in
  one `compaction_delta`. Billed usage is the sum of `usage.iterations`. Context editing:
  `clear_tool_uses_20250919` / `clear_thinking_20251015` with beta `context-management-2025-06-27`.
  ([Compaction](https://platform.claude.com/docs/en/build-with-claude/compaction),
  [on demand](https://platform.claude.com/docs/en/build-with-claude/compaction-on-demand),
  [threshold](https://platform.claude.com/docs/en/build-with-claude/compaction-threshold),
  [context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing))
  otto: `Request.compaction` maps to both, betas follow the body (`anthropic::convert::betas_for`),
  `caps.native_compaction` per the docs' model list. Otto's summarizer stays the default.
- Server tools (`web_search_*`, `web_fetch_*`, `code_execution_20250825`; no beta needed): `server_tool_use` and
  `*_tool_result` blocks become hosted call/result parts and replay byte-exact (encrypted content included).
  Anthropic's predefined client tools (`bash_20250124`, `text_editor_*`) are not used: their fixed schemas lack
  our parameters (`background`, `cwd`, `timeout_ms`) and the model never sees our descriptions.

**Breadth.**
- Gemini: the Interactions API is GA and the recommended API (`generateContent` is legacy), so it is used.
  `x-goog-api-key`, `store:false` with full history as steps (`user_input`, `model_output`, `thought` with
  `signature`, `function_call`, `function_result{call_id, name, result}`, server tool steps), SSE `step.start /
  step.delta / step.stop`, `interaction.completed` with `requires_action` and `total_*_tokens` usage.
  ([Interactions](https://ai.google.dev/gemini-api/docs/interactions-overview),
  [streaming](https://ai.google.dev/gemini-api/docs/streaming), [reference](https://ai.google.dev/api/interactions-api))
- xAI: wire-compatible Responses (`https://api.x.ai/v1/responses`, Bearer `XAI_API_KEY`, efforts
  low/medium/high/xhigh, encrypted reasoning), so the OpenAI client runs it with provider id `xai` (opaque items
  never cross providers). Grok Build sign-in (`openai::xai`, as the TS otto does it; login wiring pending):
  `xai::model` routes grok-4.x to `api.x.ai` with the subscription bearer and `grok-build`/`grok-composer-2.5-fast`
  to `https://cli-chat-proxy.grok.com/v1` with the Grok CLI fingerprint (`x-xai-token-auth`, `x-grok-client-*`,
  `user-agent: grok-shell/0.2.22 (os; arch)`); the session is the `prompt_cache_key`. `OpenAiModel`'s ChatGPT
  branches (Codex URL, headers, body shape, WebSocket) apply only to provider `openai`. Usage: `fetch_xai_usage`
  reads `/v1/billing?format=credits` (`creditUsagePercent`, reset at the period end) as one `Credits` window.
  `xai::SPEC` is the login spec (device flow `referrer=grok-build`, or PKCE with a nonce on `127.0.0.1:56121`). ([Responses](https://docs.x.ai/developers/rest-api-reference/inference/responses),
  [Reasoning](https://docs.x.ai/developers/model-capabilities/text/reasoning))
- Compat (`otto_llm::compat`, one Chat Completions parser, a dialect table): OpenRouter (`reasoning:{effort}`,
  `reasoning_details` replay, usage chunk repeats the finish), DeepSeek (`thinking` + `reasoning_effort`;
  `reasoning_content` must be passed back with tools), Kimi and Z.ai (`reasoning_content` replay, Z.ai
  `clear_thinking:false` + `tool_stream`), Ollama / LM Studio / custom (key optional, models from `/api/tags` or
  `/v1/models` with a 1.5 s timeout), MiniMax on the Anthropic client (`api.minimax.io/anthropic`). Usage maps
  `cached_tokens`, `prompt_cache_hit_tokens`, choice-level usage (Kimi) and `cache_write_tokens`.
  ([DeepSeek thinking](https://api-docs.deepseek.com/guides/thinking_mode),
  [OpenRouter streaming](https://openrouter.ai/docs/api/reference/streaming),
  [OpenRouter reasoning](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens),
  [Z.ai thinking](https://docs.z.ai/guides/capabilities/thinking-mode),
  [Kimi thinking](https://platform.moonshot.ai/docs/guide/use-kimi-k2-thinking-model))
- Kimi Code (`compat::kimi`, provider and dialect `kimi-code`, subscription only): Chat at
  `https://api.kimi.ai/coding/v1` served by `KimiCodeModel`, not `CompatModel`. `thinking:{type}` plus
  `reasoning_effort` from the model's own `think_efforts` (nearest listed; none when it lists none; no `thinking`
  for non-reasoning models; never `disabled` for always-thinking models such as k3.1-pro, which get their lowest
  effort), `temperature: 1` always, `prompt_cache_key` = `cache.key` else the session id, `max_completion_tokens`,
  `reasoning_content` replay (empty on a tool-call turn that had none while thinking is on), tool schemas cut down
  to Kimi's dialect. Every call carries `kimi_headers(state_dir)`: `X-Msh-{Platform: kimi_cli, Version, Device-Name,
  Device-Model, Os-Version, Device-Id}` and `User-Agent: KimiCLI/<ver>`, the id persisted in
  `<state>/kimi-device-id`. A 401 on an OAuth credential refreshes once and retries. `fetch_kimi_models` (`GET
  /models`) and `fetch_kimi_usage` (`GET /usages` → `PlanUsage`: limits by window, `5h`; `usage` as `Weekly` when
  it resets within 7 days, else `Monthly`). Sign-in is `kimi::SPEC` (device flow at `auth.kimi.com`, kimi-cli's
  client id and headers, rotating refresh tokens, 401/403 = rejected); models come from the overlay's `kimi-code`
  entry (a provider models.dev does not list, its `subscription_ids` the fallback) until the account's own list is
  fetched.
- Catalog: models.dev for all of the above except Ollama/custom (local discovery only); base URLs for local and
  custom providers live in `<state>/providers.json` or `OLLAMA_HOST` / `LMSTUDIO_BASE_URL` /
  `OTTO_SILO_CUSTOM_BASE_URL`. `Usage::cache_hit_rate` is comparable across providers
  (`tests/cross_provider.rs` runs one scripted tool task through every provider offline).

**Harness hooks (otto-core, done):** opted-in sessions send `Request.compaction` (`Auto` at 75% of the window on
ordinary requests, `Now` for manual/overflow compaction; a `compaction` finish re-sends the turn's prompt after the
checkpoint and continues the turn); every provider gets otto's own tools: the spec's name, description and JSON
schema as a function, or a family presentation of ours (GPT: `apply_patch` as the freeform Codex patch tool, with
`edit`/`multiedit` for every other family); no provider-predefined tool types; tools with
`CallMode::Async` (`web_fetch`, MCP tools) go into `openai::Options.async_tools` when `caps.async_tools`.

### 4.9 Plan usage (subscription limits, built 2026-10-24)

How much of a subscription's limits is used, for the agent block's footer ring (AGENT_BLOCK.md §4b).
`otto_llm::plan_usage` parses it with pure functions into `PlanUsage { windows: [PlanWindow { name, label, used
(0–1), resets_at (unix s)? }] }`, emitted as one `StreamEvent::PlanUsage` per header set or event:
- **Claude (OAuth):** every `anthropic-ratelimit-unified-{window}-utilization` with its `-reset`, generically
  (`5h`, `7d`, per-model `7d_opus`, …). A value above 1 reads as a percent; resets are unix seconds (ms when that
  large) or RFC 3339. Malformed values drop the window.
- **ChatGPT/Codex:** headers `x-codex-{window}-used-percent` (a percent), `-window-minutes`, `-reset-at` or
  `-reset-after-seconds`, for `primary`/`secondary` (or any prefix); and the `codex.rate_limits` event
  (`rate_limits.{primary,secondary}.{used_percent,window_minutes,reset_at|reset_after_seconds}`) on the stream.
  Over WebSocket the upgrade response's headers are reported by the connection's first call.
- **API keys and local models:** neither family appears, so nothing.
- Labels from the window length: 300 min `5-hour`, 10080 `Weekly`, else `{n}d` / `{n}h`; a per-model suffix
  follows (`Weekly Opus`); unknown lengths use the window's name.
- Evidence: Claude live (2026-10-24, subscription): `-unified-5h-utilization` (a fraction), `-5h-reset`, `-5h-status`
  plus window-less `-unified-{status,reset,representative-claim,fallback,fallback-percentage,overage-status,
  overage-disabled-reason}`; no `7d` header on that response, so only `5-hour` was recorded. Codex names follow the
  Codex CLI and are unchecked (no ChatGPT plan to test with).
- Only responses to a subscription (OAuth) credential are parsed; API-key responses never report plan usage.
- Polled usage: after each response otto-core asks `Model::plan_usage` off the turn's path; Kimi Code answers from
  `GET /usages` at most once a minute. The result is recorded like a response's report.

otto-core keeps the latest per **provider** (merged by window name) in memory and in `<state>/plan_usage.json`
(read at start). Each report is announced as the global, unjournaled event `plan.usage {provider, usage}`;
`session.view`, `session.list`, `session.create`/`fork`/`update` fill `SessionInfo.plan` for the session's
provider; `plan.usage {provider?}` returns `[{provider, usage}]` so a client can show it before a session's first
reply.

---

## 5. `otto-tools`: tool model

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError>;
}
pub struct ToolSpec {
    pub name: String, pub description: String,
    pub input_schema: Value, pub output_schema: Option<Value>,
    pub effects: Effects,                 // read, write, process, network, secrets, external
    pub sensitive: Vec<JsonPointer>,      // redacted in log, events, approvals
    pub loading: Loading,                 // Eager | Deferred (tool search)
    pub mode: CallMode,                   // Sync | Async (long-running: returns a handle, result arrives later)
    pub presentations: Vec<Presentation>, // per tool_family shape: e.g. edit as apply_patch vs str_replace
    pub source: Source,                   // Builtin | Silo | Mcp{server} | Ext{plugin}
}
```

- `ToolCx`: cwd, project root, cancellation, progress sink (`tool.progress` events), approval handle, secrets,
  and an `Arc` to the session for subagent tools.
- `ToolOutput`: text, JSON, images, and file references, with truncation metadata. Large outputs spill to a file
  and the model gets a head/tail plus the path.
- **Own tools, every provider**: each model gets our tools as we define them (name, description, JSON schema), never a
  provider-predefined tool type (Claude `bash_*`/`text_editor_*`, OpenAI `shell`/`apply_patch`), whose fixed
  schemas would drop our parameters and descriptions. The only per-family differences are our own: GPT gets
  `apply_patch` as a freeform tool (`Presentation.freeform`, the Codex patch grammar) instead of `edit`/`multiedit`;
  the registry picks the set from `tool_family`. Everyone gets the full `shell` (`command`, `cwd`, `timeout_ms`,
  `background`). Paths are relative to the working directory, and the prompt and `shell` say not to `cd` into it.
  History from before this (calls named `bash` / `str_replace_based_edit_tool`, or OpenAI `shell_call` /
  `apply_patch_call` items) is read as calls to ours (`otto-core/src/legacy.rs`) for requests and session views
  (Claude's `undo_edit` as our `undo_edit`). A `shell_call` / `local_shell_call` a Responses model still emits
  becomes one `shell` call per command (`openai/stream.rs`: the first keeps the call id and the raw item, the rest
  are `{call_id}_{n}`), each its own transcript entry; `convert.rs` answers the call with one `shell_call_output`
  listing every command's output (a `local_shell_call` with a `function_call_output`); other providers get plain
  `shell` calls.
- **Sources**: builtin, silo (§6, `Runtime::add_tools`), project sources (`ProjectSources`, `Runtime::add_source`)
  and extensions later (subprocess JSON protocol like the TS native host; WASM later). Project sources are listed
  again before every model request, so a changed list reaches the next step and every provider gets the full
  current list.
- **MCP** (`otto-mcp`): a hand-written JSON-RPC client (protocol `2025-06-18`) for stdio and streamable HTTP servers
  (POST with JSON or SSE replies, `Mcp-Session-Id`; no GET stream, no OAuth: put tokens in `headers` with
  `${VAR}`). `rmcp` 3.x exists but needs crates this offline build cannot fetch. Config is Claude Code's
  `{"mcpServers": …}`, merged by name from user `mcp.json`, project `.mcp.json` and `.otto/mcp.json`, with
  `${VAR}`/`${VAR:-default}` expansion, `disabled`, `deferred`, `timeout`, `startupTimeout`. Servers start on the
  first request of a session in their project, relist on `tools/list_changed`, restart after a crash (3 starts a
  minute, 30 s backoff after a failed start), get `notifications/cancelled` on cancel, and stop with the runtime.
  Tools are `mcp__<server>__<tool>` (64 chars max), granted by the `mcp__*` pattern, `external` (+`write` unless
  `destructiveHint: false`) unless `readOnlyHint`, deferred when a server has more than 10. Schemas are sanitized:
  top-level object, combinators folded, `required` limited to properties, dangling `$ref`s dropped, `items` on
  every array.
- **Deferred tools**: portable `load_tools` takes names (a trailing `*` loads a prefix, e.g. `mcp__github__*`) or a
  query: every word must match the name or description, name hits rank first, 12 at most. Only granted,
  not-yet-loaded tools appear in its index (60 lines, then a count). Schemas enter the next model request. Loaded
  names persist in session metadata (`loaded_tools`), independently for each child. Direct calls to granted
  unloaded tools still run, and `tools.list` includes deferred tools. Native tool search with `defer_loading` is
  not used yet.
- **Shown or loadable is per agent.** An agent's `tools` are shown to the model and its `loadable` sit behind
  `load_tools` (names and `prefix*` patterns; the longest matching pattern decides, a tie goes to shown); a tool
  in neither is not granted. With both empty (the built-in `build`) every tool of the family is granted with
  its own default (`Loading`). Session meta `tools` is the whole grant, `loadable` the deferred part (absent:
  each tool's default).
- **Async tools**: native `async: true` on OpenAI. Elsewhere the call returns a job handle; the result is injected
  later as a system item, plus a `wait` tool.

### 5.1 Built-in tools (v1)

`read`, `write`, `edit`/`multiedit` (str_replace family), `apply_patch` (patch family), `ls`, `tree`, `glob`,
`search` (ripgrep's `grep` + `ignore` crates), `shell` (one-shot, pipes, timeout, background jobs), `web_fetch`,
`update_todos`, `ask_user`, `skill`, `load_tools`, `subagent` (delegate/list/message/stop/retry), `wait`; deferred
(through `load_tools`): `copy_attachment`, `web_search` (§6.3; provider-hosted search stays where it exists),
`undo_edit {path}` (takes back the session's last `edit`/`multiedit`/`write`/`apply_patch` change to that file, one
per call, newest first; an in-memory stack per session of 64 changes; granted with the edit tools; refused when the
session has nothing left to undo there, or when the file changed after that edit).

- **New tools are deferred.** Any tool added from here on is `Loading::Deferred` (in `load_tools`' index with the
  first sentence of its description, loaded by name) unless it is part of the core edit/run loop. Improvements to
  an existing eager tool (`read`) keep it eager.
- **`web_fetch {url, render?, screenshot?, selector?}`** and **`web_search {query, limit?}`**: see §6.3.

- **`read` media**: images (PNG, JPEG, GIF, WebP, TIFF, BMP) are decoded with the `image` crate, turned upright,
  downscaled to `max_edge` (default 1568 px, never upscaled) and recompressed (PNG for transparency or a PNG
  source that fits, else JPEG at `quality`, default 85) under 3.7 MB, so the base64 stays below the 5 MB
  per-image limit; small files pass unchanged. The text says `shot.png 3024×1964 → 1568×1018`. PDFs become a
  document part: sent to models whose catalog caps list `pdf` and providers that take documents (Anthropic,
  OpenAI, Gemini, OpenRouter); everyone else gets a note that it wasn't sent. `~/` and absolute paths work under
  the usual path policy. `attachment_id` reads an attachment instead of `path`.
- **Attachments** (`otto-tools/src/attachments.rs`): `attachment.put {session_id, name, mime, data_base64,
  append_to?}` → `{id, name, mime, size}` stores the original bytes at
  `<state>/attachments/<session>/<id>-<name>` (metadata in `<id>.json`, ids `att_` + 8 hex). Files up to 25 MiB
  (`MAX_ATTACHMENT`), sent in chunks of at most 8 MiB (`ATTACHMENT_CHUNK`, under `MAX_FRAME`): later chunks pass
  the id as `append_to`. A prompt names them with `InputPart::Attachment {id}`; the model gets
  `[attachment att_3f2a9c1b: shot.png, image/png, 812 KB — copy_attachment can save it into the project]`
  (pointing at the deferred tool) and, for images on vision models, the image
  downscaled as above. Forks copy them; archiving deletes them with the spilled outputs (and the 7-day expiry of
  archived sessions covers both). Older `InputPart::Image` prompts stay as they are.
- **`copy_attachment {attachment_id, path, overwrite?}`** (deferred): copies the original bytes into the project (parents
  created, an existing file refused unless `overwrite`), effects `write`, so the path policy and approvals apply.
  It finds the session's attachments, then its ancestors'. Returns the path and size. A grant fixed before it
  existed (sessions created earlier, agents listing `write`) reaches it through `write`.
- **Mentions** (`otto-core/src/step.rs`): `InputPart::Mention {kind: agent|file, target}` stands for `@target` at
  its place in the text (a folder ends in `/`). Text and mentions in a row become one text part; the user item keeps
  `mentions: [{kind, target, range: [start, end] of Item::text, missing?}]`, so replay keeps the chips. `missing`:
  no such agent for the project, or no such path under the session's folder (inside it only). At request time the
  item gets one note after its text, for the found ones only: `Mentioned agents: build. Delegate the matching part
  with subagent {agent:"build"}.` and `Mentioned files: src/main.rs, docs/ (paths relative to the project; read them
  as needed).` Contents are never inlined. A native steer sends the note with it.
- **`files.complete {cwd, query, limit = 50}`** → `[{path, dir}]` (`otto-tools/src/complete.rs`): files and
  folders under `cwd` for an `@` popup, `.gitignore` respected (`ignore`, as `glob`/`search`), `.git` skipped, at
  most 20k entries walked, limit capped at 200. Fuzzy subsequence on the relative path, a basename match first,
  shallower and shorter paths ahead; an empty query lists shallow entries. A query naming an existing folder
  (`src/ma`) matches the rest inside it. Served by otto so it works where the agent runs.

### 5.2 Policy

- Approval modes: `auto` (**default**, "yolo": nothing prompts; deny rules still apply), `ask-on-effects`
  (write/process/external need approval unless a rule allows it), `ask-all`. Decision 7. There is no plan mode:
  a stored session whose mode is `plan` (or whose agent was the removed `plan` built-in) opens in `auto`.
- Rules: `{tool, arg pattern, decision}`, e.g. `shell: "cargo test*" → allow`, stored per project and per user.
  "Always allow" answers write rules.
- Canonical path resolution (symlinks, `~`, project/reference/outside) runs before guards and before the approval
  is shown, and the tool runs with the same resolved value.
- Sandbox (later): macOS seatbelt profile and Linux landlock/seccomp for `shell`, with network as its own toggle.

---

## 6. Silo toolset (for any harness)

> **Superseded by `docs/AGENTS.md`**: client-side `silo-tools` crate (no `tools.call`), CDP accessibility snapshots
> for browsers. Kept below for history, except §6.2 (parked command blocks for agent commands), which is current
> and which AGENTS.md §3.5 describes as built.

**Idea:** silo publishes a curated, agent-shaped tool registry next to its control API. It does not expose all of
`api.describe`: layout ratios aren't agent tools. Otto consumes it natively; other harnesses get it over MCP.

- `silo-proto/src/tools.rs`: `ToolDescriptor {name, description, input_schema, output_schema, effects, access}`.
  Schemas come from `schemars`, pinned in `protocol.json`, with one example each and a test (like
  `every_method_describes_and_its_example_validates`).
- Control methods: `tools.list {}` → descriptors; `tools.call {name, call_id, input}` → result; events
  `tool.progress {call_id, …}`; `tools.cancel {call_id}`. Access is checked per tool through the Phase 7 need table,
  and owner tools go through the existing approval card.
- `silo mcp`: a stdio MCP server generated from the same registry, for Claude Code, Codex and others.
- Otto (`otto-silo-tools`) connects with `role: agent`, `client_name: "otto · <agent>"`, and optionally a scoped grant per
  session or subagent (`grant.create {kind: token, scope: session|block}`), so a subagent can be limited to its own
  panes.

### 6.1 First tools

| Tool | Built from | Notes |
|---|---|---|
| `terminal.open` | `block.split` / `window.create` + spawn | The agent gets its own pane in the user's session, visible and attachable |
| `terminal.run` | `terminal.send` + wait for OSC 133 D / `COMMAND_END` + `terminal.command_output` | Returns exit, duration and output; async mode for long runs; uses busy detection when there are no marks |
| `terminal.send` / `terminal.read` | `terminal.send`, `terminal.capture` | For interactive programs (REPLs, TUIs); `read` can wait for quiet output or a match |
| `terminal.list` | `session.view`, `blocks.busy`, `block.commands` | What is running where, last commands and exits |
| `browser.open` / `navigate` / `back` / `reload` | existing `browser.*` | State-only today |
| `browser.snapshot` / `click` / `type` / `press` / `scroll` / `wait_for` / `screenshot` / `evaluate` / `console` / `network` | **new**: forwarded to the app that owns the block (`app.request` path), executed in CEF through the DevTools protocol (`execute_dev_tools_method`) | Engine-side actions need an attached app. With none: an error, not a timeout (TS lesson). Headless server-side Chromium later (PLAN §6) |
| `notify` | Phase 7 notifications | Tell the user on every device |

### 6.2 Where agent commands run (decided 2026-10-22)

The rule is "whichever is cheaper, but use silo blocks when they work well". Proposal: **every agent command runs
in its own parked silo command block when silo is connected; pipes are the fallback.** Built in O5 with the routing
the cost gate called for (below): short read-only commands always use pipes.

**Parked blocks (new in silo).** A session gets `parked: Vec<BlockId>`: blocks that live in the session but in no
layout, so nobody sees them by default. Their PTY, server terminal, snapshot, status and access rules are the same
as any other block. They live in the user's current session, next to the agent block that started them (no
separate hidden session).
- `block.park {block_id}` and `block.reveal {block_id, as: float|split}`. Revealing a float reuses `Float`.
  Parking a block in a layout removes it from the layout and keeps it running.
- Their owner is the agent block that started them: the agent block lists them under each tool call. The silo
  switcher shows them per session, with busy and exit status.

**`shell` (one-shot), with silo:**
1. `block.split`-style spawn of a **command block** (`SpawnSpec.command = [$SHELL, -lc, cmd]`, cwd, env), created
   parked. A command block is one process: it exits when the command does, and the exit code is the block's
   `Exited{code}`. That needs no OSC 133 marks and leaks no shell state between calls.
2. Output for the model comes from the **server terminal**, not the raw bytes: `terminal.capture {history: true}`
   formatted as plain text. Carriage-return progress bars and redraws resolve to their final screen, and escape
   codes disappear. That is cleaner than pipes for tools like cargo, npm and docker. Head and tail go to the model;
   the full text goes to a file.
3. Env: `PAGER=cat`, `GIT_PAGER=cat`, `OTTO_SILO=1`. The size is wide (for example 200 columns), so soft wraps are
   rare, and the formatter joins wrapped lines.
4. **Waiting for input** (password prompts, `ssh` host keys, `y/N`): the block is foreground-busy, the tty is in
   canonical mode, and output has been quiet for 2 s. In that state the tool reports `waiting_for_input` with the
   last screen. The agent block shows "needs you" and reveals the block on click; the user types into it, and the
   call completes when the process exits. The agent can also answer with `terminal.send`.
5. Parallel calls are separate blocks. A timeout kills the process group; `background: true` returns a handle
   instead (async tool, §5): for work that finishes and reports later (a test run, a build) while the agent keeps
   working. Its result (exit code and output, as for a one-shot call) is delivered to the session when it exits;
   the agent can also check on it or stop it.
6. **No retention:** a command block lives exactly as long as its command. On exit the server captures the final
   screen and scrollback (before the block goes), otto keeps the plain text (head and tail to the model, the full
   text in its output file, shown in the tool card), and the block closes at once. Nothing is kept or restored
   after a restart. One exception: a block the user has revealed stays on screen as an exited block until they
   close it.

**Lifetimes:**

| | Runs | Lives until | Afterwards |
|---|---|---|---|
| `shell` | one command | it exits | text kept, block closed at once |
| `shell {background: true}` | one finite command (tests, a build) | it exits or is stopped (otto, the user, or the agent session closing) | result delivered to the session, text kept, block closed |
| `terminal.*` | a login shell, REPL, `ssh`, a dev server or watcher… | closed by otto or the user, or its shell exits | a normal block |

Running background blocks can be revealed and typed into. When the agent session closes, its background blocks are
stopped and its parked (hidden) terminals closed; terminals the user revealed, or that otto opened visible, belong
to the layout and stay.

**`shell` without silo** (silo not running, or a remote otto without a silo next to it): pipes, stdin
`/dev/null`, the same env, the same output limits. The model sees the same tool either way.

**`terminal.*`** stays for long-lived and interactive work: dev servers and watchers (processes that never return
a result), REPLs, `ssh`, anything where state carries across calls (cwd, env, a venv) or the user may take over.
Those are normal shell blocks, parked or revealed (`terminal.open {visible}`), reused across calls; the agent reads
their output with `terminal.read`/`wait`.

**Why blocks over pipes when silo is there:** watching and taking over are free, interactive prompts stop being
failures, output comes from the server terminal (redraws resolved, no escape codes), and the agent block can embed
a live mini-terminal in a running tool card by reusing silo's terminal renderer on the block's data stream.

**Cost, measured before it becomes the default (O5 gate):** each call costs a PTY, a process spawn, a server
terminal and a ring. Pipes pay only the spawn. The gate is spawn-to-first-byte and capture overhead on
`git status`, `rg`, and `cargo check` with no-op builds. If a block adds more than about 10 ms at p50, short
read-only commands go through pipes and the rest through blocks.

Measured 2026-10-22 on the dev Mac (`silo-tools/tests/cost.rs`, ignored; throwaway server; `zsh -lc`, the same
env both ways; block = `block.spawn` → `block.changed` exit → `terminal.capture`), p50 in ms (debug build in
brackets). `rg` is not installed here, `git grep` stands in:

| command | pipe | block | overhead |
|---|---|---|---|
| `true` | 4.5 | 12.5 | 8.0 (9.5) |
| `echo hi` | 4.5 | 12.8 | 8.3 (9.0) |
| `git status` | 16.5 | 25.5 | 9.0 (10.7) |
| `git grep` | 18.2 | 26.7 | 8.5 (9.0) |
| `cargo check` (no-op) | 198.0 | 198.1 | 0.1 (2.8) |

Pipe spawn-to-first-byte is 4 ms; `block.spawn` itself takes 6.7 ms (PTY, server terminal, actor thread). A block
adds 8–9 ms (release) to 9–11 ms (debug), at the gate, and doubles or triples short commands. **Routing:** short
read-only commands (`otto_tools::builtin::shell::quick`: `git status|diff|log|show|…`, `ls`, `cat`, `rg`, `grep`,
`find` without actions, `sed` without `-i`, … chained with pipes, `&&` or `;`, no redirection or substitution)
run through pipes even with silo; everything else in command blocks.

**As built:** `block.spawn {block_id: owner, command, cwd, env, size}` starts a parked block (no command: a login
shell); `block.park`, `block.reveal {as: float|split}`; `SessionInfo.parked: [{id, owner}]`; closing the owner closes
its parked blocks; a parked block closes when its program ends, and a command block's final screen and scrollback
answer `terminal.capture` for 5 minutes; `Capture` gained `lifecycle`, `exited` and `line_prompt` (canonical tty,
cursor ending a non-empty line). Blocks an owner parked (`block.spawn`) are dropped on a restart (hot upgrades carry
them); the close guard counts an owner's running parked blocks when it closes. Otto
finds the agent block showing the session (or its nearest ancestor) and owns its command blocks with it; without
one, pipes. A prompt (`line_prompt` and 2 s quiet) returns `waiting_for_input` with the screen, `block_id` and a
`job_id`; the call then continues as a job. Background results and those of answered prompts arrive as system
messages through the outbox (`{notice}` entries from the session to itself); `job` checks or stops them; archiving a
session stops its jobs. `terminal_open {visible: false}` parks a terminal next to the agent's block. A command
block announces itself with a `block:<id>` progress message; closing its block stops the command (`stopped
(closed)`). The agent block shows Open / Stop on such lines, `Needs you` for `waiting_for_input`, and background
cards that finish when their report arrives (docs/AGENT_BLOCK.md §4f).

**Detach (built 2026-10-24):** while a foreground `shell` call runs, `shell.detach {session_id, call_id}` makes it
return now with `Started <job_id>[ in block <id>]: the user detached this command …` (metadata `job_id`, `block_id`,
`background`, `detached`) and the command runs on as a job: in its block, watched as a background command (an
explicit `timeout_ms` keeps what is left of it, the 120 s default no longer applies); through pipes, the command runs
in its own task from the start, so the call only stops waiting (its progress stops; a dropped call, a cancelled turn
or the timeout still kill it before then). A full job table (64) refuses and the call stays in the foreground.
`job.list {session_id}` lists `JobInfo {job_id, command, call_id, block_id?, started_at, completed}`; `job.kill
{session_id, job_id}` stops one (its report says `was stopped (cancelled)`). The `shell` description steers finite
long work — test suites, builds, deploys, `gh run watch`, `gh pr checks --watch`, polling loops — to `background:
true` (do other work or end the turn; the report starts a turn when idle; don't poll with `job` or `sleep`), and
only what never exits (dev servers, file watchers) or is driven interactively to `terminal_open`.

### 6.3 Web: rendered fetch and search (decided 2026-10-23, built)

Plain HTTP `web_fetch` fails on bot protection, JavaScript-built pages and consent walls. Decisions:

- **New tools are loadable, not first class.** Every tool added from here on is `Loading::Deferred` (found through
  `load_tools` by name, prefix or query) unless it is part of the core edit/run loop. `web_fetch` stays eager
  (existing); `web_search` is deferred.
- **`web_fetch` renders when HTTP fails.** It tries plain HTTP first; when the result looks blocked or empty
  (403/429/503 challenge pages, known bot-wall markers, a JS shell with almost no text), it loads the URL in a hidden
  silo browser (CEF), waits for load and network quiet (bounded), extracts the main content as markdown, and closes
  the page. Result: title, final URL, markdown (output limits as usual); optional `screenshot: true` (image of the
  viewport) and `selector` (limit to an element). `render: true|false` forces either path. Without the silo app
  (CLI only, remote otto) it stays HTTP and says rendering needs the app.
- **Agent profile.** Rendered fetches use a persistent `agent` browser profile (cookies, consent remembered), never
  the user's default profile.
- **Headless browsers in the app (closes a gap).** Today a browser block in an unshown tab has no pane and reports
  `no_app`. The app gets a host for off-screen browsers that belong to no pane (windowless CEF already renders off
  screen): browser blocks in unshown tabs work for every `browser_*` tool, and `web_fetch`/`web_search` pages live
  there too, never visible, never in a layout.
- **`web_search {query, limit?}`** works for every provider: it renders a search engine results page in the same
  hidden browser (agent profile) and returns `[{title, url, snippet}]`. Engine: one configurable default, with
  the parser per engine kept small. Provider server-side search (Anthropic `web_search_*`) stays available where
  it exists; otto's own `web_search` is the provider-neutral one.

**As built (silo side, docs/AGENTS.md §3.3, §4.1, §7):** the app's off-screen host (`silo-app/src/browser_host.rs`,
feature `browser_host`): a browser block no window shows runs there for every `browser_*` tool, and a window showing
it adopts the same page (no reload) and hands it back when it stops; the page closes with its block. The persistent
`agent` profile. Silo's deferred `browser_fetch` (`browser.fetch`: load, then network quiet, ≤ 60 s; `selector`,
`screenshot`; 3 pages at once, the rest queued; a challenge that doesn't clear in 6 s returns `blocked`; `no_app`,
`timeout`, `navigation`). Pages no window shows, and tabs a window doesn't show, render at 5 fps until shown. The
`Renderer` contract (`otto_silo_tools::connect(..).renderer`).

**As built (otto side):** both tools reach silo's `browser_fetch` (docs/AGENTS.md) through
`otto_tools::builtin::render::Renderer` (`Runtime::set_renderer`; shared with the engine as `web_fetch::Web`);
without one, or on `no_app`, they behave as without the app. Both are `network` (approvals as before;
`web_search` is also reached by a `web_fetch` grant). HTML helpers are regex-based (`otto-tools/src/html.rs`):
simple CSS selectors, main content (`main`/`[role=main]`, else the largest `article`, else the body without
nav/aside/header/footer; a candidate with under 200 characters and under a third of the body's text loses), markdown.
- **Heuristic** (`web_fetch::needs_render`): blocked when 403/429/503 carries a challenge marker (Cloudflare `Just
  a moment`/`cf-chl`/`challenge-platform`/`Attention Required`, DataDome, PerimeterX, Akamai `Access Denied`,
  captcha, `unusual traffic`, DDoS-Guard), or a page with under 200 characters of text carries an unmistakable
  one; empty when under 200 characters and ≥3 scripts, an empty app root (`#root`, `#app`, `#__next`, …,
  `<app-root>`) or an "enable JavaScript" notice. Non-HTML never renders.
- **`web_fetch`**: `render: true` or `screenshot: true` go straight to the browser; `render: false` never renders.
  Output `Title:`/`URL:`/`Status:` (non-2xx)/`Note:` lines, then markdown; `Note: rendered in browser`, then
  `blocked by a bot check (Cloudflare)` (silo's `blocked`: the challenge didn't clear in 6 s), `not settled by the
  deadline; may be partial`, or why it couldn't render (`bot check; rendering needs the Silo app`, `built by
  JavaScript; …`, `rendering failed: …`). Forced rendering without the app: `Rendering needs the Silo app.`
  A `selector` filters the fetched HTML on the HTTP path and is passed to the browser otherwise.
- **`web_search`** (deferred): engine `search_engine` in config.json (`duckduckgo` default, `bing`, `google`), one
  parser each (`web_search::{duckduckgo, bing, google}`), redirect wrappers unwrapped (DDG `uddg=`, Google
  `/url?q=`, Bing `/ck/a?u=a1…`), ads dropped. Rendered Google pages link through opaque `/goto?url=` tokens:
  `web_search::resolve` reads each redirect's `Location` (no follow, 8 s) and drops hits it can't resolve. Without the app it asks DuckDuckGo's HTML endpoint over plain HTTP
  and, only if that is turned away (202 bot check), says search needs the app. Text list plus `metadata.results`.

---

## 7. `otto-core`: runtime

### 7.1 Model

- **Project** (root, config, instructions from `AGENTS.md`/`CLAUDE.md`), **Session** (agent, model, parent, cwd),
  **Turn** (one user input until the model stops), **Step** (one model call + its tool calls).
- **Agents** as markdown with frontmatter (`name, description, model, tools, loadable, disallowedTools, effort,
  approval_mode`/`permissionMode`) in user `agents/`, `.claude/agents/` and `.otto/agents/` (later wins).
  Claude Code tool names map to otto's (`Read`→`read`, `Bash`→`shell`, …), `model: inherit` keeps the session's
  model, `*` suffixes grant prefixes. The only built-in is `build`. A session started with an agent, or switched
  to one, takes its model (unless one is given), effort and approval mode.
  - `agents.get {name, cwd?}` → `AgentDef {name, description, model, effort, approval_mode, prompt, tools,
    loadable, disallowed, scope: builtin|user|project, overrides_builtin}`; `agents.save {agent, previous_name?,
    cwd?}` writes `<config>/agents/<name>.md` (user) or `<cwd>/.otto/agents/<name>.md` (project) and returns it
    read back; a different `previous_name` removes that scope's old file (rename; clone is a save under a new
    name). Names are 1–64 of `[A-Za-z0-9_-]`. `agents.delete {name, scope, cwd?}` removes the file the scope's
    definition was read from (`.claude/agents` counts as project); deleting an override of `build` resets it,
    the built-in itself can't be deleted. `tools.catalog {cwd?}` lists every grantable tool (the default
    model's family, `skill`, `subagent`, extras, `cwd`'s MCP tools) with its own default loading.
- **Skills**: `SKILL.md` folders (user `skills/`, `.claude/skills`, `.agents/skills`, `.otto/skills`), loaded on
  demand by the `skill` tool with their folder path; names and descriptions only in the prompt;
  `disable-model-invocation` hides one from the model. `skills.list` lists them.
- **Commands**: `/name args` from markdown templates (user `commands/`, `.claude/commands`, `.otto/commands`;
  subfolders give `folder:name`), with `$ARGUMENTS`, `$1`…`$9`, or the arguments appended; `description` and
  `argument-hint` frontmatter; `commands.list` lists them.
- **Hooks**: Claude Code's format from user `hooks.json`, `.claude/settings{,.local}.json` and `.otto/hooks.json`:
  `SessionStart`, `SessionEnd` (archive), `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop`,
  `SubagentStop`, `PreCompact`. Matchers are anchored regexes over the tool name (Claude Code names too), the
  SessionStart source or the PreCompact trigger. JSON stdin with the documented fields; exit 2 blocks with stderr,
  other failures only warn; stdout JSON `continue`/`stopReason`, `decision`/`reason`, `systemMessage`, and
  `hookSpecificOutput` `permissionDecision` (`allow` never overrides policy, `ask` forces approval),
  `updatedInput`, `additionalContext`; plain stdout is context for SessionStart and UserPromptSubmit. A Stop
  block continues the turn (3 times at most). Timeout 60 s default, 600 s max.

### 7.2 Turn engine (single owner per session)

One actor per session owns all state transitions; everything else sends it messages. That replaces the TS
runner's shared mutable flags.

```
Idle ─prompt─▶ Building ─▶ Streaming ─tool calls─▶ Gating(approvals) ─▶ Running tools ─▶ Building …
                    ▲            │                                                     │
                    └─ steer ────┴─ finish(no calls) ─▶ Idle   cancel ─▶ Cancelling ─▶ Idle
```

- Tools in one step run in parallel unless one is marked exclusive (writes to the same path, interactive
  terminal). Results return in call order.
- **Queue vs steer**: `prompt` on a busy session queues by default. `steer` uses `response.steer` where the model
  supports it (OpenAI GPT-6 family with an API key, §4.7) and a response is streaming; otherwise the input lands
  at the next step boundary as a user item, and in-flight tools finish.
  `interrupt` cancels the current step and then continues with the new input. Pending inputs can be withdrawn
  (`session.queue_remove`) or moved between steer, queue and interrupt (`session.queue_update`) until the model has
  them; `queue.changed` lists steers first (`delivery: steer`), then the queue.
- Errors: classified (`overloaded`, `rate_limited`, `context_overflow`, `auth`, `invalid_request`, `network`).
  Retries happen inside the step; context overflow compacts and then retries once; auth asks the client to sign in.
- Output-limit continuation: when the model hits `max_output` in the middle of text, continue automatically
  (bounded).

### 7.3 Context management

- **Stable prefix**: system prompt and tool list fixed per session. Changes (mode switch, new tools, reminders)
  are appended as `System`/`ToolsChanged` items, sent natively as Anthropic mid-conversation system messages and
  inline tools, and as developer/system items elsewhere.
- Caching: automatic Anthropic breakpoints (system, tools, last stable item), OpenAI `prompt_cache_key` per
  session, Gemini implicit caching. Cache hit rates are reported in usage events.
- **Compaction**: otto's checkpoint summarizer by default (bounded, sectioned, instructions reserved, the last items
  verbatim, `through` boundary). Native compaction is opt-in (`native_compaction` config): used when the model has
  `caps.native_compaction` and the history is that provider and model's alone: ordinary requests ask for provider
  threshold compaction at 75% of the window, manual and overflow compaction ask for it now, and the opaque
  checkpoint is replayed. Otherwise otto compacts at 85% of the window and on context overflow; `PreCompact` sees
  `manual`/`auto`.
- **Context editing** (every request): orphaned results dropped, missing results filled with an error, and above a
  budget (a quarter of the window, 32–400 KB) older tool results cleared newest-first, the last 3 always kept, so
  call/result pairs stay valid. Hook context produced while tools run is committed after their results.
- Provider state (`previous_response_id`, WS lane, `previous_interaction_id`) is a cache keyed by
  `(session, provider, model, last item seq)`. On a miss we replay full history; after a fork we open a new lane.

### 7.4 Subagents

- A child is a session with `parent`, `agent`, `task`. `delegate` returns right away; each finished child turn
  (done, error, or interrupted by a restart) goes to the parent's **outbox** as a record versioned by the child's
  turn, written once. The parent is then woken, its actor opened if needed: entries are injected at its next step
  boundary, or a new turn starts when it is idle. The item and its acknowledgement commit in one transaction, so
  each entry is delivered exactly once. `Runtime::deliver_pending` (daemon start) wakes parents whose children
  finished while they were closed. This is the record remote delegation (§12.2) will reuse.
- **A waiting child tells its parent.** When a child's `ask_user` or tool approval starts waiting, an outbox entry
  from the child (version `-seq` of its `question.asked` / `approval.requested`, apart from the turn versions, so
  once per item) wakes the parent with a notice: `Sub-agent "<title>" (child <id>) asks: <question> (options: …).
  Answer with subagent answer {session_id, answer}.` / `… needs approval: <summary>. Only the user can approve it.`
  Its delivery emits `subagent.changed` with `waiting_approval`. `subagent answer {session_id, answer}` answers the
  child's oldest pending question of its turn (`question.answer`; an error when there is none, and for approvals,
  which stay the user's). `status` also lists `pending_questions`. A notice the user answered first still arrives.
- `delegate {task, agent, session_id?}` (reuse), `list`, `status`, `message{queue|steer|interrupt}`, `stop`,
  `retry`, `answer`; `agent` is required (the tool lists the agents; a missing or unknown one is an error naming them);
  depth 2, 4 running children; parent cancel stops its children; the agent's model, else the parent's;
  tool set = the agent's set ∩ the parent's grant, minus both deny lists.
- **Per-subagent grants.** `delegate {…, tools: [names]}` narrows a new child to those of its agent's tools
  (`CreateSession.tools`, kept as `meta.grant` so a model or agent change re-applies it). `create_session` rejects a
  name the agent can't use (built-ins, silo's `terminal_*`/`browser_*`, MCP tools, `skill`, `subagent`) with the list
  of available ones; with a `session_id` (reuse) it is an error. A call to anything else fails with "X is not among the
  tools this sub-agent was given. Available tools: …".
- **User takeover.** A client's `session.prompt` / `session.cancel` / `queue_update` to a child are the user's
  (the parent's tool calls and its cancel cascade are not). User input marks the child (`meta.user`: `steered`, or
  `took_over` once the user stopped or interrupted a running turn); the next report adds one line, `The user steered
  this sub-agent; …` / `The user took over this sub-agent; …`, and clears the mark.
  - A turn the user stopped or interrupted does not report (no `cancelled` result for the parent to act on). It
    holds (`meta.held` = that turn, `SessionInfo.report_held`) for as long as the user keeps it; there is no timer.
    A later child turn that finishes reports instead.
  - **Hand-back:** `session.hand_back {session_id}` (`otto-silo hand-back <id>`) sends the held turn now, as status
    `stopped` (`The user stopped this sub-agent and handed it back.`), with the child's last assistant text, or the
    reply the stop cut off (its streamed deltas). `bad_request` when nothing is held or the child runs again. The
    agent block calls it from `Hand back`; `silo server` calls it when the last agent block showing the child closes.
  - A daemon restart still releases a held turn, so nothing is lost (`…; the otto server restarted before they
    handed it back.`). All of these go through the same versioned outbox record: exactly once, also across restarts.
  - A user message to a finished child starts a new turn, which reports again and wakes the parent as usual.
  - The parent's own `stop` still reports nothing.
- With silo: each subagent can get its own scoped grant and its own panes; the agent block opens a child in its own
  block for the user to steer or take over (docs/AGENT_BLOCK.md §4f).

### 7.5 Persistence (`otto-store`)

- SQLite, one DB per project plus a global one for sessions index/auth metadata. **Append-only `events`** per
  session (`seq`, `turn`, `step`, `kind`, `payload`) is the truth; `items`, `tool_calls` and `usage` are
  projections updated in the same transaction.
- Streaming deltas are coalesced (for example every 50 ms or 4 KiB) into the log. Clients get every delta live
  and the coalesced record on replay.
- Tool journal: `intent` before executing, `outcome` before the result item, both fsynced. On restart, an intent
  without an outcome becomes a `ToolResult{is_error, "outcome unknown"}` and is never re-run.
- Turns interrupted by a daemon restart are marked `interrupted`; the user resumes them explicitly.

---

## 8. `otto-server` and protocol

- Same framing and conventions as silo (§2.1). Control frames are JSON-RPC style; events carry a per-session `seq`
  plus a global `seq` for the session index. Clients do `session.view` (snapshot) and then events from `seq`;
  overflow closes the connection.
- Listen: a unix socket under `<state>/server.sock` (§3.1), with the peer uid as owner. Remote: only through silo
  tunnels (§9).
- Methods (sketch): `server.info`, `api.list/describe`, `project.open/list`, `session.create/list/view/fork/rename/
  archive`, `session.prompt {input, attachments, delivery: queue|steer|interrupt}`, `session.cancel`,
  `session.compact`, `approval.answer`, `question.answer`, `model.list`, `auth.status/login/logout`,
  `tools.list`, `tools.catalog`, `agents.list/get/save/delete`, `skills.list`, `config.get/set`, `subagent.*`.
- For remote clients (additive): the server hello carries `home`, the daemon user's home directory (for shortening
  paths); `project.open {root?, cwd?}` takes a non-empty `root` as is, else resolves `cwd`'s git toplevel on the
  server (falling back to `cwd`), else opens the daemon user's home; a pending `auth.login` reports
  `callback: {port, path}` when the provider redirects the browser to a loopback listener (OpenAI's browser
  login: `{1455, "/auth/callback"}`), so a client on another machine can forward it.
- Events: `session.changed`, `turn.started/finished`, `item.started`, `text.delta`, `reasoning.delta`,
  `tool.input_delta`, `tool.call`, `tool.progress`, `tool.result`, `approval.requested/resolved`,
  `question.asked`, `usage`, `compaction`, `subagent.changed`, `queue.changed`, `error`.
- `SessionInfo.context: {used, window}` (additive): `used` is the last step's prompt as the provider reported it
  (`input + cache_read + cache_write`), `window` the session model's `context_window` (0 when unknown). Kept in
  session meta (`meta.context`): each step's `usage` event writes it and carries it (`usage.context`), a
  `compaction` resets `used` to 0 until the next step, a model change sets `window`. `usage` (the session's
  totals) stays cumulative.
- `api.describe` and `protocol.json` pinning work as in silo; `otto-proto` builds for wasm32.

---

## 9. Silo integration

- **Otto is its own daemon (decision 1)**, started on demand by its CLI or the app, or supervised by silo's
  service manager next to `silo`. Neither needs the other to run.
- **Remote access goes over silo tunnels.** `silo/tunnel/1` connects to TCP host:port pairs only
  (`silo-transport/src/tunnel.rs` `TunnelOpen{host, port}`), and otto listens on a unix socket. Additive change:
  `TunnelOpen` gains `service: Option<String>`. A server-registered name (`"otto"`) maps to a local unix socket
  path, which the otto daemon registers with silo at startup. That avoids a loopback TCP port with its own token.
  Old silo servers answer `bad_request`.
  - Auth: tunnels already need control · server (PHASE7 §3.4), which is shell access as the user, so a tunneled
    connection is owner on otto. For attribution, silo writes a one-line `TunnelPeer {device, label}` preamble
    before the bytes on service tunnels. Otto trusts it only on its own socket, from its own uid.
  - As built (phase 1): `silo_server::Config.services` maps names to a `ServiceFn` returning the socket path
    (15 s limit); unknown names get `not_found`, an error or a dead socket `unavailable`. Service tunnels splice
    with no idle limit (TCP ones keep 10 minutes). A server with `"otto"` adds the hello feature `otto`. No
    `TunnelPeer` preamble yet. Client side, `silo_client::tunnel::TunnelDialer` (one cached connection per host)
    opens TCP and service streams for the browser proxy and the agent block.
  - Remote OAuth sign-in: `silo_client::callback::CallbackForward` listens on `127.0.0.1` and `[::1]` at the
    login's callback port while the login is pending (ttl), forwards only `GET <path>[?…]` to the remote's
    `127.0.0.1:<port>` through the dialer (404 for anything else, 4 connections, 30 s idle), and stops when dropped.
    A taken port is `PortBusy`.
- **otto is embedded in `silo`** (phase 1): `silo otto <args…>` (hidden) runs `otto_cli::main_from` before silo's
  own runtime and logging, so tarballs, `silo update`, remote upgrade and install all carry otto. `silo server`
  registers the `"otto"` service: it starts `silo otto server --daemon` (own process group, `SILO_HOME` and
  `OTTO_SILO_HOME` set to the server's homes) through `otto_client::ensure_started`, and returns its socket. A
  running daemon of an older version serving this Silo home is asked to stop (`server.shutdown`) once none of
  its sessions is busy or waiting for approval, then the current one starts; while one is busy the old daemon
  keeps serving. A daemon
  with another protocol is not touched (it idles out). Processes otto starts itself (the daemon, the catalog
  refresh) run `<exe> otto …` when embedded. Clients reach a remote daemon with `Connection::connect_dial(Dial)`,
  which reconnects through the same dialer; a follower retries with a 250 ms to 5 s backoff.
- **One otto daemon per Silo server** (decided 2026-10-24): each has its own socket, locks, db, sign-ins and
  sessions, so a dev app (`SILO_HOME=/tmp/silo-otto`) never drives or shares the installed app's daemon.
  - The otto home is `otto_proto::paths::home()`, used by `otto-silo`, `silo otto`, `silo server` and the app alike:
    `$OTTO_SILO_HOME`; else, when the effective Silo home (`$SILO_HOME`, else Silo's default) is Silo's default,
    the default otto home (Application Support / XDG state `otto-silo`, unchanged); else `<silo home>/otto`. So
    `SILO_HOME=/tmp/silo-otto otto-silo auth login …` signs the dev daemon in. The config dir
    (`OTTO_SILO_CONFIG_DIR`, `~/.config/otto-silo`) stays shared.
  - The daemon records the Silo home its silo tools drive (`paths::silo_home()`) as `silo_home` in `hello`,
    `server.info` and `runtime.json`. `otto_claim` runs in the background at every `silo server` start (incl.
    `--adopt`/`--takeover` after an update) and before each `otto` service call: a daemon in this otto home that
    serves another Silo home, or reports none (pre-`silo_home`), is asked to stop even when busy (its interrupted
    turns resume at otto's next start), its lock is awaited and the service starts this server's own; an older
    daemon serving this home goes through the upgrade path above (kept while busy). Only when such a daemon does
    not stop is the result the error "otto here serves another Silo". At startup nothing is started when no daemon
    runs; the first agent starts it. E.g. the installed app's server replaces a daemon a dev app started in the
    default otto home.
  - The local app reaches its daemon through its own server too: `Connection::connect_dial` with a dial that calls
    `service.socket {name}` (owner only; the service's socket, or `not_found` / `unavailable`) on the app's silo
    server and connects to it. A loopback `silo/tunnel/1` would need the app paired with its own server, so the
    server only names the socket; starting and the home check stay in one place for local and remote.
- **The remote agent flow** (A = the Mac, B = a paired host):
  1. B's `silo server` registers `"otto"` and advertises feature `otto`; A's app routes that host's agent blocks to
     `Otto::Remote{id}`.
  2. A opens `open_service("otto")` on B's `TunnelDialer`; B's server starts its embedded daemon if needed and
     splices the stream to its socket. A runs `Connection::connect_dial` over it (`hello.home` is B's home).
  3. Everything runs on B: sessions, tools, commands, and `auth.login` credentials (B's otto home only).
  4. A browser sign-in answers `Pending{url, callback}`: A opens the URL and runs `CallbackForward` on the callback's
     port until the login completes, so the provider's redirect reaches B's loopback listener.
  5. A dropped tunnel redials through the same dialer, and followers resume from `last_seq`.
  Tested end to end in `silo-cli/tests/remote_agent.rs`.
- A new `BlockKind::Agent` whose server-side state is `{otto_session_id}`. The GPUI block (`otto-client` mirror +
  renderer) talks to otto directly, through the tunnel when remote. Silo only places it in the layout and shows
  status (busy, needs approval, done) through the Phase 7 status channel.
- Approvals for otto tools appear in the agent block. Approvals for silo owner actions use silo's card. Both reach
  every device.

---

## 10. Phases

**O0: provider spike.** `otto-llm` with Anthropic Messages and OpenAI Responses (SSE), both with API keys and with
subscription sign-in (Claude Pro/Max OAuth, ChatGPT/Codex OAuth): text, tools, parallel calls, reasoning replay,
usage, errors, cancellation. Recorded fixtures. `otto-silo auth login` and `otto-silo ask "…"`.
*Exit:* a tool-using conversation round-trips on all four (2 providers × 2 auth modes) with reasoning replay;
fixtures replay offline; token refresh survives two concurrent processes.

**O1: loop + tools, in-process.** `otto-tools` built-ins, `otto-core` turn engine, `otto-store` log + journal,
approvals in the terminal. `otto-silo run` headless.
*Exit:* it edits this repo with tests; kill -9 mid-tool leaves a valid session with "outcome unknown".

**O2: server.** `otto-proto`, `otto-server`, `otto-client`; CLI as a client; multiple clients mirror one session;
steer/queue/interrupt; approvals over the wire.
*Exit:* two CLI clients watch one session gap-free; reconnect resumes from `seq`.

**O3: provider depth, then breadth.** First OpenAI WS (lanes, steering, async tools, compaction, Codex WS),
Anthropic compaction, inline tools and server tools, and tool presentations per family. Then Gemini
Interactions, xAI, the compat family, and the catalog.
*Exit:* the same scripted task passes on every provider; cache hit rate reported.

**O4: harness depth.** Subagents + outbox, agents/skills/commands/hooks, MCP client, deferred tools, compaction
(native + own), context editing.

**O5: silo toolset.** `tools.rs`, `tools.list/call`, parked blocks (`block.park/reveal`), command blocks for
`shell` (§6.2) with the cost gate, `terminal.open/run/send/read/list`, `otto-silo-tools`, `silo mcp`, scoped grants
per subagent, and the `service` tunnel target.
*Exit:* otto runs `cargo test` as a parked command block, reads the exit code, and the user reveals it mid-run; the
block closes on exit unless revealed; a `sudo` prompt reports `waiting_for_input` and completes after the user
types; a background `cargo test` reports its result while the agent keeps working; a dev server runs in a terminal
until stopped; Claude Code runs the same tools via `silo mcp`; a remote CLI reaches otto over a service tunnel.

**O6: browser tools.** App-side CDP executor in `silo-app`/`silo-web`; forwarding; snapshot refs; screenshot to
image result.

**O7: GPUI agent block.** Separate design.

**O8: agents across machines and harnesses (§12).** Remote delegate otto ↔ otto first, then handover, then
harness adapters (terminal driver, ACP, Claude Code and Codex structured modes).
*Exit:* an agent on machine A delegates a task on its repo to machine B, B works on a handoff branch in a worktree
inside a visible agent block, A gets the result and the branch back; the same task runs with Claude Code on B as
the harness; A survives a restart mid-task and resubscribes.

**Later:** sandboxing, ACP agent mode (Zed/JetBrains), memory, goals/orchestrator, batch jobs.

---

## 11. Decisions and open questions

### 11.1 Decided

| # | Question | Decision |
|---|---|---|
| 1 | Process model | Otto is its own daemon, reached remotely through silo tunnels (§9). Crash isolation, its own deps and release cadence; a provider stream in the middle of an HTTP response can't be handed over in a hot upgrade anyway |
| 2 | Subscription sign-in | Supported for every provider that has it, from O0. If a provider asks us to remove it, we remove it; that stays cheap because each subscription mode is one module (§4.6) |
| 3 | Extensions | MCP plus a subprocess JSON protocol for v1; WASM components later |
| 4 | First providers | Anthropic and OpenAI (API key and subscription each); others in O3 |
| 5 | Naming | `otto-silo` binary and state dir for this version, crates `otto-*` in this workspace; rename to `otto` later (§3.1) |
| 6 | History ownership | Our log is the truth (`store=false` by default); provider-side state is only a cache (§7.3) |
| 7 | Default approval mode | `auto` ("yolo"). The other modes stay one flag or config value away (§5.2) |
| 8 | Parked blocks' home | The user's current session, next to the agent block (§6.2) |
| 9 | Command block retention | None: a command block lives as long as its command; otto keeps the text. Background shells are for finite work (tests, builds) whose result arrives later; servers and watchers run in terminals. Hidden terminals close with the agent session, revealed ones stay (§6.2) |

### 11.2 Open

1. **Cross-machine conflicts (§12).** When A and B both edit after a handover: does A become read-only for that
   branch (proposed), or do both keep working with a merge at the end?

---

## 12. Agents across machines and harnesses (proposal)

> **Superseded by `docs/AGENTS.md` §1, §5.4**: other harnesses are programs in terminal blocks that the main agent
> drives (send, wait on their status hook, read); cross-machine is the same over `--host`. No agent block type or
> `agent.*` protocol in v1. Kept below for history.

**Idea:** an agent with silo access can get work done on another paired machine. It can start an agent there for
a task, hand its whole session over, or message one that is already running. Both sides keep talking over silo,
with callbacks. The agent on the other side can be otto or another harness: Claude Code, Codex, opencode, the TS
otto.

### 12.1 What silo already gives

- Paired hosts over iroh with owner trust (pairing is shell access, PHASE7 §3.4).
- Service tunnels to otto's unix socket (§9): two otto daemons speak otto-proto unchanged, and
  `otto_client::SessionFollower` already follows a session gap-free over any stream.
- Upload streams (`UPLOAD_HELLO`) to move files to the other machine.
- Blocks for visibility: `BlockKind::Agent` (§9) and terminal/command blocks, so the work on B is always a pane the
  user can watch and take over.
- Scoped grants (`grant.create`), so a remote child gets less than the full pairing.

### 12.2 Three verbs

| Verb | What happens | Session lives on |
|---|---|---|
| `delegate` | A starts a child session on B; B's result lands in A's outbox (§7.4) | B; A follows it |
| `handover` | Transcript and code move to B; B continues; A becomes a read-only follower | B |
| `message` / `ask` | Input into a running agent on B, optionally waiting for its reply | unchanged |

For the model this is the existing `subagent` tool with a `host` field (`delegate {host?, harness?, cwd?, task,
code: auto|none}`), plus `hosts.list` and `handover {host}`. A remote child is a subagent whose daemon is elsewhere,
so the async semantics, limits and outbox stay the same. Handover needs no provider state: our log is the truth
(decision 6), and B uses its own sign-in; tokens never travel.

### 12.3 Getting the code there

Picked automatically, in this order:

1. B already has the repo (same remote URL or root commit): fetch.
2. Shared remote: commit A's work, dirty tree included, to `otto/handoff/<session>` through a temporary index (the
   user's branch and index are untouched), push, and B fetches it.
3. No shared remote: `git bundle` of the commits B lacks, sent over a silo upload stream.
4. Not a git repo: a tarball over upload, size-capped and approved.

B always works in a git worktree under its own projects dir, so it never clobbers a checkout the user has there.
Results come back the same way (a branch or a bundle), and A shows the diff or merges it.

### 12.4 Communication and callbacks

- **otto ↔ otto:** A's daemon opens a service tunnel to B's daemon, calls `session.create {parent: {host,
  session}}`, and subscribes. Completion, questions and approvals arrive as events. A stores `(host endpoint,
  remote session, last_seq)` so it resubscribes after a restart, with the same durable outbox rules as local
  subagents.
- **Approvals** a remote child raises appear on both machines; the first answer wins (approval ids are per session).
- **Callbacks from other harnesses:** they get silo's toolset through `silo mcp` (§6), including `agent.message
  {to}` and `agent.reply`. A Claude Code on B can report back to the otto on A that started it, or ask it questions.

### 12.5 Other harnesses

An adapter in otto-core: `HarnessAdapter { start(task, cwd), send(input), events() -> status and canonical items,
stop }`. Interfaces to verify when building:

| Harness | Structured interface | Fallback |
|---|---|---|
| otto | otto-proto over a service tunnel | – |
| ACP agents (opencode, Gemini CLI; Claude Code and Codex through Zed's adapters) | Agent Client Protocol over stdio | terminal |
| Claude Code | `claude -p --input-format stream-json --output-format stream-json` | terminal |
| Codex | `codex app-server` (JSON-RPC) or `codex exec --json` | terminal |
| TS otto | its HTTP/SSE server | terminal |

The **terminal driver** works with anything. It runs the CLI in a silo terminal block, types with `terminal.send`,
detects completion with OSC 133 and busy detection, and reads output with `terminal.capture`. Every adapted run
lives in a silo block, so it is visible and can be taken over on either machine.

### 12.6 Trust

- A paired host already has owner trust, so spawning there adds no capability. Each remote child still gets a
  session-scoped grant instead of the full pairing.
- The child runs under B's approval mode (default `auto`); B's user can set a stricter mode for incoming work per
  host.
- Every cross-machine action is an event on both sides, attributed to the device and agent (`TunnelPeer`).

### 12.7 Dependencies

O5 (service tunnel target, `silo mcp`, scoped grants) and §7.4 subagents. Phase O8 (§10).
