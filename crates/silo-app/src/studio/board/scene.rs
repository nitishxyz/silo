//! The canvas laid out once per change ([`Scene`]): every node's [`Laid`], the draw order (frames below,
//! the selection on top), frame nesting, the content's bounds; and hit testing against it: a point → what
//! is under it, a marquee → the nodes inside, a rect → the frame it lands in.

use std::collections::{HashMap, HashSet};

use silo_proto::studio::{Canvas, EdgeRole, NodeBody, Project};

use super::geom::{Corner, Point, Rect};
use super::layout::{self, Laid, LayoutInput, PORT_HIT, PORT_SIZE, Shape};
use super::wire::Curve;

/// Points are hit at least this many view points around small targets (ports, dots, wires).
pub const HIT_SLOP_VIEW: f64 = 6.0;
/// Resize handles: squares this many view points wide on a selected node's corners.
pub const HANDLE_VIEW: f64 = 10.0;

/// A part of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// Anywhere else on it.
    Body,
    /// A card's header (outside its stepper); a document's title bar.
    Header,
    /// A card's, asset's or entity's picture.
    Face,
    /// A card's prompt.
    Prompt,
    /// A card's Run (Stop while it runs, Retry when it failed).
    Run,
    /// The take stepper's halves.
    TakeBack,
    TakeForward,
    /// A frame's title bar.
    Title,
    /// A frame's body: empty space inside it.
    FrameBody,
}

/// What is under a point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    Empty,
    Node {
        id: String,
        part: Part,
    },
    /// A card's input port.
    Port {
        id: String,
        port: EdgeRole,
    },
    /// A node's output dot.
    Output {
        id: String,
    },
    Wire {
        id: String,
    },
    /// A selected node's resize corner.
    Resize {
        id: String,
        corner: Corner,
    },
}

impl Hit {
    /// The node it is on (a port, dot, corner or part).
    #[must_use]
    pub fn node(&self) -> Option<&str> {
        match self {
            Self::Node { id, .. } | Self::Port { id, .. } | Self::Output { id } | Self::Resize { id, .. } => Some(id),
            Self::Empty | Self::Wire { .. } => None,
        }
    }
}

/// The canvas laid out: rebuilt when the canvas, the selection's set, the project or a measure changes.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    /// One per `canvas.nodes`, same index.
    pub laid: Vec<Laid>,
    /// Draw order, bottom to top: frames (outer first), then the rest by `z`; the selection last in each.
    pub order: Vec<usize>,
    pub ids: HashMap<String, usize>,
    /// The frame each node is in (a missing or non-frame parent: none).
    pub parent: Vec<Option<usize>>,
    pub children: Vec<Vec<usize>>,
    /// The bounds of every node (ports and dots included).
    pub content: Option<Rect>,
}

impl Scene {
    /// Lays `canvas` out. `prompts`: measured prompt heights by card id; `selected`: drawn on top; `open`: cards
    /// whose prompt shows in full.
    #[must_use]
    pub fn build(
        canvas: &Canvas,
        project: Option<&Project>,
        prompts: &HashMap<String, f64>,
        selected: &HashSet<String>,
        open: &HashSet<String>,
    ) -> Self {
        let n = canvas.nodes.len();
        let ids: HashMap<String, usize> = canvas.nodes.iter().enumerate().map(|(i, node)| (node.id.clone(), i)).collect();
        let parent: Vec<Option<usize>> = canvas
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let p = node.parent.as_deref().and_then(|p| ids.get(p).copied())?;
                (p != i && matches!(canvas.nodes[p].body, NodeBody::Frame { .. })).then_some(p)
            })
            .collect();
        let mut children = vec![Vec::new(); n];
        for (i, p) in parent.iter().enumerate() {
            if let Some(p) = p {
                children[*p].push(i);
            }
        }
        let mut wired = vec![0u16; n];
        for e in &canvas.edges {
            let Some(&to) = ids.get(&e.to) else { continue };
            if let NodeBody::Gen { makes, .. } = &canvas.nodes[to].body
                && let Some(i) = makes.ports().iter().position(|p| *p == e.role)
                && i < 16
            {
                wired[to] |= 1 << i;
            }
        }
        let laid: Vec<Laid> = canvas
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let depth = depth_of(&parent, i);
                let aspect =
                    if matches!(node.body, NodeBody::Gen { .. }) { layout::card_aspect(project, canvas, node) } else { layout::DEFAULT_ASPECT };
                let input = LayoutInput { aspect, prompt: prompts.get(&node.id).copied(), open: open.contains(&node.id), depth, wired: wired[i] };
                layout::lay(i, node, Rect::new(node.x, node.y, node.w.max(1.0), node.h.max(1.0)), input)
            })
            .collect();
        let content = Rect::bounds(laid.iter().map(Laid::outer));
        let mut scene = Self { laid, order: Vec::new(), ids, parent, children, content };
        scene.reorder(canvas, selected);
        scene
    }

    /// Recomputes the draw order for a new selection.
    pub fn reorder(&mut self, canvas: &Canvas, selected: &HashSet<String>) {
        let mut order: Vec<usize> = (0..self.laid.len()).collect();
        order.sort_by_key(|&i| {
            let l = &self.laid[i];
            let frame = l.shape.is_frame();
            (!frame, if frame { l.depth } else { 0 }, selected.contains(&canvas.nodes[i].id), canvas.nodes[i].z.unwrap_or(0), i)
        });
        self.order = order;
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Laid> {
        self.ids.get(id).map(|&i| &self.laid[i])
    }

    #[must_use]
    pub fn index(&self, id: &str) -> Option<usize> {
        self.ids.get(id).copied()
    }

    /// Every node inside node `i`'s frame, at any depth (not `i`).
    #[must_use]
    pub fn descendants(&self, i: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![i];
        let mut seen = HashSet::from([i]);
        while let Some(at) = stack.pop() {
            for &c in self.children.get(at).map_or(&[][..], Vec::as_slice) {
                if seen.insert(c) {
                    out.push(c);
                    stack.push(c);
                }
            }
        }
        out
    }

    /// Whether node `i` is `ancestor` or inside it.
    #[must_use]
    pub fn within(&self, i: usize, ancestor: usize) -> bool {
        let mut at = Some(i);
        for _ in 0..=self.laid.len() {
            match at {
                Some(a) if a == ancestor => return true,
                Some(a) => at = self.parent[a],
                None => return false,
            }
        }
        false
    }

    /// The innermost frame (smallest) whose rect holds `rect`'s centre, larger than `rect`, not `skip`ped:
    /// the frame a node dropped at `rect` joins.
    #[must_use]
    pub fn frame_at(&self, rect: Rect, skip: impl Fn(usize) -> bool) -> Option<usize> {
        let c = rect.center();
        self.laid
            .iter()
            .filter(|l| l.shape.is_frame() && !skip(l.index) && l.rect.contains(c) && l.rect.area() > rect.area())
            .min_by(|a, b| a.rect.area().total_cmp(&b.rect.area()))
            .map(|l| l.index)
    }

    /// The nodes a marquee selects: any it touches, but frames only when wholly inside (so a marquee inside a
    /// scene selects its shots, not the scene).
    #[must_use]
    pub fn marquee(&self, r: Rect) -> Vec<usize> {
        self.order
            .iter()
            .copied()
            .filter(|&i| if self.laid[i].shape.is_frame() { r.contains_rect(self.laid[i].rect) } else { r.intersects(self.laid[i].rect) })
            .collect()
    }

    /// The wire curve of edge `e` (a port role into a card on this canvas).
    #[must_use]
    pub fn curve(&self, canvas: &Canvas, e: usize) -> Option<Curve> {
        let edge = canvas.edges.get(e)?;
        if !edge.role.is_port() {
            return None;
        }
        let (from, to) = (self.get(&edge.from)?, self.get(&edge.to)?);
        if !matches!(to.shape, Shape::Card(_)) || !from.output {
            return None;
        }
        Some(Curve::wire(from.output_center(), to.input_point(edge.role)))
    }

    /// What is under canvas point `p` at `zoom`, `selected` showing resize corners. In order: corners of the
    /// selection, ports and output dots, nodes (top first, frames by their title), wires, frame bodies.
    #[must_use]
    pub fn hit(&self, canvas: &Canvas, p: Point, zoom: f64, selected: &[String]) -> Hit {
        let slop = (HIT_SLOP_VIEW / zoom).max(PORT_HIT);
        let handle = HANDLE_VIEW / zoom;
        for id in selected.iter().rev() {
            let Some(l) = self.get(id) else { continue };
            for corner in Corner::ALL {
                if Rect::around(l.rect.corner(corner), handle, handle).outset(slop / 2.0).contains(p) {
                    return Hit::Resize { id: id.clone(), corner };
                }
            }
        }
        let radius = PORT_SIZE / 2.0 + slop;
        for &i in self.order.iter().rev() {
            let l = &self.laid[i];
            if !l.outer().outset(slop).contains(p) {
                continue;
            }
            if l.output && l.output_center().dist(p) <= radius {
                return Hit::Output { id: canvas.nodes[i].id.clone() };
            }
            if let Some(k) = (0..l.ports.len()).find(|&k| l.port_center(k).dist(p) <= radius) {
                return Hit::Port { id: canvas.nodes[i].id.clone(), port: l.ports[k] };
            }
        }
        for &i in self.order.iter().rev() {
            let l = &self.laid[i];
            // Frames are hit here by their title bar only; their bodies come after wires.
            let on = if l.shape.is_frame() { l.title.is_some_and(|t| t.contains(p)) } else { l.rect.contains(p) };
            if on {
                return Hit::Node { id: canvas.nodes[i].id.clone(), part: part_at(l, p) };
            }
        }
        for (e, edge) in canvas.edges.iter().enumerate() {
            let Some(curve) = self.curve(canvas, e) else { continue };
            if curve.bounds().outset(slop).contains(p) && curve.distance(p) <= slop {
                return Hit::Wire { id: edge.id.clone() };
            }
        }
        for &i in self.order.iter().rev() {
            let l = &self.laid[i];
            if l.shape.is_frame() && l.rect.contains(p) {
                return Hit::Node { id: canvas.nodes[i].id.clone(), part: part_at(l, p) };
            }
        }
        Hit::Empty
    }
}

/// Frames a node is in, from its parent links (cycles end it).
fn depth_of(parent: &[Option<usize>], i: usize) -> u16 {
    let mut d = 0u16;
    let mut at = parent[i];
    while let Some(p) = at {
        if usize::from(d) >= parent.len() || p == i {
            break;
        }
        d += 1;
        at = parent[p];
    }
    d
}

/// Which part of `l` is under `p` (inside its rect).
#[must_use]
pub fn part_at(l: &Laid, p: Point) -> Part {
    if let Some(c) = &l.card {
        if let Some((back, fwd)) = c.stepper {
            if back.contains(p) {
                return Part::TakeBack;
            }
            if fwd.contains(p) {
                return Part::TakeForward;
            }
        }
        return if c.run.contains(p) {
            Part::Run
        } else if c.header.contains(p) {
            Part::Header
        } else if c.face.contains(p) {
            Part::Face
        } else if c.prompt.contains(p) {
            Part::Prompt
        } else {
            Part::Body
        };
    }
    if let Some(t) = l.title {
        return if t.contains(p) { Part::Title } else { Part::FrameBody };
    }
    match l.face {
        Some(f) if f.contains(p) => Part::Face,
        // A document's title bar.
        Some(_) if l.shape.is_document() => Part::Header,
        _ => Part::Body,
    }
}
