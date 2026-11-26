//! Menus as data: the context menu on empty canvas, on each kind of node, on a selection and on a wire; the
//! selection actions over a single node; the drag-to-create menu; the toolbar. The view draws entries and
//! calls [`super::Board::choose`] with the chosen [`Action`].

use silo_proto::studio::{Canvas, EdgeRole, GenKind, GenStatus, NodeBody, Section, SourceMedia, StudioMedia, default_port};

use super::wire::{KINDS, WireEnd, create_options, kind_label};

/// The board's pointer tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Tool {
    /// Select, move, resize, wire.
    #[default]
    Select,
    /// Click (or drag a rect) to place a note; then back to Select.
    Note,
    /// Drag a rect (or click) to make a frame around what is inside; then back to Select.
    Frame,
}

/// Something a menu entry, a selection action or a toolbar button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Empty canvas: a `makes` card at the menu's point, its prompt focused.
    Generate(GenKind),
    NewNote,
    NewFrame,
    /// The view picks an entity, then calls [`super::Board::add_entity`].
    AddFromWorld,
    Run,
    Stop,
    Settings,
    Takes,
    /// A `makes` card wired from the node (or, in the drag-to-create menu, the wire's end) into `port`:
    /// Make from this, Visualize, Animate, Edit, Extend, drag to create.
    Make {
        makes: GenKind,
        port: EdgeRole,
    },
    /// Drag to create into a `text` port: a note feeding it.
    MakeNote,
    AskAgent,
    SetAsEntitySheet,
    RevealInFiles,
    Copy,
    Save,
    Duplicate,
    Delete,
    /// Assets: move the file to the Trash (the view asks first).
    DeleteFile,
    /// Open the image/video viewer.
    View,
    /// A document card: open the document panel.
    OpenDocument,
    EditNote,
    OpenInWorld,
    /// A frame's title.
    Rename,
    Unwire,
    Tool(Tool),
    /// The canvas prompt at the view's centre.
    Prompt,
    /// The view opens a file picker, imports, then calls [`super::Board::place_imports`].
    Import,
    /// Empty canvas: the server re-flows the whole canvas so nothing overlaps (`tidy`).
    Tidy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuEntry {
    pub action: Action,
    pub label: String,
    pub enabled: bool,
    /// Why it is disabled (a tooltip).
    pub reason: Option<String>,
    /// For display (`⌘D`).
    pub shortcut: Option<&'static str>,
    /// Destructive: drawn in red.
    pub danger: bool,
    /// Toolbar: the current tool.
    pub active: bool,
}

impl MenuEntry {
    #[must_use]
    pub fn new(action: Action, label: impl Into<String>) -> Self {
        Self { action, label: label.into(), enabled: true, reason: None, shortcut: None, danger: false, active: false }
    }

    #[must_use]
    pub fn key(mut self, shortcut: &'static str) -> Self {
        self.shortcut = Some(shortcut);
        self
    }

    #[must_use]
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// Disabled with `reason` when it is `Some`.
    #[must_use]
    pub fn unless(mut self, reason: Option<String>) -> Self {
        self.enabled = reason.is_none();
        self.reason = reason;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuGroup {
    pub title: Option<String>,
    pub entries: Vec<MenuEntry>,
}

impl MenuGroup {
    fn new(title: Option<&str>, entries: Vec<MenuEntry>) -> Self {
        Self { title: title.map(str::to_owned), entries }
    }
}

/// What a menu is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuTarget {
    /// Empty canvas (in frame `frame`, if any).
    Canvas {
        frame: Option<String>,
    },
    Node(String),
    /// Several selected nodes.
    Selection(Vec<String>),
    Wire(String),
    /// The drag-to-create menu for a wire dropped on empty canvas.
    Create(WireEnd),
}

/// An open menu: where (view points, and the canvas point things made from it go), about what, its groups.
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    pub at: super::geom::Point,
    pub canvas_at: super::geom::Point,
    pub target: MenuTarget,
    pub title: Option<String>,
    pub groups: Vec<MenuGroup>,
}

/// What a node outputs, and why it can't give it now (`Run Keyframe 3 first`).
fn output(canvas: &Canvas, sections: &[Section], id: &str) -> Option<(SourceMedia, Option<String>)> {
    canvas.output_of(id, sections).map(|o| (o.media, o.problem))
}

/// The file a node shows (an asset's, an entity's sheet, a card's picked take).
#[must_use]
pub fn path_of(canvas: &Canvas, sections: &[Section], id: &str) -> Option<String> {
    match &canvas.node(id)?.body {
        NodeBody::Asset { path, .. } => Some(path.clone()),
        NodeBody::Gen { .. } | NodeBody::Entity { .. } => canvas.output_of(id, sections)?.path,
        _ => None,
    }
}

/// Run, Run again or Stop for a card, by its status.
fn run_entry(body: &NodeBody, wired: bool) -> Option<MenuEntry> {
    let NodeBody::Gen { status, takes, prompt, .. } = body else { return None };
    Some(match status {
        GenStatus::Queued | GenStatus::Running => MenuEntry::new(Action::Stop, "Stop"),
        GenStatus::Failed => MenuEntry::new(Action::Run, "Retry"),
        _ if takes.is_empty() => MenuEntry::new(Action::Run, "Run")
            .unless((prompt.trim().is_empty() && !wired).then(|| "Write a prompt or wire an input first".to_owned())),
        _ => MenuEntry::new(Action::Run, "Run again"),
    })
}

/// Make from this: a card of each kind that can take what `id` outputs.
fn make_entries(canvas: &Canvas, sections: &[Section], id: &str) -> Vec<MenuEntry> {
    let Some((media, problem)) = output(canvas, sections, id) else { return Vec::new() };
    KINDS
        .iter()
        .filter_map(|&k| {
            default_port(k, media, &[]).map(|port| MenuEntry::new(Action::Make { makes: k, port }, kind_label(k)).unless(problem.clone()))
        })
        .collect()
}

/// The context menu on empty canvas.
#[must_use]
pub fn canvas_menu() -> Vec<MenuGroup> {
    vec![
        MenuGroup::new(Some("Generate here"), KINDS.iter().map(|&k| MenuEntry::new(Action::Generate(k), kind_label(k))).collect()),
        MenuGroup::new(
            None,
            vec![
                MenuEntry::new(Action::NewNote, "Note").key("N"),
                MenuEntry::new(Action::NewFrame, "Frame").key("F"),
                MenuEntry::new(Action::AddFromWorld, "Add from World…"),
            ],
        ),
        MenuGroup::new(None, vec![MenuEntry::new(Action::Tidy, "Tidy up")]),
    ]
}

/// The context menu on node `id`.
#[must_use]
pub fn node_menu(canvas: &Canvas, sections: &[Section], id: &str) -> Vec<MenuGroup> {
    let Some(node) = canvas.node(id) else { return Vec::new() };
    let wired = canvas.edges.iter().any(|e| e.to == id && e.role.is_port());
    let path = path_of(canvas, sections, id);
    let document = canvas.document_of(id).is_some();
    let image = output(canvas, sections, id).is_some_and(|(m, _)| m == SourceMedia::Image) && path.is_some();
    let mut groups = Vec::new();
    let mut first = Vec::new();
    match &node.body {
        NodeBody::Gen { takes, .. } => {
            first.extend(run_entry(&node.body, wired));
            if takes.len() > 1 {
                first.push(MenuEntry::new(Action::Takes, "Takes"));
            }
            first.push(MenuEntry::new(Action::Settings, "Settings"));
        }
        NodeBody::Note { .. } => first.push(MenuEntry::new(Action::EditNote, "Edit")),
        NodeBody::Asset { media: StudioMedia::Image | StudioMedia::Video | StudioMedia::Audio, .. } => {
            first.push(MenuEntry::new(Action::View, "View"))
        }
        NodeBody::Asset { media: StudioMedia::Text, .. } => {
            first.push(MenuEntry::new(Action::OpenDocument, "Open").key("↩"));
            first.push(MenuEntry::new(Action::RevealInFiles, "Open in Files"));
        }
        NodeBody::Entity { .. } => first.push(MenuEntry::new(Action::OpenInWorld, "Open in World")),
        NodeBody::Frame { .. } => first.push(MenuEntry::new(Action::Rename, "Rename")),
        _ => {}
    }
    groups.push(MenuGroup::new(None, first));
    groups.push(MenuGroup::new(Some("Make from this"), make_entries(canvas, sections, id)));
    let mut rest = vec![MenuEntry::new(Action::AskAgent, "Ask agent about this").key("⌥⌘↩")];
    if image {
        rest.push(MenuEntry::new(Action::SetAsEntitySheet, "Set as entity sheet…"));
    }
    if path.is_some() && !document {
        rest.push(MenuEntry::new(Action::RevealInFiles, "Reveal in Files"));
    }
    if !matches!(node.body, NodeBody::Frame { .. }) {
        rest.push(MenuEntry::new(Action::Copy, "Copy").key("⌘C"));
    }
    if path.is_some() && !document {
        rest.push(MenuEntry::new(Action::Save, "Save…"));
    }
    rest.push(MenuEntry::new(Action::Duplicate, "Duplicate").key("⌘D"));
    groups.push(MenuGroup::new(None, rest));
    let mut del = vec![MenuEntry::new(Action::Delete, "Delete").key("⌫").danger()];
    // A document's Delete takes the card off the canvas; its file (the script) stays.
    if matches!(node.body, NodeBody::Asset { .. }) && !document {
        del.push(MenuEntry::new(Action::DeleteFile, "Delete File…").danger());
    }
    groups.push(MenuGroup::new(None, del));
    groups.retain(|g| !g.entries.is_empty());
    groups
}

/// The context menu on several selected nodes.
#[must_use]
pub fn selection_menu(count: usize) -> Vec<MenuGroup> {
    vec![
        MenuGroup::new(
            Some(&format!("{count} selected")),
            vec![MenuEntry::new(Action::AskAgent, "Ask agent about these").key("⌥⌘↩"), MenuEntry::new(Action::Duplicate, "Duplicate").key("⌘D")],
        ),
        MenuGroup::new(None, vec![MenuEntry::new(Action::Delete, "Delete").key("⌫").danger()]),
    ]
}

/// The context menu on a wire.
#[must_use]
pub fn wire_menu() -> Vec<MenuGroup> {
    vec![MenuGroup::new(None, vec![MenuEntry::new(Action::Unwire, "Unwire").key("⌫").danger()])]
}

/// The actions floating over a single selected node (§4.2): a note gets Visualize; an image Animate and
/// Edit; a video Extend; a card Run again (Run, Stop), Takes and Settings.
#[must_use]
pub fn selection_actions(canvas: &Canvas, sections: &[Section], id: &str) -> Vec<MenuEntry> {
    let Some(node) = canvas.node(id) else { return Vec::new() };
    let mut out = Vec::new();
    if let Some((media, problem)) = output(canvas, sections, id) {
        let make = |label: &str, makes, port| MenuEntry::new(Action::Make { makes, port }, label).unless(problem.clone());
        match media {
            SourceMedia::Text => out.push(make("Visualize", GenKind::Image, EdgeRole::Text)),
            SourceMedia::Image => {
                out.push(make("Animate", GenKind::Video, EdgeRole::FirstFrame));
                out.push(make("Edit", GenKind::Image, EdgeRole::Reference));
            }
            SourceMedia::Video => out.push(make("Extend", GenKind::Video, EdgeRole::Video)),
            SourceMedia::Audio | SourceMedia::Unknown => {}
        }
    }
    if let NodeBody::Gen { takes, .. } = &node.body {
        let wired = canvas.edges.iter().any(|e| e.to == id && e.role.is_port());
        out.extend(run_entry(&node.body, wired));
        out.push(MenuEntry::new(Action::Takes, "Takes").unless((takes.len() < 2).then(|| "Only one take".to_owned())));
        out.push(MenuEntry::new(Action::Settings, "Settings"));
    }
    out
}

/// The drag-to-create menu for a wire from `end` dropped on empty canvas, titled `Make from <source>` or
/// `Into <port>`.
#[must_use]
pub fn create_menu(canvas: &Canvas, sections: &[Section], end: &WireEnd) -> (String, Vec<MenuGroup>) {
    let title = match end {
        WireEnd::Out { from } => format!("Make from {}", canvas.label_of(from, sections)),
        WireEnd::In { port, .. } => format!("Into {}", port.name()),
    };
    let entries = create_options(canvas, end)
        .into_iter()
        .map(|o| match o.makes {
            Some(makes) => MenuEntry::new(Action::Make { makes, port: o.port }, kind_label(makes)),
            None => MenuEntry::new(Action::MakeNote, "Note"),
        })
        .collect();
    (title, vec![MenuGroup::new(None, entries)])
}

/// The toolbar (bottom centre): Select, Note, Frame, Prompt, Import.
#[must_use]
pub fn toolbar(tool: Tool) -> Vec<MenuEntry> {
    let t = |tl: Tool, label: &str, key: &'static str| {
        let mut e = MenuEntry::new(Action::Tool(tl), label).key(key);
        e.active = tool == tl;
        e
    };
    vec![
        t(Tool::Select, "Select", "V"),
        t(Tool::Note, "Note", "N"),
        t(Tool::Frame, "Frame", "F"),
        MenuEntry::new(Action::Prompt, "Prompt").key("G"),
        MenuEntry::new(Action::Import, "Import"),
    ]
}
