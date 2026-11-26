//! Ops applied to a local [`Canvas`] the way the server applies them (for immediate feedback), each with its
//! inverse (for undo); the undo/redo stacks of the block's own edits.

use serde_json::{Map, Value};
use silo_proto::studio::{Canvas, Edge, NewEdge, NewNode, Node, NodeBody, Op, Project, mention};

use super::layout;

/// What [`apply`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Applied {
    /// One per `add_node` / `add_edge` op, in op order (empty when it failed), as `canvas_edit` answers.
    pub ids: Vec<String>,
    /// Ops undoing what applied, in the order to send them.
    pub inverse: Vec<Op>,
    /// Ops that didn't apply: their index and why (an id gone, a wire refused).
    pub failed: Vec<(usize, String)>,
}

/// An `update_node` op changing nothing yet: set its fields.
#[must_use]
pub fn update(id: &str) -> Op {
    Op::UpdateNode { id: id.to_owned(), x: None, y: None, w: None, h: None, z: None, parent: None, body: None }
}

/// `update_node` moving `id` to `x`, `y`.
#[must_use]
pub fn moved(id: &str, x: f64, y: f64) -> Op {
    Op::UpdateNode { id: id.to_owned(), x: Some(x), y: Some(y), w: None, h: None, z: None, parent: None, body: None }
}

/// `update_node` merging `patch` into `id`'s body.
#[must_use]
pub fn patch(id: &str, patch: Value) -> Op {
    Op::UpdateNode { id: id.to_owned(), x: None, y: None, w: None, h: None, z: None, parent: None, body: Some(patch) }
}

/// `add_node` for a whole node (its id kept).
#[must_use]
pub fn add_node(node: &Node) -> Op {
    Op::AddNode {
        node: NewNode {
            id: Some(node.id.clone()),
            x: Some(node.x),
            y: Some(node.y),
            w: Some(node.w),
            h: Some(node.h),
            z: node.z,
            parent: node.parent.clone(),
            body: node.body.clone(),
        },
    }
}

/// `add_edge` for a whole edge (its id kept).
#[must_use]
pub fn add_edge(edge: &Edge) -> Op {
    Op::AddEdge {
        edge: NewEdge { id: Some(edge.id.clone()), from: edge.from.clone(), to: edge.to.clone(), role: edge.role, label: edge.label.clone() },
    }
}

/// RFC 7396: `patch` merged into `target`.
pub fn merge(target: &mut Value, patch: &Value) {
    let Value::Object(p) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(t) = target {
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    }
}

/// The patch that undoes merging `patch` into `target`.
#[must_use]
pub fn inverse_patch(target: &Value, patch: &Value) -> Value {
    let Value::Object(p) = patch else { return target.clone() };
    let mut out = Map::new();
    for (k, v) in p {
        let prev = target.get(k);
        let back = match (v, prev) {
            (Value::Object(_), Some(prev @ Value::Object(_))) => inverse_patch(prev, v),
            (_, Some(prev)) => prev.clone(),
            (_, None) => Value::Null,
        };
        out.insert(k.clone(), back);
    }
    Value::Object(out)
}

/// Where the server puts a node added without coordinates, about: right of the last node in its frame (or of
/// the content), [`layout::CARD_GAP`] away when either is a card, else [`layout::GAP`]; else inside the frame's
/// top-left (a card's ports inside it), else the origin.
fn place(canvas: &Canvas, parent: Option<&str>, body: &NodeBody) -> (f64, f64) {
    let card = |b: &NodeBody| matches!(b, NodeBody::Gen { .. });
    let last = canvas.nodes.iter().rev().find(|n| n.parent.as_deref() == parent && !matches!(n.body, NodeBody::Frame { .. }));
    if let Some(n) = last {
        return (n.x + n.w + if card(body) || card(&n.body) { layout::CARD_GAP } else { layout::GAP }, n.y);
    }
    let ports = if card(body) { layout::PORT_GUTTER } else { 0.0 };
    parent.and_then(|p| canvas.node(p)).map_or((0.0, 0.0), |f| (f.x + layout::FRAME_PAD + ports, f.y + layout::FRAME_TITLE + layout::FRAME_PAD))
}

/// Applies `ops` to `canvas` in order, each on its own (one that fails is reported and skipped), as
/// `studio.canvas_edit` does: `remove_node` takes its edges and frees a frame's children; a wire into a frame
/// port replaces the one there; body patches merge (`kind` stays). `layout` is left to the server (the next
/// read shows it), and so is `tidy`, whose inverse moves every node back ([`tidy_inverse`]). Ops without ids
/// get `make_id()`'s.
pub fn apply(canvas: &mut Canvas, ops: &[Op], project: Option<&Project>, make_id: &mut dyn FnMut() -> String) -> Applied {
    let mut out = Applied::default();
    let mut groups: Vec<Vec<Op>> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        match apply_one(canvas, op, project, make_id) {
            Ok((id, inverse)) => {
                if let Some(id) = id {
                    out.ids.push(id);
                }
                groups.push(inverse);
            }
            Err(why) => {
                if matches!(op, Op::AddNode { .. } | Op::AddEdge { .. }) {
                    out.ids.push(String::new());
                }
                out.failed.push((i, why));
            }
        }
    }
    out.inverse = groups.into_iter().rev().flatten().collect();
    out
}

type OneResult = Result<(Option<String>, Vec<Op>), String>;

fn apply_one(canvas: &mut Canvas, op: &Op, project: Option<&Project>, make_id: &mut dyn FnMut() -> String) -> OneResult {
    match op {
        Op::AddNode { node } => {
            let id = node.id.clone().filter(|i| !i.is_empty()).unwrap_or_else(&mut *make_id);
            if canvas.node(&id).is_some() {
                return Err(format!("{id} exists"));
            }
            let (px, py) = place(canvas, node.parent.as_deref(), &node.body);
            let aspect = layout::own_aspect(&node.body).unwrap_or_else(|| layout::effective_aspect(project, canvas, node.parent.as_deref()));
            let size = layout::new_size(&node.body, aspect);
            canvas.nodes.push(Node {
                id: id.clone(),
                x: node.x.unwrap_or(px),
                y: node.y.unwrap_or(py),
                w: node.w.unwrap_or(size.w),
                h: node.h.unwrap_or(size.h),
                z: node.z,
                parent: node.parent.clone(),
                body: node.body.clone(),
            });
            Ok((Some(id.clone()), vec![Op::RemoveNode { id }]))
        }
        Op::UpdateNode { id, x, y, w, h, z, parent, body } => {
            let n = canvas.nodes.iter_mut().find(|n| &n.id == id).ok_or_else(|| format!("no node {id}"))?;
            let mut back = update(id);
            let Op::UpdateNode { x: bx, y: by, w: bw, h: bh, z: bz, parent: bp, body: bb, .. } = &mut back else { unreachable!() };
            let mut prompt_set = false;
            if let Some(patch) = body {
                prompt_set = patch.get("prompt").is_some();
                let mut patch = patch.clone();
                if let Value::Object(m) = &mut patch {
                    m.remove("kind");
                }
                let mut json = serde_json::to_value(&n.body).map_err(|e| e.to_string())?;
                let inverse = inverse_patch(&json, &patch);
                merge(&mut json, &patch);
                n.body = serde_json::from_value(json).map_err(|e| format!("bad body for {id}: {e}"))?;
                *bb = Some(inverse);
            }
            for (new, old, slot) in [(x, &mut n.x, bx), (y, &mut n.y, by), (w, &mut n.w, bw), (h, &mut n.h, bh)] {
                if let Some(v) = new {
                    *slot = Some(*old);
                    *old = *v;
                }
            }
            if let Some(v) = z {
                *bz = Some(n.z.unwrap_or(0));
                n.z = Some(*v);
            }
            if let Some(p) = parent {
                *bp = Some(n.parent.clone());
                n.parent.clone_from(p);
            }
            if prompt_set {
                unwire_mentions(canvas, id);
            }
            Ok((None, vec![back]))
        }
        Op::RemoveNode { id } => {
            let at = canvas.nodes.iter().position(|n| &n.id == id).ok_or_else(|| format!("no node {id}"))?;
            let node = canvas.nodes.remove(at);
            let mut inverse = vec![add_node(&node)];
            for c in canvas.nodes.iter_mut().filter(|c| c.parent.as_deref() == Some(id)) {
                c.parent = None;
                let mut back = update(&c.id);
                if let Op::UpdateNode { parent, .. } = &mut back {
                    *parent = Some(Some(id.clone()));
                }
                inverse.push(back);
            }
            let before = canvas.edges.clone();
            canvas.edges.retain(|e| &e.from != id && &e.to != id);
            let gone: Vec<String> = before.iter().filter(|e| &e.from == id || &e.to == id).map(|e| e.id.clone()).collect();
            inverse.extend(restore_edges(&before, &gone));
            Ok((None, inverse))
        }
        Op::AddEdge { edge } => {
            let replaces = canvas.wire_check(&edge.from, &edge.to, edge.role)?;
            let id = edge.id.clone().filter(|i| !i.is_empty()).unwrap_or_else(&mut *make_id);
            if canvas.edges.iter().any(|e| e.id == id) {
                return Err(format!("{id} exists"));
            }
            let mut inverse = vec![Op::RemoveEdge { id: id.clone() }];
            let (gone, kept): (Vec<Edge>, Vec<Edge>) = std::mem::take(&mut canvas.edges).into_iter().partition(|e| replaces.contains(&e.id));
            canvas.edges = kept;
            inverse.extend(gone.iter().map(add_edge));
            canvas.edges.push(Edge {
                id: id.clone(),
                from: edge.from.clone(),
                to: edge.to.clone(),
                role: edge.role,
                label: edge.label.clone(),
                mention: false,
            });
            Ok((Some(id), inverse))
        }
        Op::RemoveEdge { id } => {
            let at = canvas.edges.iter().position(|e| &e.id == id).ok_or_else(|| format!("no edge {id}"))?;
            let before = canvas.edges.clone();
            canvas.edges.remove(at);
            Ok((None, restore_edges(&before, std::slice::from_ref(id))))
        }
        Op::Tidy { frame } => {
            if let Some(f) = frame
                && !matches!(canvas.node(f).map(|n| &n.body), Some(NodeBody::Frame { .. }))
            {
                return Err(format!("no frame {f}"));
            }
            Ok((None, tidy_inverse(canvas)))
        }
        Op::Layout { .. } | Op::Unknown => Ok((None, Vec::new())),
    }
}

/// As the server does after an edit setting card `id`'s prompt: the wires mentions made into it ([`Edge::mention`])
/// whose source no token of the prompt names any more go. Not in the inverse: putting the prompt back makes the
/// server wire them again (a wire added back by `add_edge` would be one drawn by hand). The wires a new token makes
/// come with the server's canvas.
fn unwire_mentions(canvas: &mut Canvas, id: &str) {
    let Some(NodeBody::Gen { prompt, .. }) = canvas.node(id).map(|n| &n.body) else { return };
    let named: Vec<mention::Target> = mention::tokens(prompt).into_iter().map(|t| t.target).collect();
    let gone: Vec<String> = canvas
        .edges
        .iter()
        .filter(|e| e.to == id && e.mention && !named.iter().any(|t| canvas.mention_names(t, &e.from, &[])))
        .map(|e| e.id.clone())
        .collect();
    canvas.edges.retain(|e| !gone.contains(&e.id));
}

/// Most nodes a `tidy` is undoable for: its inverse is one op per node, and `canvas_edit` takes 2,000.
const TIDY_UNDO_MAX: usize = 2_000;

/// Undoing a `tidy` (applied by the server only): every node back where it is now, frames at their size too.
/// It moves nodes and frames' sizes only, and which ones the server moves isn't known here, so all of them
/// (an unchanged one is a no-op); none above [`TIDY_UNDO_MAX`] nodes (not undoable).
fn tidy_inverse(canvas: &Canvas) -> Vec<Op> {
    if canvas.nodes.len() > TIDY_UNDO_MAX {
        return Vec::new();
    }
    canvas
        .nodes
        .iter()
        .map(|n| {
            let frame = matches!(n.body, NodeBody::Frame { .. });
            let (w, h) = if frame { (Some(n.w), Some(n.h)) } else { (None, None) };
            Op::UpdateNode { id: n.id.clone(), x: Some(n.x), y: Some(n.y), w, h, z: None, parent: None, body: None }
        })
        .collect()
}

/// The ops bringing back edges `removed` from `before` (the edges as they were) in their places within their
/// ports: `add_edge` appends, so the wires made after one into the same port are taken out and added again
/// after it (the first `reference` wire stays image 1).
fn restore_edges(before: &[Edge], removed: &[String]) -> Vec<Op> {
    let (mut out, mut readd) = (Vec::new(), Vec::new());
    for (i, e) in before.iter().enumerate() {
        let gone = removed.contains(&e.id);
        let after_gone = before[..i].iter().any(|r| removed.contains(&r.id) && r.to == e.to && r.role == e.role);
        if !gone && after_gone {
            out.push(Op::RemoveEdge { id: e.id.clone() });
        }
        if gone || after_gone {
            readd.push(add_edge(e));
        }
    }
    out.extend(readd);
    out
}

/// Renames id `from` to `to` in `ops` (node and edge ids, ends, parents): after the server made another id.
pub fn rename(ops: &mut [Op], from: &str, to: &str) {
    let fix = |s: &mut String| {
        if s == from {
            to.clone_into(s);
        }
    };
    for op in ops {
        match op {
            Op::AddNode { node } => node.id.iter_mut().chain(node.parent.iter_mut()).for_each(fix),
            Op::UpdateNode { id, parent, .. } => {
                fix(id);
                if let Some(Some(p)) = parent {
                    fix(p);
                }
            }
            Op::RemoveNode { id } | Op::RemoveEdge { id } => fix(id),
            Op::AddEdge { edge } => edge.id.iter_mut().chain([&mut edge.from, &mut edge.to]).for_each(fix),
            Op::Layout { ids, frame, .. } => ids.iter_mut().chain(frame.iter_mut()).for_each(fix),
            Op::Tidy { frame } => frame.iter_mut().for_each(fix),
            Op::Unknown => {}
        }
    }
}

/// Renames id `from` to `to` in a canvas.
pub fn rename_in_canvas(canvas: &mut Canvas, from: &str, to: &str) {
    for n in &mut canvas.nodes {
        if n.id == from {
            to.clone_into(&mut n.id);
        }
        if n.parent.as_deref() == Some(from) {
            n.parent = Some(to.to_owned());
        }
    }
    for e in &mut canvas.edges {
        for s in [&mut e.id, &mut e.from, &mut e.to] {
            if s == from {
                to.clone_into(s);
            }
        }
    }
}

/// An undo step: the ops that undo one batch, and the batch they undo.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub ops: Vec<Op>,
    pub batch: u64,
}

/// The block's own edits, session-local: undo and redo stacks of inverse batches.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub undo: Vec<Step>,
    pub redo: Vec<Step>,
}

impl History {
    /// Most steps kept.
    pub const LIMIT: usize = 200;

    /// A new edit: its inverse on the undo stack; the redo branch ends.
    pub fn push(&mut self, step: Step) {
        self.redo.clear();
        self.push_undo(step);
    }

    pub fn push_undo(&mut self, step: Step) {
        if step.ops.is_empty() {
            return;
        }
        self.undo.push(step);
        if self.undo.len() > Self::LIMIT {
            self.undo.remove(0);
        }
    }

    pub fn push_redo(&mut self, step: Step) {
        if !step.ops.is_empty() {
            self.redo.push(step);
        }
    }

    /// Adds `ops` to the end of the step undoing batch `batch` (on either stack): what the server did besides the
    /// batch's ops (an Entity node a mention added). Nothing when that step is gone (taken by an undo already).
    pub fn extend(&mut self, batch: u64, ops: Vec<Op>) {
        if let Some(step) = self.undo.iter_mut().chain(self.redo.iter_mut()).find(|s| s.batch == batch) {
            step.ops.extend(ops);
        }
    }

    /// Forgets the steps of batch `batch` (it failed).
    pub fn forget(&mut self, batch: u64) {
        self.undo.retain(|s| s.batch != batch);
        self.redo.retain(|s| s.batch != batch);
    }

    pub fn rename(&mut self, from: &str, to: &str) {
        for s in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            rename(&mut s.ops, from, to);
        }
    }
}
