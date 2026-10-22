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

## 4b. Visual spec v2 (supersedes the drawings above where they differ)

Reference: `target/agent-ref/composer-reference.jpg` (the target composer), `target/agent-ref/current-bad.jpg` (v1,
rejected: wrong spacing, double focus ring, left-aligned model, text carets as icons, API-key-only sign-in).
Units are points.

- **Icons:** real SVG icons (lucide shapes, monochrome masks in `assets/icons/`), never text glyphs (`⌄ ⋮ ✓ ✕ ✎ ↗`).
  `agent` (sparkles; also the block's tab/header icon, replacing `settings-shapes`), `chevron-down`, `chevron-up`,
  `history`, `more-vertical`, `arrow-up`, `stop`, `brain`, `image`, `list-todo`, `circle`, `circle-dot`,
  `circle-check`, `x-circle`, `file-text`, `search`, `folder`, `pencil`, `arrow-up-right`, `copy`, `rotate-ccw`,
  `bot`, `provider-anthropic`, `provider-openai`.
- **Column:** transcript and composer share one centered column, max 760 wide, 20 side padding. Vertical rhythm:
  20 between a user message and what follows, 12 between a group and text, trace lines 24 tall (13 text), prose
  13.5/1.55, code 12.5 mono.
- **Header:** no in-pane top bar; see §4d (the silo pane header carries the session controls).
- **Composer** (bottom, 12 above the pane bottom edge incl. footer): a **pill**: 46 tall single-line, corner radius
  23 (stays 23 when it grows to multi-line, up to 8 lines), fill one step above the pane background, border 1.5 at
  ~45% foreground; focus = border to ~70% foreground, nothing else (no second ring). Left: `more-vertical` 16 at 18
  from the left edge; text starts at 48; mono 14 (terminal font) for input and placeholder `Ask anything`. Right: send
  button 36 circle, 5 inset, foreground-colored fill (white in dark) with a background-colored `arrow-up`; empty
  input → fill at 25% and arrow muted; running → `stop` square glyph, same circle, morphs.
- **Tray** (todos / queued): sits behind the composer's top edge, inset 28 from the composer's left/right, 34 visible
  height, radius 12 top corners, 1 border at ~15% foreground, fill one step above background. Row: `list-todo` 14 +
  `Todos` (mono 12.5, foreground), a 1×14 divider, then the current item with its status icon (`circle-check` done,
  `circle-dot` in progress, `circle` pending), completed items struck through and muted, right `n/m done` muted +
  `chevron-up` 12. Expanded: the list grows upward above the row, same panel.
- **Footer** (under the composer, 8 gap, 18 tall, mono 12, muted): **three columns**: left `BUILD` + `chevron-up`
  (agent picker, uppercase agent name); **center** (truly centered on the composer, not after the left item)
  provider logo 13 + `/` + model id + ` (pro)`/` (max)` dimmer when on a subscription, the whole thing the model
  picker button; right `brain` 14 (reasoning, tinted violet when on) and `image` 14 (attach, tinted green when the
  model accepts images), each a 22×22 button. Context % and cost move into the model picker header and tooltip.
  As built: the subscription marker reads ` (sub)` (auth status has no plan tier), and `brain` is hidden because otto
  has no per-session reasoning control (effort comes only from the agent definition).
- **Popovers** (agent picker, model picker, ⋮ menu, switcher): anchored above their trigger, radius 12, fill one step
  above background, 1 border ~12% foreground, soft shadow, 6 padding, rows 30 tall with 8 radius hover fill;
  model picker 320 wide with a search field and provider groups (logo + name headers), current model checked.
- **Trace lines:** 16 icon column (per-kind icon: `brain` thought, `file-text` read, `search` search, `folder`
  list, `terminal` shell, `pencil` edit, `globe` web/browser, `bot` subagent, `list-todo` todos, `question` ask),
  8 gap, title, then muted description right-aligned; running → spinner icon + shimmer; error → `x-circle` red +
  red title; group header uses `chevron-down`/right rotation, never a text caret. Detail block: 24 indent, 8
  radius, fill ~4% foreground, mono 12, 8×10 padding, max 240 tall scrolling.
- **User message:** right-aligned, max 80%, 13.5 foreground at ~85%, fill ~6% foreground, radius 14, 8×12 padding
  (a soft bubble, not a bordered card).
- **Empty states:** vertically centered in the transcript area, text centered: `agent` icon 22 muted, 10 gap, title
  15 medium, 6 gap, subtitle 12.5 muted. New session: `What should we work on?` / project name.
- **Sign-in:** same centered stack, 340 wide: title `Sign in to start`, subtitle `Choose a model provider`, then one
  row per provider (logo 16 + name), each with a primary pill button for the subscription (`Claude Pro/Max`,
  `ChatGPT`) and a secondary text button `API key`. Pending login: the provider row turns into the flow (opened
  browser → paste-code field for Anthropic; ChatGPT completes on its own via loopback, else shows the device code
  big + `Copy`, polling until done), with `Cancel`.

## 4c. Commands and motion (user requests)

- **No global chord overrides.** ⌘N stays New Window everywhere. Agent actions live in the composer as slash
  commands: typing `/` at the start opens a filtered menu above the composer (↑/↓, ⏎/Tab pick, esc close):
  `/new` (new session), `/sessions` (switcher; ⌘J also), `/model`, `/agent`, `/mode auto|ask|plan`,
  `/providers`, `/clear`. Menus show no ⌘N hint.
- **Every state change animates** (silo `motion.rs`, `SILO_MOTION_SLOWDOWN`, reduce-motion → none), ~200–320 ms
  ease-out `cubic-bezier(0.25,1,0.5,1)`:
  - expand/collapse (todos tray, group fold/unfold, `›` details, popovers, slash menu, switcher): height + opacity,
    chevrons rotate;
  - things appearing/leaving (rows, trace lines, approval/question cards, attachment chips, tray, `↓ New activity`
    pill, empty → transcript, sign-in → composer): fade + 6–8 px slide from the side they come from;
  - things pushed up/down by others (transcript when the composer grows, tray/cards inserting above the composer,
    lines pushed by a new line): their position animates instead of jumping;
  - send ↔ stop morph, status icon swaps (scale 0.8→1 + fade), shimmer on running titles, button hover/press fills.
  - Nothing snaps; nothing animates longer than ~350 ms; streaming text never waits for an animation.
- **Inputs that aren't the composer go in dialogs** (API key, paste code, device code, rename…): a pane-scoped
  dialog in the app's sheet style, never inline rows that push other content.
- **Every control has states:** hover, pressed, keyboard focus, disabled, and loading (same width, spinner in place
  of the label, sibling actions disabled); success animates into the new state.
- **Pill input primitive** (`target/agent-ref/pill-input-{invalid,valid}.jpg|png`): one reusable component for
  every single-field entry (API key, paste code, filters, rename): a pill (height ~52, full radius) with a soft outer
  halo (a second, wider pill ~6 around it, one step above the background), 1.5 border ~30% fg (brighter on focus),
  leading icon 18 muted (`key`, `search`, …), text 16 from ~52, and a trailing 40 circle submit button with
  `arrow-right`: while the value is invalid/empty the circle is a dark fill with a muted arrow and is disabled; once
  valid it animates (~180 ms) to a light fill (foreground) with a dark arrow. Loading: spinner in the circle, field
  read-only. Error: border and halo tint red, one red line below. ⏎ submits only when valid; esc cancels. Optional
  `validate` fn per use (API key prefix/length, code format), optional masked mode with a reveal toggle.
- **Alignment:** fixed icon/logo columns, shared text left edges, right-aligned button column with equal widths per
  kind, one row height and padding per list.

## 4d. Review fixes (user screenshots `target/agent-ref/{composer,providers}-v2-bad.jpg`)

- **One header, not two.** The silo pane header already shows the block icon and title; the in-pane title row
  duplicates it with a different left edge. Remove the in-pane top bar. The pane header shows the session title
  (block title via `agent.set`, `New session` before the first prompt) and, for agent blocks only, a `chevron-down`
  after the title (click → switcher) plus `history` and `new-session` icon buttons placed with the header's
  existing split/zoom/close buttons, same size and hover style (`layout.rs` pane header).
  Session controls (title chevron, `history`, `new-session`) show only on the chat screen with a provider
  connected; on sign-in, Providers and other sub-views they are hidden. The header's insets are the pane header's
  own (no extra left/right padding from the content column).
- **Sub-views** (Providers, sign-in): a small left-aligned `‹ Back` text button (not a full-width bar, not
  centered) at the content column's left edge, then the view's title. Use a plug/link icon for Providers, not the agent
  sparkles. Every element of a view shares the content column's left edge.
- **Composer = the pill input primitive.** Same metrics and states as §4c: height 52, soft halo, 1–1.5 border at
  ~25% fg (~40% focused), leading `more-vertical` centered on the text line, 40 circle 6 inset; empty → dark circle
  + muted arrow, has text → light circle + dark arrow (animated), running → stop. Equal vertical and right inset
  around the circle; nothing touches the border.
- **Footer insets symmetric:** left item starts at the composer's text start (aligned with the placeholder), right
  icon ends at the send circle's center line mirrored (same inset as the left); model exactly centered. Image icon
  muted by default (no green); disabled at 40% when the model takes no images; hover fill like other icon buttons.
- **List rows** (providers, sessions, models): one row height, logo column centered on the two-line text block,
  status/plan line in the same muted style, the action column right-aligned with equal widths — `Disconnect` sits in
  the same column and right edge as `Sign in`, as a quiet button of the same height. Primary = filled, secondary =
  outlined, quiet = text; same height and radius everywhere.

## 4e. As shipped (polish round 2)

- **Header:** `AgentPane::header_title()` (chat screen with a provider only) → the pane header shows it with a
  `chevron-down` (switcher) and adds `history` + `new-session` before split/zoom/close, same buttons and hover. On
  sign-in / Providers the header shows the block title (`Agent`) and no session controls.
- **Pill primitive** (`agent/pill_input.rs`): `halo`, `pill`, `circle`, `circle_colors`, `lead_button` and metrics
  (`HEIGHT` 52, `HALO` 5, `CIRCLE` 40, `INSET` 6, `LEAD` = circle center mirrored, `TEXT_X` 52) are shared by the
  dialog field and the composer. Fills are opaque (the tray tucks behind). Single-line fields scroll sideways.
- **Composer:** mono 14/20, `⋮` on the first line, circle bottom-right; dark → light when there is text (180 ms),
  `stop` while running with an empty input. Height eases between line counts (editor `animate_growth`), pushing the
  transcript smoothly. Footer: agent label starts at the text start, image glyph ends as far from the right, model
  centered; image muted, 40% and inert when the model takes no images.
- **Slash menu:** `/new /sessions /model /agent /mode auto|ask|plan /providers /clear` (`/clear` also drops
  attachments); fuzzy filter, ↑/↓, ⏎/Tab, esc (keeps the text, menu stays closed until the `/` goes); the current mode
  is checked. No chord overrides: ⌘N is New Window everywhere; menus show `/new`, `⌘J`, `/providers`.
- **Rows** (`agent/rows.rs`): one 44 tall row (28 icon column, title + muted subtitle, trailing check/note/buttons,
  selected/hover/pressed fills) for the switcher, model/agent pickers, ⋮ and slash menus and provider rows; a small
  search pill above filterable lists. Arrow keys work in every popover.
- **Motion** (`motion::{ease_ui, enter, Toggle}`, `UI` 260 ms / `UI_FAST` 180 ms, `cubic-bezier(0.25,1,0.5,1)`,
  `SILO_MOTION_SLOWDOWN`, Reduce Motion → none): popovers open/close (height, opacity, 6 pt slide), group fold and
  unfold and `›` details (height + opacity, chevrons turn), tray expand (chevron turns) and rise from behind the
  composer, approval/question cards rise in and sink away, new trace lines push older ones up, status icons swap in,
  attachment chips, `↓ New activity`, body switches (connecting / sign-in / empty / transcript), composer rise after
  sign-in, send↔stop. Opening a group or detail stops tail-follow so the clicked line stays put.
- **Errors:** a session that can't be opened says so with a `New session` action (sending starts one); otto's auth
  hints (`run otto-silo auth login …`) become "connect it with /providers"; a turn's `Auth:` error reads "The
  provider didn't accept the sign-in · /providers" (full text in its details).

## 5. Session switcher

Click the title or `history` in the pane header, `/sessions`, or ⌘J with the agent block focused. A popover under the
header: search pill, then one row per session of the block's project (git toplevel of `cwd`), newest first: status
dot, title (the shown session falls back to its first prompt until otto names it, others to `Untitled`), `model ·
running · 2h`, check on the shown one; `New session` last. Picking one calls `agent.set`; other sessions keep running.

## 6. Motion

See §4e. Rows and lines enter with opacity 0→1 and y +6→0 over 320 ms; everything else as listed there.

## 7. Code layout

- `silo-proto` / `silo-server` / `silo-cli`: kind, state, `agent.set`, persistence, handoff, `block.report` on agent
  blocks, golden files.
- `otto-tools`: `update_todos` builtin.
- `silo-app/src/agent/`: the pane (`AgentPane`, slash commands), otto connection, transcript model (pure,
  unit-tested: grouping, summaries, line titles), rendering (`view.rs`), editor, pill primitive (`pill_input.rs`),
  list rows (`rows.rs`), sign-in (`signin.rs`), markdown. Motion helpers in `silo-app/src/motion.rs`.
- `silo-app` wiring (`agent_block.rs`, workspace, layout, new tab menu, commands, titlebar icons): creates
  `AgentPane`s for agent blocks, forwards its events to the server.

## 8. Status and known gaps

Implemented on branch `otto` and driven in the GUI against an offline otto daemon (`OTTO_SILO_TEST_SCRIPT`, which also
reports the `test` provider as signed in with its `test/script` model): New Agent / New Tab row, composer (⏎, ⇧⏎,
send↔stop, queue), live group and fold, `›` details, markdown, approval and question cards, todos tray, header
session controls, slash commands, ⌘J switcher, tab marks via `block.report`, sign-in and API-key dialog, restore after
an app restart, dark and light, `SILO_MOTION_SLOWDOWN=12` frames.

Known gaps:
- The app creates panes (agent and browser) only for the shown tab. An agent block in another tab reports no status
  until shown, and (pre-existing) a browser block in a tab not shown after an app restart answers browser actions
  with `no_app` until that tab is opened.
- New session (`/new`, header button) only clears the pane; the server keeps the previous `session_id` until the
  next prompt creates the new session, so a restart in between shows the previous session.
- A tool waiting for approval shows as `Running …` and its duration includes the wait.
- Until otto names a session the header shows its first prompt (other sessions in the switcher say `Untitled`); no
  remote-host agent blocks; hover actions are copy and retry only; jump `↗` needs the block in a session this app can
  show. Leaving animations exist for popovers, cards, the tray list and the activity pill; removed transcript rows
  and attachment chips disappear at once.
