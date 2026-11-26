//! Making things, as the ops to send: cards (the canvas prompt, Make from this, drag to create), notes,
//! frames that adopt what they are drawn around, imported files, world entities, duplicates and deletions.
//! Every added node and edge gets an id made here ([`Ids`]), so later ops in the same batch can name it and
//! the view can select or focus it at once; `canvas_edit` answers the ids in op order (the given ones, unless
//! the server had to make others: [`super::Board::acked`] renames them).

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use silo_proto::studio::{Canvas, EdgeRole, FrameRole, GenKind, GenStatus, NewEdge, NewNode, NodeBody, Op, Project, StudioMedia};

use super::apply::{self, update};
use super::geom::{Point, Rect, Size};
use super::layout::{self, CARD_W, FRAME_PAD, NOTE_SIZE, OUTPUT_GUTTER, PORT_GAP, PORT_GUTTER, PORT_SIZE, PORT_STEP, PORT_TOP};
use super::scene::Scene;

/// Ids for new nodes (`n_` + 8 hex) and edges (`e_` + 8 hex), from a seed the view passes in (random per
/// board): deterministic, so tests can predict them.
#[derive(Clone, Debug)]
pub struct Ids {
    state: u64,
}

impl Ids {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u32 {
        // splitmix64
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        #[allow(clippy::cast_possible_truncation)]
        let out = (z ^ (z >> 31)) as u32;
        out
    }

    pub fn node(&mut self) -> String {
        format!("n_{:08x}", self.next())
    }

    pub fn edge(&mut self) -> String {
        format!("e_{:08x}", self.next())
    }
}

/// Space kept between placed things (imports, notes); a card made from a node is [`CARD_GAP`] right of it (its
/// ports and the source's output float in between). The server's ([`silo_proto::studio::size`]).
pub use layout::{CARD_GAP, GAP};

/// A file imported into `assets/` (`studio.import`'s path) to place.
#[derive(Clone, Debug, PartialEq)]
pub struct Imported {
    /// Root-relative.
    pub path: String,
    pub media: StudioMedia,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub seconds: Option<f64>,
}

/// What a file is, by its extension (`.md`, `.markdown`, `.txt`: a text document).
#[must_use]
pub fn media_of_path(path: &str) -> StudioMedia {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "heif" => StudioMedia::Image,
        "avi" => StudioMedia::Video,
        "aiff" | "aif" => StudioMedia::Audio,
        _ => StudioMedia::of_path(path),
    }
}

/// A draft card's body.
#[must_use]
pub fn card_body(makes: GenKind, prompt: &str) -> NodeBody {
    NodeBody::Gen {
        makes,
        title: None,
        prompt: prompt.to_owned(),
        model: None,
        mode: None,
        options: Value::Null,
        status: GenStatus::Draft,
        progress: None,
        job: None,
        error: None,
        error_code: None,
        takes: Vec::new(),
        pick: None,
        inputs_used: Vec::new(),
        tool: None,
        input: None,
    }
}

/// `add_node` with every field set.
#[must_use]
pub fn add(id: &str, rect: Rect, parent: Option<String>, body: NodeBody) -> Op {
    Op::AddNode {
        node: NewNode {
            id: Some(id.to_owned()),
            x: Some(rect.x.round()),
            y: Some(rect.y.round()),
            w: Some(rect.w.round()),
            h: Some(rect.h.round()),
            z: None,
            parent,
            body,
        },
    }
}

/// `add_edge` with its id.
#[must_use]
pub fn wire(id: &str, from: &str, to: &str, role: EdgeRole) -> Op {
    Op::AddEdge { edge: NewEdge { id: Some(id.to_owned()), from: from.to_owned(), to: to.to_owned(), role, label: None } }
}

/// What creating needs: the canvas as shown, its layout, the project (formats) and the id source.
pub struct Maker<'a> {
    pub canvas: &'a Canvas,
    pub scene: &'a Scene,
    pub project: Option<&'a Project>,
    pub ids: &'a mut Ids,
}

/// Ops made, with the nodes they add (to select) and the one to focus.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Made {
    pub ops: Vec<Op>,
    pub nodes: Vec<String>,
    /// A new card whose prompt (or a new note to edit) takes focus.
    pub focus: Option<String>,
}

impl Maker<'_> {
    /// The frame a rect lands in (by its centre), as an id.
    fn frame_for(&self, rect: Rect) -> Option<String> {
        self.scene.frame_at(rect, |_| false).map(|i| self.canvas.nodes[i].id.clone())
    }

    /// A new card's size, at the effective aspect of where it lands.
    fn card_size(&self, makes: GenKind, at: Rect) -> Size {
        let frame = self.frame_for(at);
        let aspect = layout::effective_aspect(self.project, self.canvas, frame.as_deref());
        let w = layout::card_width(makes, aspect);
        Size::new(w, layout::card_height(makes, w, aspect, 1))
    }

    /// `rect` (a card's: with room for its ports and output) moved down until it overlaps no node (frames
    /// aside, their ports and dots counted).
    #[must_use]
    pub fn free_spot(&self, rect: Rect) -> Rect {
        let mut r = rect;
        for _ in 0..64 {
            let room = Rect::new(r.x - PORT_GUTTER, r.y, r.w + PORT_GUTTER + OUTPUT_GUTTER, r.h).outset(GAP / 2.0);
            let hit =
                self.scene.laid.iter().filter(|l| !l.shape.is_frame() && l.outer().intersects(room)).map(|l| l.outer().bottom()).reduce(f64::max);
            match hit {
                Some(bottom) => r.y = bottom + GAP,
                None => break,
            }
        }
        r
    }

    /// A card from the canvas prompt at canvas point `at` (centred on it, its top a little above).
    pub fn prompt_card(&mut self, makes: GenKind, text: &str, at: Point) -> Made {
        let size = self.card_size(makes, Rect::around(at, 1.0, 1.0));
        let rect = Rect::new(at.x - size.w / 2.0, at.y - 40.0, size.w, size.h);
        let id = self.ids.node();
        let parent = self.frame_for(rect);
        Made { ops: vec![add(&id, rect, parent, card_body(makes, text))], nodes: vec![id.clone()], focus: Some(id) }
    }

    /// Make from this (and the selection actions): a `makes` card to the right of `source` in free space,
    /// wired from it into `port`.
    pub fn make_from(&mut self, source: &str, makes: GenKind, port: EdgeRole) -> Option<Made> {
        let src = self.scene.get(source)?;
        let probe = Rect::new(src.rect.right() + CARD_GAP, src.rect.y, CARD_W, 1.0);
        let size = self.card_size(makes, probe);
        let rect = self.free_spot(Rect::new(probe.x, probe.y, size.w, size.h));
        let (id, edge) = (self.ids.node(), self.ids.edge());
        let parent = self.frame_for(rect);
        let ops = vec![add(&id, rect, parent, card_body(makes, "")), wire(&edge, source, &id, port)];
        Some(Made { ops, nodes: vec![id.clone()], focus: Some(id) })
    }

    /// Drag to create from a source's output, dropped at `at`: a `makes` card whose `port` lands there.
    pub fn create_from(&mut self, source: &str, makes: GenKind, port: EdgeRole, at: Point) -> Made {
        let size = self.card_size(makes, Rect::around(at, 1.0, 1.0));
        let k = makes.ports().iter().position(|p| *p == port).unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        let y = at.y - PORT_TOP - k as f64 * PORT_STEP;
        let rect = self.free_spot(Rect::new(at.x + PORT_GAP + PORT_SIZE / 2.0, y, size.w, size.h));
        let (id, edge) = (self.ids.node(), self.ids.edge());
        let parent = self.frame_for(rect);
        Made { ops: vec![add(&id, rect, parent, card_body(makes, "")), wire(&edge, source, &id, port)], nodes: vec![id.clone()], focus: Some(id) }
    }

    /// Drag to create from card `to`'s `port`, dropped at `at`: a `makes` card (`None`: a note) feeding it,
    /// its output there.
    pub fn create_into(&mut self, to: &str, port: EdgeRole, makes: Option<GenKind>, at: Point) -> Made {
        let size = makes.map_or(NOTE_SIZE, |m| self.card_size(m, Rect::around(at, 1.0, 1.0)));
        let y = if makes.is_some() { at.y - PORT_TOP } else { at.y - size.h / 2.0 };
        let rect = self.free_spot(Rect::new(at.x - PORT_GAP - PORT_SIZE / 2.0 - size.w, y, size.w, size.h));
        let (id, edge) = (self.ids.node(), self.ids.edge());
        let parent = self.frame_for(rect);
        let body = makes.map_or(NodeBody::Note { text: String::new() }, |m| card_body(m, ""));
        Made { ops: vec![add(&id, rect, parent, body), wire(&edge, &id, to, port)], nodes: vec![id.clone()], focus: Some(id) }
    }

    /// A note: `rect` drawn, else its default size with its top-left at `at`.
    pub fn note(&mut self, at: Point, rect: Option<Rect>) -> Made {
        let rect = rect.unwrap_or(Rect::new(at.x, at.y, NOTE_SIZE.w, NOTE_SIZE.h));
        let id = self.ids.node();
        let parent = self.frame_for(rect);
        Made { ops: vec![add(&id, rect, parent, NodeBody::Note { text: String::new() })], nodes: vec![id.clone()], focus: Some(id) }
    }

    /// A frame over `rect`; it adopts the nodes wholly inside it that were in the frame it lands in (or none).
    pub fn frame(&mut self, rect: Rect) -> Made {
        let id = self.ids.node();
        let parent_ix = self.scene.frame_at(rect, |_| false);
        let parent = parent_ix.map(|i| self.canvas.nodes[i].id.clone());
        let body = NodeBody::Frame { title: String::new(), role: FrameRole::Group, order: None, format: None };
        let mut ops = vec![add(&id, rect, parent, body)];
        for l in &self.scene.laid {
            if self.scene.parent[l.index] == parent_ix && rect.contains_rect(l.rect) {
                let mut op = update(&self.canvas.nodes[l.index].id);
                if let Op::UpdateNode { parent, .. } = &mut op {
                    *parent = Some(Some(id.clone()));
                }
                ops.push(op);
            }
        }
        Made { ops, nodes: vec![id], focus: None }
    }

    /// Imported files placed in a row from `at` (the first centred on it), each in the frame it lands in.
    pub fn imports(&mut self, files: &[Imported], at: Point) -> Made {
        let mut made = Made::default();
        let mut x = None;
        for f in files {
            let body = NodeBody::Asset { path: f.path.clone(), media: f.media, width: f.width, height: f.height, seconds: f.seconds, prompt: None };
            let size = layout::new_size(&body, layout::DEFAULT_ASPECT);
            let x0 = *x.get_or_insert(at.x - size.w / 2.0);
            let rect = Rect::new(x0, at.y - size.h / 2.0, size.w, size.h);
            x = Some(rect.right() + GAP);
            let id = self.ids.node();
            let parent = self.frame_for(rect);
            made.ops.push(add(&id, rect, parent, body));
            made.nodes.push(id);
        }
        made
    }

    /// A document card for root-relative `path`, centred at `at` (moved down off what it would cover).
    pub fn document(&mut self, path: &str, at: Point) -> Made {
        let body = NodeBody::Asset { path: path.to_owned(), media: StudioMedia::Text, width: None, height: None, seconds: None, prompt: None };
        let size = layout::new_size(&body, 1.0);
        let rect = self.free_spot(Rect::around(at, size.w, size.h));
        let id = self.ids.node();
        let parent = self.frame_for(rect);
        Made { ops: vec![add(&id, rect, parent, body)], nodes: vec![id], focus: None }
    }

    /// A world entity's node centred at `at`; `aspect`: its sheet's, when the view knows it.
    pub fn entity(&mut self, entity: &str, at: Point, aspect: Option<f64>) -> Made {
        let body = NodeBody::Entity { id: entity.to_owned() };
        let size = layout::new_size(&body, aspect.unwrap_or(1.0));
        let rect = Rect::around(at, size.w, size.h);
        let id = self.ids.node();
        let parent = self.frame_for(rect);
        Made { ops: vec![add(&id, rect, parent, body)], nodes: vec![id], focus: None }
    }

    /// Duplicates of `ids` (with a frame's contents) `offset` away: wires between them copied, wires into
    /// a copied card from the rest kept (it is made from the same inputs). Copies of running cards don't run.
    /// `nodes`: the copies of `ids`.
    pub fn duplicate(&mut self, ids: &[String], offset: Point) -> Made {
        let (canvas, scene) = (self.canvas, self.scene);
        let mut take: Vec<usize> = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            let Some(i) = scene.index(id) else { continue };
            for j in std::iter::once(i).chain(scene.descendants(i)) {
                if seen.insert(j) {
                    take.push(j);
                }
            }
        }
        // Parents before children.
        take.sort_by_key(|&i| (!scene.laid[i].shape.is_frame(), scene.laid[i].depth, i));
        let map: HashMap<&str, String> = take.iter().map(|&i| (canvas.nodes[i].id.as_str(), self.ids.node())).collect();
        // The copies of what was asked for (not a frame's contents) are what gets selected.
        let mut made = Made { nodes: ids.iter().filter_map(|id| map.get(id.as_str()).cloned()).collect(), ..Made::default() };
        for &i in &take {
            let n = &canvas.nodes[i];
            let mut body = n.body.clone();
            if let NodeBody::Gen { status, job, progress, takes, .. } = &mut body {
                *job = None;
                *progress = None;
                if matches!(status, GenStatus::Queued | GenStatus::Running) {
                    *status = if takes.is_empty() { GenStatus::Draft } else { GenStatus::Done };
                }
            }
            let parent = n.parent.as_deref().map(|p| map.get(p).cloned().unwrap_or_else(|| p.to_owned()));
            let new = &map[n.id.as_str()];
            made.ops.push(add(new, Rect::new(n.x + offset.x, n.y + offset.y, n.w, n.h), parent, body));
        }
        for e in &canvas.edges {
            let (from, to) = (map.get(e.from.as_str()), map.get(e.to.as_str()));
            let Some(to) = to else { continue };
            if from.is_none() && !e.role.is_port() {
                continue;
            }
            let from = from.cloned().unwrap_or_else(|| e.from.clone());
            let mut op = wire(&self.ids.edge(), &from, to, e.role);
            if let Op::AddEdge { edge } = &mut op {
                edge.label.clone_from(&e.label);
            }
            made.ops.push(op);
        }
        made
    }
}

/// Deleting `nodes` and `edges`: `remove_node` each (their wires go with them; a frame's contents stay) and
/// `remove_edge` for wires not already going.
#[must_use]
pub fn delete(canvas: &Canvas, nodes: &[String], edges: &[String]) -> Vec<Op> {
    let mut ops: Vec<Op> = nodes.iter().filter(|id| canvas.node(id).is_some()).map(|id| Op::RemoveNode { id: id.clone() }).collect();
    for id in edges {
        if let Some(e) = canvas.edges.iter().find(|e| &e.id == id)
            && !nodes.contains(&e.from)
            && !nodes.contains(&e.to)
        {
            ops.push(Op::RemoveEdge { id: id.clone() });
        }
    }
    ops
}

/// `update_node` for a card's prompt.
#[must_use]
pub fn set_prompt(id: &str, text: &str) -> Op {
    apply::patch(id, serde_json::json!({ "prompt": text }))
}

/// `update_node` for a note's text.
#[must_use]
pub fn set_note(id: &str, text: &str) -> Op {
    apply::patch(id, serde_json::json!({ "text": text }))
}

/// `update_node` for a frame's title.
#[must_use]
pub fn set_title(id: &str, title: &str) -> Op {
    apply::patch(id, serde_json::json!({ "title": title }))
}

/// `update_node` picking take `pick`.
#[must_use]
pub fn set_pick(id: &str, pick: u32) -> Op {
    apply::patch(id, serde_json::json!({ "pick": pick }))
}

/// A frame's padded inside, where adopted things go.
#[must_use]
#[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
pub fn frame_inside(frame: Rect) -> Rect {
    Rect::new(
        frame.x + FRAME_PAD,
        frame.y + layout::FRAME_TITLE + FRAME_PAD,
        (frame.w - 2.0 * FRAME_PAD).max(0.0),
        (frame.h - layout::FRAME_TITLE - 2.0 * FRAME_PAD).max(0.0),
    )
}
