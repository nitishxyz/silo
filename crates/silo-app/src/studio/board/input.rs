//! The board's plain input events and the effects it answers with.

use silo_proto::studio::Op;

use super::geom::{Corner, Point};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub shift: bool,
    /// `⌘`.
    pub cmd: bool,
    /// `⌥`.
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub const NONE: Self = Self { shift: false, cmd: false, alt: false, ctrl: false };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A printable key, as typed (`'a'`, `'A'` with shift, `'/'`, `'0'`, `'='`, `'-'`).
    Char(char),
    Delete,
    Backspace,
    Escape,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Space,
}

/// Input, in view points (origin at the board's top-left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// `clicks`: 2 for a double click.
    Down {
        at: Point,
        button: Button,
        mods: Mods,
        clicks: u32,
    },
    Move {
        at: Point,
        mods: Mods,
    },
    Up {
        at: Point,
        button: Button,
        mods: Mods,
    },
    /// Two-finger scroll or a wheel: `delta` as the platform gives it (positive y: the content moves down);
    /// with `⌘` it zooms.
    Scroll {
        at: Point,
        delta: Point,
        mods: Mods,
    },
    /// Pinch: `factor` > 1 zooms in.
    Pinch {
        at: Point,
        factor: f64,
    },
    /// The pointer left the board.
    Leave,
    KeyDown {
        key: Key,
        mods: Mods,
    },
    KeyUp {
        key: Key,
        mods: Mods,
    },
}

/// What the view shows for the pointer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cursor {
    #[default]
    Default,
    /// Over something clickable (Run, a port, a wire, the stepper).
    Pointer,
    /// Space held, or over a node that can be dragged.
    Grab,
    Grabbing,
    /// Note and Frame tools; wiring.
    Crosshair,
    /// Over a prompt.
    Text,
    Resize(Corner),
}

/// What the view does after an event. Batches are sent in order with `studio.canvas_edit`; their result goes
/// back through [`super::Board::acked`] (or [`super::Board::failed`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Something drawn changed (camera, drafts, hover, the canvas): repaint.
    Redraw,
    /// The selection changed (the settings panel and the status bar follow).
    Selection,
    /// `studio.canvas_edit { ops, base_rev }`; already applied locally.
    Send {
        batch: u64,
        ops: Vec<Op>,
        base_rev: u64,
    },
    /// A menu opened: [`super::Board::menu`].
    Menu,
    CloseMenu,
    /// Show the canvas prompt (kind picker and text) at view point `view`; Enter calls
    /// [`super::Board::submit_prompt`].
    OpenPrompt {
        at: Point,
        view: Point,
    },
    /// The image/video viewer on a node.
    View {
        node: String,
    },
    /// The document panel on a document (root-relative): a document card opened.
    OpenDocument {
        path: String,
    },
    /// Edit a note's text in place.
    EditNote {
        node: String,
    },
    /// Focus a card's prompt.
    FocusPrompt {
        node: String,
    },
    /// Edit a frame's title in place.
    RenameFrame {
        node: String,
    },
    /// otto's `studio.run_card` / `studio.stop_card`.
    Run {
        node: String,
    },
    Stop {
        node: String,
    },
    Settings {
        node: String,
    },
    Takes {
        node: String,
    },
    /// Ask agent: these nodes as mentions in the linked agent's composer.
    AskAgent {
        nodes: Vec<String>,
    },
    /// The World view on an entity.
    OpenWorld {
        entity: String,
    },
    /// Pick an entity, then [`super::Board::add_entity`] at canvas point `at`.
    PickEntity {
        at: Point,
    },
    /// Pick files, `studio.import` them, then [`super::Board::place_imports`] at canvas point `at`.
    Import {
        at: Point,
    },
    SetAsEntitySheet {
        path: String,
    },
    RevealInFiles {
        path: String,
    },
    Copy {
        nodes: Vec<String>,
    },
    Save {
        path: String,
    },
    /// Ask, then move the file to the Trash (the node is removed with a batch of its own).
    DeleteFile {
        node: String,
        path: String,
    },
    /// Say this briefly (a wire refused, ops that failed).
    Notice(String),
}
