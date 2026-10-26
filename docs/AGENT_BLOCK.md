# Agent block (otto in silo)

Native GPUI block that shows one otto session as a chat. Supersedes `docs/OTTO.md` §9 for the block itself ("O7").
See §8 for status.

## 1. Model

- New `BlockKind::Agent`. A block with no process, like a browser block. Server state:
  `BlockInfo.agent: Option<AgentState>`, `AgentState { session_id: Option<String>, title: String }`, `Some` exactly
  when `kind == Agent`. Persisted, restored and handed off like browser state. `title` mirrors the otto session
  title (empty before the first prompt); the block title is `title`, else `"Agent"`.
- `agent.set {block_id, session_id?, title?, cwd?}` (access: interact) updates it and emits the usual session change.
  The app calls it when the user picks or starts a session, when the session title changes, and with `cwd` (absolute)
  when the user moves the block to another project: a new `cwd` clears the session and title unless `session_id` is
  given too. `cwd` is the block's, persisted and broadcast with it.
- Status: the app reports the turn state of the shown session with the existing `block.report` (`working` while a
  turn runs, `blocked --kind permission|question` on a pending approval/question, `done` when a turn finishes,
  `error`). `block.report` must work on agent blocks (attention, tab marks, notifications), not only terminals.
- One block shows one session at a time. Switching sessions only changes what the block shows; nothing is archived
  from the block. A session is created lazily on the first prompt (no empty sessions), in the block's `cwd`.
- The otto daemon is the one this Mac's silo server serves (one per Silo home, OTTO.md §9): the pane connects with
  `Connection::connect_dial`, whose dial asks the app's own server for `service.socket {name: "otto"}` (the server
  starts or checks the daemon exactly as for a tunnel) and connects to that socket. One shared connection per app,
  redialed the same way. The app no longer starts a daemon itself; `otto-silo` still ships in the bundle
  (`scripts/dev-bundle.sh`, `scripts/bundle-macos.sh`, release workflow) next to `silo`, which also embeds otto
  (`silo otto …`).
- Agent blocks of a remote host run on that host's otto: when its server advertises feature `otto` (it registered
  the tunnel service), the pane connects with `Connection::connect_dial` over `open_service("otto")` on the host's
  `TunnelDialer` (shared with its browser tunnel proxy; `agent/client.rs`, one connection per `Otto` target). The
  project is `project.open {cwd}` (the daemon resolves the git root) and paths shorten with its `hello.home`.
  Older remotes and shared links show the "Not supported" card and don't offer creating them
  (`agent_block::agents_supported`).
- A remote agent block runs entirely on the host: sessions, tools, shell commands and credentials live in the
  host's otto home. Signing in from a remote block signs the *host* in (the Mac stores nothing). For a browser
  login the pane opens the URL here and forwards the provider's redirect to the host's loopback
  (`LoginResult::Pending.callback` → `CallbackForward`, only while the login is pending). While the app is
  connected, it is also the host's browser host: the host's browser tools and rendered fetches run in this Mac's app.

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
- **Live group:** at most ~6 visible lines (plus room for each open detail as drawn: its box as last measured and
  8 above and below, clipped like its wrapper while it opens, so a pinned window keeps following); older lines fade out at the top under a
  gradient mask. The window is an inner box (`scroll.rs` rules): pinned to its newest line, scrolled up in it, it
  stays put while lines arrive below, with a bottom fade, until scrolled back down (`scroll::Pin`). Its header is
  the turn's one indicator while it ends the transcript; otherwise the activity row is. Reasoning streams as dim
  text under a `Thinking` line (its newest
  lines at the bottom of a window up to four lines tall, older ones moving up under a top fade), then becomes
  `Thought for Ns ›`.
- **Fold:** when text starts after the group, or the turn ends, the group folds (~500 ms after its last line) into
  one header line: `› Worked for 42s · read 3 files · ran 2 commands, 1 failed · edited 1 file`. Click toggles a
  nested full list (`⌄`). Reloaded sessions start folded. A group of thinking only folds at once, its line gone in
  the fold's first third (it would repeat the header).
- **Timing:** wall time, as seen live. A step runs from when it first appears to when it finishes, so time spent
  waiting for an approval or an answer counts; a group's `Worked for` spans its first step's start to its last
  step's end. A live header shows the turn's clock (`Working for` since the turn started, as the activity row it
  replaces did), so it can read more than the group's `Worked for`. While it waits the line reads `Needs approval`
  (`Needs your answer`) with a lock (question) icon, its subject muted on the right and no timer. The pane's clock is
  wall time. Steps that ran before the session loaded (an app or daemon restart, an old session, a sub-agent opened
  mid-run) take otto's recorded times, `SessionView.part_times` (`otto_proto::timing`: from the journal, by the same
  rules: reasoning from its first delta until what follows streams, a call from its first input or its item until its
  result or the turn's end), so `Worked for`, line times and `Thought for` read as they did live; the turn's start
  comes from `SessionView.turn_started_at` and running calls' from `RunningToolCall.started_at`.
- **Line kinds:** reasoning (`Thought for Ns`), read/ls/tree/glob/search (`Read x`, `Searched for q  N results`),
  shell (`Ran cmd  1.2s` / `Failed to run cmd  exit N`), edit/write/multiedit/apply_patch (`Edited f  +a −d`),
  web (`Searched the web for "q"  N results`, `Fetched example.com/path  rendered`, `Couldn't fetch …`; details: the
  results, the page's markdown), silo terminal/browser tools (`Opened browser localhost:3000  ↗`), subagents,
  ask_user, errors. Running
  titles are present tense (`Running …`) and shimmer. Failed: red icon + red title. Stopped by the user (■, `job
  kill`, a closed block, a stopped turn or sub-agent stop cutting a call off): a muted stop square, neutral title
  (`Stopped cargo test`; other tools `Read a.rs  stopped`), counted apart in the fold (`ran 2 commands, 1 stopped`).
  A timeout stays an error (`timed out`).
- **Paths:** display only (otto gets what the model sent): a leading `cd <project> &&` / `;` is dropped, `<project>/x`
  reads `x`, other paths under home `~/…` (`transcript::display`), in line titles, descriptions and details, approval
  cards and todos.
- **Expand `›`:** inline detail under the line: command output tail, diff, tool output, in muted mono with a faintly
  lighter background (no border). **Jump `↗`:** for silo tool calls that name a `block_id`, focus that block.
- **Assistant text:** markdown (paragraphs, headings, lists, inline code, fenced code with mono font, links,
  bold/italic). Streams line by line (`agent/reveal.rs`): a wrapped line shows once complete (the next line or
  block began; a paragraph's last line at its blank line, a heading's or code line's at its newline; all of it when
  the stream ends); the message's height glides (`Glide`, 180 ms) to the bottom of the last line whose sweep
  started or is next (each leaf reports its top after layout), so space opens line by line with the sweeps and a
  burst never opens a blank slot ahead of its text: at most one line of space below what shows; once caught up,
  everything but the line being written (at the bottom the transcript scrolls smoothly). Once the height drawn is
  within 4 of its slot
  (and at least 60 ms after it completed) it sweeps in left to right right where it ends up: a soft edge 20% of the
  line wide, at a constant speed (250 ms for a full-width line, at least 50 ms; × `slowdown`), the next line
  starting halfway through. Nothing slides out from under a clip, and the message may draw at most 4 past its
  height (the gap above the activity row). A line never starts more than 200 ms after it completed (past that, a
  burst cascades a frame apart rather than starting together); queued further than 100 ms behind, sweeps speed up
  (down to 0.4×). Lines held while their slot opens are released in order, each halfway through the one before
  (within the same cap). Text sweeps across its own laid-out width, cut into
  ~16 highlight `fade_out` segments from its glyph x positions (only lines mid-sweep; gpui blends a highlight's
  colour over the text's, so a transparent colour does nothing); a code line sweeps across its own width (its
  display width × the mono advance: tabs to the next stop of 4 (drawn as spaces), East Asian wide characters and
  emoji two cells; at most the box's visible width, the mask running under the right padding where
  long lines show) under a gradient mask of the box's colour. A code block draws only its complete lines given
  their space (nothing before the first; the box grows with its revealed lines); the box fades in during the
  first sixth of its first line's sweep, and that line's text sweeps
  in the rest (a mask under the box's own fade would let the whole line through). A list marker comes in as its
  item's first line starts. Text already there (a block opened mid-reply: otto's `session.view` carries the reply
  being written with what's been journaled, and live deltas continue it) and rewrapped lines after a width change
  show at once; later lines sweep as usual. Only the last row reveals: once any row follows a message (a group, the
  next text) it settles at once, full height and all shown, so its glide never fights the newer row's. Its rows
  keep their keys through `item.done` (text rows by their place among the item's texts, `item#tN`, not part index),
  so a finished reply looks as its last streaming frame did.
  Parsing goes through `markdown::Doc`: a top-level block followed by another is final, only the open last one
  parses again (at most once a frame). Reduce Motion: lines show at once, no glide.
- **Hover actions** under a finished answer: copy, retry.
- **Approvals and questions:** a card pinned above the composer (the one other framed element), keyboard: ⏎ allow,
  esc deny, plus "Always" when the decision supports it; questions show options + free text.
- **Scroll:** follows new activity while at the bottom; scrolling up (anywhere, including into an open group) stops
  following and shows `↓ New activity`; scrolling back to the bottom or the pill resumes. Opening or closing a group
  or detail doesn't change following (at the bottom it keeps following; scrolled up, the rows above stay put). "At
  the bottom" is the scroll position (within 3 pt of the end, `view::Tail`), not only the list's follow flag: back
  there (scrolled, or content settling) the pill hides, what was new counts as seen, and following resumes.
  **Inner boxes** (open `›` details, max 240; code blocks) scroll on their own (`agent/scroll.rs`): mono output,
  diffs and code scroll sideways to their longest line, no scrollbars. A gesture belongs to what it started on: over
  a box that can scroll that way, the whole gesture (momentum too) moves only the box, even at its edge or once the
  pointer left it; otherwise only the transcript moves. While a box takes a gesture the transcript doesn't follow
  its tail; afterwards the usual rules apply (back at the end it follows, else the pill).

## 4. Composer

```
        ╭────────────────────────────────────────────────╮
        │ ☰ Todos │ ⊘ Disable Hyperdrive que…   5/5 done ⌃│   tray tucked behind the box (todos / queued inputs)
        ╰─┬────────────────────────────────────────────┬─╯
  ╭───────┴────────────────────────────────────────────┴───────╮
  │  ⋮    th▍                                         ╭─────╮  │
  │                                                   │  ↑  │  │   round send button (always send; never stop)
  │                                                   ╰─────╯  │
  ╰────────────────────────────────────────────────────────────╯   rounded, soft focus ring
    BUILD ⌃              A\ / claude-opus-5-5 (pro)    12% · $0.42
```

- Multi-line, grows to ~8 lines then scrolls. ⏎ sends, ⇧⏎ newline. While a turn runs, ⏎ steers
  (`Delivery::Steer`, see §4f); queued inputs appear as chips in the tray. Stop lives on the activity row (§4f).
- `⋮` menu: approval mode (auto / ask), new session, sessions, Rename… (§5b), review changes, providers.
- Footer: agent (`BUILD`, …) picker; provider glyph / model id (subscription marker) → searchable model picker
  from `model.list` limited to signed-in providers; on the right, before the buttons, the context's used count (`48k`),
  muted, amber above 80%, red above 95%; its tooltip gives the window, exact counts, the session totals and cost
  (see §4b Footer).
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
- **Working in the icon slot:** while an agent block (or a sub-agent's) reports working, its header icon and, when
  it is the tab's focused block, the tab icon are the spinner (same 12 size, swapped in place like a loading
  browser's) and the trailing working spinner is dropped; a reported percent still shows as text. Needs you, done
  and error keep their trailing marks, with the sparkles back. A tab whose working block isn't its focused one keeps
  the trailing spinner (`block_status::spins`, `tab_mark`).
- **Column:** transcript and composer share one centered column, max 760 wide, 20 side padding. Vertical rhythm:
  20 between a user message and what follows, 12 between a group and text, trace lines 24 tall (13 text), prose
  13.5/1.55, code 12.5 mono.
- **Header:** no in-pane top bar; see §4d (the silo pane header carries the session controls).
- **Composer column** (notice, card, bars, pill, footer) floats over the transcript at the pane bottom (absolute,
  opaque pane background; only it takes the pointer, `occlude`), with a 28 fade above it from the pane background to
  transparent, so the transcript scrolls under it and fades out instead of being cut. Its height is measured each
  frame: the transcript's end (the activity row's room) is padded by it plus the fade, so the last row and the
  activity row scroll clear; a change (the input grows, a bar comes or goes) keeps a transcript at the bottom there.
  `↓ New activity` sits 8 above the column, empty states centre above it, and inner boxes under it don't take the
  wheel (`scroll::Inner::set_inset`).
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
  As built (context): the used count of `SessionInfo.context` on the footer's right, before the `brain` and image
  buttons (order: count, plan ring, `brain`, image; the buttons keep their place whatever the count or ring shows), 22 tall like the other footer controls (`999`, `9,999`, `12k`, `1.2M`), tinted by `used / window` (muted
  ink; `theme::warning` above 80%, `theme::danger` above 95%; `agent/ring.rs` keeps the 14 ring and these levels for
  the plan-usage ring); the window shows only in the tooltip:
  `48,213 of 200,000 tokens (24%)` and the session's `In 1.2M · Out 84k · Cache 900k · $0.31`. Hidden without a
  session or a known window; the model button has no
  usage tooltip and the model picker no usage header. A sub-agent's block shows its own.
  As built (plan usage, `agent/plan.rs`): after the count, before the buttons, the 14 ring of `agent/ring.rs`
  in a 22-tall slot, filled to the subscription's tightest window (highest `used`) and tinted by the same levels.
  Tooltip: a line per window, `5-hour: 62% · resets in 1h 40m` (relative under 24 h), `Weekly: 18% · resets Mon
  09:00` (local day and time), then the plan's name when the catalog knows it (`Claude Max`, `ChatGPT Plus`). A
  window past its reset reads 0% until the next report. Hidden for API keys and local models (no subscription
  sign-in for the provider) and while nothing is known. The pane calls `plan.usage` when it loads and follows
  otto's global `plan.usage` events; the session's `SessionInfo.plan` is the fallback. Static, so Reduce Motion
  has nothing to drop. A sub-agent's block shows its own provider's.
  As built: the subscription marker is the plan otto reports (` (Max)`, ` (Pro)`), ` (sub)` when it reports none.
  `brain` shows only when the model has `caps.reasoning` and opens the effort menu (also `/effort`): a 260 popover
  above it, right-aligned, rows `Default` (the agent's effort, else the model's), `Low`, `Medium`, `High`, `Max`,
  each with a short muted note and a check on the current one; arrows / ⏎ / esc as in the other pickers. `Default`
  sends `session.update {reset_effort}`, a level `{effort}` (or goes into the create options before the session
  exists). The brain is violet at any level but the default (or off) and its tooltip (`Effort: High`) follows a
  change made while hovered. otto-llm clamps an effort a model doesn't take to the nearest one it does (catalog
  `reasoning_options`; ties go up), e.g. `Max` is `xhigh` on GPT-5.2–5.5 and `high` on GPT-5.1.
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
  big + `Copy`, polling until done), with `Cancel`. Kimi Code and xAI device logins show their code the same way.
  When a browser login's redirect can't reach otto (a remote host's callback port is busy on this Mac, or there is
  nothing to forward) and the login takes a pasted address, the dialog shows a paste pill instead of waiting
  (`Paste the address you were sent to`); it completes the login with it (never also waiting: otto refuses a
  second waiting call). Antigravity instead restarts as Google's paste login (its page shows a code to paste).
  Logins that can't take one keep `Port N is in use on this Mac.` + `Use a code`.
  Antigravity first asks `Sign in with Google Antigravity?` / "Google's Antigravity terms forbid third-party use. Your
  Google account may be suspended." with `Cancel` / `Continue`; only Continue starts the login (opens the browser).
  A sign-in made elsewhere (the CLI) reaches open panes: otto watches its credential store and sends
  `auth.changed`, and every view of that otto reloads its providers.

## 4c. Commands and motion (user requests)

- **No global chord overrides.** ⌘N stays New Window everywhere. Agent actions live in the composer as slash
  commands: typing `/` at the start opens a filtered menu above the composer (↑/↓, ⏎/Tab pick, esc close):
  `/new` (new session), `/sessions` (switcher; ⌘J also), `/rename [title]` (§5b), `/model`, `/effort`, `/agent`, `/mode auto|ask`,
  `/providers`, `/clear`. Menus show no ⌘N hint.
- **Every state change animates** (silo `motion.rs`, `SILO_MOTION_SLOWDOWN`, reduce-motion → none), ~200–320 ms
  ease-out `cubic-bezier(0.25,1,0.5,1)`:
  - expand/collapse (todos tray, group fold/unfold, `›` details, popovers, slash menu, switcher): height + opacity,
    chevrons rotate;
  - things appearing/leaving (rows, trace lines, approval/question cards, attachment chips, tray, `↓ New activity`
    pill, empty → transcript, sign-in → composer): fade + 6–8 px slide from the side they come from;
  - things pushed up/down by others (transcript when the composer grows, tray/cards inserting above the composer,
    lines pushed by a new line): their position animates instead of jumping;
  - send circle dark ↔ light, status icon swaps (scale 0.8→1 + fade), shimmer on running titles, button
    hover/press fills; the activity row (§4f) fades + slides 6 in/out and eases its height.
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
  + muted arrow, has text → light circle + dark arrow (animated); running → stays send (§4f). Equal vertical and right inset
  around the circle; nothing touches the border.
- **Footer insets symmetric:** left item starts at the composer's text start (aligned with the placeholder), right
  icon ends at the send circle's center line mirrored (same inset as the left); model exactly centered. Image icon
  muted by default (no green); hover fill like other icon buttons. (Now the `plus` attach button, see §4g.)
- **List rows** (providers, sessions, models): one row height, logo column centered on the two-line text block,
  status/plan line in the same muted style, the action column right-aligned with equal widths — `Disconnect` sits in
  the same column and right edge as `Sign in`, as a quiet button of the same height. Primary = filled, secondary =
  outlined, quiet = text; same height and radius everywhere.

## 4e. As shipped (polish round 2)

- **Header:** `AgentPane::header_title()` (chat screen with a provider only) → the pane header shows it with a
  `chevron-down` (switcher) and adds `history` + `new-session` + Otto settings (`settings-shapes`, "Otto settings")
  before split/zoom/close, same buttons and hover (`header::actions`; none on a sub-agent). On sign-in / Otto
  settings the header shows the block title (`Agent`) and no session controls.
  Since 2026-10 the agent fills the header through `BlockHeader` (`pane_header.rs`, parts in `agent/header.rs`): the
  title + switcher is its middle, History / New Session its actions; the workspace keeps icon, status, pane
  buttons, drag and focus. Headers show per block type (Settings › Appearance › Block headers: Terminal, Agent;
  `app.json` `pane_headers`); browsers always show theirs (shrunk to nav + address when the others are off).
- **Pill primitive** (`agent/pill_input.rs`): `halo`, `pill`, `circle`, `circle_colors`, `lead_button` and metrics
  (`HEIGHT` 46, `HALO` 3, `CIRCLE` 36, `INSET` 5, `BORDER` 1, `LEAD` = circle center mirrored, `TEXT_X` 51) are
  shared by the dialog field and the composer (supersedes the 52/40 of §4c/§4d). Border 1 at 20% fg (30% focused),
  halo a faint 3-wide ring (3–3.5% fg); glyphs 16. Fills are opaque (bars tuck behind). Single-line fields scroll
  sideways.
- **Composer:** mono 14/20; the `⋮` is a circle mirroring the send circle (bottom-left, same size and inset, no fill
  at rest), text at `TEXT_X` = inset + circle + 10. Send circle bottom-right; dark → light when there is text (180 ms);
  it never turns into stop. Height eases between line counts (editor `animate_growth`), pushing the transcript
  smoothly. Footer: the agent label's glyphs start under the `⋮` circle's left edge and the attach glyph ends under
  the send circle's right edge (inset by the circles' inset; hover fills reach past them), model exactly centered;
  attach muted, always on (§4g).
- **Slash menu:** `/new /sessions /compact /model /effort /agent /mode auto|ask /providers /settings /clear` (`/clear` also
  drops attachments; `/compact` calls `session.compact` on the pane's otto: no sending until it returns (the draft
  stays), the compaction card in the thread and a count back to 0 after; refused inline with no
  session or while a turn runs, never queued; `/providers` and `/settings` open the Otto settings, §4h); fuzzy filter, ↑/↓, ⏎/Tab, esc (keeps
  the text, menu stays closed until the `/` goes); the current mode
  is checked. No chord overrides: ⌘N is New Window everywhere; menus show `/new`, `⌘J`, `/providers`.
- **Compaction card** (otto's `compaction.started` / `.delta` / `compaction` / `.failed`, `/compact` or auto): a
  tool-style row in the thread. Running: spinner, `Compacting context (auto)` or `(/compact)` shimmering, the summary
  streaming into a 160 tall box that follows its newest lines (scrolled up in it, it stays put); auto compaction
  ending a running turn is its indicator (Stop on the right, no activity row). Done: `history Context compacted · N
  messages summarised` (users' and the agent's messages since the checkpoint before), muted, a click opens the
  summary as markdown (selectable while open; otto's verbatim context for the model is left out). Failed: red with
  its error under it; stopped: muted `Compaction stopped`. A stored checkpoint (a reopened session) is the folded
  card; a threshold checkpoint opens its reply. Keyed `<checkpoint item>#compaction`, so the running card becomes
  the stored one in place.
- **Rows** (`agent/rows.rs`): one 44 tall row (28 icon column, title + muted subtitle, trailing check/note/buttons,
  selected/hover/pressed fills) for the switcher and provider rows; a small search pill above filterable lists.
  The composer's popups (`@`, `/`, model, agent and effort pickers, `⋮`) use `rows::menu`: one-line 28 rows, a 14
  icon, the description inline after the name (muted 11.5, cut with `…`), 4 popup padding with the row radius (6)
  nested in the popup's (10), and an empty state as one such row with its text where row text starts. Arrow keys
  work in every popover.
- **Motion** (`motion::{ease_ui, enter, Toggle, Glide}`, `UI` 260 ms / `UI_FAST` 180 ms, `cubic-bezier(0.25,1,0.5,1)`,
  `SILO_MOTION_SLOWDOWN`, Reduce Motion → none): popovers open/close (height, opacity, 6 pt slide), group fold and
  unfold and `›` details (height + opacity, chevrons turn), tray expand (chevron turns) and rise from behind the
  composer, approval/question cards rise in and sink away, new trace lines push older ones up, status icons swap in,
  attachment chips, `↓ New activity`, body switches (connecting / sign-in / empty / transcript), composer rise after
  sign-in, the activity row. Opening a group or detail leaves tail-follow as it is (§3 Scroll).
- **Live group motion** (one model): keys and `entered` times are stamped once per key (`transcript::arrivals`),
  so nothing re-enters on unrelated updates. A group appearing where the activity row was takes its place: the row
  hides at once and the header stays where `Thinking…` was (no re-entry; the row uses the gap the group would, and
  keeps the turn's clock). Each new line owes one line height until
  it arrived (summed over lines still entering): the window is that much shorter and its list that much lower, so
  the header and older lines stay put while the window grows (once full at 6, they slide up together); the new line
  fades and slides 6 in; a line pushed past the two newest dims with it; the top fade comes in with the 6th line.
  Streaming reasoning eases its height to what it measured and folds away when it ends. A finished step only
  crossfades its icon. Durations use tabular figures. A changed row is remeasured, not spliced (scroll anchor kept);
  streaming text is remeasured every frame while its reveal runs (§3 Assistant text).
- **Errors:** a session that can't be opened says so with a `New session` action (sending starts one); otto's auth
  hints (`run otto-silo auth login …`) become "connect it with /providers"; a turn's `Auth:` error reads "The
  provider didn't accept the sign-in · /providers" (full text in its details).
- **otto unreachable:** every request goes through the shared connection, which drops a cached connection whose
  daemon went away and redials through the server (which starts the daemon again). A send that still fails puts the draft back in the
  composer and says why in the notice slot above it.

## 4f. Running turn (review round 3)

- **Instant feedback:** ⏎ shows the user's bubble and the activity row at once, before otto confirms. The bubble
  (`optimistic-user`) is replaced in place, without fading in again, when the session has one more user item with that
  text; a failed send removes it and puts the text back in the composer.
- **Activity indicator:** while a turn runs (or a send waits up to 15 s for it to start) the transcript always ends in
  one. If the last row is a live group, its header is it; otherwise the list's last item is a slim row: shimmering
  `Thinking…` (nothing after the prompt yet) or `Working for 12s` (after text or a folded group, 4 above it). Both carry
  `· N queued` muted when inputs are pending, and a quiet `Stop  esc` text button right-aligned (text on the column
  edge, hover fill past it). The row fades + slides 6 in/out and eases its height. esc in the composer stops the turn
  when the composer is empty and nothing is open (an open popover, slash menu or approval card takes esc first).
- **Steer:** ⏎ while a turn runs sends with `Delivery::Steer`: otto injects it at the next step boundary and the
  agent continues without stopping. Placeholder `Message the agent`, send tooltip `Steer  ⏎`. ⌘⏎ stays New Pane.
- **Bars above the composer** (InputQueueBar/TodosBar/SubagentsBar of the otto web app): one chrome for all, top to
  bottom Sub-agents, Shells, Todos, Queue. The bottom-most bar tucks under the composer (top radius 12, open bottom); bars
  above it float (radius 12, full border, 6 gap). Each has a 34 header that crossfades between collapsed
  (`icon · Label | current item … note ⌃`) and expanded (`icon Title … note ⌄`, divider) with its 28 rows below
  (height + opacity, chevron turns), and rises from behind what's below (a bottom-anchored clip) when it appears.
  Wrappers must not carry negative margins: taffy then sizes the composer column wrong and the composer slides out.
  - **Shells** (`agent/jobs.rs`): the shown session's running background shell jobs, from the transcript (a
    `shell` result with a `job_id` and no `Background shell <id> …` report yet: `background: true`, a command
    waiting for input, or one the user detached). `terminal · Shells | first command … N running` (`Needs input`,
    warning, while one waits); a row per job: spinner (or `terminal` in warning), command, `Running` / `Needs
    input`; on hover the note gives way to `↗` (its block, when it runs in one; a click on the row opens it too)
    and `■` (`job.kill`; an older otto without it: closing its block). A row leaves when its report arrives.
  - **Detach:** a running foreground `shell` line (seen live) has a bordered `Detach` button before its `↗` / `■`
    (`shell.detach {session_id, call_id}`): the call returns `Started <job_id>` at once, the turn goes on, and the
    command runs on as a job (the Shells bar), reporting when it exits like a background one. Dimmed while the
    request is out; a refusal (it already ended) shows in the notice.
  - **Queue:** `clock · Queued | first … N queued`; expanded `Queued messages · N waiting`, numbered rows from otto's
    queue (`queue.changed` / `session.view`, empty subagent wake-ups hidden): pending steers first, noted `Next step`,
    then inputs waiting for the turn, `After turn`. A steer just sent shows from the pane until otto lists it (matched
    by its `input_id`, or its text before that arrives), so it appears at once and only once. On hover the note gives
    way (opacity, no layout shift: the note reserves the actions' width) to `Steer now` / `Send later`
    (`session.queue_update` to `steer` / `queue`) and `×` (`session.queue_remove`). Edits show at once and are dropped
    if otto refuses (an input already handed to the model can't be withdrawn; the notice says so). Leaving rows
    disappear without an exit animation (as other removed rows), and so does the bar when the queue empties.
  - **Todos:** current item and `n/m done`; expanded, every item with its status icon.
  - **Sub-agents:** child sessions (`parent_id` = the shown session) that are busy or waiting, from `session.list`,
    refreshed on otto's `sessions.changed` (coalesced over 150 ms) and when subagent calls change or the turn
  starts/ends; no polling. `bot · N running` (`Needs you` while a child waits), rows spinner (`Needs you` when waiting, question or lock icon; a muted stop square
  + `Stopped` while a child the user stopped holds its report, `SessionInfo.report_held`, with `Hand back` on hover in
  the note's place; `circle-check` + `Returned` once its report went to the parent) · agent · title · status, oldest
  first, returned last. A returned row stays while the parent's turn runs (or, the parent idle, through the turn its
  report starts) and fades out (opacity and height, 260 ms) when that turn ends. A row (hover fill, `↗` on hover)
  opens the child; so does `↗` on a transcript's subagent line.
  - **A waiting child's cards in the parent:** for each waiting row the pane fetches the child's `session.view`
    (on each session-list refresh) and, when the shown session has no card of its own, shows the oldest child's
    question or approval card above the composer, headed `bot Sub-agent · <title>`. Its options, the composer
    (typed answer) and Allow/Always/Deny answer the child (`question.answer` / `approval.answer` on the child); a
    child's approval takes a click (no ⏎/esc hints or keys). No wire change.
- **Sub-agent reports in the thread:** otto's `Subagent completion:` system item is a quiet `Row::Report` where it
  arrived (enters like any row, 12 gaps as a marker): `bot Sub-agent finished · <title>` (`stopped` muted, `failed`
  in the error colour), its error or the first line of its reply as the tooltip; a click opens the child. A waiting
  child's notice (`Sub-agent "<title>" (child <id>) asks: …` / `… needs approval: …`) is the same quiet row:
  `Sub-agent asks · <title>` / `Sub-agent needs approval · <title>`, the question or summary as the tooltip. A turn otto
  starts with a notice (a report, a job's end) never streams the previous reply again: text streams only while its
  item ends the transcript.
- **Sub-agent block:** opening a child focuses the agent block already showing it (the server's blocks carry
  `agent.session_id`), else splits a new agent block beside the pane (`block.split` kind agent in its folder, with
  `agent: {session_id, title}` so it shows the child from its first frame), composer focused. It is a normal agent
  pane on the child session, plus a 32 strip on top: `bot Sub-agent of <parent title>` and `← Main agent` (focuses
  the parent's block, or opens one); the pane header keeps the title but drops History / New Session. The composer takes
  the child over: ⏎ steers it while it runs, Stop/esc stops its turn, a message after that (or after it finished)
  starts a new child turn; approvals, questions and the model/agent/effort pickers apply to the child as in any
  session. Results still return to the parent (docs/OTTO.md §7.4). Stopped, it holds its report until the user hands
  it back: `↑ Hand back` in the strip beside `← Main agent` (or the bar row's), or closing its block
  (`session.hand_back`); a new message reports with its turn instead. The close is the silo server's: whenever the
  last agent block showing an otto session goes (block, tab or session closed; any window, device or the CLI),
  `silo server` calls `session.hand_back` for it once (`Config::agent_closed`); otto refuses sessions holding
  nothing, and that refusal is dropped quietly. Otherwise closing the block is only a view change: the child keeps
  running and reporting, and stays in the parent's bar.
- **Narrow panes** (a split can be 60 wide; `view::Fit`): nothing overlaps; text truncates with `…` and items drop as
  the pane narrows: the token count (<460), the plan usage ring (<450), the plan marker (<440), the provider logo
  (<340), bars' current item,
  sub-agent agent names and the strip's parent title (<320), effort/image (<270), the
  composer's `⋮` (<240, the slash menu covers it), labels
  beside icons (`Main agent`, bar labels, <230), the model (<200), the footer (<130), then the send circle (⏎
  sends), the strip's `Sub-agent` and `Stop  esc` (a stop icon instead) under 120. Side padding 20 → 8 under 300,
  the activity row's too.
  The composer's placeholder stays on one line.
- **Group header (Trace-style):** the summary (`Worked for 1s · ran 1 command`) starts at the column's left edge,
  the same x as reply text and every other row; the chevron follows it (8 gap) and turns when opened. Live: shimmer
  text, then a 12 spinner. Lines nest 12 under the header with their own 16 icon column. Opening a header or a `›`
  detail scrolled up keeps the clicked row and everything above it still (the list keeps its top anchor), only what's
  below moves; at the bottom the list keeps following (the opened rows push up from the composer). Header rows are 24 in every state.
- **Composer `⋮`:** a circle the size of send (36), no fill at rest, hover/press fill only.
- **Tool names** are always otto's own (`shell`, `read`, `edit`, …): providers get no predefined tools, and otto
  shows old sessions' provider-defined calls as calls to ours (docs/OTTO.md §5).
- **Command blocks** (docs/OTTO.md §6.2): a `shell` running in a parked command block gets `↗ Open` (reveals the
  block as a float, `block.reveal`) and a `■ Stop` (closes the block; the result reads `stopped (closed)`), from
  otto's `block:<id>` progress message. `waiting_for_input` turns the line into `Needs you` (warning terminal icon,
  the command as its desc, the waiting screen as its detail, Open and Stop); once the turn is over the block reports
  blocked. A background shell reads `Running <cmd> · background` with Open and Stop until its report arrives (a
  system item `Background shell <job> … exited with code N.`), then `Ran` / `Failed to run` with `exit N` (or
  `stopped`) and the reported output as its detail. Terminal lines (`terminal_open`, …) open their block the same
  way; a parked one floats in first. Opening a block that has closed says so.
- **Providers view:** every otto provider in daemon order (no fixed count), each with a monochrome logo
  (`provider-*.svg`; Custom uses `plug`, Antigravity the Google logo, Kimi Code Kimi's): Anthropic, OpenAI and xAI
  `API key` + `Sign in`; Kimi Code and Antigravity `Sign in` (`Kimi membership`, `Google account`); Google,
  OpenRouter, DeepSeek, Kimi, Z.ai, MiniMax `API key`;
  Ollama and LM Studio (`Local, no key needed`) and Custom (`OpenAI-compatible server`) `Connect` / `Change`. Connect
  opens the dialog with an address pill (prefilled with the current or default address, `http(s)://` required) and,
  for Custom only, an optional key pill below; it saves with `auth.login {base_url}` (no key needed). A local
  provider shows Connected once its server answers; otherwise the dialog says `Saved, but no server answers there`.

## 4g. Attachments

- **Any file:** the footer's `plus` (`Attach files`, the file picker), ⌘V and a drop anywhere on the pane attach
  files of any kind, several at once; folders are skipped. ⌘V takes copied files first, then text, then a copied
  image (as `Pasted image.png`). Over 25 MB: the notice says `name is over 25 MB` and the rest still attach.
- **Chips** (`agent/attach.rs`): name and kind glyph (a 16 px thumbnail for images, `file-text` otherwise), never a
  path; `×` removes one. They sit in their own row above the input row, spanning the pill from the buttons'
  inset (the `⋮` and send circle stay with the input row and reserve no space beside the chips); at narrow widths
  a chip's name ellipsizes down to its glyph. The row's height glides as chips come and go (the pill eases, the
  tail stays put). On send each file is uploaded with `attachment.put` (8 MB chunks) and the prompt names
  them (`InputPart::Attachment`); a failed send puts the text and the chips back.
- **Transcript:** a user message shows its files' chips above the bubble (thumbnails made in the background from
  the image otto sent with it, by `item#part`); otto's `[attachment …]` notes are hidden from the bubble. Older
  inline images show as `Image` chips. `copy_attachment` reads `Copied shot.png → assets/shot.png` with the size,
  failed `Couldn't copy shot.png: already exists` (the reason in a few words, no path); `read` of an attachment
  reads `Read shot.png` (the result's name, else the message's note). A model without vision isn't sent images
  (otto puts a `not sent` line in their place at request time; `read` answers with name, size and dimensions), but
  the image stays in the message, so its chip keeps the thumbnail.

## 4g2. Mentions

- **`@` popup** (`agent/mention.rs`): `@` at a word start opens a menu built like the slash menu (↑/↓, ⏎/⇥,
  esc keeps the text and stays closed until that `@` goes). Agents first (the pane's catalog, `bot`, at most 8,
  fuzzy on the name), then the project's files and folders from otto's `files.complete` (at most 50; ranked by
  otto, basename first; `.gitignore` respected). Queries wait 60 ms of quiet; older replies are dropped. A query
  naming a folder (`src/ma`) lists inside it. Until otto's first reply the agents show with a quiet, unpickable
  `Finding files…` row (never `No matches`). Moving the caret (keys, a click) reopens, refilters or closes it as
  typing does.
- **Chips:** a pick becomes an inline token in the editor (`@build`, `@src/main.rs`): a rounded fill, its icon
  (`bot`, `file-text`, `folder`) in place of the `@`. The caret skips over it, backspace removes it whole, an edit
  inside it makes it text. A folder chip gets no trailing space: typing on reopens it as a query (drilling in).
  A plain `@word` typed without picking stays text.
- **Sent:** the draft goes as text and `InputPart::Mention`s; the optimistic bubble, the transcript and replay show
  them inline as chips (from the user item's `mentions`); one otto couldn't find is struck through. A failed send
  puts the chips back. Retry re-sends the message's text with its mentions rebuilt from the item's `mentions`
  (its files aren't sent again). A `/command` message shows no chips (its expansion changes the text the ranges
  point into); otto still gets the note.

## 4h. Otto settings

Plan mode is gone (otto has no `plan` approval mode): no `/mode plan`, no Plan row in `⋮`, the agent picker's
fallback is just `build`.

Silo's Settings are for this Mac only; otto's are a view inside the agent pane (`agent/settings.rs`), shown in
the chat's place like the first-run sign-in, talking only to the pane's own otto (`connection(&self.target)`).
It uses the pane's full width at the pane's side padding (descriptions stop at 560). Title `Otto`, or
`Otto on <host>` for a remote host's; `‹ Back` or Esc returns to the chat (the agent editor takes
Esc first). Tabs (`settings::Tab::ALL`, a row of pills; MCP, skills and tools go there later):
- **Providers:** the sign-in list and dialogs of §4b (`provider_list`, shared with the first-run screen); API key
  (or Connect, or a signed-in row's action) in the rightmost column straight down the list, Sign in to its left.
- **Models:** otto's defaults for new sessions (`config.get` on open, `config.set` on every change; errors inline,
  in a few words): model (`Automatic` = the signed-in default, then the signed-in providers' models → `provider` +
  `model`), effort (Default = none set, Low, Medium, High, Max → `effort`; an agent's own effort wins), agent (`agents.list` →
  `agent`), approvals (Auto, Ask, Ask all → `approval_mode`), native compaction (→ `native_compaction`). "Applies to
  new sessions." A saved change reloads the pickers of every agent pane on the same otto (`client::OttoChanged`).
- **Agents:** the agent list and editor below (`agent/settings_agents.rs`) on the pane's project; a save or delete
  reloads the pickers of every agent pane on the same otto.
- **Search:** `search_engine` (DuckDuckGo, Bing, Google).

Entry points (`settings::entry_tab`): the header gear, `/settings` and the palette's `Otto Settings` open Models
(Providers while nothing is signed in); `/providers` and the `⋮` menu's Providers open Providers. `Otto Settings`
uses the agent pane in front, else opens a new agent tab on its settings on the active host when it runs agents
(else `Not supported by <host>` / `<host> is not connected`).

The **Agents** tab calls otto only once it shows. It works on the pane's project (git toplevel of its folder),
named by folder only.
- **List** (`agents.list`): name, scope badge (`Built-in`, `Yours`, `Project`), `Edited` for a definition named like
  a built-in (`overrides_builtin` from `agents.get`), the description on one muted line, a model chip (`Session
  model` when it inherits). Actions: `New`; Clone (`copy`, `agents.get` then a new agent `<name>-copy`, numbered when
  taken, its name focused and selected); Delete (`close`) or, for an override, Reset (`rotate-ccw`), each confirmed
  inline. Built-ins have no delete: editing one saves an override.
- **Editor** (`agents.get`): back, the name as title, Save (disabled while unchanged). Name, description, `Saved in`
  (`Yours` | `Project · <folder>`), model (`Session model` + the signed-in providers' models), effort (Default, Low,
  Medium, High, Max), approvals (`Session default`, Auto, Ask, Ask all). The prompt is a multi-line editor that grows
  to 28 lines then scrolls (⏎ newline, ⌘⏎ saves).
- **Tools** (`tools.catalog`): Default, Loadable, Off columns and a filter over all three. A tool is Default if it
  matches `tools`, Loadable if it matches `loadable`, else Off (`disallowed` is Off too); with both empty every tool
  sits in its own `loading` (`Defaults`). Chips: name, source glyph (built-in `bot`, silo `laptop`, MCP `plug`,
  extension `agent`), tooltip with description and effects. An MCP server is one `mcp__<server>__*` chip with its
  count; its menu shows its tools, each movable alone (the lists then name them; a server whose tools share a column
  is its pattern again). Drag chips between columns, or click one for `Move to …` (the keyboard path). Any move makes
  the lists explicit; `Use defaults` empties both. Patterns naming no listed tool (a server not running) are kept.
- **Saving:** `agents.save` with `previous_name` on a rename; moving an agent to the other scope saves it there and
  deletes the old definition. Problems show inline in a few words (`Name the agent`, `That name is taken`, `Grant a
  tool, or use defaults`, otto's reason without paths). Leaving with changes (back, another tab, Back to the chat)
  asks `Discard your changes?`; asking again discards. Esc closes a menu, then leaves the editor.

## 5. Session switcher

Click the title or `history` in the pane header, `/sessions`, or ⌘J with the agent block focused. A popover under the
header: the project row (`folder` · project folder name · `Project` · `chevron-right`), search pill, then one row per
session of the block's project (git toplevel of `cwd`), newest first: status dot, title (otto names a session after
its first turn and the title arrives live; the shown one falls back to its first prompt until then, others to
`Untitled`), `model · running · 2h`, check on the shown one; `New session` last. The selection starts on the first
session. Picking one calls `agent.set`; other sessions keep running.

The project row (⏎ or click) swaps the popover for the recent projects (`project.list`, most recent first): folder
name, the parent folder's name only when two share a name, `2h`, check on the current one; `Open folder…` last (the
native folder picker). Picking one moves the block there (`agent.set {cwd}`): it shows that project's sessions and a
new session, in every window. Paths are never shown.

Model, agent and brain picks apply to the shown session with `session.update` (from its next model request; the
pickers say `Applies from the next step` while a turn runs), shown at once and confirmed by `session.changed`; a
refusal drops the pick and shows otto's reason in the notice slot. Before the first prompt they go into the create
options.

A fork (`SessionInfo.forked_from`) is listed like any session, a `git-branch` glyph after its title ("Branched from
<title>"); it is never a sub-agent (no strip under its source, no back-link), though `parent_id` names its source.

## 5a. Branching

Hovering a user message (left of the bubble) or a turn's reply (after Copy and Retry) shows `git-branch` Branch.
It forks the session (`session.fork {at_seq}`) at that message and shows the fork in the same block (as a switcher
pick), titled `<title> (branch)` (`(branch 2)`, … past titles taken; a branch of a branch numbers its original's),
with `Branched from <title> · Back to original ×` above the composer. The fork holds the history through the
message: a user message is its last item (the composer stays as it was, ready to continue); a reply keeps the tool
results right after it. `at_seq` is the latest stored sequence of the kept items (`SessionMirror.item_seqs`).
While a turn runs Branch is disabled ("Stop the turn to branch"): the running turn's items aren't stored yet and a
fork mid-turn would copy a half-finished step. Not on a sub-agent's block; an otto without `item_seqs` says
"Update otto to branch".

## 5b. Renaming

The session's title is edited in place: double-click the header's title (a single click still opens the switcher),
the `⋮` menu's `Rename…`, a bare `/rename`, or a switcher row's `pencil` (on hover; the row's title becomes the field).
The field starts with the whole title selected; ⏎ saves (`session.rename`), esc or a click elsewhere cancels, and an
empty or unchanged title saves nothing. `/rename <title>` renames at once. The new title shows at once (header,
switcher row) and otto's `session.changed` confirms it live in every pane; a refusal puts the old one back and says
why in the notice slot.

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
steer while running, esc stop, activity row), live group and fold, `›` details, markdown, approval and question cards, todos tray, header
session controls, slash commands, ⌘J switcher, tab marks via `block.report`, sign-in and API-key dialog, restore after
an app restart, dark and light, `SILO_MOTION_SLOWDOWN=12` frames; queue rows (next step / after turn, switch,
remove), the 12-provider view and the endpoint dialog (dark and light, 2026-10-22, `/tmp/final4-*.png`). Live with the
Anthropic subscription: a turn steered mid-run with ⏎, the steer applied at the next step.

Known gaps:
- The app creates panes (agent and browser) only for the shown tab. An agent block in another tab reports no status
  until shown, and (pre-existing) a browser block in a tab not shown after an app restart answers browser actions
  with `no_app` until that tab is opened.
- New session (`/new`, header button) only clears the pane; the server keeps the previous `session_id` until the
  next prompt creates the new session, so a restart in between shows the previous session.
- Recorded times come from the coalesced journal, so a reloaded step can read up to 50 ms shorter than it did live;
  a forked session's copied items have none.
- The effort clamp knows only models in the bundled catalog; an unlisted one gets its provider's common set.
- Web tool lines match `web_search`/`web_fetch` on their metadata (`results`, `rendered`) and notes; a change to
  those shapes needs `transcript::tool` to follow.
- Remote-host agent blocks have no reconnect strip or host label yet; hover actions are copy and retry only; jump `↗` needs the block in a session this app
  can show. Leaving animations exist for popovers, cards, the tray list and the activity pill; removed transcript rows
  and attachment chips disappear at once.
- Sub-agents: an already-open child is found only in the shown silo session (otherwise a new block opens);
  `Returned` rows only cover children this pane saw out (running, waiting or held), so earlier turns' don't come back
  after a reload; a held report shows `Stopped` even after a reload. The close hand-back reaches the otto home of the
  silo server's environment (`otto_proto::paths::home`, the same daemon its `otto` service serves); with no otto
  daemon running it is skipped, and otto's next start reports the held turn. Any session with a
  `parent_id` (forks too, which the app doesn't create today) gets the sub-agent strip; ⌘J in a sub-agent block still
  opens the switcher.
- Live motion verified with `SILO_MOTION_SLOWDOWN=12` frames (handoff, arrivals under and past 6 lines, parallel
  arrivals, reasoning + tool, step finish, fold); hover hold, Reduce Motion and slowly streamed reasoning were not
  captured (the offline script streams instantly).
