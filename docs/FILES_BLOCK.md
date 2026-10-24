# Files block

A block that shows files: a file tree, a viewer with syntax highlighting, and tabs for open files. Like a terminal,
it lives on the server that holds the session, so it works for remote machines and survives restarts. The app only
draws it.

Decided on 2026-10-24:
- File tabs live in the block header.
- The file tree is on the right, and a button in the header collapses it.
- The first version is read-only. Editing comes later.
- Syntax highlighting uses tree-sitter, following Zed's grammars and `highlights.scm` queries.
- It opens from the block launcher. `launcher.rs` already has a slot for the Files kind.

---

## 1. Model

`BlockKind::Files`. This is additive: an older app shows it as `Unknown`, which already renders as "unsupported".

Each block's state is saved with it, the same way a browser block's URL is:

```
FilesState {
  root: String,              // absolute path; the block never reads outside it
  open: Vec<String>,         // open tabs (paths relative to root), in order
  active: Option<String>,    // the tab shown
  tree: bool,                // tree expanded (default true)
  expanded: Vec<String>,     // expanded directories in the tree
  scroll: Map<path, line>,   // per-tab top line, so tabs reopen where they were
}
```

This goes in `BlockInfo.files: Option<FilesState>`, sent in session views like `browser` is. The checkpoint saves it,
so restore and upgrade keep it.

**Root:** an explicit `cwd` or path when given. Otherwise it is the cwd of the block it was opened next to (the
launcher's split target or the focused block). If neither exists, it is `$HOME`.

## 2. Server API

Every call takes `block_id` and paths relative to the block's root. They need view rights on the block, or interact
rights to change its state.

| Method | Params | Result |
|---|---|---|
| `files.list` | `path` (dir) | entries: `name, kind (file/dir/symlink), size, mtime` |
| `files.read` | `path`, `max_bytes?` (default 2 MiB) | `text` (UTF-8) or `binary` (`mime`, `size`), `truncated`, `mtime`, `size` |
| `files.read_image` | `path` | bytes plus `mime`, for png, jpeg, gif, webp and svg, up to 20 MiB |
| `files.find` | `query`, `limit` | fuzzy-matched paths under root; skips `.git`, `target`, `node_modules` and `.gitignore`d paths |
| `files.set` | partial `FilesState` (open, active, tree, expanded, scroll) | the updated state, broadcast as `block.changed` |
| `files.root` | `path` | re-root the block (interact rights) |

**Events:** `files.changed { block_id, paths }`. The server watches the root (FSEvents on macOS, inotify on Linux,
through the `notify` crate), debounced to 150 ms, and sends changed paths relative to the root. Clients re-read
paths that are open, and re-list directories that are expanded.

**Safety:**
- Paths are joined to the root, canonicalised and must stay under the root's canonical form. That rejects `..` and
  symlinks that escape.
- Symlinks inside the root are followed, and their targets are checked too.
- There are size caps, and no writes.

## 3. Syntax highlighting (app side, `silo-app/src/syntax/`)

This follows Zed (`crates/grammars`, `crates/language`), but in a much smaller form, without a `Buffer`,
`SyntaxMap` or language registry.

- **Crates:** `tree-sitter` plus grammars at the same versions as Zed's workspace (`tree-sitter = 0.26`). Languages:
  rust, toml, json, markdown (block and inline), javascript, typescript, tsx, python, go, bash, c, cpp, css, html,
  yaml and diff. Toml has no grammar in Zed's set, so it uses the `tree-sitter-toml-ng` crate.
- **Queries:** Zed's `crates/grammars` queries are **GPL-3.0**, so none are copied. Instead, each grammar crate's own
  MIT `queries/*.scm` (`highlights.scm`, and `injections.scm` where it exists) is copied into
  `silo-app/src/syntax/queries/<lang>/` with its origin and licence header. `queries/NOTICE` lists the sources and our
  changes. Zed is a reference for how the pieces fit together only. (Built: `583696e`.)
- **Language detection:** by extension and file name (`Cargo.lock` is toml, `Makefile` and `*.sh` are bash, and so
  on). A shebang line is the fallback. Anything else is plain text.
- **Highlighting:**
  - Parse the whole file with `tree_sitter::Parser`, then run `QueryCursor` over the highlights query.
  - Map capture names to theme colours as Zed's `HighlightMap` does: longest dotted prefix wins, so
    `function.method` falls back to `function`.
  - The output is a list of `(byte range, style)` per line, passed to `StyledText::with_highlights` as the agent
    pane's code blocks already are.
- **Performance:** parse off the UI thread. Cache the tree and highlights per path and mtime. Re-parse on
  `files.changed`. Files over 1 MiB, or with lines over 10k characters, get no highlighting.
- **Theme:** a `syntax` table in Silo's theme, with keys such as keyword, function, type, string, number, comment,
  constant, variable.special, property, punctuation, operator, tag, attribute and title. Defaults come from Zed's One
  Dark and One Light syntax colours, mapped to the nearest Silo theme colour.
- **Reuse:** the agent pane's markdown code blocks (`agent/view.rs`) use the same highlighter for fenced code with a
  language. This is the second consumer, so the module is shared and not specific to files.

## 4. App

**Block header**, left to right:
- File tabs: name, a dot when the file changed on disk since it was opened, × to close, middle-click to close, drag
  to reorder.
- `…` when tabs overflow.
- ⌘P to find a file.
- The tree toggle (a sidebar icon), on the right edge.

**Body:**
- **Viewer (left):**
  - Text and code: monospace with a line-number gutter and syntax highlighting.
  - Markdown: rendered with the agent pane's renderer (tables, task lists, code highlighting), with a header toggle
    to switch between Rendered and Source.
  - Images: shown fitted, with their size.
  - Binary files: a summary line, "binary · 3.2 MB · application/zip".
  - Truncated files: a note and a "Load all" button, up to 20 MiB.
- **Tree (right, about 240 px, resizable):**
  - Lazy listing, with a disclosure arrow per folder.
  - Click opens a file in a tab. ⌘-click opens it without switching to it.
  - Hidden files are dimmed. `.git` is never shown.
  - Arrow keys navigate, and → / ← expand and collapse.

**Keys inside the block:**

| Key | Action |
|---|---|
| ⌘P | Find a file. The fuzzy finder reuses `agent/finder.rs` scoring and calls `files.find` |
| ⌘W | Close the tab; the last tab closes the block |
| ⌃Tab / ⌃⇧Tab | Next / previous tab |
| ⌘⇧E | Toggle the tree |
| ⌘F | Find in the file |
| ⌘L | Go to line |
| ⌘C | Copy the selection |

Selection and copy work like the agent pane's text selection (`agent/select.rs`).

**Opening a Files block:**
- The launcher's Files kind.
- Typing a path in the launcher opens Files rooted at that directory, or at the file's directory with the file open.
- The palette commands "New Files Pane" and "Open Folder…".
- `silo files [path]` on the CLI.

## 5. otto

A `files_open { path, line? }` tool in silo-tools (and otto-silo-tools). It opens or reuses a Files block next to the
agent's block, then opens the file and scrolls to that line. This is how an agent says "look at this".

**Later:** live edits. When otto edits a file that a Files tab has open, the tab shows the change as it lands, which
`files.changed` already gives us. A diff view against `HEAD` comes after that.

## 6. Order of work

1. **Server and protocol:**
   - `BlockKind::Files` and `FilesState`.
   - The `files.*` methods, the watcher and `files.changed`.
   - Saving the state with the block.
   - Path safety.
   - Tests in `silo-server/tests/files.rs`: list, read, find, symlink escape, size caps, watch events, state that
     survives a restart.
2. **Syntax module (app):** tree-sitter, the copied queries, the highlight map, the theme table, and tests (expected
   captures for a small sample per language). Switch the agent pane's code blocks to it.
3. **Files block view:** header tabs, the tree on the right, the viewer, the finder, keys, and the launcher kind.
4. **The CLI `silo files` command and the otto `files_open` tool.**

**Later:** editing, diffs, PDF preview, a git status column in the tree, and search across files.

## 7. Open questions

- Limits on how many paths one block watches on Linux (inotify watches): watch only expanded directories and open
  files rather than the whole tree.
- Remote images: send the bytes over the control channel up to 20 MiB, or stream them over a data stream like
  terminal output? Start with the control channel and its cap.
