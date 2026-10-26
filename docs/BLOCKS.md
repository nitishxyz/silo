# Blocks in the app

Blocks are Silo's building blocks. The app (`crates/silo-app`) has one core
for what every block shares, so a kind is not re-implemented per place and a
new kind gets the shared behaviour for free. A kind brings its own pane (a
terminal's pty, a browser's page and address bar, an agent's composer, a
folder's tree and tabs) and a few small answers to the core.

## The core

| Piece | Where | What it is |
| --- | --- | --- |
| `Kind`, `kind_of` | `blocks.rs` | The client's kind of a block, from its `BlockInfo` (`Unsupported`: a newer server's kind). Fixed facts per kind: `icon` (its glyph), `has_folder` (a split starts in its cwd), `exit` (how it leaves the layout). |
| `BlockPane` | `blocks.rs` | One handle for a block's pane, whatever its kind. Its methods are exhaustive `match`es with no `_` arm, so a new variant is a compile error at every place that has to handle it. |
| `Blocks` | `blocks.rs` | `Workspace.blocks`: the one list of panes (id → `BlockPane` + its subscription). Shared code uses `get`, `iter`, `insert`, `remove`, `retain`. Only kind-specific code asks for a kind by name (`terminal`, `browser`, `agent`, `files`). |
| Workspace helpers | `blocks.rs` | `block_kind`, `block_pane`, `block_label` (the title every list and tab shows), `block_icon`, `block_with_focus`, `focus_block`, `has_pane`, `block_bounds`, `exit_body`. |
| `BlockRow` | `block_list.rs` | One row model for every list of blocks: ⌘P's Blocks, Browse Blocks (⇧⌘B, ⌘P on `#`) and the background menu. Kind (glyph), title, state, who started it, command, place; `detail`, `status`, `target`. `Workspace::pick_block` goes to a picked row from any list. |
| Motion | `motion.rs`, `workspace.rs` `track_motion`, `layout.rs` | Entering and leaving the tiled layout (open, close, split, float, park, dock), zoom. `pane_moves` decides who moves and never looks at the kind; `Kind::exit` and `BlockPane::last_frame` decide what the leaving ghost shows. |

## What a kind must provide

1. A pane type: a GPUI view with a `FocusHandle` it tracks, and `bounds()`
   (its content bounds from the last paint).
2. `pane_header::BlockHeader` for it: the header's middle, its own actions,
   optional progress, and the label (also the drag pill's and the ghost's).
3. A `BlockPane` variant, then an answer in each of its matches:
   `focus`, `has_focus`, `set_info` (the server's latest `BlockInfo`),
   `content_bounds`, `element`, `header_parts`, `title` (`None`: the server's
   title), `icon` / `favicon`, `backdrop`, `inactive_dim`, `set_card`,
   `set_control`, `unsaved` (the close guard), `last_frame`.
4. A `Kind` variant and its answers: `icon`, `has_folder`, `exit`; `kind_of`
   maps its `BlockKind` to it; `pane_header::header_mode` says whether its
   header can hide.
5. An `ensure_<kind>` that creates its pane on first sight and inserts it in
   `Workspace.blocks` with its subscription; `set_view` calls it from its
   `match` on `Kind`.

## What it gets from the core

- Opening and closing animations: it slides in when it joins the layout (a
  split, a dock) and out when it leaves it (closed, floated, parked), drawn
  live under an inert cover, or as its card when its pane is shown elsewhere.
  Zoom grows and shrinks it like any pane. A float that closes, parks or
  moves to another tab recedes where it was (shrinks a little and fades,
  `motion::float_exit`, `floats::floats_leaving`); one docked slides in.
- The header frame: icon slot (spinner, favicon or glyph), status mark,
  split, zoom and close buttons, click to focus, drag to swap or dock, the
  float's move handle, the progress line.
- Focus and focus navigation, the server's focus followed, refocus after an
  overlay (`refocus_block`).
- Card chrome: corners, outline, inactive wash or fade, Frosted veils,
  adaptive backdrop (when it reports one). The card clips its content to
  its rounded shape (`rounded_clip`, inside its outline), so a pane may fill
  its whole area with a square background; it never has to round its own
  corners to stay inside the card.
- Drag, drop and docking, floats, zoom and park: all by block id.
- Close: Close Pane, ⌘W, the header's ×, the busy-program dialog, and the
  unsaved-changes guard when `unsaved` lists anything.
- Server info: `set_info` on every view and `block.changed`.
- Listing: a `BlockRow` in ⌘P, Browse Blocks and the background menu, its
  tab's title and icon when it is focused.
- Reconnect: `set_control` with the host's new connection.

## Checklist for a new kind

- [ ] Server: the `BlockKind` variant and how it is created
      (`window.create` / `block.split` `kind`).
- [ ] `Kind` variant, added to `Kind::ALL`, with `icon`, `has_folder`, `exit`;
      `kind_of` maps it.
- [ ] The pane type and its `BlockHeader`.
- [ ] `BlockPane` variant; let the compiler list every match to fill in.
- [ ] `ensure_<kind>`, called from `set_view`'s `match` on `Kind`.
- [ ] `header_mode` for it (and a `PaneHeaders` setting if it can hide).
- [ ] The launcher's row (`launcher.rs` `Kind`, its command and shortcut).
- [ ] Kind-specific commands only where they are its own (a browser's
      reload, an agent's composer), asking `Workspace.blocks` for the kind.
- [ ] Tests: `blocks.rs` (`all_lists_every_kind_once`, glyphs),
      `motion.rs` (`every_kind_slides_in_and_out`), `block_list.rs`
      (`every_kind_lists_alike_in_a_tab_or_the_background`) cover it once it
      is in `Kind::ALL`.
- [ ] By hand: open it as a split (slides in), close it (slides out), float
      it, close the float (recedes) and dock one back, zoom it, park and
      show it, find it in ⌘P and ⇧⌘B.

## Kept per kind, on purpose

A terminal's mirror, checkpoints, cache and translucency; a browser's page
host (a parked or detached page runs on off-screen), its throttling when not
shown, its address bar and commands; an agent's otto connection, composer and
reconnect; a folder's tree, tabs, saving and file events. An agent's running
command block parks instead of closing (`close_confirm::pane_close`): that is
the server's rule for command terminals.
