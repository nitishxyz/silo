# Otto: an agent harness for silo (research + plan)

Status: direction decided (§11.1). **O0–O2 implemented on branch `otto`, verified offline** (§0). Still open: the
shell runner details (§6.2, §11.2), cross-machine conflicts (§12).

## 0. Implementation status

| Phase | State | Where |
|---|---|---|
| O0 providers | Anthropic Messages + OpenAI Responses (SSE), each with API key and subscription sign-in (Claude Pro/Max code paste; ChatGPT/Codex browser loopback + device code). Recorded-fixture tests. models.dev catalog with per-model auth availability | `otto-llm` |
| O1 loop + tools | Turn engine (one actor per session), approvals (default `auto`, per-session modes), `Policy::prepare` path binding, tool journal + crash recovery, compaction, subagents, agents/skills/hooks/commands, built-in tools | `otto-core`, `otto-tools`, `otto-store` |
| O2 server | Daemon with gap-free subscriptions, live deltas, bounded queues, idle exit; client mirror + follower; `otto-silo` CLI | `otto-proto`, `otto-server`, `otto-client`, `otto-cli` |
| O3–O8 | Not started | – |

**Verified:** workspace fmt, clippy `-D warnings`, all tests (no network), `otto-proto` on wasm32, the silo protocol pin.
Daemon integration tests cover a tool turn followed by a second client, kill -9 mid-tool → "outcome unknown", Ctrl-C,
SIGTERM and approval denial without a tty.

**Not yet verified live:** real provider calls, both subscription sign-ins and refresh, the catalog refresh from
models.dev. Run the ignored live tests with `OTTO_LIVE=1` and real credentials.

**Known gaps:**
- Hooks are a command-only subset of Claude Code's format.
- Path guards are revalidation, not a sandbox (§5.2 "later").
- Global index event seqs reset when the daemon restarts.
- A child's completion reaches its parent only while the parent's actor is open.
- Gemini and xAI are catalog-only.

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
| `otto-cli` | The `otto-silo` binary: server + CLI in one, like `silo` | – |

The harness library stays usable without silo, and silo integration lives in one adapter crate. The silo side
gets `silo-proto/src/tools.rs` plus `tools.*` methods (§6).

**Naming for this first version:** the crates live in this workspace under `crates/otto-*`. Only the user-visible
names differ from the final ones, so nothing clashes with the TS `otto` already installed on users' machines:

| Thing | Now | Later |
|---|---|---|
| Binary | `otto-silo` | `otto` |
| State dir | `~/Library/Application Support/otto-silo`, `$XDG_STATE_HOME/otto-silo`; override `OTTO_SILO_HOME` | `otto` |
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
| Google | Interactions API (`generateContent` only if needed) | O3 |
| Claude subscription (Pro/Max) | Same Messages client, OAuth bearer auth plus the request shaping the subscription endpoint expects | O0 |
| ChatGPT subscription (Codex) | Same Responses client against `chatgpt.com/backend-api/codex/responses` (HTTP and `wss://`) | O0 (HTTP), O3 (WS) |
| xAI | Responses-style API | O3 |
| Compat family | Chat Completions dialect table: OpenRouter, DeepSeek, Kimi, Z.ai, MiniMax (Anthropic dialect), Ollama, LM Studio, custom base URLs | O3 |
| Bedrock / Vertex / Azure | auth + endpoint variants of the above | later |

### 4.6 Transport and auth

- `reqwest` + rustls, an SSE parser, `tokio-tungstenite` for OpenAI WS. Retries with jitter, honoring
  `retry-after` and rate-limit headers, an idle timeout between events (not the total time), overload handling, and
  cancellation that closes the stream.
- Credentials: env vars, sealed store (`silo-store`-style), macOS Keychain. OAuth: device and loopback flows, a
  refresh coordinator with a file lock (a TS lesson: concurrent refresh rotation), request-scoped credentials.
- **Subscriptions are an auth mode of a provider, not a separate provider.** `Auth::{ApiKey, Subscription}`
  picks the endpoint, headers and request shaping inside the provider crate. The model list, tools and transcript
  stay the same. Every subscription-specific rule (required instructions, header set, Codex prompt mode, no-strict
  tool default, the TS text guard) lives in one module per provider, so it can be fixed or removed in one place
  (decision 2). The TS flows are the reference for behavior: `packages/sdk/src/auth/src/{oauth,openai-oauth}.ts`,
  `packages/sdk/src/providers/src/{anthropic-oauth-client,openai-oauth-client,openai-oauth-websocket}.ts`.
- Catalog: generated at build from a models.dev-style source plus a manual overlay, refreshed at runtime with a
  cache. Pricing feeds usage and cost events.
- Tests: **recorded stream fixtures** (raw SSE/WS bytes in, canonical events out) per provider feature, with
  `OTTO_RECORD=1` to refresh them against live APIs.

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
- **Presentations**: one executor, several model-facing shapes. Claude gets a `str_replace`-style editor and its
  trained bash shape; GPT gets `apply_patch` (custom/freeform tool) and its shell shape; others get generic JSON
  tools. The engine picks the shape from `tool_family`.
- **Sources**: `ToolSource` trait (`list`, `call`, change notifications). Builtin, silo (§6), MCP via the official
  Rust SDK (`rmcp`, verify version: stdio + streamable HTTP + OAuth), and extensions (subprocess JSON protocol like
  the TS native host; WASM later).
- **Deferred tools**: portable `load_tools` accepts names or a case-insensitive name/description query. Only granted,
  not-yet-loaded tools appear in its compact index; schemas enter the next model request. Loaded names persist in
  session metadata (`loaded_tools`), independently for each child. Direct calls to granted unloaded tools still run,
  and `tools.list` includes deferred tools. Native Anthropic/OpenAI tool search with `defer_loading` is future work.
- **Async tools**: native `async: true` on OpenAI. Elsewhere the call returns a job handle; the result is injected
  later as a system item, plus a `wait` tool.

### 5.1 Built-in tools (v1)

`read`, `write`, `edit`/`multiedit` (str_replace family), `apply_patch` (patch family), `ls`, `tree`, `glob`,
`search` (ripgrep's `grep` + `ignore` crates), `shell` (one-shot, pipes, timeout, background jobs), `web_fetch`,
`web_search` (hosted when the provider has it), `todos`, `ask_user`, `skill`, `load_tools`, `subagent`
(delegate/list/message/stop/retry), `wait`.

### 5.2 Policy

- Approval modes: `auto` (**default**, "yolo": nothing prompts; deny rules still apply), `ask-on-effects`
  (write/process/external need approval unless a rule allows it), `ask-all`, `plan` (read-only). Decision 7.
- Rules: `{tool, arg pattern, decision}`, e.g. `shell: "cargo test*" → allow`, stored per project and per user.
  "Always allow" answers write rules.
- Canonical path resolution (symlinks, `~`, project/reference/outside) runs before guards and before the approval
  is shown, and the tool runs with the same resolved value.
- Sandbox (later): macOS seatbelt profile and Linux landlock/seccomp for `shell`, with network as its own toggle.

---

## 6. Silo toolset (for any harness)

> **Superseded by `docs/AGENTS.md`**: client-side `silo-tools` crate (no `tools.call`), ordinary blocks (no parked
> blocks), CDP accessibility snapshots for browsers. Kept below for history.

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

### 6.2 Where agent commands run (proposal)

The rule is "whichever is cheaper, but use silo blocks when they work well". Proposal: **every agent command runs
in its own parked silo command block when silo is connected; pipes are the fallback.**

**Parked blocks (new in silo).** A session gets `parked: Vec<BlockId>`: blocks that live in the session but in no
layout, so nobody sees them by default. Their PTY, server terminal, snapshot, status and access rules are the same
as any other block.
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
   instead (async tool, §5).
6. **Retention:** exited command blocks stay parked with their final screen and scrollback, so the user can open
   any past command in full color. Each session keeps the last N (for example 50) and closes the oldest. Closing the
   agent session closes them all. Pinning keeps one.

**`shell` without silo** (silo not running, or a remote otto without a silo next to it): pipes, stdin
`/dev/null`, the same env, the same output limits. The model sees the same tool either way.

**`terminal.*`** stays for long-lived and interactive work: dev servers, watchers, REPLs, `ssh` sessions, anything
the user should keep. Those are normal shell blocks, parked or revealed (`terminal.open {visible}`), and reused
across calls.

**Why blocks over pipes when silo is there:** watching and taking over are free, interactive prompts stop being
failures, past commands keep their full output in silo's own scrollback store, and the agent block can embed a
live mini-terminal in each tool card by reusing silo's terminal renderer on the block's data stream.

**Cost, measured before it becomes the default (O5 gate):** each call costs a PTY, a process spawn, a server
terminal and a ring. Pipes pay only the spawn. The gate is spawn-to-first-byte and capture overhead on
`git status`, `rg`, and `cargo check` with no-op builds. If a block adds more than about 10 ms at p50 or noticeable
memory per 50 parked blocks, short read-only commands go through pipes and the rest through blocks.

---

## 7. `otto-core`: runtime

### 7.1 Model

- **Project** (root, config, instructions from `AGENTS.md`/`CLAUDE.md`), **Session** (agent, model, parent, cwd),
  **Turn** (one user input until the model stops), **Step** (one model call + its tool calls).
- **Agents** as markdown with frontmatter (`name, description, model, tools, effort`) in `.otto/agents/` and
  `~/.config/otto/agents/`. Built-ins: `build`, `plan`, `general`, `research`.
- **Skills**: `SKILL.md` folders (`.otto/skills`, `.agents/skills`, user dirs), loaded on demand by the `skill`
  tool; names and descriptions only in the prompt.
- **Commands**: slash commands from markdown templates.
- **Hooks**: `session_start`, `pre_tool`, `post_tool`, `stop`, `pre_compact` as commands with JSON stdin/stdout
  (allow/deny/modify). A format close to Claude Code's lets existing hooks port.

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
  supports it; otherwise the input lands at the next step boundary as a user item, and in-flight tools finish.
  `interrupt` cancels the current step and then continues with the new input.
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
- **Compaction**: native when the session's model has it and the history is single-provider; otherwise otto's
  checkpoint summarizer (TS design: bounded, instructions reserved, head+tail, validated, `through` boundary).
  Plus context editing: clear old tool results above a budget, keeping the call/result pairs valid.
- Provider state (`previous_response_id`, WS lane, `previous_interaction_id`) is a cache keyed by
  `(session, provider, model, last item seq)`. On a miss we replay full history; after a fork we open a new lane.

### 7.4 Subagents

- A child is a session with `parent`, `agent`, `task`. `delegate` returns right away; the result goes to the
  parent's **outbox** as a versioned record. Delivery: injected at the parent's next step boundary, or, if the
  parent is idle, a new turn wakes it.
- `list`, `status`, `message{queue|interrupt}`, `stop`, `retry`, `reuse_session`; depth and concurrency limits;
  parent cancel stops its children; per-agent model; tool set = the agent's set ∩ the parent's grant.
- With silo: each subagent can get its own scoped grant and its own panes.

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
  `tools.list`, `agents.list`, `skills.list`, `config.get/set`, `subagent.*`.
- Events: `session.changed`, `turn.started/finished`, `item.started`, `text.delta`, `reasoning.delta`,
  `tool.input_delta`, `tool.call`, `tool.progress`, `tool.result`, `approval.requested/resolved`,
  `question.asked`, `usage`, `compaction`, `subagent.changed`, `queue.changed`, `error`.
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
*Exit:* otto runs `cargo test` as a parked command block, reads the exit code, and the user reveals it mid-run; a
`sudo` prompt reports `waiting_for_input` and completes after the user types; Claude Code runs the same tools via
`silo mcp`; a remote CLI reaches otto over a service tunnel.

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
| 7 | Default approval mode | `auto` ("yolo"). The other modes stay one flag or config value away; plan agents are always read-only (§5.2) |

### 11.2 Open

1. **Shell runner (§6.2).** Parked command blocks by default when silo is connected, pipes otherwise, with a cost
   gate in O5. Alternatives: (a) pipes always, with silo only for `terminal.*`; (b) one reused hidden shell per
   agent session with OSC 133 marks (cheaper, but it leaks state between calls and serializes parallel calls).
2. **Parked blocks' home.** In the user's current session next to the agent block (proposed), or in a separate
   hidden silo session per otto session?
3. **Retention.** How many exited command blocks to keep per agent session, and whether they survive a silo
   restart (silo's snapshots already make that possible).
4. **Cross-machine conflicts (§12).** When A and B both edit after a handover: does A become read-only for that
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
