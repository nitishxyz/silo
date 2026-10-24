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
  highlights: Map<path, [FileHighlight { start, end, note? }]>,  // marked lines per tab (files_show)
}
```

`highlights` was added after the first version: `serde(default)` and not written while empty, so older peers read
and write the state as before (an older server ignores `files.set`'s `highlights`; see `compat.rs`). Lines are
1-based and inclusive; at most 50 per file, notes up to 300 characters. A tab's highlights go when the tab closes.

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
| `files.set` | partial `FilesState` (open, active, tree, expanded, scroll, highlights) | the updated state, broadcast as `block.changed` |
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
- **Performance:** the Files viewer parses off the UI thread (a background task per read, keeping the old colours
  until the new ones are ready) and re-parses when `files.changed` re-reads the file. The agent pane's code blocks
  are small: they are parsed on the UI thread once per distinct text (`syntax::cached_lines`, the last 64 texts kept,
  up to 64 KB). Files over 1 MiB, or with lines over 10k characters, get no highlighting.
- **Theme:** a `syntax` table in Silo's theme, with keys such as keyword, function, type, string, number, comment,
  constant, variable.special, property, punctuation, operator, tag, attribute and title. Defaults come from Zed's One
  Dark and One Light syntax colours, mapped to the nearest Silo theme colour.
- **Reuse:** the agent pane's markdown code blocks (`agent/view.rs`) use the same highlighter for fenced code with a
  language (built). This is the second consumer, so the module is shared and not specific to files.

## 4. App

Built in `silo-app/src/files/` (`mod.rs`: state and calls, `view.rs`: header, tree and finder, `text.rs`: the text
viewer, `model.rs`: the logic without gpui, tested) and `files_block.rs` (the workspace side).

**Block header**, left to right:
- File tabs: name (with its folder when two open tabs share a name), a dot when the file changed on disk since the
  tab was last shown, × to close (shown on the active tab, and on hover), middle-click to close. The tab strip
  scrolls sideways when it overflows. With no tab open, the header shows the root's name and path.
- For a markdown tab: a Rendered | Source switch.
- A search button: find a file (⌘P).
- The tree toggle (a sidebar icon), last before the pane buttons (split, zoom, close).
- With every pane header hidden, a Files block keeps a shrunk header (its tabs only), as a browser does.

**Body:**
- **Viewer (left):**
  - Text and code: the terminal's monospace font with a line-number gutter, coloured by `crate::syntax`. Tabs are
    drawn as 4 columns; lines are drawn up to 4000 characters. Long lines scroll sideways.
  - Markdown: rendered with the agent pane's renderer (`agent::MarkdownView`: tables, task lists, highlighted code),
    or its source. Rendered markdown has no text selection; switch to Source to select and copy.
  - Images: fitted (never upscaled), with their type, pixel size and file size under them.
  - Binary files: a summary line, "binary · 3.2 MB · application/zip".
  - Truncated files (over 2 MiB): "Showing the first 2.1 MB of 14 MB" and a Load button, which reads up to
    `FileRead::MAX_BYTES` (12 MiB: a reply must fit in a frame).
  - Highlights (from `files_show`): a tint across the lines, an accent bar and accent line numbers in the gutter,
    and the note as a small label at the right of the range's first line.
- **Tree (right, 240 px to start, drag its left edge between 140 and 640 px):**
  - Lazy listing: the root, and each expanded folder when it opens; a disclosure arrow per folder.
  - Click opens a file in a tab and shows it. ⌘-click opens it without showing it. Click a folder to open or
    close it.
  - Hidden files are dimmed, links are in italics. `.git` is never shown.
  - Arrow keys navigate (once the tree has focus), → / ← expand and collapse (or move into / up to the parent),
    ↩ opens.

**Keys inside the block:**

| Key | Action |
|---|---|
| ⌘P | Find a file: the palette's scoring (`finder::score`) over `files.find`'s paths, a match in the file name first. Open tabs first while nothing is typed |
| ⌘W | Close the tab (or the open finder / find / go-to bar); with no tab open, close the block |
| ⌃Tab / ⌃⇧Tab | Next / previous tab |
| ⌘⇧E | Toggle the tree |
| ⌘F | Find in the file (smart case; ↩ / ↓ next, ↑ previous, ⌘G / ⇧⌘G too, esc closes) |
| ⌘L | Go to line (`12` or `12:5`) |
| ⌘C / ⌘A | Copy the selection / select all |
| ↑ ↓ ← → / PgUp PgDn / Home End | Scroll the viewer |

Selection works like the agent pane's: press and drag, double-click a word, triple-click a line, ⇧-click extends,
dragging past the top or bottom scrolls. It is kept on the text (lines and bytes), so it survives scrolling and a
re-read that keeps its lines.

**State:** every change (tabs, tree shown, expanded folders, each tab's top line, saved 0.6 s after scrolling stops)
goes through `files.set`; the server's `block.changed` brings other clients' and agents' changes, so two windows on
one block show the same tabs. On `files.changed` the shown file is read again (keeping how much was loaded), other
open files are dropped and get the dot, and listed folders whose entries changed are listed again.

**Opening a Files block:**
- The launcher's Files kind (row 4), as a tab or a pane. The root is the cwd of the split target or the focused
  block (a terminal's, an agent's or another Files block's root), else `$HOME` (the server's fallback).
- Typing a path in the launcher (`/…`, `~`, `~/…`) opens Files rooted at that folder. A file opens at its folder with
  the file shown (the server says it's not a folder, and the app tries again with its parent).
- The palette commands "New Files Pane" and "New Files Tab" (host feature `files`).
- Not built yet: "Open Folder…", `silo files [path]`, drag to reorder tabs, `files.root` from the app.

## 5. otto

`files_show { path, line?, highlight?: [{ start, end?, note? }] }` in silo-tools (`src/files.rs`), offered to otto by
otto-silo-tools. It is a deferred tool (loaded on demand, like the browser tools) and read-only.

- `path` is absolute, `~/…`, or relative to the agent's working directory (otto-silo-tools passes the session's
  folder per call, as `cwd`). It must exist.
- It reuses a Files block in the agent's window whose root holds the path (the deepest root), else splits one beside
  the agent's block, rooted at the agent's folder when the path is inside it, else at the file's folder.
- It opens the file's tab and shows it, scrolls so `line` (or the first highlight) is 5 lines from the top, and
  replaces that tab's highlights (none given clears them). A folder only opens or reuses the block.
- It returns `{ block_id, root, path, created, line, highlights }`, not the file's contents.

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
   (Built.)
4. **The CLI `silo files` command and the otto tool** (built as `files_show`, with highlights; the CLI is not).

**Later:** editing, diffs, PDF preview, a git status column in the tree, and search across files.

## 7. Open questions

- Limits on how many paths one block watches on Linux (inotify watches): watch only expanded directories and open
  files rather than the whole tree.
- Remote images: send the bytes over the control channel up to 20 MiB, or stream them over a data stream like
  terminal output? Start with the control channel and its cap.
