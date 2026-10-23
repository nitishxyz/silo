# Agents on silo

Status: A1–A4 implemented on branch `otto`, verified offline and every browser action exercised in a running app
(§9). Supersedes `docs/OTTO.md` §6 and most of §12.

## 1. Idea

Silo is the fabric. Any agent connected to silo can use the whole machine (or any paired machine with `--host`):
open terminals and browsers as ordinary blocks, run commands and read their results, drive interactive programs,
and drive web pages through structured snapshots.

**Other harnesses are just programs in terminals.** The main agent runs `claude`, `codex`, `opencode` or `otto` in
a terminal block and controls it like a user would: send a message, wait until it is done or blocked, read the
screen. Delegating to another machine is the same thing with `--host`. No agent block type, no `agent.*` protocol.
The only thing a harness adds is a status report (OSC 7501 or `silo report`), installed as a hook, so waiting is
exact instead of guessed.

Otto is the one native harness: it loads the same tools in-process and records them in its own sessions.

**Ordinary blocks, some parked.** An agent's terminals and browsers are ordinary blocks in ordinary sessions,
attachable and closable by the user; placement is a parameter. One addition (`docs/OTTO.md` §6.2): a block can be
*parked*, in its session but in no layout, owned by the block that started it (the agent block). Otto runs each
`shell` command in a parked command block, and a hidden terminal is a parked shell. The user reveals one from the
switcher or the agent block; closing the owner closes its parked blocks.

## 2. Layers

| Layer | Where | What |
|---|---|---|
| Server primitives | `silo-proto`, `silo-server` | `block.report`, `block.notify`, `browser.*` actions forwarded to the app that renders the block |
| Engine | `silo-web`, `silo-app` | CDP executor per browser pane: accessibility snapshot with refs, actions, console, network, dialogs, screenshots |
| Tools | `silo-tools` (new crate) | Tool descriptors (name, description, JSON Schema, effects) and their execution over a `silo_client::Control` |
| Surfaces | `silo-cli` | `silo mcp` (stdio MCP server), `silo report`, `silo notify`, `silo hooks install` |
| Otto | `otto-silo-tools` (new crate) | The same tools as `otto_tools::Tool`s (`Source::Silo`) |

Every tool runs over an ordinary control connection: the access table, grants (`SILO_TOKEN`) and approvals apply to
each inner call. There is no new trust path and no server-side tool registry.

## 3. Server primitives

### 3.1 `block.report` (status from hooks)

Same record model as OSC 7501 (`status.rs`), reported over the API so hooks that have no tty can report.

```
block.report {block_id, id?: "", state: idle|working|done|blocked|error, kind?: permission|question|auth,
              progress?: 0..=100, app?, title?, message?}  -> {}
```

- Feeds `Records::report` exactly like the OSC path: same limits, `untrusted` text, attention, `block.status`
  event, notifications.
- Access: interact on the block. A pane client may always report for its own pane (`client.pane == block_id`).
- `state` absent is invalid; a CLEAR is `block.report {state: idle}` (no separate clear in v1).

### 3.2 `block.notify`

```
block.notify {block_id, title?, body}  -> {}
```

Raises the same attention as OSC 9/777 (`AttentionReason::Notification`), paced the same way. Access: interact.

### 3.3 Browser actions

Methods on browser blocks. The server forwards each to the client that **owns** the block (the app running its
engine) as `app.request {request_id, request: {method, params}, caller}`, and the app answers with `app.reply`.

- The owner must have advertised feature `browser_actions` (`silo_proto::compat::feature::BROWSER_ACTIONS`). It may
  be local or remote: remote hosts' browser blocks run in per-host profiles (`silo-web/src/profile.rs`), so an
  agent on host X only reaches pages of host X's blocks.
- No owner: the local app that advertised `browser_host` (§4.1) runs it off screen. An owner without the feature,
  or no owner and no such app: `no_app` ("No Silo app is running this browser block: …"). An app claims a browser
  block (`own_block` `request`) as soon as it runs its page.
- Timeout: `APP_TIMEOUT`, or the call's `timeout_ms + 5 s` when it has one (max 120 s).
- Only the owner may reply; a reply for another request is `not_found` (as `app.reply` today).

| Method | Params (all have `block_id`) | Result | Access |
|---|---|---|---|
| `browser.snapshot` | `within?: ref, depth?: u32, boxes?: bool` | `{url, title, snapshot: string}` | view |
| `browser.find` | `text?: string, regex?: string` (exactly one) | `{matches: string}` (matching nodes with their path) | view |
| `browser.click` | `target, double?: bool, button?: left\|right\|middle` | `Action` | interact |
| `browser.hover` | `target` | `Action` | interact |
| `browser.fill` | `target, value` (inputs, textareas, selects, `"true"/"false"` for checkboxes) | `Action` | interact |
| `browser.fill_form` | `fields: [{target, value}]` | `Action` | interact |
| `browser.type` | `text, submit?: key` (into the focused element) | `Action` | interact |
| `browser.press` | `key` (`Enter`, `Control+A`, `Meta+Shift+R`) | `Action` | interact |
| `browser.scroll` | `target?, dx?: f64, dy?: f64` (target: scroll into view) | `Action` | interact |
| `browser.wait_for` | `text?: [string], gone?: bool, timeout_ms?` | `{found: bool, url, title}` | view |
| `browser.dialog` | `accept: bool, prompt_text?` | `Action` | interact |
| `browser.eval` | `function: string` (JS function source), `args?: [ref]` | `{value: json}` | control |
| `browser.console` | `since?: u64, level?: string, limit?: u32` | `{messages: [{id, level, text, url?, line?, at}]}` | view |
| `browser.network` | `since?: u64, limit?: u32, failed_only?: bool` | `{requests: [{id, method, url, status?, type, failed?: string, at}]}` | view |
| `browser.screenshot` | `target?, full_page?: bool, format?: png\|jpeg, quality?` | `{mime, data_base64, width, height}` | view |

`browser.fetch {url, timeout_ms?, screenshot?: bool, selector?}` (no `block_id`, control on the server) →
`{url, title, status?: u16, html, timed_out, blocked, truncated, screenshot?: {mime, data_base64, width, height}}` goes to
the local `browser_host` app (never a remote one) and loads `url` in a hidden page there (§4.1). Errors:
`no_app` (no such app), `timeout` (nothing rendered in time; a page that rendered but never settled returns its
HTML with `timed_out: true`; a bot check that doesn't clear returns its page with `blocked: true`), `navigation`
(DNS, TLS, connection), `bad_request` (selector matched nothing, not http(s)). Server timeout 120 s, queue included.

`target` is a ref from the last snapshot (`e12`) or a CSS selector (anything not matching `^e\d+$`).

`Action` = `{url, title, dialog?: {kind, message}, snapshot?: string, navigating_to?: string}`; every action takes
`include_snapshot?: bool` (default false). A node keeps its ref until the page navigates; a ref from before fails
with `bad_request` "ref e12 is not on the current page; take a new snapshot".
Click, press, type with `submit` and dialog wait up to 300 ms for a main-frame navigation to start and then up to 5 s
for it to commit and fire DOMContentLoaded, so the result shows the new page; if it has not committed,
`navigating_to` carries the requested URL and there is no snapshot (the engine holds page commands until commit).

All these are additive (protocol stays version 1); `protocol.json` is regenerated.

### 3.4 Agent blocks

| Method | Params | Result | Access |
|---|---|---|---|
| `agent.set` | `block_id, session_id?, title?` (the otto session an agent block shows; `docs/AGENT_BLOCK.md`) | `{}` | interact |

`block.report` and `block.notify` work on agent blocks too (the app reports the shown session's turn state).

### 3.5 Parked blocks

| Method | Params | Result | Access |
|---|---|---|---|
| `block.spawn` | `block_id` (owner), `command?: [string]` (empty: login shell), `cwd?`, `env?: {k: v}`, `size?` (default 200×50) | `Created` (`window_id` empty) | control on the owner's session |
| `block.park` | `block_id` (not the only block of its window) | `{}` | control |
| `block.reveal` | `block_id`, `as?: float\|split` (default float; in the owner's window, else the active one) | `{}` | control |

- `SessionInfo.parked: [{id, owner?}]`; parked blocks are in `session.view`'s `blocks` and `status`, with the same
  PTY, server terminal, snapshot, status and access rules as any block.
- A parked block closes when its program ends (a revealed command block stays, exited, until closed). A command
  block's final screen and scrollback answer `terminal.capture` for 5 minutes after it closed.
- `terminal.capture` also returns `lifecycle`, `exited` and `line_prompt` (the program reads a line: canonical tty,
  cursor ending a non-empty line).
- Persisted like other blocks while alive; parked command blocks are dropped on a restart.

## 4. Engine: CDP in the app

Research (Playwright MCP, Chrome DevTools MCP, Oct 2026): agents work best from **accessibility snapshots with
element refs**, not screenshots. A snapshot costs a few hundred tokens, targets are exact, no vision model needed.
Screenshots stay available for visual checks. Both tools add `find` (search the tree), `wait_for` (text), console,
network, dialog handling and `evaluate` with element args. We follow that design.

- `silo-web`: a DevTools channel per `Browser` (`CefBrowserHost::execute_dev_tools_method` +
  `add_dev_tools_message_observer`), async `cdp(method, params) -> Result<Value>`, and event subscription. No remote
  debugging port is opened.
- Snapshot: `Accessibility.getFullAXTree` (main frame in v1), pruned like Playwright's aria snapshot (ignored and
  generic nodes folded, text joined), printed as YAML-ish lines:
  `- button "Submit" [ref=e12]`, `- textbox "Email" [ref=e5]: "a@b.c"`, `- link "Docs" [ref=e9]`, with
  `[checked]`, `[disabled]`, `[expanded]`, `[level=2]`, `[box=x,y,w,h]` when asked. Labels and listbox popups fold
  away, text repeating its parent's name or value is dropped, empty unlabelled containers are pruned, and text
  lines carry no ref. Refs map to `backendDOMNodeId` in a per-document table: stable across snapshots, cleared on
  main-frame navigation.
- Actions: resolve ref (`DOM.resolveNode`) or selector (`Runtime.evaluate` `document.querySelector`), scroll into
  view (`DOM.scrollIntoViewIfNeeded`), box (`DOM.getBoxModel`), then `Input.dispatchMouseEvent` /
  `Input.insertText` / `Input.dispatchKeyEvent`. `fill` sets value through `Runtime.callFunctionOn` with input and
  change events (selects and checkboxes included). `scroll` by `dx`/`dy` scrolls the target's scroller (or the
  document) by script: synthetic wheel events miss scrollers away from the pointer.
- Console (`Runtime.consoleAPICalled`, `Runtime.exceptionThrown`, `Log.entryAdded`) and network
  (`Network.requestWillBeSent`/`responseReceived`/`loadingFailed`) kept in ring buffers per pane (500 each).
- Dialogs: while `Page` is enabled the CEF `JsdialogHandler` holds the dialog's callback (a windowless engine shows
  none); `Page.javascriptDialogOpening` marks it pending and actions report it. `browser.dialog` answers through the
  held callback: a page blocked in a dialog answers no CDP, `Page.handleJavaScriptDialog` included.
- Console entries from silo's own page scripts (`\u{1}silo-…`) are left out.
- Screenshot: `Page.captureScreenshot`, capped at 4 MiB (jpeg fallback, then scaled).
- The app accepts `browser.*` requests from **any** host for a block it owns and renders (not only the local one, as
  `app.*` requests are).

### 4.1 Off-screen host (`silo-app/src/browser_host.rs`)

The app (feature `browser_host`) hosts the browsers no pane shows, on the local server only:

- **Blocks.** A `browser.*` request for a block no window has a pane for (an unshown tab, a session no window
  shows, a block nobody owns) runs in the host: it makes a windowless `BrowserPane` (same entity a window uses,
  800×600, never in a layout), claims the block and executes. A window showing the block adopts that entity (no
  reload, same history, refs and console), and a window that stops showing it (switching session or host, closing
  the window) hands it back instead of closing the page. A hosted page closes when its block closes, its session
  closes or the local server disconnects. A window whose pane's block lost its owner runs the request itself and
  claims the block. Hosted and fetch pages render at 5 fps (`set_windowless_frame_rate`, not `was_hidden`, so
  timers, rAF and CDP screenshots keep working); a window adopting one restores 60 fps. Screenshots `invalidate`
  first, so they capture a fresh frame.
- **Fetches.** `browser.fetch` opens a page in the persistent `agent` profile (`<home>/web/profiles/agent`, made
  like a server profile: own cookies and storage, never the user's `default`), loads the URL, waits for the load
  event and then network quiet (≤ 2 requests in flight for 500 ms), bounded by `timeout_ms` (default 15 s, max
  60 s). A Cloudflare-style challenge page (title `Just a moment…`/`Attention Required`/…, or a challenge form;
  checked when settled, else every second once loaded) keeps waiting for 6 s from first sight, then returns
  `blocked: true` with its title. It reads `outerHTML` (of `selector`, if given), takes
  the viewport screenshot if asked (1280×800), and closes the page. At most 3 fetch pages at once; the rest queue.

Later: `silo browser cdp <block>` exposing a scoped CDP endpoint for Playwright.

## 5. `silo-tools`

A library: `Tools::new(control, events, Context)` → descriptors + `call(name, input) -> ToolResult`.
`Context {agent_block: Option<BlockId> (from SILO_BLOCK), default_session: String, cwd}`. One event fan-out task
feeds concurrent waits. Tool names are MCP-safe (`[a-z_]`).

### 5.1 Terminal

| Tool | Does | Built from |
|---|---|---|
| `terminal_open` | New shell block; optional `command` typed into it (or `exec: true` to make it the block's program); `cwd`; `place: split\|tab\|session` (default: split next to the agent's own block, else a tab in the `agents` session); `visible: false` parks it next to the agent's block instead; returns `{block_id, session_id}` | `block.split`, `window.create`, `session.create`, `block.spawn`, `terminal.send` |
| `terminal_run` | Run a command in a block's shell (default: the agent's own reusable shell block, opened on first use), wait for it, return `{exit, duration_ms, output, truncated}` | `block.commands` + `terminal.send` + `block.command_finished` + `terminal.command_output` (as `silo run`) |
| `terminal_send` | Type `text`, then `keys` (`enter`, `tab`, `esc`, `up`, `ctrl-c`, …); `submit: true` sends Enter after a short pause (TUIs treat text+CR in one write as a paste) | `terminal.send` |
| `terminal_read` | Screen text, or the last `lines` with scrollback | `terminal.capture` |
| `terminal_wait` | Wait until any of: a status record updated after the call started enters one of `status` (default `done, blocked, error`); the next command finishes; output quiet for `quiet_ms`; `text` (regex) on screen; the block exits. Returns `{reason, status, screen}` | events + polling `terminal.capture` |
| `terminal_list` | Blocks with title, kind, cwd, busy program, status records, last command and exit | `session.list`, `session.view`, `blocks.busy`, `block.commands` |
| `terminal_close` | Close a block (`force` for busy ones) | `block.close` |

### 5.2 Browser

`browser_open {url, place}` (a browser block), `browser_navigate {url | back | forward | reload}`, then one tool per
§3.3 method: `browser_snapshot`, `browser_find`, `browser_click`, `browser_hover`, `browser_fill`,
`browser_fill_form`, `browser_type`, `browser_press`, `browser_scroll`, `browser_wait_for`, `browser_dialog`,
`browser_eval`, `browser_console`, `browser_network`, `browser_screenshot` (an image result).

`browser_fetch {url, timeout_ms?, screenshot?, selector?}` (`browser.fetch`, §3.3): JSON `{url, title, status?,
html, timed_out, blocked, truncated}` (capped like every tool result) plus the screenshot as an image part. Validation
(`silo_tools::fetch_params`): http(s) URL, `timeout_ms` capped at 60000, non-empty `selector`. The typed
`Tools::fetch(&input) -> anyhow::Result<BrowserFetchResult>` returns the uncapped result; its errors downcast to
`RpcError` with the codes above.

### 5.3 Other

`session_list`, `notify {title?, body}` (on the agent's block, else the target block).

### 5.4 Driving other harnesses

Nothing harness-specific in the tools. The pattern, documented in the tool descriptions:

1. `terminal_open {command: "claude", cwd}` (or `codex`, `opencode`, `otto`; with `--host`, on another machine).
2. `terminal_send {block_id, text: "…", submit: true}`.
3. `terminal_wait {block_id}`: returns on `done` / `blocked` from the harness's hook, or on quiet output.
4. `terminal_read` to see the answer; on `blocked` answer the prompt with `terminal_send` (or tell the user).

For structured results, run the harness non-interactively in a block: `terminal_run {command: "claude -p … --output-format json"}`
or `codex exec --json …`. The user still sees it live.

Getting code to another machine is plain terminal work (`git push` a branch, `git clone`/`git fetch` there).

## 6. CLI surfaces

- `silo mcp [--host H]`: stdio MCP server (JSON-RPC 2.0: `initialize`, `tools/list`, `tools/call`, `ping`), the
  `silo-tools` set. Images as MCP image content. Use: `claude mcp add silo -- silo mcp`.
- `silo report <state> [--kind K] [--app A] [--title T] [--message M] [--id ID] [--block B]`: `block.report` for
  `$SILO_BLOCK`. Outside silo (no block) it exits 0 silently: hooks must never break a harness.
  `--codex-notify` reads Codex's notify JSON (last argument) and reports `done`.
- `silo notify [--title T] <body…>`: `block.notify`.
- `silo hooks install <claude|codex|opencode> [--print]`: wires the harness to `silo report`:
  - Claude Code (`~/.claude/settings.json`, merged): `UserPromptSubmit` → `working`, `Stop` → `done`,
    `Notification` → `blocked --kind permission`, `SessionEnd` → `idle`.
  - Codex (`~/.codex/config.toml`): `notify = ["silo", "report", "--codex-notify"]` (left alone if `notify` is already
    set; `--print` shows what to add).
  - opencode (`~/.config/opencode/plugin/silo.js`): `session.idle` → `done`, `permission.updated` → `blocked`,
    `session.error` → `error`, prompt → `working`.
  - Otto emits OSC 7501 natively (no hook).

## 7. Otto

`otto-silo-tools`: a `ToolSource` that connects to the local silo server (no spawn; absent server → no tools), wraps
each `silo-tools` descriptor as a `Tool` with `Source::Silo`, maps effects (read tools `READ`, terminal and browser
actions `PROCESS`, eval `PROCESS`), images to `OutputPart::Image`, and passes `cx.cancel` through. Browser tools are
`Loading::Deferred`. Otto's own policy (default `auto`) applies before the call; silo's access and approvals apply
inside it.

It also gives otto's `shell` its terminal host (`otto_silo_tools::silo`, `Runtime::set_terminals`): a command
that is not short and read-only runs in a parked command block owned by the agent block showing the session (or
its nearest ancestor), through `Tools::spawn_command`, `watch` and `stop`; new terminals open next to that block
too. Without silo or such a block, pipes run it. Results have the same shape either way.

Rendered fetches for `web_fetch`/`web_search`: `otto_silo_tools::connect(config) -> Silo {tools, terminals,
renderer}` (`silo()` is `connect` without the renderer), sharing the one connection. The renderer implements
`otto_tools::builtin::render::Renderer`:

```rust
#[async_trait] pub trait Renderer: Send + Sync {
    async fn render(&self, request: RenderRequest, cx: &ToolCx) -> Result<Rendered, RenderError>;
}
pub struct RenderRequest { pub url: String, pub timeout_ms: Option<u64>, pub screenshot: bool, pub selector: Option<String> }
pub struct Rendered { pub url: String, pub title: String, pub status: Option<u16>, pub html: String,
                      pub timed_out: bool, pub blocked: bool, pub screenshot: Option<(String /* mime */, String /* base64 */)> }
pub enum RenderError { NoApp(String), Timeout(String), Navigation(String), Failed(String) }
```

`NoApp`: silo not running, no `browser_host` app, or a server without `browser.fetch`. It calls `browser.fetch`
directly (not through otto's policy: the calling tool's own policy applies).

## 8. Phases

| Phase | Scope | Status |
|---|---|---|
| A1 | §3: `block.report`, `block.notify`, `browser.*` methods, access table, forwarding, tests, `protocol.json` | Done (`silo_proto::agent`, `silo-server/tests/agent_api.rs`) |
| A2 | §4: CDP channel in `silo-web`, executor and request handling in `silo-app` | Done (`silo-web/src/{devtools,snapshot}.rs`, `silo-app/src/browser_actions.rs`); live-tested (§9) |
| A3 | §5–6: `silo-tools`, `silo mcp`, `silo report`, `silo notify`, `silo hooks install` | Done |
| A4 | §7: `otto-silo-tools`; otto reports its turn state as OSC 7501 in a silo block | Done (`Runtime::add_tools`, `otto-cli/src/silo_status.rs`) |
| Later | Headless server browser, scoped CDP endpoint, iframes in snapshots, a `silo agent` convenience command | – |

Otto honours `Loading::Deferred` through `load_tools` (names or query), with loaded names persisted per session;
browser schemas stay out of prompts until loaded. Native provider tool search is not yet used.
Known gaps:
- Browser tools and `browser_fetch` need the Silo app running on the server's Mac (a remote host's browser blocks
  run only in an app that shows them).
- `browser_navigate` back/forward retries once after 1.1 s, because a followed link reaches the block's history only
  after the engine's report.
- `wait_for` defaults to 10 s.
- Blocks of an in-process test server with a non-default home don't get `SILO_HOME`.

## 9. Tests

- Server: `block.report` from a pane and from a token (and refused without interact), attention/events; browser
  forwarding to a fake owner app (reply, timeout, `no_app`, non-owner reply refused).
- Tools: against a real server (`silo-server` test harness): open, run (exit and output), send/read, wait on
  `silo report done` from inside the block, wait on quiet output and on text, list, close.
- MCP: `initialize` → `tools/list` → `tools/call terminal_run` over stdio.
- Fetch and host: `browser.fetch` routing (`no_app` without a `browser_host` app, forwarded to one), unowned blocks
  to the local host (`app_bridge` unit test, `agent_api.rs`); `browser_fetch` descriptor, input validation and
  `no_app` against a real server (`silo-tools/tests/fetch.rs`); renderer error mapping (`otto-silo-tools`); the
  host's adoption bookkeeping, request routing, load/network-quiet tracking and fetch slots
  (`silo-app/src/browser_host.rs` unit tests, no CEF).
- App: snapshot formatting, pruning and stable refs on recorded AX trees (unit tests, no CEF).
- Live, Oct 2026: a dev bundle (`scripts/ui-env.sh`, throwaway `SILO_HOME`) and `silo mcp` driving a local test
  page. Covered: snapshot, depth, find, fill, fill_form (text, select, checkbox), click by ref and selector, type
  with submit, press (`Meta+A`, `Backspace`), hover, eval with ref args, scroll to a ref and by `dy`, wait_for
  (found and timeout), dialog accept and dismiss, console, network, full and element screenshots (png, jpeg), link
  navigation, back, URL navigation, stale refs after navigation, `no_app` for an unshown session.
