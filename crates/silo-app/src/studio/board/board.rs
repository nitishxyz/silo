//! [`Board`]: the canvas's state apart from drawing (camera, tool, selection, the gesture and its drafts,
//! pending batches, undo) driven by [`Event`]s and answering [`Effect`]s.

use std::collections::{HashMap, HashSet};

use silo_proto::studio::{Canvas, CanvasEdited, GenKind, GenStatus, MentionDid, NodeBody, Op, Project, Section, StudioMedia};

use super::apply::{self, History, Step};
use super::camera::{Camera, FIT_PADDING, Minimap, ZOOM_SPEED};
use super::create::{self, Ids, Imported, Made, Maker};
use super::geom::{Corner, Point, Rect, Size};
use super::input::{Button, Cursor, Effect, Event, Key, Mods};
use super::layout::{self, Laid, LayoutInput, Shape};
use super::menu::{self, Action, Menu, MenuEntry, MenuTarget, Tool};
use super::scene::{Hit, Part, Scene};
use super::settings::{self, CardSettings, SettingChange, SettingsInput};
use super::snap::{Guide, SNAP_VIEW, Snapper};
use super::wire::{self, Curve, WireDrop, WireEnd};

/// A press turns into a drag past this many view points.
pub const DRAG_VIEW: f64 = 3.0;
/// A drawn note or frame smaller than this (view points) is a click: the default size.
pub const DRAW_MIN_VIEW: f64 = 8.0;
/// Arrow keys move the selection this far (with shift: [`NUDGE_BIG`]).
pub const NUDGE: f64 = 1.0;
pub const NUDGE_BIG: f64 = 10.0;
/// `⌘D` puts copies this far down and right (with a card among them: [`create::CARD_GAP`] right of the
/// selection, level with it, so no card's ports land on another).
pub const DUPLICATE_OFFSET: f64 = 32.0;
/// `⌘=` / `⌘-` zoom by this factor.
pub const ZOOM_STEP: f64 = 1.25;

/// A batch sent and not yet in a canvas the server sent.
#[derive(Clone, Debug)]
struct Pending {
    batch: u64,
    ops: Vec<Op>,
    /// The rev the server answered with; `None` until acked.
    rev: Option<u64>,
}

/// How a batch enters the history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Record {
    /// A new edit: undoable, ends the redo branch.
    Edit,
    Undo,
    Redo,
}

/// What a press is over before it becomes a click or a drag.
#[derive(Clone, Debug)]
enum Gesture {
    Press { hit: Hit, view: Point, at: Point, mods: Mods, collapse: bool, deselect: bool },
    Move { ids: Vec<String>, roots: Vec<String>, mask: Vec<bool>, start: Point, delta: Point, bbox: Rect, snapper: Snapper },
    Resize { id: String, corner: Corner, start: Box<Laid>, input: LayoutInput, draft: Box<Laid>, snapper: Snapper },
    Marquee { start: Point, now: Point, base: Vec<String>, before: Vec<String> },
    Pan { view: Point, pan: Point },
    Wire { end: WireEnd, at: Point, hit: Hit, drop: WireDrop },
    Draw { tool: Tool, start: Point, now: Point },
    Minimap,
}

/// The selection actions floating over a single selected node.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionBar {
    pub node: String,
    /// View point: the node's top centre (the bar sits above it).
    pub anchor: Point,
    pub entries: Vec<MenuEntry>,
}

/// A wire to draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WireView<'a> {
    /// Its index in `canvas.edges`.
    pub edge: usize,
    pub id: &'a str,
    pub curve: Curve,
    pub selected: bool,
    /// Touches the selection or is hovered.
    pub active: bool,
}

/// The wire being dragged.
#[derive(Clone, Debug, PartialEq)]
pub struct WireDraft<'a> {
    pub end: &'a WireEnd,
    pub curve: Curve,
    /// What dropping it now would do (refusals carry the reason to show).
    pub drop: &'a WireDrop,
}

/// How to draw a port module.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortState {
    pub wired: bool,
    pub hovered: bool,
    /// While wiring: the drop would land here.
    pub target: bool,
    /// While wiring: it would be refused here.
    pub refused: bool,
    /// While wiring from a source: this port takes its media.
    pub fits: bool,
}

/// The board. Owns the canvas as shown: the server's last copy with the batches still in flight applied.
#[derive(Debug)]
pub struct Board {
    server: Option<Canvas>,
    canvas: Canvas,
    project: Option<Project>,
    sections: Vec<Section>,
    scene: Scene,
    prompts: HashMap<String, f64>,
    /// The card whose prompt is being edited (the view says).
    editing: Option<String>,
    /// Cards whose prompt shows in full, as last laid out: the edited one and the single selected card.
    open: HashSet<String>,
    pending: Vec<Pending>,
    next_batch: u64,
    history: History,
    after_ack: HashMap<u64, Vec<Effect>>,
    ids: Ids,
    camera: Camera,
    size: Size,
    fitted: bool,
    minimap_frame: Option<Rect>,
    tool: Tool,
    selection: Vec<String>,
    selected_set: HashSet<String>,
    selected_edge: Option<String>,
    gesture: Option<Gesture>,
    guides: Vec<Guide>,
    hover: Hit,
    pointer: Option<Point>,
    space: bool,
    menu: Option<Menu>,
    prompt_at: Option<Point>,
}

fn empty_canvas() -> Canvas {
    Canvas {
        name: String::new(),
        title: String::new(),
        episode: None,
        format: None,
        rev: 0,
        nodes: Vec::new(),
        edges: Vec::new(),
        out_of_date: Vec::new(),
        out_of_date_why: Default::default(),
    }
}

/// The take index after stepping `by` from the picked one (`None` at the ends).
fn stepped(takes: usize, pick: Option<u32>, by: isize) -> Option<u32> {
    let last = takes.checked_sub(1)?;
    let now = pick.map_or(last, |p| (p as usize).min(last));
    u32::try_from(now.checked_add_signed(by).filter(|t| *t <= last)?).ok()
}

impl Board {
    /// A board with no canvas yet; `seed` (random per board) makes its ids.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            server: None,
            canvas: empty_canvas(),
            project: None,
            sections: Vec::new(),
            scene: Scene::default(),
            prompts: HashMap::new(),
            editing: None,
            open: HashSet::new(),
            pending: Vec::new(),
            next_batch: 1,
            history: History::default(),
            after_ack: HashMap::new(),
            ids: Ids::new(seed),
            camera: Camera::default(),
            size: Size::default(),
            fitted: false,
            minimap_frame: None,
            tool: Tool::Select,
            selection: Vec::new(),
            selected_set: HashSet::new(),
            selected_edge: None,
            gesture: None,
            guides: Vec::new(),
            hover: Hit::Empty,
            pointer: None,
            space: false,
            menu: None,
            prompt_at: None,
        }
    }

    // ------------------------------------------------------------ data in

    /// The server's canvas (a read after `studio.changed`, or the first). Older revs are ignored; another
    /// canvas resets the board. Batches the server has (acked at or below its rev) are dropped; the rest are
    /// applied again on top.
    pub fn set_canvas(&mut self, server: &Canvas) -> Vec<Effect> {
        let same = self.server.as_ref().is_some_and(|s| s.name == server.name);
        if same && self.server.as_ref().is_some_and(|s| server.rev < s.rev) {
            return Vec::new();
        }
        let mut fx = vec![Effect::Redraw];
        if !same {
            let had = !self.selection.is_empty() || self.selected_edge.is_some();
            *self = Self {
                ids: self.ids.clone(),
                project: self.project.take(),
                sections: std::mem::take(&mut self.sections),
                size: self.size,
                minimap_frame: self.minimap_frame,
                next_batch: self.next_batch,
                ..Self::new(0)
            };
            if had {
                fx.push(Effect::Selection);
            }
        }
        self.pending.retain(|p| p.rev.is_none_or(|r| r > server.rev));
        self.server = Some(server.clone());
        self.rebuild_canvas();
        if self.prune_selection() {
            fx.push(Effect::Selection);
        }
        self.fit_once();
        fx
    }

    /// The project (formats) and the world's sections (entity sheets and names).
    pub fn set_context(&mut self, project: Option<&Project>, sections: &[Section]) {
        self.project = project.cloned();
        self.sections = sections.to_vec();
        self.rebuild_scene();
    }

    /// The board's size in view points.
    pub fn set_view_size(&mut self, size: Size) {
        self.size = size;
        self.fit_once();
    }

    /// Where the minimap is drawn (view points), so presses there move the camera; `None`: hidden.
    pub fn set_minimap_frame(&mut self, frame: Option<Rect>) {
        self.minimap_frame = frame;
    }

    /// Card `id`'s prompt as the view measured it as drawn (its height in canvas units): the card's height
    /// follows, collapsed or open ([`layout::CardParts::open`]).
    pub fn measure_prompt(&mut self, id: &str, height: f64) -> Vec<Effect> {
        let h = height.clamp(layout::PROMPT_MIN, layout::PROMPT_OPEN_MAX);
        if self.prompts.get(id).is_some_and(|old| (old - h).abs() < 0.5) {
            return Vec::new();
        }
        self.prompts.insert(id.to_owned(), h);
        self.rebuild_scene();
        vec![Effect::Redraw]
    }

    /// Batch `batch` applied on the server: ids it made are adopted (renamed here when they differ from the
    /// ones given), failed ops reported, and effects waiting for it (a Run after a card is made) released.
    pub fn acked(&mut self, batch: u64, result: &CanvasEdited) -> Vec<Effect> {
        let mut fx = Vec::new();
        let Some(at) = self.pending.iter().position(|p| p.batch == batch) else { return fx };
        let given: Vec<String> = self.pending[at]
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::AddNode { node } => Some(node.id.clone().unwrap_or_default()),
                Op::AddEdge { edge } => Some(edge.id.clone().unwrap_or_default()),
                _ => None,
            })
            .collect();
        let mut rebuild = false;
        for (mine, theirs) in given.iter().zip(&result.ids) {
            if !mine.is_empty() && !theirs.is_empty() && mine != theirs {
                self.rename(mine, theirs);
                rebuild = true;
            }
        }
        // Mentions the server couldn't wire say why with the ops that failed.
        let not_wired = result.mentions.iter().filter(|m| m.did == MentionDid::NotWired).filter_map(|m| m.why.as_deref());
        let problems: Vec<&str> = result.failed.iter().map(|f| f.message.as_str()).chain(not_wired).collect();
        if !problems.is_empty() {
            fx.push(Effect::Notice(problems.join("; ")));
        }
        // Undoing the batch takes away the Entity nodes its mentions added (their wires go with them).
        let added: Vec<Op> = result.mentions.iter().filter_map(|m| m.node.clone()).map(|id| Op::RemoveNode { id }).collect();
        if !added.is_empty() {
            self.history.extend(batch, added);
        }
        self.pending[at].rev = Some(result.rev);
        if self.server.as_ref().is_some_and(|s| s.rev >= result.rev) {
            self.pending.remove(at);
            rebuild = true;
        }
        // The server's copy may already hold the renamed nodes (a read that beat this answer): show it
        // with what is still pending, not the batch applied again under the ids it gave.
        if rebuild {
            self.rebuild_canvas();
        }
        fx.extend(self.after_ack.remove(&batch).unwrap_or_default());
        fx
    }

    /// Batch `batch` was refused as a whole: it is undone here and forgotten.
    pub fn failed(&mut self, batch: u64, message: &str) -> Vec<Effect> {
        self.pending.retain(|p| p.batch != batch);
        self.history.forget(batch);
        self.after_ack.remove(&batch);
        self.rebuild_canvas();
        let mut fx = vec![Effect::Redraw, Effect::Notice(message.to_owned())];
        if self.prune_selection() {
            fx.push(Effect::Selection);
        }
        fx
    }

    fn rename(&mut self, from: &str, to: &str) {
        apply::rename_in_canvas(&mut self.canvas, from, to);
        for p in &mut self.pending {
            apply::rename(&mut p.ops, from, to);
        }
        self.history.rename(from, to);
        for s in self.selection.iter_mut().chain(self.selected_edge.iter_mut()) {
            if s == from {
                to.clone_into(s);
            }
        }
        if let Some(h) = self.prompts.remove(from) {
            self.prompts.insert(to.to_owned(), h);
        }
        if self.editing.as_deref() == Some(from) {
            self.editing = Some(to.to_owned());
        }
        for fx in self.after_ack.values_mut().flatten() {
            if let Effect::Run { node } | Effect::FocusPrompt { node } | Effect::EditNote { node } = fx
                && node == from
            {
                to.clone_into(node);
            }
        }
        self.selected_set = self.selection.iter().cloned().collect();
        self.rebuild_scene();
    }

    fn rebuild_canvas(&mut self) {
        let mut canvas = self.server.clone().unwrap_or_else(empty_canvas);
        let ids = &mut self.ids;
        for p in &self.pending {
            let _ = apply::apply(&mut canvas, &p.ops, self.project.as_ref(), &mut || ids.node());
        }
        self.canvas = canvas;
        self.rebuild_scene();
    }

    /// The cards whose prompt shows in full: the one edited and the single selected card.
    fn open_cards(&self) -> HashSet<String> {
        let single = match self.selection.as_slice() {
            [id] if matches!(self.canvas.node(id).map(|n| &n.body), Some(NodeBody::Gen { .. })) => Some(id.clone()),
            _ => None,
        };
        self.editing.iter().cloned().chain(single).collect()
    }

    /// The selection's set after the selection changed: laid out again when a card opens or closes, else only
    /// reordered.
    fn selection_changed(&mut self) {
        self.selected_set = self.selection.iter().cloned().collect();
        if self.open_cards() == self.open {
            self.scene.reorder(&self.canvas, &self.selected_set);
        } else {
            self.rebuild_scene();
        }
    }

    /// Card `id`'s prompt is being edited (`None`: no prompt is): it shows in full.
    pub fn set_editing(&mut self, id: Option<&str>) -> Vec<Effect> {
        if self.editing.as_deref() == id {
            return Vec::new();
        }
        self.editing = id.map(str::to_owned);
        self.rebuild_scene();
        vec![Effect::Redraw]
    }

    fn rebuild_scene(&mut self) {
        self.open = self.open_cards();
        self.scene = Scene::build(&self.canvas, self.project.as_ref(), &self.prompts, &self.selected_set, &self.open);
        // A gesture's indices follow the new layout.
        if let Some(Gesture::Move { ids, mask, .. }) = &mut self.gesture {
            *mask = vec![false; self.scene.laid.len()];
            for id in ids.iter() {
                if let Some(i) = self.scene.index(id) {
                    mask[i] = true;
                }
            }
        }
        if let Some(Gesture::Resize { id, start, draft, .. }) = &mut self.gesture {
            match self.scene.index(id) {
                Some(i) => (start.index, draft.index) = (i, i),
                None => self.gesture = None,
            }
        }
    }

    /// Drops selected ids that are gone; whether the selection changed.
    fn prune_selection(&mut self) -> bool {
        let before = (self.selection.len(), self.selected_edge.is_some());
        let scene = &self.scene;
        self.selection.retain(|id| scene.index(id).is_some());
        let canvas = &self.canvas;
        if self.selected_edge.as_ref().is_some_and(|e| !canvas.edges.iter().any(|x| &x.id == e)) {
            self.selected_edge = None;
        }
        let changed = before != (self.selection.len(), self.selected_edge.is_some());
        if changed {
            self.selection_changed();
        }
        changed
    }

    fn fit_once(&mut self) {
        if !self.fitted && self.server.is_some() && self.size.w > 0.0 && self.size.h > 0.0 {
            self.fitted = true;
            self.camera =
                self.scene.content.map_or_else(|| Camera::default().centered_on(Point::ZERO, self.size), |c| Camera::fit(c, self.size, FIT_PADDING));
        }
    }

    // ------------------------------------------------------------ reading

    /// The canvas as shown (the server's with pending batches applied).
    #[must_use]
    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    /// The world's sections ([`Self::set_context`]).
    #[must_use]
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// Every batch sent is in a canvas the server sent (mentions' wires are drawn as the server made them).
    #[must_use]
    pub fn settled(&self) -> bool {
        self.pending.is_empty()
    }

    #[must_use]
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    #[must_use]
    pub fn camera(&self) -> Camera {
        self.camera
    }

    /// Whether the camera is placed: the canvas read and the view's size known (it was fitted once). Before
    /// that, a [`Self::reveal`] would be undone by the first fit.
    #[must_use]
    pub fn placed(&self) -> bool {
        self.fitted
    }

    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn set_camera(&mut self, camera: Camera) -> Vec<Effect> {
        self.camera = Camera { pan: camera.pan, zoom: super::camera::clamp_zoom(camera.zoom) };
        vec![Effect::Redraw]
    }

    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn set_tool(&mut self, tool: Tool) -> Vec<Effect> {
        self.tool = tool;
        vec![Effect::Redraw]
    }

    /// Selected node ids, in the order they were selected.
    #[must_use]
    pub fn selection(&self) -> &[String] {
        &self.selection
    }

    #[must_use]
    pub fn is_selected(&self, id: &str) -> bool {
        self.selected_set.contains(id)
    }

    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn selected_edge(&self) -> Option<&str> {
        self.selected_edge.as_deref()
    }

    #[must_use]
    pub fn hovered(&self) -> &Hit {
        &self.hover
    }

    #[must_use]
    pub fn menu(&self) -> Option<&Menu> {
        self.menu.as_ref()
    }

    pub fn close_menu(&mut self) -> Vec<Effect> {
        if self.menu.take().is_some() { vec![Effect::CloseMenu] } else { Vec::new() }
    }

    /// Snap lines to draw while moving or resizing.
    #[must_use]
    pub fn guides(&self) -> &[Guide] {
        &self.guides
    }

    /// The marquee being dragged (canvas units).
    #[must_use]
    pub fn marquee(&self) -> Option<Rect> {
        match &self.gesture {
            Some(Gesture::Marquee { start, now, .. }) => Some(Rect::from_points(*start, *now)),
            _ => None,
        }
    }

    /// The note or frame being drawn (canvas units) and with which tool.
    #[must_use]
    pub fn drawing(&self) -> Option<(Tool, Rect)> {
        match &self.gesture {
            Some(Gesture::Draw { tool, start, now }) => Some((*tool, Rect::from_points(*start, *now))),
            _ => None,
        }
    }

    /// Whether a gesture (a drag of any kind) is under way.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.gesture.as_ref().is_some_and(|g| !matches!(g, Gesture::Press { .. }))
    }

    /// The canvas rect in view, for culling.
    #[must_use]
    pub fn visible_rect(&self) -> Rect {
        self.camera.visible(self.size)
    }

    /// Node `i` as drawn now: moved or resized by the gesture's draft.
    #[must_use]
    pub fn shown(&self, i: usize) -> Laid {
        match &self.gesture {
            Some(Gesture::Move { mask, delta, .. }) if mask.get(i).copied().unwrap_or(false) => self.scene.laid[i].translated(*delta),
            Some(Gesture::Resize { draft, .. }) if draft.index == i => **draft,
            _ => self.scene.laid[i],
        }
    }

    /// Nodes to draw, bottom to top, culled to the view, drafts applied. `canvas().nodes[laid.index]` is the node.
    pub fn visible(&self) -> impl Iterator<Item = Laid> + '_ {
        let vis = self.visible_rect().outset(layout::PORT_SIZE * 2.0);
        self.scene.order.iter().map(|&i| self.shown(i)).filter(move |l| l.outer().intersects(vis))
    }

    /// Node `id` as drawn now.
    #[must_use]
    pub fn laid(&self, id: &str) -> Option<Laid> {
        self.scene.index(id).map(|i| self.shown(i))
    }

    /// Wires to draw (under the nodes), culled to the view, drafts applied.
    pub fn wires(&self) -> impl Iterator<Item = WireView<'_>> + '_ {
        let vis = self.visible_rect();
        let hovered = match &self.hover {
            Hit::Wire { id } => Some(id.as_str()),
            _ => None,
        };
        self.canvas.edges.iter().enumerate().filter_map(move |(e, edge)| {
            if !edge.role.is_port() {
                return None;
            }
            let (from, to) = (self.scene.index(&edge.from)?, self.scene.index(&edge.to)?);
            let (a, b) = (self.shown(from), self.shown(to));
            if !matches!(b.shape, Shape::Card(_)) || !a.output {
                return None;
            }
            let curve = Curve::wire(a.output_center(), b.input_point(edge.role));
            if !curve.bounds().intersects(vis) {
                return None;
            }
            let selected = self.selected_edge.as_deref() == Some(edge.id.as_str());
            let active =
                selected || hovered == Some(edge.id.as_str()) || self.selected_set.contains(&edge.from) || self.selected_set.contains(&edge.to);
            Some(WireView { edge: e, id: edge.id.as_str(), curve, selected, active })
        })
    }

    /// The wire being dragged.
    #[must_use]
    pub fn wiring(&self) -> Option<WireDraft<'_>> {
        let Some(Gesture::Wire { end, at, drop, .. }) = &self.gesture else { return None };
        let curve = match end {
            WireEnd::Out { from } => {
                let a = self.laid(from)?.output_center();
                let b = match drop {
                    WireDrop::Wire { to, port, .. } => self.laid(to).map_or(*at, |l| l.input_point(*port)),
                    _ => *at,
                };
                Curve::wire(a, b)
            }
            WireEnd::In { to, port } => {
                let b = self.laid(to)?.input_point(*port);
                let a = match drop {
                    WireDrop::Wire { from, .. } => self.laid(from).map_or(*at, |l| l.output_center()),
                    _ => *at,
                };
                Curve::wire(a, b)
            }
        };
        Some(WireDraft { end, curve, drop })
    }

    /// How to draw port `k` of `laid`.
    #[must_use]
    pub fn port_state(&self, laid: &Laid, k: usize) -> PortState {
        let id = &self.canvas.nodes[laid.index].id;
        let Some(&role) = laid.ports.get(k) else { return PortState::default() };
        let mut s = PortState { wired: laid.is_wired(k), ..PortState::default() };
        s.hovered = matches!(&self.hover, Hit::Port { id: h, port } if h == id && *port == role);
        if let Some(Gesture::Wire { end, drop, .. }) = &self.gesture {
            match drop {
                WireDrop::Wire { to, port, .. } => s.target = to == id && *port == role,
                WireDrop::Refused { node, port, .. } => s.refused = node == id && port.is_none_or(|p| p == role),
                _ => {}
            }
            if let WireEnd::Out { from } = end {
                s.fits = self.canvas.source_media(from).is_some_and(|m| role.accepts() == Some(m));
            }
        }
        s
    }

    /// The minimap in `frame` (view points), when the content isn't all in view.
    #[must_use]
    pub fn minimap(&self, frame: Rect) -> Option<Minimap> {
        Minimap::new(frame, self.scene.content, self.visible_rect())
    }

    /// The cursor for the pointer's place and the gesture.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        match &self.gesture {
            Some(Gesture::Pan { .. } | Gesture::Move { .. }) => return Cursor::Grabbing,
            Some(Gesture::Resize { corner, .. }) => return Cursor::Resize(*corner),
            Some(Gesture::Wire { .. } | Gesture::Draw { .. } | Gesture::Marquee { .. }) => return Cursor::Crosshair,
            _ => {}
        }
        if self.space {
            return Cursor::Grab;
        }
        if self.tool != Tool::Select {
            return Cursor::Crosshair;
        }
        match &self.hover {
            Hit::Resize { corner, .. } => Cursor::Resize(*corner),
            Hit::Port { .. } | Hit::Output { .. } | Hit::Wire { .. } => Cursor::Pointer,
            Hit::Node { part: Part::Run | Part::TakeBack | Part::TakeForward, .. } => Cursor::Pointer,
            Hit::Node { part: Part::Prompt, .. } => Cursor::Text,
            Hit::Node { part: Part::FrameBody, .. } | Hit::Empty => Cursor::Default,
            Hit::Node { .. } => Cursor::Grab,
        }
    }

    /// The selection actions over a single selected node (none while dragging).
    #[must_use]
    pub fn selection_bar(&self) -> Option<SelectionBar> {
        if self.busy() || self.selection.len() != 1 {
            return None;
        }
        let id = &self.selection[0];
        let entries = menu::selection_actions(&self.canvas, &self.sections, id);
        if entries.is_empty() {
            return None;
        }
        let r = self.laid(id)?.rect;
        Some(SelectionBar { node: id.clone(), anchor: self.camera.to_view(Point::new(r.center().x, r.y)), entries })
    }

    /// The toolbar's entries.
    #[must_use]
    pub fn toolbar(&self) -> Vec<MenuEntry> {
        menu::toolbar(self.tool)
    }

    /// The settings panel of card `id`.
    #[must_use]
    pub fn settings(&self, id: &str, input: &SettingsInput<'_>) -> Option<CardSettings> {
        let input = SettingsInput {
            project: input.project.or(self.project.as_ref()),
            sections: if input.sections.is_empty() { &self.sections } else { input.sections },
            ..*input
        };
        settings::card_settings(&self.canvas, id, &input)
    }

    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn can_undo(&self) -> bool {
        !self.history.undo.is_empty()
    }

    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn can_redo(&self) -> bool {
        !self.history.redo.is_empty()
    }

    // ------------------------------------------------------------ edits

    fn base_rev(&self) -> u64 {
        self.server.as_ref().map_or(0, |s| s.rev)
    }

    /// Applies `ops` here and asks the view to send them.
    fn send(&mut self, ops: Vec<Op>, record: Record) -> Vec<Effect> {
        if ops.is_empty() {
            return Vec::new();
        }
        let batch = self.next_batch;
        let ids = &mut self.ids;
        let applied = apply::apply(&mut self.canvas, &ops, self.project.as_ref(), &mut || ids.node());
        let mut fx = Vec::new();
        if applied.failed.len() == ops.len() {
            if let Some((_, why)) = applied.failed.first() {
                fx.push(Effect::Notice(why.clone()));
            }
            return fx;
        }
        self.next_batch += 1;
        let step = Step { ops: applied.inverse, batch };
        match record {
            Record::Edit => self.history.push(step),
            Record::Undo => self.history.push_redo(step),
            Record::Redo => self.history.push_undo(step),
        }
        self.pending.push(Pending { batch, ops: ops.clone(), rev: None });
        self.rebuild_scene();
        if self.prune_selection() {
            fx.push(Effect::Selection);
        }
        fx.push(Effect::Send { batch, ops, base_rev: self.base_rev() });
        fx.push(Effect::Redraw);
        fx
    }

    /// The last batch's number (the one [`Effect::Send`] just named).
    fn last_batch(&self) -> u64 {
        self.next_batch - 1
    }

    /// Sends what was made, selects its nodes and focuses the one to focus (`run`: runs that card once the
    /// server has it).
    fn commit_made(&mut self, made: Made, run: bool, edit_note: bool) -> Vec<Effect> {
        let mut fx = self.send(made.ops, Record::Edit);
        if !fx.iter().any(|f| matches!(f, Effect::Send { .. })) {
            return fx;
        }
        fx.extend(self.select_ids(made.nodes));
        if let Some(id) = made.focus {
            if run {
                self.after_ack.entry(self.last_batch()).or_default().push(Effect::Run { node: id });
            } else if edit_note || matches!(self.canvas.node(&id).map(|n| &n.body), Some(NodeBody::Note { .. })) {
                fx.push(Effect::EditNote { node: id });
            } else {
                fx.push(Effect::FocusPrompt { node: id });
            }
        }
        fx
    }

    fn maker(&mut self) -> Maker<'_> {
        Maker { canvas: &self.canvas, scene: &self.scene, project: self.project.as_ref(), ids: &mut self.ids }
    }

    /// Undoes the block's last edit.
    pub fn undo(&mut self) -> Vec<Effect> {
        match self.history.undo.pop() {
            Some(step) => self.send(step.ops, Record::Undo),
            None => Vec::new(),
        }
    }

    pub fn redo(&mut self) -> Vec<Effect> {
        match self.history.redo.pop() {
            Some(step) => self.send(step.ops, Record::Redo),
            None => Vec::new(),
        }
    }

    /// The canvas prompt's submit: a `makes` card with `text` where the prompt was opened (else the view's
    /// centre); `run`: run it once the server has it (Enter), else focus its prompt (⌥Enter, the card only).
    pub fn submit_prompt(&mut self, makes: GenKind, text: &str, run: bool) -> Vec<Effect> {
        let at = self.prompt_at.take().unwrap_or_else(|| self.visible_rect().center());
        let made = self.maker().prompt_card(makes, text, at);
        self.commit_made(made, run && !text.trim().is_empty(), false)
    }

    /// The canvas prompt closed without making anything.
    pub fn close_prompt(&mut self) {
        self.prompt_at = None;
    }

    /// A card's prompt edited in place.
    pub fn set_prompt(&mut self, id: &str, text: &str) -> Vec<Effect> {
        match self.canvas.node(id).map(|n| &n.body) {
            Some(NodeBody::Gen { prompt, .. }) if prompt != text => self.send(vec![create::set_prompt(id, text)], Record::Edit),
            _ => Vec::new(),
        }
    }

    /// A note's text edited in place.
    pub fn set_note(&mut self, id: &str, text: &str) -> Vec<Effect> {
        match self.canvas.node(id).map(|n| &n.body) {
            Some(NodeBody::Note { text: old }) if old != text => self.send(vec![create::set_note(id, text)], Record::Edit),
            _ => Vec::new(),
        }
    }

    /// A frame's title edited in place.
    pub fn set_frame_title(&mut self, id: &str, title: &str) -> Vec<Effect> {
        match self.canvas.node(id).map(|n| &n.body) {
            Some(NodeBody::Frame { title: old, .. }) if old != title => self.send(vec![create::set_title(id, title)], Record::Edit),
            _ => Vec::new(),
        }
    }

    /// Steps card `id`'s picked take by `by` (the header's `1/3`).
    pub fn step_take(&mut self, id: &str, by: isize) -> Vec<Effect> {
        let Some(NodeBody::Gen { takes, pick, .. }) = self.canvas.node(id).map(|n| &n.body) else { return Vec::new() };
        match stepped(takes.len(), *pick, by) {
            Some(to) => self.send(vec![create::set_pick(id, to)], Record::Edit),
            None => Vec::new(),
        }
    }

    /// Picks take `index` of card `id` (the Takes list).
    pub fn pick_take(&mut self, id: &str, index: u32) -> Vec<Effect> {
        self.send(vec![create::set_pick(id, index)], Record::Edit)
    }

    /// A change in card `id`'s settings panel.
    pub fn change_setting(&mut self, id: &str, change: &SettingChange) -> Vec<Effect> {
        self.send(vec![settings::change_op(id, change)], Record::Edit)
    }

    /// The settings panel's × on an input: unwires it.
    pub fn unwire(&mut self, edge: &str) -> Vec<Effect> {
        self.send(vec![Op::RemoveEdge { id: edge.to_owned() }], Record::Edit)
    }

    /// Files imported into `assets/`, placed at canvas point `at` (else the view's centre).
    pub fn place_imports(&mut self, files: &[Imported], at: Option<Point>) -> Vec<Effect> {
        let at = at.unwrap_or_else(|| self.visible_rect().center());
        let made = self.maker().imports(files, at);
        self.commit_made(made, false, false)
    }

    /// An entity from the World placed at canvas point `at` (else the view's centre); `aspect`: its sheet's.
    pub fn add_entity(&mut self, entity: &str, at: Option<Point>, aspect: Option<f64>) -> Vec<Effect> {
        let at = at.unwrap_or_else(|| self.visible_rect().center());
        let made = self.maker().entity(entity, at, aspect);
        self.commit_made(made, false, false)
    }

    /// A document card for root-relative `path` at canvas point `at` (else the view's centre), selected.
    pub fn add_document(&mut self, path: &str, at: Option<Point>) -> Vec<Effect> {
        let at = at.unwrap_or_else(|| self.visible_rect().center());
        let made = self.maker().document(path, at);
        self.commit_made(made, false, false)
    }

    /// Removes the selected nodes (their wires go with them) or the selected wire.
    pub fn delete_selection(&mut self) -> Vec<Effect> {
        let edges: Vec<String> = self.selected_edge.iter().cloned().collect();
        let ops = create::delete(&self.canvas, &self.selection, &edges);
        self.send(ops, Record::Edit)
    }

    /// `⌘D`: copies of the selection, selected.
    pub fn duplicate_selection(&mut self) -> Vec<Effect> {
        if self.selection.is_empty() {
            return Vec::new();
        }
        let ids = self.selection.clone();
        // A small offset, as everywhere: a copy beside the selection could land on its neighbour.
        let made = self.maker().duplicate(&ids, Point::new(DUPLICATE_OFFSET, DUPLICATE_OFFSET));
        self.commit_made(made, false, false)
    }

    /// Moves the selection (frames with their contents) by `d`.
    fn nudge(&mut self, d: Point) -> Vec<Effect> {
        let moving = self.moving_set(&self.selection.clone());
        let ops: Vec<Op> =
            moving.iter().map(|&i| apply::moved(&self.canvas.nodes[i].id, self.canvas.nodes[i].x + d.x, self.canvas.nodes[i].y + d.y)).collect();
        self.send(ops, Record::Edit)
    }

    // ------------------------------------------------------------ selection and camera

    fn select_ids(&mut self, ids: Vec<String>) -> Vec<Effect> {
        if ids == self.selection && self.selected_edge.is_none() {
            return Vec::new();
        }
        self.selection = ids;
        self.selected_edge = None;
        self.selection_changed();
        vec![Effect::Selection, Effect::Redraw]
    }

    /// Selects `ids` (and no wire).
    pub fn select(&mut self, ids: &[String]) -> Vec<Effect> {
        self.select_ids(ids.iter().filter(|id| self.scene.index(id).is_some()).cloned().collect())
    }

    fn select_edge(&mut self, id: Option<String>) -> Vec<Effect> {
        let mut fx = self.select_ids(Vec::new());
        if self.selected_edge != id {
            self.selected_edge = id;
            fx = vec![Effect::Selection, Effect::Redraw];
        }
        fx
    }

    /// `⌘A`.
    pub fn select_all(&mut self) -> Vec<Effect> {
        let ids = self.scene.order.iter().map(|&i| self.canvas.nodes[i].id.clone()).collect();
        self.select_ids(ids)
    }

    /// `⌘0`: everything in view.
    pub fn fit_all(&mut self) -> Vec<Effect> {
        let Some(c) = self.scene.content else { return Vec::new() };
        self.camera = Camera::fit(c, self.size, FIT_PADDING);
        vec![Effect::Redraw]
    }

    /// `f`: the selection in view.
    pub fn fit_selection(&mut self) -> Vec<Effect> {
        let Some(r) = Rect::bounds(self.selection.iter().filter_map(|id| self.scene.get(id)).map(Laid::outer)) else { return Vec::new() };
        self.camera = Camera::fit(r, self.size, FIT_PADDING);
        vec![Effect::Redraw]
    }

    /// Pans the least needed to show node `id` (a jump from the settings panel's inputs), selecting it.
    pub fn reveal(&mut self, id: &str) -> Vec<Effect> {
        let Some(l) = self.scene.get(id) else { return Vec::new() };
        self.camera = self.camera.reveal(l.outer(), self.size, FIT_PADDING);
        let mut fx = self.select_ids(vec![id.to_owned()]);
        fx.push(Effect::Redraw);
        fx
    }

    fn zoom_by(&mut self, at: Point, factor: f64) -> Vec<Effect> {
        self.camera = self.camera.zoomed_at(at, self.camera.zoom * factor);
        vec![Effect::Redraw]
    }

    /// Indices of `ids` that aren't inside another of them, then everything inside the frames among them.
    fn moving_set(&self, ids: &[String]) -> Vec<usize> {
        let picked: Vec<usize> = ids.iter().filter_map(|id| self.scene.index(id)).collect();
        let mut out: Vec<usize> = Vec::new();
        let mut seen = HashSet::new();
        for &i in &picked {
            if picked.iter().any(|&j| j != i && self.scene.within(i, j)) {
                continue;
            }
            for k in std::iter::once(i).chain(self.scene.descendants(i)) {
                if seen.insert(k) {
                    out.push(k);
                }
            }
        }
        out
    }

    fn roots(&self, ids: &[String]) -> Vec<String> {
        let picked: Vec<usize> = ids.iter().filter_map(|id| self.scene.index(id)).collect();
        picked.iter().filter(|&&i| !picked.iter().any(|&j| j != i && self.scene.within(i, j))).map(|&i| self.canvas.nodes[i].id.clone()).collect()
    }

    fn snapper_except(&self, skip: impl Fn(usize) -> bool) -> Snapper {
        let vis = self.visible_rect().outset(self.visible_rect().w.max(self.visible_rect().h) * 0.5);
        Snapper::new(self.scene.laid.iter().filter(|l| !skip(l.index) && l.rect.intersects(vis)).map(|l| (l.rect, l.shape.is_frame())))
    }

    // ------------------------------------------------------------ events

    /// Handles an input event.
    pub fn handle(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Down { at, button, mods, clicks } => self.down(at, button, mods, clicks),
            Event::Move { at, mods } => self.moved(at, mods),
            Event::Up { at, button, mods } => self.up(at, button, mods),
            Event::Scroll { at, delta, mods } => {
                if mods.cmd || mods.ctrl {
                    self.zoom_by(at, (-delta.y * ZOOM_SPEED).exp())
                } else {
                    self.camera = self.camera.panned(delta);
                    vec![Effect::Redraw]
                }
            }
            Event::Pinch { at, factor } => self.zoom_by(at, factor),
            Event::Leave => {
                self.pointer = None;
                if self.hover == Hit::Empty {
                    Vec::new()
                } else {
                    self.hover = Hit::Empty;
                    vec![Effect::Redraw]
                }
            }
            Event::KeyDown { key, mods } => self.key(key, mods),
            Event::KeyUp { key: Key::Space, .. } => {
                self.space = false;
                vec![Effect::Redraw]
            }
            Event::KeyUp { .. } => Vec::new(),
        }
    }

    fn down(&mut self, at: Point, button: Button, mods: Mods, clicks: u32) -> Vec<Effect> {
        let mut fx = self.close_menu();
        self.pointer = Some(at);
        let p = self.camera.to_canvas(at);
        if button == Button::Right {
            fx.extend(self.context_menu(at, p));
            return fx;
        }
        if let Some(frame) = self.minimap_frame.filter(|f| f.contains(at))
            && let Some(map) = self.minimap(frame)
        {
            self.camera = map.camera_at(self.camera, at, self.size);
            self.gesture = Some(Gesture::Minimap);
            fx.push(Effect::Redraw);
            return fx;
        }
        if self.space || button == Button::Middle {
            self.gesture = Some(Gesture::Pan { view: at, pan: self.camera.pan });
            fx.push(Effect::Redraw);
            return fx;
        }
        if self.tool != Tool::Select {
            self.gesture = Some(Gesture::Draw { tool: self.tool, start: p, now: p });
            return fx;
        }
        let hit = self.scene.hit(&self.canvas, p, self.camera.zoom, &self.selection);
        if clicks >= 2 {
            fx.extend(self.double_click(&hit, at, p));
            return fx;
        }
        match &hit {
            Hit::Resize { id, corner } => {
                if let Some(i) = self.scene.index(id) {
                    let start = self.scene.laid[i];
                    let input = LayoutInput {
                        aspect: start.card.map_or(layout::DEFAULT_ASPECT, |c| c.aspect),
                        prompt: start.card.map(|c| c.prompt.h),
                        open: start.card.is_some_and(|c| c.open),
                        depth: start.depth,
                        wired: start.wired,
                    };
                    let snapper = self.snapper_except(|j| j == i);
                    self.gesture =
                        Some(Gesture::Resize { id: id.clone(), corner: *corner, start: Box::new(start), input, draft: Box::new(start), snapper });
                }
            }
            Hit::Output { id } => {
                let end = WireEnd::Out { from: id.clone() };
                self.gesture = Some(Gesture::Wire { end, at: p, hit: hit.clone(), drop: WireDrop::Nothing });
                fx.push(Effect::Redraw);
            }
            Hit::Port { id, port } => {
                let end = WireEnd::In { to: id.clone(), port: *port };
                self.gesture = Some(Gesture::Wire { end, at: p, hit: hit.clone(), drop: WireDrop::Nothing });
                fx.push(Effect::Redraw);
            }
            Hit::Wire { id } => fx.extend(self.select_edge(Some(id.clone()))),
            Hit::Node { id, part } if *part != Part::FrameBody => {
                let selected = self.selected_set.contains(id);
                let (mut collapse, mut deselect) = (false, false);
                if mods.shift {
                    if selected {
                        deselect = true;
                    } else {
                        let mut ids = self.selection.clone();
                        ids.push(id.clone());
                        fx.extend(self.select_ids(ids));
                    }
                } else if selected {
                    collapse = self.selection.len() > 1;
                    if self.selected_edge.take().is_some() {
                        fx.push(Effect::Selection);
                    }
                } else {
                    fx.extend(self.select_ids(vec![id.clone()]));
                }
                self.gesture = Some(Gesture::Press { hit, view: at, at: p, mods, collapse, deselect });
            }
            _ => self.gesture = Some(Gesture::Press { hit, view: at, at: p, mods, collapse: false, deselect: false }),
        }
        fx
    }

    fn double_click(&mut self, hit: &Hit, view: Point, p: Point) -> Vec<Effect> {
        self.gesture = None;
        match hit {
            Hit::Empty | Hit::Node { part: Part::FrameBody, .. } => {
                self.prompt_at = Some(p);
                vec![Effect::OpenPrompt { at: p, view }]
            }
            Hit::Node { id, part } => {
                let node_id = id.clone();
                let mut fx = self.select_ids(vec![node_id.clone()]);
                let Some(node) = self.canvas.node(id) else { return fx };
                fx.push(match (&node.body, part) {
                    (NodeBody::Note { .. }, _) => Effect::EditNote { node: node_id },
                    (NodeBody::Frame { .. }, _) => Effect::RenameFrame { node: node_id },
                    (NodeBody::Gen { .. }, Part::Face) if node.body.take().is_some() => Effect::View { node: node_id },
                    (NodeBody::Gen { .. }, _) => Effect::FocusPrompt { node: node_id },
                    (NodeBody::Entity { id: entity }, _) => Effect::OpenWorld { entity: entity.clone() },
                    (NodeBody::Asset { path, media: StudioMedia::Text, .. }, _) => Effect::OpenDocument { path: path.clone() },
                    (NodeBody::Asset { .. }, _) => Effect::View { node: node_id },
                    (NodeBody::Unknown, _) => return fx,
                });
                fx
            }
            _ => Vec::new(),
        }
    }

    fn context_menu(&mut self, view: Point, p: Point) -> Vec<Effect> {
        self.gesture = None;
        let hit = self.scene.hit(&self.canvas, p, self.camera.zoom, &self.selection);
        let mut fx = Vec::new();
        let (target, groups) = match &hit {
            Hit::Wire { id } => {
                fx.extend(self.select_edge(Some(id.clone())));
                (MenuTarget::Wire(id.clone()), menu::wire_menu())
            }
            Hit::Empty => (MenuTarget::Canvas { frame: None }, menu::canvas_menu()),
            Hit::Node { id, part: Part::FrameBody } => (MenuTarget::Canvas { frame: Some(id.clone()) }, menu::canvas_menu()),
            _ => {
                let Some(id) = hit.node().map(str::to_owned) else { return fx };
                if self.selected_set.contains(&id) && self.selection.len() > 1 {
                    (MenuTarget::Selection(self.selection.clone()), menu::selection_menu(self.selection.len()))
                } else {
                    fx.extend(self.select_ids(vec![id.clone()]));
                    let groups = menu::node_menu(&self.canvas, &self.sections, &id);
                    (MenuTarget::Node(id), groups)
                }
            }
        };
        self.menu = Some(Menu { at: view, canvas_at: p, target, title: None, groups });
        fx.push(Effect::Menu);
        fx
    }

    fn moved(&mut self, at: Point, mods: Mods) -> Vec<Effect> {
        self.pointer = Some(at);
        let p = self.camera.to_canvas(at);
        let zoom = self.camera.zoom;
        let Some(gesture) = self.gesture.take() else {
            let hit = self.scene.hit(&self.canvas, p, zoom, &self.selection);
            if hit == self.hover {
                return Vec::new();
            }
            self.hover = hit;
            return vec![Effect::Redraw];
        };
        let mut fx = vec![Effect::Redraw];
        let next = match gesture {
            Gesture::Press { hit, view, at: start, mods: down_mods, collapse, deselect } => {
                if view.dist(at) < DRAG_VIEW {
                    return {
                        self.gesture = Some(Gesture::Press { hit, view, at: start, mods: down_mods, collapse, deselect });
                        Vec::new()
                    };
                }
                match &hit {
                    Hit::Node { id, part } if *part != Part::FrameBody && self.selected_set.contains(id) => {
                        let roots = self.roots(&self.selection);
                        let moving = self.moving_set(&self.selection);
                        let mut mask = vec![false; self.scene.laid.len()];
                        for &i in &moving {
                            mask[i] = true;
                        }
                        let bbox = Rect::bounds(roots.iter().filter_map(|r| self.scene.get(r)).map(|l| l.rect)).unwrap_or_default();
                        let snapper = self.snapper_except(|j| mask[j]);
                        let ids = moving.iter().map(|&i| self.canvas.nodes[i].id.clone()).collect();
                        Gesture::Move { ids, roots, mask, start, delta: Point::ZERO, bbox, snapper }
                    }
                    // A frame's body starts a marquee (inside a scene: its shots).
                    Hit::Node { part, .. } if *part != Part::FrameBody => return Vec::new(),
                    _ => {
                        let before = self.selection.clone();
                        let base = if down_mods.shift { before.clone() } else { Vec::new() };
                        Gesture::Marquee { start, now: start, base, before }
                    }
                }
            }
            other => other,
        };
        self.gesture = Some(match next {
            Gesture::Move { ids, roots, mask, start, bbox, snapper, .. } => {
                let mut delta = p - start;
                self.guides.clear();
                if !mods.cmd {
                    let (shift, guides) = snapper.snap_rect(bbox.translate(delta), SNAP_VIEW / zoom);
                    delta = delta + shift;
                    self.guides = guides;
                }
                Gesture::Move { ids, roots, mask, start, delta, bbox, snapper }
            }
            Gesture::Resize { id, corner, start, input, mut draft, snapper } => {
                let (q, guides) = if mods.cmd { (p, Vec::new()) } else { snapper.snap_point(p, SNAP_VIEW / zoom) };
                self.guides = guides;
                let rect = resized(&start, corner, q, mods.shift);
                let node = &self.canvas.nodes[start.index];
                *draft = layout::lay(start.index, node, rect, input);
                Gesture::Resize { id, corner, start, input, draft, snapper }
            }
            Gesture::Marquee { start, base, before, .. } => {
                let r = Rect::from_points(start, p);
                let mut ids = base.clone();
                for i in self.scene.marquee(r) {
                    let id = &self.canvas.nodes[i].id;
                    if !ids.contains(id) {
                        ids.push(id.clone());
                    }
                }
                fx.extend(self.select_ids(ids));
                Gesture::Marquee { start, now: p, base, before }
            }
            Gesture::Pan { view, pan } => {
                self.camera.pan = pan + (at - view);
                Gesture::Pan { view, pan }
            }
            Gesture::Wire { end, hit: old, drop, .. } => {
                let hit = self.scene.hit(&self.canvas, p, zoom, &[]);
                let drop = if hit == old { drop } else { wire::resolve(&self.canvas, &end, &hit) };
                Gesture::Wire { end, at: p, hit, drop }
            }
            Gesture::Draw { tool, start, .. } => Gesture::Draw { tool, start, now: p },
            Gesture::Minimap => {
                if let Some(map) = self.minimap_frame.and_then(|f| self.minimap(f)) {
                    self.camera = map.camera_at(self.camera, at, self.size);
                }
                Gesture::Minimap
            }
            press @ Gesture::Press { .. } => press,
        });
        fx
    }

    fn up(&mut self, at: Point, _button: Button, _mods: Mods) -> Vec<Effect> {
        let p = self.camera.to_canvas(at);
        let Some(gesture) = self.gesture.take() else { return Vec::new() };
        self.guides.clear();
        match gesture {
            Gesture::Press { hit, collapse, deselect, mods: down, .. } => self.click(&hit, collapse, deselect, down),
            Gesture::Move { roots, mask, delta, .. } => self.finish_move(&roots, &mask, delta),
            Gesture::Resize { id, draft, .. } => self.finish_resize(&id, &draft),
            Gesture::Marquee { .. } | Gesture::Pan { .. } | Gesture::Minimap => vec![Effect::Redraw],
            Gesture::Wire { end, drop, .. } => self.finish_wire(end, drop, at, p),
            Gesture::Draw { tool, start, .. } => {
                let r = Rect::from_points(start, p);
                let drawn = (r.w.max(r.h) * self.camera.zoom >= DRAW_MIN_VIEW).then_some(r);
                self.tool = Tool::Select;
                let made = match tool {
                    Tool::Frame => {
                        let r = drawn.map_or_else(|| Rect::new(p.x, p.y, layout::FRAME_SIZE.w, layout::FRAME_SIZE.h), |r| r.normalized());
                        let min = layout::FRAME_MIN;
                        self.maker().frame(Rect::new(r.x, r.y, r.w.max(min.w), r.h.max(min.h)))
                    }
                    _ => {
                        let min = layout::NOTE_MIN;
                        let drawn = drawn.map(|r| Rect::new(r.x, r.y, r.w.max(min.w), r.h.max(min.h)));
                        self.maker().note(p, drawn)
                    }
                };
                self.commit_made(made, false, tool == Tool::Note)
            }
        }
    }

    fn click(&mut self, hit: &Hit, collapse: bool, deselect: bool, mods: Mods) -> Vec<Effect> {
        match hit {
            Hit::Empty => {
                if mods.shift {
                    Vec::new()
                } else {
                    self.select_edge(None)
                }
            }
            Hit::Node { id, part: Part::FrameBody } => {
                if mods.shift {
                    let mut ids = self.selection.clone();
                    if let Some(at) = ids.iter().position(|x| x == id) {
                        ids.remove(at);
                    } else {
                        ids.push(id.clone());
                    }
                    self.select_ids(ids)
                } else {
                    self.select_ids(vec![id.clone()])
                }
            }
            Hit::Node { id, part } => {
                let mut fx = Vec::new();
                if deselect {
                    let ids = self.selection.iter().filter(|x| *x != id).cloned().collect();
                    return self.select_ids(ids);
                }
                if collapse {
                    fx.extend(self.select_ids(vec![id.clone()]));
                }
                let Some(node) = self.canvas.node(id) else { return fx };
                match (part, &node.body) {
                    (Part::Run, NodeBody::Gen { status, .. }) => fx.push(if matches!(status, GenStatus::Queued | GenStatus::Running) {
                        Effect::Stop { node: id.clone() }
                    } else {
                        Effect::Run { node: id.clone() }
                    }),
                    (Part::TakeBack, _) => fx.extend(self.step_take(id, -1)),
                    (Part::TakeForward, _) => fx.extend(self.step_take(id, 1)),
                    (Part::Prompt, NodeBody::Gen { .. }) if !mods.shift => fx.push(Effect::FocusPrompt { node: id.clone() }),
                    // The name strip under an entity's sheet opens it in the World view.
                    (Part::Body, NodeBody::Entity { id: entity }) if !mods.shift => fx.push(Effect::OpenWorld { entity: entity.clone() }),
                    _ => {}
                }
                fx
            }
            _ => Vec::new(),
        }
    }

    fn finish_move(&mut self, roots: &[String], mask: &[bool], delta: Point) -> Vec<Effect> {
        let d = Point::new(delta.x.round(), delta.y.round());
        if d == Point::ZERO {
            return vec![Effect::Redraw];
        }
        let mut ops: Vec<Op> = Vec::new();
        let mut index_of: HashMap<usize, usize> = HashMap::new();
        for (i, &m) in mask.iter().enumerate() {
            if m {
                let n = &self.canvas.nodes[i];
                index_of.insert(i, ops.len());
                ops.push(apply::moved(&n.id, n.x + d.x, n.y + d.y));
            }
        }
        // Moved roots join the frame their centre lands in (or leave theirs).
        for id in roots {
            let Some(i) = self.scene.index(id) else { continue };
            let rect = self.scene.laid[i].rect.translate(d);
            let frame = self.scene.frame_at(rect, |j| mask.get(j).copied().unwrap_or(false));
            if frame != self.scene.parent[i]
                && let Some(Op::UpdateNode { parent, .. }) = index_of.get(&i).and_then(|&k| ops.get_mut(k))
            {
                *parent = Some(frame.map(|f| self.canvas.nodes[f].id.clone()));
            }
        }
        self.send(ops, Record::Edit)
    }

    fn finish_resize(&mut self, id: &str, draft: &Laid) -> Vec<Effect> {
        let Some(i) = self.scene.index(id) else { return Vec::new() };
        let r = draft.rect;
        let n = &self.canvas.nodes[i];
        if (r.x - n.x).abs() < 0.5 && (r.y - n.y).abs() < 0.5 && (r.w - n.w).abs() < 0.5 && (r.h - n.h).abs() < 0.5 {
            return vec![Effect::Redraw];
        }
        let mut op = apply::update(id);
        if let Op::UpdateNode { x, y, w, h, .. } = &mut op {
            (*x, *y, *w, *h) = (Some(r.x.round()), Some(r.y.round()), Some(r.w.round()), Some(r.h.round()));
        }
        let mut ops = vec![op];
        if draft.shape.is_frame() {
            // A resized frame adopts what is now wholly inside it and lets go of what is now outside.
            let outer = self.scene.parent[i];
            let outer_id = outer.map(|o| self.canvas.nodes[o].id.clone());
            for l in &self.scene.laid {
                if l.index == i || self.scene.within(i, l.index) {
                    continue;
                }
                let nid = &self.canvas.nodes[l.index].id;
                let set = |p: Option<String>| {
                    let mut op = apply::update(nid);
                    if let Op::UpdateNode { parent, .. } = &mut op {
                        *parent = Some(p);
                    }
                    op
                };
                if self.scene.parent[l.index] == Some(i) && !r.contains(l.rect.center()) {
                    ops.push(set(outer_id.clone()));
                } else if self.scene.parent[l.index] == outer && r.contains_rect(l.rect) {
                    ops.push(set(Some(id.to_owned())));
                }
            }
        }
        self.send(ops, Record::Edit)
    }

    fn finish_wire(&mut self, end: WireEnd, drop: WireDrop, view: Point, p: Point) -> Vec<Effect> {
        match drop {
            WireDrop::Wire { from, to, port, replaces } => {
                let mut ops: Vec<Op> = replaces.into_iter().map(|id| Op::RemoveEdge { id }).collect();
                ops.push(create::wire(&self.ids.edge(), &from, &to, port));
                self.send(ops, Record::Edit)
            }
            WireDrop::Refused { reason, .. } => vec![Effect::Redraw, Effect::Notice(reason)],
            WireDrop::Create => {
                let (title, groups) = menu::create_menu(&self.canvas, &self.sections, &end);
                if groups.iter().all(|g| g.entries.is_empty()) {
                    return vec![Effect::Redraw];
                }
                self.menu = Some(Menu { at: view, canvas_at: p, target: MenuTarget::Create(end), title: Some(title), groups });
                vec![Effect::Redraw, Effect::Menu]
            }
            WireDrop::Nothing => vec![Effect::Redraw],
        }
    }

    /// Escape: cancels the gesture (a marquee restores the selection), else closes the menu, else goes back
    /// to Select, else clears the selection.
    pub fn escape(&mut self) -> Vec<Effect> {
        self.guides.clear();
        if let Some(g) = self.gesture.take() {
            if let Gesture::Marquee { before, .. } = g {
                let mut fx = self.select_ids(before);
                fx.push(Effect::Redraw);
                return fx;
            }
            return vec![Effect::Redraw];
        }
        if self.menu.is_some() {
            return self.close_menu();
        }
        if self.tool != Tool::Select {
            return self.set_tool(Tool::Select);
        }
        self.select_edge(None)
    }

    /// Where things made from the keyboard go: the pointer when it is over the board, else the centre.
    fn here(&self) -> (Point, Point) {
        let view = self.pointer.unwrap_or(Point::new(self.size.w / 2.0, self.size.h / 2.0));
        (self.camera.to_canvas(view), view)
    }

    fn key(&mut self, key: Key, mods: Mods) -> Vec<Effect> {
        let centre = Point::new(self.size.w / 2.0, self.size.h / 2.0);
        match key {
            Key::Space => {
                self.space = true;
                vec![Effect::Redraw]
            }
            Key::Escape => self.escape(),
            Key::Delete | Key::Backspace => self.delete_selection(),
            Key::Enter if mods.cmd && mods.alt => self.ask_agent(),
            Key::Enter => match self.selection.as_slice() {
                [id] => {
                    let id = id.clone();
                    match self.canvas.node(&id).map(|n| &n.body) {
                        Some(NodeBody::Note { .. }) => vec![Effect::EditNote { node: id }],
                        Some(NodeBody::Gen { .. }) => vec![Effect::FocusPrompt { node: id }],
                        Some(NodeBody::Asset { path, media: StudioMedia::Text, .. }) => vec![Effect::OpenDocument { path: path.clone() }],
                        Some(NodeBody::Asset { .. }) => vec![Effect::View { node: id }],
                        Some(NodeBody::Entity { id: e }) => vec![Effect::OpenWorld { entity: e.clone() }],
                        Some(NodeBody::Frame { .. }) => vec![Effect::RenameFrame { node: id }],
                        _ => Vec::new(),
                    }
                }
                _ => Vec::new(),
            },
            Key::Left | Key::Right | Key::Up | Key::Down => {
                let s = if mods.shift { NUDGE_BIG } else { NUDGE };
                let d = match key {
                    Key::Left => Point::new(-s, 0.0),
                    Key::Right => Point::new(s, 0.0),
                    Key::Up => Point::new(0.0, -s),
                    _ => Point::new(0.0, s),
                };
                self.nudge(d)
            }
            Key::Char(c) if mods.cmd => match c.to_ascii_lowercase() {
                'a' => self.select_all(),
                'd' => self.duplicate_selection(),
                'z' if mods.shift => self.redo(),
                'z' => self.undo(),
                'y' => self.redo(),
                '0' => self.fit_all(),
                '=' | '+' => self.zoom_by(centre, ZOOM_STEP),
                '-' => self.zoom_by(centre, 1.0 / ZOOM_STEP),
                'c' if !self.selection.is_empty() => vec![Effect::Copy { nodes: self.selection.clone() }],
                _ => Vec::new(),
            },
            Key::Char(c) if !mods.ctrl && !mods.alt => match c {
                'v' | 'V' => self.set_tool(Tool::Select),
                'n' | 'N' => self.set_tool(Tool::Note),
                // `f` fits the selection; with nothing selected (or `⇧F`) it is the Frame tool.
                'f' if !self.selection.is_empty() => self.fit_selection(),
                'f' | 'F' => self.set_tool(Tool::Frame),
                'g' | 'G' | '/' => {
                    let (at, view) = self.here();
                    self.prompt_at = Some(at);
                    vec![Effect::OpenPrompt { at, view }]
                }
                _ => Vec::new(),
            },
            Key::Char(_) => Vec::new(),
        }
    }

    fn ask_agent(&self) -> Vec<Effect> {
        if self.selection.is_empty() { Vec::new() } else { vec![Effect::AskAgent { nodes: self.selection.clone() }] }
    }

    // ------------------------------------------------------------ menus

    /// A menu entry, a selection action or a toolbar button was chosen. A menu's own target and point are
    /// used while it is open; otherwise the selection (a single node for node actions) and the view's centre.
    pub fn choose(&mut self, action: &Action) -> Vec<Effect> {
        let menu = self.menu.take();
        let mut fx = if menu.is_some() { vec![Effect::CloseMenu] } else { Vec::new() };
        let (at, target) = match &menu {
            Some(m) => (m.canvas_at, Some(m.target.clone())),
            None => (self.visible_rect().center(), None),
        };
        let nodes: Vec<String> = match &target {
            Some(MenuTarget::Node(id)) => vec![id.clone()],
            Some(MenuTarget::Selection(ids)) => ids.clone(),
            _ => self.selection.clone(),
        };
        let single = (nodes.len() == 1).then(|| nodes[0].clone());
        let path = single.as_deref().and_then(|id| menu::path_of(&self.canvas, &self.sections, id));
        let node_fx = |f: fn(String) -> Effect| single.clone().map(f).into_iter().collect::<Vec<_>>();
        fx.extend(match action {
            Action::Generate(makes) => {
                let made = self.maker().prompt_card(*makes, "", at);
                self.commit_made(made, false, false)
            }
            Action::NewNote => {
                let made = self.maker().note(at, None);
                self.commit_made(made, false, true)
            }
            Action::NewFrame => {
                let made = self.maker().frame(Rect::new(at.x, at.y, layout::FRAME_SIZE.w, layout::FRAME_SIZE.h));
                self.commit_made(made, false, false)
            }
            Action::AddFromWorld => vec![Effect::PickEntity { at }],
            Action::Run => node_fx(|node| Effect::Run { node }),
            Action::Stop => node_fx(|node| Effect::Stop { node }),
            Action::Settings => node_fx(|node| Effect::Settings { node }),
            Action::Takes => node_fx(|node| Effect::Takes { node }),
            Action::View => node_fx(|node| Effect::View { node }),
            Action::OpenDocument => match single.as_deref().and_then(|id| self.canvas.document_of(id)) {
                Some(path) => vec![Effect::OpenDocument { path: path.to_owned() }],
                None => Vec::new(),
            },
            Action::EditNote => node_fx(|node| Effect::EditNote { node }),
            Action::Rename => node_fx(|node| Effect::RenameFrame { node }),
            Action::OpenInWorld => match single.as_deref().and_then(|id| self.canvas.node(id)).map(|n| &n.body) {
                Some(NodeBody::Entity { id }) => vec![Effect::OpenWorld { entity: id.clone() }],
                _ => Vec::new(),
            },
            Action::Make { makes, port } => match &target {
                Some(MenuTarget::Create(WireEnd::Out { from })) => {
                    let made = self.maker().create_from(from, *makes, *port, at);
                    self.commit_made(made, false, false)
                }
                Some(MenuTarget::Create(WireEnd::In { to, port })) => {
                    let made = self.maker().create_into(to, *port, Some(*makes), at);
                    self.commit_made(made, false, false)
                }
                _ => match single.as_deref().and_then(|s| self.maker().make_from(s, *makes, *port)) {
                    Some(made) => self.commit_made(made, false, false),
                    None => Vec::new(),
                },
            },
            Action::MakeNote => match &target {
                Some(MenuTarget::Create(WireEnd::In { to, port })) => {
                    let made = self.maker().create_into(to, *port, None, at);
                    self.commit_made(made, false, true)
                }
                _ => Vec::new(),
            },
            Action::AskAgent => {
                if nodes.is_empty() {
                    Vec::new()
                } else {
                    vec![Effect::AskAgent { nodes }]
                }
            }
            Action::SetAsEntitySheet => path.map(|path| Effect::SetAsEntitySheet { path }).into_iter().collect(),
            Action::RevealInFiles => path.map(|path| Effect::RevealInFiles { path }).into_iter().collect(),
            Action::Save => path.map(|path| Effect::Save { path }).into_iter().collect(),
            Action::DeleteFile => match (single, path) {
                (Some(node), Some(path)) => vec![Effect::DeleteFile { node, path }],
                _ => Vec::new(),
            },
            Action::Copy => {
                if nodes.is_empty() {
                    Vec::new()
                } else {
                    vec![Effect::Copy { nodes }]
                }
            }
            Action::Duplicate => {
                let mut fx = self.select_ids(nodes);
                fx.extend(self.duplicate_selection());
                fx
            }
            Action::Delete => {
                let mut fx = self.select_ids(nodes);
                fx.extend(self.delete_selection());
                fx
            }
            Action::Unwire => match &target {
                Some(MenuTarget::Wire(id)) => self.unwire(id),
                _ => match self.selected_edge.clone() {
                    Some(id) => self.unwire(&id),
                    None => Vec::new(),
                },
            },
            Action::Tool(t) => self.set_tool(*t),
            Action::Prompt => {
                let view = Point::new(self.size.w / 2.0, self.size.h / 2.0);
                let at = self.camera.to_canvas(view);
                self.prompt_at = Some(at);
                vec![Effect::OpenPrompt { at, view }]
            }
            Action::Import => vec![Effect::Import { at }],
            // The whole canvas, wherever the menu opened; the server moves things, undo moves them back.
            Action::Tidy => self.send(vec![Op::Tidy { frame: None }], Record::Edit),
        });
        fx
    }
}

/// `start` resized by dragging `corner` to `p`: the opposite corner stays; pictures keep their aspect (`free`:
/// not), cards change width only (their height follows); never below the kind's minimum.
fn resized(start: &Laid, corner: Corner, p: Point, free: bool) -> Rect {
    let anchor = start.rect.corner(corner.opposite());
    let min = start.shape.min_size();
    let mut w = (p.x - anchor.x).abs().max(min.w);
    let mut h = (p.y - anchor.y).abs().max(min.h);
    match (start.shape, layout::locked_aspect(start)) {
        (Shape::Card(_), _) => h = start.rect.h,
        (Shape::Entity, Some(a)) if !free => {
            if w / (h - layout::ENTITY_NAME).max(1.0) > a {
                h = w / a + layout::ENTITY_NAME;
            } else {
                w = (h - layout::ENTITY_NAME).max(1.0) * a;
            }
        }
        (_, Some(a)) if !free => {
            if w / h > a {
                h = w / a;
            } else {
                w = h * a;
            }
        }
        _ => {}
    }
    let x = if corner.left() { anchor.x - w } else { anchor.x };
    // A card's top stays where it is.
    let y = if matches!(start.shape, Shape::Card(_)) {
        start.rect.y
    } else if corner.top() {
        anchor.y - h
    } else {
        anchor.y
    };
    Rect::new(x, y, w, h)
}
