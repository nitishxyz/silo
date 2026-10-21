# Agent block (otto in silo)

Native GPUI block that shows one otto session as a chat. Supersedes `docs/OTTO.md` §9 for the block itself ("O7").
See §8 for status.

## 1. Model

- New `BlockKind::Agent`. A block with no process, like a browser block. Server state:
  `BlockInfo.agent: Option<AgentState>`, `AgentState { session_id: Option<String>, title: String }`, `Some` exactly
  when `kind == Agent`. Persisted, restored and handed off like browser state. `title` mirrors the otto session
  title (empty before the first prompt); the block title is `title`, else `"Agent"`.
- `agent.set {block_id, session_id?, title?}` (access: interact) updates it and emits the usual session change. The
  app calls it when the user picks or starts a session, and when the session title changes.
- Status: the app reports the turn state of the shown session with the existing `block.report` (`working` while a
  turn runs, `blocked --kind permission|question` on a pending approval/question, `done` when a turn finishes,
  `error`). `block.report` must work on agent blocks (attention, tab marks, notifications), not only terminals.
- One block shows one session at a time. Switching sessions only changes what the block shows; nothing is archived
  from the block. A session is created lazily on the first prompt (no empty sessions), in the block's `cwd`.
- The otto daemon is the app's local one: `otto_client::connect_or_start(otto_proto::paths::home(), <otto-silo next
  to the app executable>, &["server", "--daemon"])`. One shared connection per app. `otto-silo` ships in the
  bundle (`scripts/dev-bundle.sh`, `scripts/bundle-macos.sh`, release workflow) next to `silo`; not in the `silo`
  tarballs (no app there).
- Agent blocks of a remote host: not supported yet; the app shows the "Not supported" card and does not offer
  creating them on remote hosts.

## 2. Creating

- New Tab menu: a third row `Agent` (glyph `settings-shapes` until there is a dedicated one,
  `agent_block::ICON`), command `NewAgentTab`.
- `New Agent` (⌘⇧O): an agent block split beside the focused block, `cwd` = focused block's cwd. Focuses the
  composer. ⌘T on a focused agent block opens another agent tab (same-kind rule).
- `silo new --agent [-s NAME] [-c DIR]`: a session holding an agent block; prints the session id.

## 3. Layout

```
                               why does pnpm install fail in CI?     user: plain text, right-aligned, muted, max ~80%

  ◌ Working for 42s                                                  turn group header, shimmer while live
    ✓ Read package.json                                              older lines dim
    ✓ Searched for frozen-lockfile                       4 results
    ✕ Failed to run pnpm install --frozen-lockfile         exit 1  ›
    ✎ Edited pnpm-lock.yaml                              +48 −12   ›
    ◐ Running pnpm install                                   14s   ↗ newest: full brightness, shimmering title

  The last commit bumped a version in package.json …▍                assistant text: full width, markdown
```

- **No containers.** Steps are plain Trace-style lines (icon, sentence title, muted right-aligned description,
  status). Structure comes from indent, spacing, dimming and folding only. No borders/cards/dividers in the
  transcript. The composer is the only framed element.
- **Grouping rule:** within a turn, every non-text part (reasoning, any tool call/result, errors) goes into the
  current group; only assistant **text** ends a group. Tools after text open a new group. Typical turn: one group,
  then the answer.
- **Live group:** at most ~6 visible lines; older lines fade out at the top under a gradient mask and the list
  auto-scrolls; hovering pauses auto-scroll. Reasoning streams as dim text under a `Thinking` line, then becomes
  `Thought for Ns ›`.
- **Fold:** when text starts after the group, or the turn ends, the group folds (~500 ms after its last line) into
  one header line: `› Worked for 42s · read 3 files · ran 2 commands, 1 failed · edited 1 file`. Click toggles a
  nested full list (`⌄`). Reloaded sessions start folded.
- **Line kinds:** reasoning (`Thought for Ns`), read/ls/tree/glob/search (`Read x`, `Searched for q  N results`),
  shell (`Ran cmd  1.2s` / `Failed to run cmd  exit N`), edit/write/multiedit/apply_patch (`Edited f  +a −d`),
  web_fetch, silo terminal/browser tools (`Opened browser localhost:3000  ↗`), subagents, ask_user, errors. Running
  titles are present tense (`Running …`) and shimmer. Failed: red icon + red title.
- **Expand `›`:** inline detail under the line: command output tail, diff, tool output, in muted mono with a faintly
  lighter background (no border). **Jump `↗`:** for silo tool calls that name a `block_id`, focus that block.
- **Assistant text:** markdown (paragraphs, headings, lists, inline code, fenced code with mono font, links,
  bold/italic). Streams with a `▍` caret; newly arrived words fade in.
- **Hover actions** under a finished answer: copy, retry.
- **Approvals and questions:** a card pinned above the composer (the one other framed element), keyboard: ⏎ allow,
  esc deny, plus "Always" when the decision supports it; questions show options + free text.
- **Scroll:** pinned to bottom while streaming unless the user scrolled up; then a small `↓ New activity` pill.

## 4. Composer

```
        ╭────────────────────────────────────────────────╮
        │ ☰ Todos │ ⊘ Disable Hyperdrive que…   5/5 done ⌃│   tray tucked behind the box (todos / queued inputs)
        ╰─┬────────────────────────────────────────────┬─╯
  ╭───────┴────────────────────────────────────────────┴───────╮
  │  ⋮    th▍                                         ╭─────╮  │
  │                                                   │  ↑  │  │   round send button; becomes ■ stop while running
  │                                                   ╰─────╯  │
  ╰────────────────────────────────────────────────────────────╯   rounded, soft focus ring
    BUILD ⌃              A\ / claude-opus-5-5 (pro)    12% · $0.42
```

- Multi-line, grows to ~8 lines then scrolls. ⏎ sends, ⇧⏎ newline. While a turn runs, ⏎ queues
  (`Delivery::Queue`); queued inputs appear as chips in the tray. ■ cancels the turn.
- `⋮` menu: approval mode (auto / ask / plan), new session.
- Footer: agent (`BUILD`/`PLAN`) picker; provider glyph / model id (subscription marker) → searchable model picker
  from `model.list` limited to signed-in providers; right side: context used % and session cost when known.
- Todos tray: from the latest `update_todos` call of the session (new otto builtin `update_todos {todos: [{step,
  status: pending|in_progress|completed|cancelled}]}`); collapsed shows the current item and `n/m done`; `⌃`
  expands the list. Hidden when there are no todos and no queued inputs.
- Not signed in: the transcript area shows a short empty state with the providers and a sign-in action using the
  daemon's `auth.login` RPCs (API key field, or subscription login opening the browser / showing a device code).

## 5. Session switcher

Click the title in the pane header, or ⌘J with the agent block focused (pick a free chord if taken):

```
╭─ Sessions ───────────────────────────────────── filter ─╮
│ ● fix frozen lockfile        opus-5-5    running   now  │
│   otto block design          gpt-6       done      2h   │
│ ◆ refactor pty spawn         opus-5-5    needs you 1d   │
│ ＋ New session                                     ⌘N   │
╰─────────────────────────────────────────────────────────╯
```

Lists the sessions of the block's project (git toplevel of `cwd`, as `otto-silo sessions`), newest first, never
archived ones hidden by the block. Picking one calls `agent.set`. Other sessions keep running in the daemon.

## 6. Motion

Reuse silo's motion helpers (and `SILO_MOTION_SLOWDOWN`). Rows enter with opacity 0→1 and y +6px→0 over ~320 ms,
ease-out `cubic-bezier(0.25, 1, 0.5, 1)`; fold animates height + opacity; status icon swaps with a short scale;
send↔stop morph; tray slides up. With reduce motion, no animation.

## 7. Code layout

- `silo-proto` / `silo-server` / `silo-cli`: kind, state, `agent.set`, persistence, handoff, `block.report` on agent
  blocks, golden files.
- `otto-tools`: `update_todos` builtin.
- `silo-app/src/agent/`: the pane (`AgentPane`), otto connection, transcript model (pure, unit-tested: grouping,
  summaries, line titles), rendering, composer, switcher, markdown.
- `silo-app` wiring (`agent_block.rs`, workspace, layout, new tab menu, commands, titlebar icons): creates
  `AgentPane`s for agent blocks, forwards its events to the server.

## 8. Status and known gaps

Implemented on branch `otto` and driven in the GUI against an offline otto daemon (`OTTO_SILO_TEST_SCRIPT`, which also
reports the `test` provider as signed in with its `test/script` model): New Agent / New Tab row, composer (⏎, ⇧⏎,
send↔stop, queue), live group and fold, `›` details, markdown, approval and question cards, todos tray, ⌘J switcher
(⌘N new session), tab marks via `block.report`, sign-in empty state, restore after an app restart, dark and light.

Known gaps:
- The app creates panes (agent and browser) only for the shown tab. An agent block in another tab reports no status
  until shown, and (pre-existing) a browser block in a tab not shown after an app restart answers browser actions
  with `no_app` until that tab is opened.
- ⌘N / New session only clears the pane; the server keeps the previous `session_id` until the next prompt creates
  the new session, so a restart in between shows the previous session.
- A tool waiting for approval shows as `Running …` and its duration includes the wait.
- No title before otto names the session (the header says `Untitled`); no remote-host agent blocks; hover actions
  are copy and retry only; jump `↗` needs the block in a session this app can show.
