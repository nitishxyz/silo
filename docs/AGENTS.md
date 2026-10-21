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

**No special blocks.** An agent's terminals and browsers are ordinary blocks in ordinary sessions, visible,
attachable and closable by the user. Placement is a parameter, not a concept.

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
- No owner, or an owner without the feature: `no_app` ("No Silo app is showing this browser block: …"). An app
  claims a browser block (`own_block` `request`) as soon as it runs its page, so a block in a session the app shows
  works without the user focusing it; a block in a session nobody has open has no engine.
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

Later: headless engine on the server so browser tools work with no app attached; `silo browser cdp <block>` exposing
a scoped CDP endpoint for Playwright.

## 5. `silo-tools`

A library: `Tools::new(control, events, Context)` → descriptors + `call(name, input) -> ToolResult`.
`Context {agent_block: Option<BlockId> (from SILO_BLOCK), default_session: String, cwd}`. One event fan-out task
feeds concurrent waits. Tool names are MCP-safe (`[a-z_]`).

### 5.1 Terminal

| Tool | Does | Built from |
|---|---|---|
| `terminal_open` | New shell block; optional `command` typed into it (or `exec: true` to make it the block's program); `cwd`; `place: split\|tab\|session` (default: split next to the agent's own block, else a tab in the `agents` session); returns `{block_id, session_id}` | `block.split`, `window.create`, `session.create`, `terminal.send` |
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
- Browser tools need a Silo app showing the block's session (no headless engine yet).
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
- App: snapshot formatting, pruning and stable refs on recorded AX trees (unit tests, no CEF).
- Live, Oct 2026: a dev bundle (`scripts/ui-env.sh`, throwaway `SILO_HOME`) and `silo mcp` driving a local test
  page. Covered: snapshot, depth, find, fill, fill_form (text, select, checkbox), click by ref and selector, type
  with submit, press (`Meta+A`, `Backspace`), hover, eval with ref args, scroll to a ref and by `dy`, wait_for
  (found and timeout), dialog accept and dismiss, console, network, full and element screenshots (png, jpeg), link
  navigation, back, URL navigation, stale refs after navigation, `no_app` for an unshown session.
