# Changes block

A block that reviews code changes: what an agent's turn changed, what a session changed, the uncommitted work, or
a branch against another. It lives on the server that holds the session, like a Files block, so it works on remote
machines and survives restarts. The app only draws it. It reuses Files' parts (header tabs, the side list, syntax
highlighting, the folder watcher).

Decided on 2026-10-25:
- A kind of its own, `BlockKind::Changes`, not a mode of Files: scopes and review actions are a different job.
  It shares Files' code rather than copying it.
- otto snapshots the work tree at the start and end of every turn in a git repo, on by default
  (`turn_snapshots` in otto's config turns it off).
- The `git` command line does the work (no git library): the user's config, hooks, attributes and signing apply.
- Built in three phases (§7), in parallel: server and protocol, otto, app.

---

## 1. Model

`BlockKind::Changes`. Additive: an older app shows it as `Unknown` ("unsupported").

```
ChangesState {
  root: String,                 // the repo's top level (absolute); the block never reads outside it
  scope: ChangesScope,          // what is compared
  open: Vec<String>,            // open file tabs (repo-relative), in order
  active: Option<String>,       // the file shown; none: the summary
  list: bool,                   // the file list shown (a new block: hidden)
  split: bool,                  // side-by-side; false: unified (default)
  context: u32,                 // context lines (default 3)
  comments: Vec<ReviewComment>, // unsent review comments (§5)
  agent: Option<AgentLink>,     // the agent the review goes back to
}

enum ChangesScope (tag = "kind", snake_case) {
  Working,                                  // work tree + index vs HEAD, untracked included (default)
  Staged,                                   // index vs HEAD
  Unstaged,                                 // work tree vs index
  Tree { base: String, head: Option<String>, label: String },
      // between two tree ids (otto's turn snapshots); head None: the work tree now.
      // label: "Turn 4", "This session".
  Range { base: String, head: Option<String> },
      // revisions, e.g. base "main" head "otto"; head None: the work tree
}

ReviewComment { id: String, path: String, side: Side (old|new), line: u32, end_line: Option<u32>, text: String, created_at: u64 }
AgentLink { otto_session: String, block_id: Option<BlockId> }   // the agent block that owns the session, if any
```

`BlockInfo.changes: Option<Box<ChangesState>>`, sent in session views like `files`. The checkpoint saves it.
`serde(default)` and skip-when-empty everywhere, so peers read it as they would.

**Root:** the git top level of the given cwd (`git rev-parse --show-toplevel`), else the cwd of the block it was
opened next to, else the session's folder (`Workspace::cwd_of`). Not a git repo: the block opens and says so
("Not a git repository"); only `Tree` scopes from otto's fallback (§4) work there.

## 2. Server API (feature `changes`)

Every call takes `block_id`. Paths are repo-relative and stay under the root (as Files: canonicalised, no `..`,
no escaping symlinks). View rights to read, interact rights to change anything.

| Method | Params | Result |
|---|---|---|
| `changes.status` | — | `ChangesStatus { branch, head (short sha), upstream?, ahead, behind, files: [ChangedFile], totals: {files, added, removed}, truncated }` for the block's scope |
| `changes.diff` | `path`, `context?` | `FileDiff { path, old_path?, status, binary, hunks: [Hunk], truncated }` |
| `changes.set` | partial state: `scope, open, active, list, split, context, comments, agent` | the state, broadcast as `block.changed` |
| `changes.stage` / `changes.unstage` | `path`, `hunks?: [hunk ids]` | `ChangesStatus` |
| `changes.revert` | `path`, `hunks?` | `ChangesStatus`; work tree changes only, see below |
| `changes.commit` | `message`, `amend?` | `{ sha, summary }` |
| `changes.root` | `path` | re-root the block (interact rights) |

```
ChangedFile { path, old_path?, status: added|modified|deleted|renamed|copied|untracked|conflicted|type_changed,
              staged: bool, unstaged: bool, added: u32, removed: u32, binary: bool }
Hunk { id: String,           // stable: hash of the header and lines, so stage/revert name what the user saw
       old_start, old_lines, new_start, new_lines, header: String,   // the "@@ … @@ fn name" part
       lines: [DiffLine { kind: context|added|removed|no_newline, old: Option<u32>, new: Option<u32>, text }] }
```

**How:** `git status --porcelain=v2 -z --untracked-files=all`, `git diff --numstat -z` and
`git diff --no-color --no-ext-diff -U<context>` (with `--cached`, a range or two trees per scope). Untracked files
diff against `/dev/null` (`git diff --no-index`). `GIT_OPTIONAL_LOCKS=0` on every read, so a review never takes the
index lock from a running agent. Limits: 2,000 files in a status (`truncated`), 1 MiB of diff per file (`truncated`,
the hunks so far), a 10 s timeout per git command.

**Writing:**
- Stage and unstage a whole file with `git add` / `git restore --staged`; hunks with `git apply --cached`
  (`--reverse` to unstage) of a patch built from the hunks the user saw. A hunk id the file no longer has is
  `conflict` ("The file changed; review it again").
- Revert discards work tree changes: a whole file with `git restore` (an untracked file is moved to the Trash on
  macOS, deleted elsewhere), hunks with `git apply --reverse`. Staged changes are not reverted; unstage first.
  The app asks before a revert (§3).
- Commit runs `git commit -m` with hooks (no `--no-verify`); its output on failure is the error message.
- Scopes `Tree` and `Range` with a `head` are read-only: stage, unstage and revert answer `bad_request`.

**Events:** `changes.changed { block_id }`, debounced 300 ms: the Files watcher on the root, plus `.git/index` and
`.git/HEAD`, so the view follows an agent's edits, commits and branch switches.

**`compat`:** `feature::CHANGES = "changes"` in `compat::FEATURES`. An older server answers `method_not_found`;
the app's launcher hides Changes there.

**Built** (differences from the above):
- `ChangesStatus` has `repo: bool` (false outside a git work tree: nothing else filled); `branch`/`head` are
  `Option`s (detached, unborn). `ChangesState.marks: Vec<ChangeMark { path, line, note? }>` holds `changes_show`'s
  marks (§4; at most 50, `changes.set { marks }` replaces them). `FileStatus`, `LineKind` and `ChangesScope` have an
  `Unknown` (`serde(other)`) for newer peers. `changes.set`'s `agent: null` unlinks.
- Root: the nearest folder up from the cwd with a `.git` (read from the file system, not `git rev-parse`, as the
  block is made under the registry lock); the cwd is the split block's, else the focused block's (`window.create`),
  else `~` — the app passes the session's folder as `spawn.cwd` when it wants that. A split off an agent block
  links it (`agent: { otto_session, block_id }`).
- Hunk ids are a hash of the section and lines (not the `@@` numbers), so a hunk keeps its id when lines above it
  change; they depend on `context`, so writes use the block's `context`. Staging a hunk of an untracked file stages
  the file. Unstage before the first commit is `git rm --cached`.
- Revert in the `staged` scope is `bad_request`. In a `Tree`/`Range` scope without `head` (a turn against the work
  tree) revert goes back to the scope's base (the turn's tree, or the merge base), not just the index.
- Errors: a failing git command is code `git` with its output (hooks' too); a command over its timeout is `timeout`.
  `changes.commit` reads the message from stdin (`-F -`) and has 5 min, for hooks.
- Watcher: its own recursive watch on the root (`.gitignore`d paths dropped, the root's `.gitignore` and
  `info/exclude` only) plus the git dir's `index`, `HEAD`, `refs/`, `packed-refs` and merge heads.
- `changes_show` takes `turn_base` / `session_base` (tree ids) in its input from the host (otto); without them
  `turn`/`session` fall back to `working` and the result says so (`note`).

## 3. App

**Layout** (same shell as Files):

```
┌ ± Changes · term  [Working ▾] [Unified|Split]  view.rs  lib.rs  × ── +142 −37 · 5 files  ◧ ┐
│ @@ -805,7 +812,9 @@ fn queue_row                          │ ● Staged                         │
│   let text = one_line(&msg);                              │   agent/view.rs        +28 −4    │
│ - div().child(msg.clone())                                │ ● Changes                        │
│ + div().child(text)                                       │   silo-tools/lib.rs    +12 −3    │
│   [Stage hunk] [Revert hunk] [Comment]                    │ ● Untracked                      │
│ 💬 why not truncate at 80?                                │   block_list.rs        new       │
├───────────────────────────────────────────────────────────┴──────────────────────────────────┤
│ Commit message…                                     [Ask agent] [Commit ⌘↩]   2 comments · Send review │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Header:** the scope picker (Working, Staged, Unstaged, each of the linked agent's turns, This session, Branch…),
  Unified/Split, file tabs, totals, the list toggle.
- **No file open:** a summary of every file's hunks, scrolled as one (like a PR's Files tab); clicking a file
  header opens its tab.
- **Diff:** line numbers on both sides, added and removed lines tinted, word-level highlights inside changed lines,
  syntax highlighting from Files (`silo-app/src/syntax/`) on both sides, long unchanged runs folded ("12 lines").
- **List:** grouped Staged / Changes / Untracked / Conflicts; a checkbox stages or unstages a file; ⌘-click opens
  the file in a Files block at that line.
- **Actions per hunk** on hover: Stage (Unstage in Staged), Revert, Comment. Revert asks first, through the shared
  close-guard confirm card, and names what goes ("Discard 3 changed lines in view.rs?").
- **Keys:** `j`/`k` next/previous hunk, `n`/`p` next/previous file, `s` stage, `u` unstage, `r` revert, `c` comment,
  ⌘↩ commit, ⌥⌘↩ Send review.
- **Footer:** the commit message field; Commit is shown in Working and Staged scopes, Amend from its menu.
  "Ask agent" sends the linked agent "Write a commit message for the staged changes in this repo's style" and puts
  the reply in the field (§5). The review count and Send review when there are comments.
- **Live:** `changes.changed` re-reads the status and the open file; the scroll stays on the hunk it showed.
- **Block core:** a `BlockPane::Changes` arm in every shared match (docs/BLOCKS.md checklist), `Kind::Changes` with
  glyph `git-diff` (`has_folder: true`), the launcher's Changes row, New Changes Tab/Pane, ⌘P "Review Changes".

**In the agent thread:** when a turn ends with changes, a card under its reply:

```
 5 files changed  +142 −37                                   Review ›
 agent/view.rs · silo-tools/lib.rs · docs/BLOCKS.md · +2
```

"Review" focuses this agent's Changes block, or splits one off the agent block, with scope `Tree` from the turn's
`base_tree` to its `end_tree` (label "Turn 4") and `agent` linked. The composer's ⋮ menu gets "Review changes"
(scope: This session). Counts come from the turn (§4); the card shows nothing for a turn with no changes.

## 4. otto

**Turn snapshots** (`otto-core`, a small `snapshot.rs`):
- At `TurnStarted` and before `TurnFinished`, when the session's cwd is in a git work tree and `turn_snapshots`
  is on (default true), otto writes the work tree to a tree object without touching the user's index:
  `GIT_INDEX_FILE=<tmp copy of .git/index> git add -A && git write-tree`, `GIT_OPTIONAL_LOCKS=0`, a 10 s timeout,
  `.gitignore` respected. The tree ids go in the `turns` table (`base_tree`, `end_tree`; a migration).
  Failure never fails a turn; it's logged and the ids stay empty.
- A size guard: when untracked files would add more than 50 MB, the snapshot is skipped for that turn.
- The trees are reachable from nothing, so `git gc` may prune them after its grace period (2 weeks by default).
  That's acceptable; an older turn's Review then says "This turn's snapshot is gone".
- Not a git repo: no snapshot; the turn's summary comes from the edit records (`undo_edit`'s), paths only.

**Protocol (otto-proto), additive:**
- `TurnFinished` gains `changes: Option<TurnChanges>`: `{ base_tree, end_tree, files: [path], added, removed }`
  from `git diff --numstat <base> <end>` (at most 50 paths listed, the counts cover all). None when nothing changed
  or there's no snapshot.
- `session.view` turns carry the same, so a reopened thread shows its cards.
- The app sets `turn_snapshots` from the Otto settings page (a switch, "Snapshot each turn for review").

**Tools** (`silo-tools`, deferred like `files_show`):
- `changes_show { path?, scope?: "turn"|"session"|"working"|"staged", hunks?: [{ path, line, note? }] }`: opens or
  focuses the calling agent's Changes block on that scope (default: the current turn) and marks the hunks with
  notes ("check this", "the risky part"). Marks are kept like Files' highlights (at most 50, notes up to 300 chars).
- The review comes back as a normal user message (§5), so no tool is needed for it.

## 5. The review loop

1. Comments are added on lines or ranges (`c`, or the hunk's Comment) and kept in `ChangesState.comments`, so they
   survive a restart and show on every client.
2. **Send review** (⌥⌘↩) queues one message on the linked agent (`AgentLink`), through the same path as the
   composer, then clears the comments:

   ```
   Review of your changes (Turn 4):

   crates/silo-app/src/agent/view.rs:812 (new)
   > div().child(text)
   why not truncate at 80?

   docs/BLOCKS.md:14-20
   ...
   ```

   With no linked agent, Send review picks the agent block in this session (more than one: a menu).
3. "Ask agent" for a commit message queues a message asking for one; the reply's first fenced block (or its text)
   fills the field when it ends. It doesn't commit by itself.

## 6. Tests

- Server (`silo-server/tests/changes.rs`, a temp repo per test): status per scope with every file status, untracked
  and binary files, renames; diff hunks and line numbers; stage/unstage/revert of a file and of one hunk among
  three; a stale hunk id is `conflict`; commit with and without a hook that fails; read-only scopes refuse writes;
  paths outside the root refused; `changes.changed` after an edit, a commit and a branch switch; reads never take
  `index.lock` while another process holds it.
- otto: a turn in a temp repo records both trees and its `TurnChanges` (edit tool and `shell` edits both
  counted); the user's index is unchanged byte for byte; a non-repo turn and a failing git leave the turn done;
  `turn_snapshots: false` takes none.
- App: the diff view model (parsing `FileDiff` into rows, folding, word highlights, split pairing), the list
  grouping, the review message text, the turn card's text, every shared `BlockPane` match covers Changes
  (`Kind::ALL`), the scope picker's rows.
- Tools: `changes_show` opens the caller's block in its own session, scope and marks.

## 7. Order of work

Three subagents in parallel, in the same tree (each stages only its own paths):

1. **Server + protocol** (`silo-proto`, `silo-server`, `silo-tools`): the model and params first (one commit the
   others build on), then status and diff, then the writes, the watcher, `changes_show`, `protocol.json`.
2. **otto** (`otto-core`, `otto-proto`, `otto-store`, `otto-server`): snapshots, `TurnChanges`, the migration,
   the config switch.
3. **App** (`silo-app`): the block (view model first, against fixtures), the core matches, the launcher, the turn
   card, the review loop, the commit footer, the Otto settings switch.

Phase 1 (read only: scopes, diff, list, card) lands before the writes in each; phases 2 (stage, revert, commit)
and 3 (comments, Send review, `changes_show`, Ask agent) follow in the same agents.

## 8. Open questions

- Conflicts: show both sides and "Mark resolved" (`git add`), or only list them for now? (Listed only, first.)
- Pushing and pull requests: out of scope here; a later Push button and a `gh pr create` flow.
- Large monorepos: status over 2,000 files is truncated; a per-folder scope may be needed.
