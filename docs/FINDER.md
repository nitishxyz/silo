# ⌘P finder: categories and fff search

⌘P searches everything at once today, with one-character prefixes (`>` commands, `:` settings, `@` sessions,
`#` blocks; `finder.rs` `Scope`). This adds named categories that become a chip in the field, and file and
content search through [fff](https://github.com/dmtrKovalenko/fff) (crate `fff-search`, MIT) on the server.

Decided on 2026-10-26: categories as chips with icons; the old prefixes stay as shortcuts; fff runs on the
server (files can be on a remote machine), its plain Rust build (no `zlob`/Zig); `@` in the agent composer is
unchanged (it attaches files there).

## 1. Categories and chips (app, `finder.rs`, `palette.rs`)

Typing `@` at the start of the field lists the categories, filtered as you type (`@fi` → Files). Tab or ↩ turns
the pick into a chip at the field's left; the rest of the text searches only that category. ⌫ on an empty field
removes the chip and leaves the field empty. Esc with a chip clears the chip first, then closes.
The old prefixes become their chip as soon as they're typed (`>` → Commands, `:` → Settings, `#` → Blocks,
`@` stays the category list). Browse Blocks (⇧⌘B) opens with the Blocks chip.

| Chip | Icon | Searches | ↩ |
|---|---|---|---|
| Files | `file-text` | files under the workspace's folder (fff) | open in a Files block (`path:42` at that line) |
| Grep | `search` | text inside files (fff) | the file at the line |
| Blocks | `layers` | every block: tiled, floating, parked | focus it |
| Spaces | `laptop` | workspaces (sessions), this Mac and other machines | switch |
| Tabs | `split-right` | tabs of this workspace | switch |
| Agents | `agent` | otto sessions of this workspace's project | open in an agent block |
| Commands | `terminal` | commands | run |
| Settings | `settings` | settings | open |
| Pages | `globe` | recent browser pages | open |
| Changes | `git-diff` | changed files of the workspace's repo | open in the Changes block |

The chip: a pill with the icon and the label (⌫ or Esc removes it), theme
`control_fill`. Each category row in the `@` list shows its icon, label and a one-line hint. Result rows keep
today's rows; Files and Grep rows show the path dimmed after the name, a git status letter (M/A/?) on the right,
and Grep rows the line with the match highlighted.

Without a chip: everything as now, plus a Files group (top 5) once the query has 2+ characters.

Workspace folder: `Workspace::new_block_folder` (the session's folder rules). Remote workspaces search on their
server. The server without feature `search` (older): Files and Grep are hidden.

## 2. Server search (silo-server, silo-proto; feature `search`)

- One fff `FilePicker` per folder in use (canonical root), created on first search, dropped after 10 minutes
  unused; at most 8 at once (least recently used goes). `FFFMode` for humans (not `Ai`).
- Frecency (LMDB) and query history at `<silo home>/search/` (the channel's home, so dev and release stay apart).
- `search.files { root, query, limit? (50, max 200) }` → `{ items: [{ path (root-relative), name, git: Option<status>,
  score }], total, indexing: bool }`. The query goes through fff's `QueryParser` (globs, `!excludes`,
  `git:modified`, `path:line:col`).
- `search.grep { root, query, mode?: plain|regex|fuzzy, limit? }` → `{ items: [{ path, line, col, text, ranges }],
  total, indexing }`, with a 300 ms time budget.
- `search.opened { root, path }`: records an open for frecency (the app sends it when ⌘P or a Files block opens a file).
- `root` must be a directory the caller may view: the folder of a block in a session it can see, or under it
  (same rule as Files blocks' roots). View rights.
- `files.find` (Files blocks) uses the same picker when the feature is on; its results and API are unchanged.

## 3. Tests

Server: a temp tree with a repo; files ranked (exact name first, frecency moves an opened file up), globs and
excludes, `git:modified`, grep plain/regex with line numbers, roots outside allowed folders refused, the picker
cache evicts, indexing flag while scanning. App: the `@` list filtering, chip from Tab/↩ and from each old prefix,
⌫/Esc, the query sent per chip, each category's rows and targets, Files hidden without the feature.
