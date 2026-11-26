//! The Studio canvas board's logic (docs/STUDIO.md §4.2), everything but drawing: pure, `f64` canvas units,
//! no gpui, clocks or I/O. The view owns a [`Board`], feeds it data and input, draws what it reads back and
//! carries out the [`Effect`]s.
//!
//! **Data in.** [`Board::new`]`(seed)` (a random seed: the board makes node ids `n_xxxxxxxx` and edge ids
//! `e_xxxxxxxx` from it). Then [`Board::set_canvas`] with every canvas the store reads (older revs are
//! ignored; another canvas name resets the board and fits it), [`Board::set_context`] with the project and
//! world sections, [`Board::set_view_size`] on layout (the first canvas is fitted once both are known),
//! [`Board::set_minimap_frame`] where the minimap is drawn (presses there pan), and
//! [`Board::measure_prompt`] with each card's measured prompt height (cards grow with their prompt; until
//! measured it is estimated).
//!
//! **Input.** [`Board::handle`] takes an [`Event`] (pointer down / move / up with button, clicks and
//! [`Mods`]; scroll; pinch; leave; key down / up) in view points and returns `Vec<`[`Effect`]`>`. The view
//! doesn't forward keys while a text field (a note, a prompt, a title, the canvas prompt) has focus, nor
//! pointer events over its own overlays (menus, the selection bar, the settings panel, the toolbar).
//! Handled: click / shift-click / marquee selection, `⌘A`, drag to move (snapping to nodes' edges and
//! centres and to frames' padded insides, `⌘` held: no snapping; dropped nodes join the frame their centre
//! lands in or leave theirs; a frame moves with its contents), corner resize (pictures keep their aspect,
//! `⇧`: free; cards change width, their height follows; a resized frame adopts what is wholly inside and
//! lets go of what falls out), Delete / Backspace (nodes with their wires; a selected wire unwires), `⌘D`,
//! `⌘Z` / `⌘⇧Z` (`⌘Y`), Escape (cancels a gesture, else closes the menu, else back to Select, else clears
//! the selection), space-drag, middle-drag and two-finger scroll pan, pinch and `⌘`-scroll zoom around the
//! pointer, `⌘=` / `⌘-`, `⌘0` fits everything, `f` fits the selection (with nothing selected it picks the
//! Frame tool; `⇧F` always does), `V` / `N` select tools, `G` or `/` opens the canvas prompt at the pointer,
//! arrows nudge (1, `⇧` 10), Enter edits / views the single selected node, `⌥⌘↩` asks the agent, `⌘C`
//! copies. Double-click: empty canvas (or a frame's body) → the canvas prompt; a note → edit; a card →
//! its prompt (its face with a take → the viewer); an asset → the viewer (a document → the document panel,
//! [`Effect::OpenDocument`]; Enter too); an entity → the World view; a
//! frame's title → rename. Single click on a card's Run → Run / Stop, on its stepper halves → the take
//! before / after, on its prompt → focus it; on an entity's name strip → the World view. Right press →
//! [`Effect::Menu`] (selects what is under it first).
//!
//! **Effects.** [`Effect::Send`] `{ batch, ops, base_rev }`: already applied locally; send it with
//! `studio.canvas_edit` (in order), then call [`Board::acked`]`(batch, &CanvasEdited)` (ids the server
//! made instead of the given ones are renamed, failed ops become a [`Effect::Notice`], effects waiting for
//! the batch are released: the canvas prompt's Run comes back from here) or [`Board::failed`]`(batch,
//! message)` (rolled back and forgotten). The rest are requests: `Redraw`, `Selection`, `Menu` /
//! `CloseMenu`, `OpenPrompt`, `View`, `OpenDocument`, `EditNote`, `FocusPrompt`, `RenameFrame`, `Run` / `Stop` (otto's
//! `studio.run_card` / `stop_card`), `Settings`, `Takes`, `AskAgent`, `OpenWorld`, `PickEntity` (then
//! [`Board::add_entity`]), `Import` (pick, `studio.import`, then [`Board::place_imports`]),
//! `SetAsEntitySheet`, `RevealInFiles`, `Copy`, `Save`, `DeleteFile`, `Notice`.
//!
//! **Drawing.** [`Board::camera`] (`to_view` / `to_canvas`, `zoom`), [`Board::visible`] (nodes bottom to top,
//! culled, drag drafts applied: a [`Laid`] with the rect and parts: a card's header, face at its aspect,
//! prompt, footer, Run, stepper; its ports with `port_center(i)` / [`Board::port_state`]; `output_center()`;
//! a frame's `title`; an asset's or entity's `face`; `canvas().nodes[laid.index]` is the node),
//! [`Board::wires`] (curves, selected / active), [`Board::wiring`] (the wire being dragged and what its drop
//! would do, refusals with their reason), [`Board::marquee`], [`Board::drawing`] (a note or frame being drawn),
//! [`Board::guides`] (snap lines), [`Board::hovered`] (port names on hover), [`Board::cursor`],
//! [`Board::selection`] / [`Board::selected_edge`] / [`Board::is_selected`], [`Board::minimap`],
//! [`Board::selection_bar`], [`Board::menu`], [`Board::toolbar`], [`Board::tool`].
//!
//! **Menus** are [`MenuEntry`] lists (an [`Action`], label, enabled + reason, shortcut, danger); the view
//! calls [`Board::choose`]`(&action)` (an open menu supplies its target and point; otherwise the selection
//! and the view's centre). The context menus, the selection actions (Visualize, Animate, Edit, Extend, Run /
//! Run again / Stop, Takes, Settings), the drag-to-create menu and the toolbar all go through it.
//!
//! **Edits from the view:** [`Board::submit_prompt`] / [`Board::close_prompt`] (the canvas prompt: Enter runs
//! the new card once the server has it, ⌥Enter makes the card only and focuses it), [`Board::set_prompt`],
//! [`Board::set_note`], [`Board::set_frame_title`], [`Board::step_take`], [`Board::pick_take`],
//! [`Board::change_setting`] and [`Board::unwire`] (the settings panel; [`Board::settings`] gives its fields
//! from the models, voices and preview otto gives the view), [`Board::reveal`] (jump to an input),
//! [`Board::add_document`] (Add to canvas: a document card at the view's centre),
//! [`Board::select`], [`Board::undo`] / [`Board::redo`], [`Board::fit_all`] / [`Board::fit_selection`],
//! [`Board::set_tool`], [`Board::set_camera`], [`Board::escape`].
//!
//! Every edit is one batch and one undo step (undo sends the inverse batch: [`apply`]). Ids of new nodes and
//! edges are made here and given in the ops, so a batch can add a card and wire it, and the view can select
//! and focus it before the server answers; `canvas_edit` answers ids in op order, and [`Board::acked`]
//! renames any it had to change.

pub mod apply;
#[allow(clippy::module_inception)]
mod board;
pub mod camera;
pub mod create;
pub mod geom;
pub mod input;
pub mod layout;
pub mod menu;
pub mod scene;
pub mod settings;
pub mod snap;
pub mod wire;

#[allow(unused_imports, reason = "the board's API as the guide above names it; tests and later views use the rest")]
pub use board::{Board, PortState, SelectionBar, WireDraft, WireView};
#[allow(unused_imports, reason = "the board's API as named above")]
pub use camera::{Camera, Minimap};
pub use create::{Imported, media_of_path};
pub use geom::{Corner, Point, Rect, Size};
pub use input::{Button, Cursor, Effect, Event, Key, Mods};
#[allow(unused_imports, reason = "the board's API as named above")]
pub use layout::{CardParts, Laid, Shape};
#[allow(unused_imports, reason = "the board's API as named above")]
pub use menu::{Action, Menu, MenuEntry, MenuGroup, MenuTarget, Tool};
#[allow(unused_imports, reason = "the board's API as named above")]
pub use scene::{Hit, Part, Scene};
#[allow(unused_imports, reason = "the board's API as named above")]
pub use settings::{CardSettings, ModelInfo, Preview, SettingChange, SettingsInput, VoiceInfo};
#[allow(unused_imports, reason = "the board's API as named above")]
pub use snap::Guide;
#[allow(unused_imports, reason = "the board's API as named above")]
pub use wire::{Curve, WireDrop, WireEnd};

#[cfg(test)]
pub(crate) mod tests;
