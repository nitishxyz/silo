//! Optimistic apply and its inverses, the undo/redo stack, and reconciling with the server's canvases.

use serde_json::json;
use silo_proto::studio::{Canvas, CanvasEdited, Edge, EdgeRole, GenKind, MentionDid, MentionSynced, NewEdge, NewNode, NodeBody, Op, OpFailed};

use super::super::apply::{self, History, Step, apply, inverse_patch, merge};
use super::super::create;
use super::super::layout::{CARD_GAP, GAP};
use super::super::{Action, Board, Key, Point};
use super::fixtures::*;

fn fixture() -> Canvas {
    let mut c = canvas(
        vec![
            frame("f", 0.0, 0.0, 1200.0, 800.0),
            in_frame(image("i", 40.0, 80.0), "f"),
            in_frame(with_options(card("c", GenKind::Video, 500.0, 80.0), json!({ "duration": 5, "aspect_ratio": "9:16" })), "f"),
            note("n", -400.0, 0.0, "beat one"),
            image("j", -400.0, 400.0),
        ],
        vec![edge("e1", "i", "c", EdgeRole::FirstFrame), edge("e2", "n", "c", EdgeRole::Text), edge("e3", "j", "c", EdgeRole::Reference)],
    );
    c.nodes[3].z = Some(2);
    c
}

fn ids() -> impl FnMut() -> String {
    let mut n = 0;
    move || {
        n += 1;
        format!("made{n}")
    }
}

/// Applies `ops` (all must apply and change the canvas), then their inverse: the canvas comes back.
#[track_caller]
fn round_trip(c: &Canvas, ops: Vec<Op>) -> Canvas {
    let mut x = c.clone();
    let mut make = ids();
    let a = apply(&mut x, &ops, Some(&project()), &mut make);
    assert!(a.failed.is_empty(), "{:?}", a.failed);
    assert_ne!(sorted(&x), sorted(c), "{ops:?} changed nothing");
    let changed = x.clone();
    let back = apply(&mut x, &a.inverse, Some(&project()), &mut make);
    assert!(back.failed.is_empty(), "{:?}", back.failed);
    assert_eq!(sorted(&x), sorted(c), "the inverse of {ops:?} ({:?})", a.inverse);
    changed
}

fn new_node(id: Option<&str>, body: NodeBody) -> NewNode {
    NewNode { id: id.map(str::to_owned), x: None, y: None, w: None, h: None, z: None, parent: None, body }
}

#[test]
fn add_node_and_back() {
    let c = fixture();
    let mut n = new_node(Some("new"), NodeBody::Note { text: "hi".into() });
    (n.x, n.y, n.w, n.h) = (Some(5.0), Some(6.0), Some(100.0), Some(80.0));
    let x = round_trip(&c, vec![Op::AddNode { node: n }]);
    let added = x.node("new").unwrap();
    assert_eq!((added.x, added.y, added.w, added.h), (5.0, 6.0, 100.0, 80.0));
}

#[test]
fn add_node_without_id_or_place_is_given_both() {
    let c = fixture();
    let mut x = c.clone();
    let a = apply(&mut x, &[Op::AddNode { node: new_node(None, super::fixtures::gen_body(GenKind::Image, "")) }], Some(&project()), &mut ids());
    assert_eq!(a.ids, ["made1"]);
    let n = x.node("made1").unwrap();
    // Right of the last node outside frames (j), CARD_GAP away (a card), sized for a 16:9 card.
    assert_eq!((n.x, n.y), (-400.0 + 320.0 + CARD_GAP, 400.0));
    assert_eq!(n.w, 340.0);
    // In a frame: right of the last node in it (a card: CARD_GAP from it).
    let mut inside = new_node(Some("k"), NodeBody::Note { text: String::new() });
    inside.parent = Some("f".into());
    apply(&mut x, &[Op::AddNode { node: inside }], None, &mut ids());
    assert_eq!(x.node("k").unwrap().x, 500.0 + 320.0 + CARD_GAP);
    // Two notes: GAP apart.
    apply(&mut x, &[Op::AddNode { node: new_node(Some("k2"), NodeBody::Note { text: String::new() }) }], None, &mut ids());
    let k2 = x.node("k2").unwrap();
    assert_eq!((k2.x, k2.w), (-400.0 + 320.0 + CARD_GAP + 340.0 + CARD_GAP, 240.0));
    let mut k3 = new_node(Some("k3"), NodeBody::Note { text: String::new() });
    k3.parent = Some("f".into());
    apply(&mut x, &[Op::AddNode { node: k3 }], None, &mut ids());
    assert_eq!(x.node("k3").unwrap().x, x.node("k").unwrap().x + 240.0 + GAP);
}

#[test]
fn update_node_geometry_parent_and_z_and_back() {
    let c = fixture();
    round_trip(&c, vec![apply::moved("i", 70.0, 90.0)]);
    let mut op = apply::update("c");
    if let Op::UpdateNode { w, h, .. } = &mut op {
        (*w, *h) = (Some(400.0), Some(10.0));
    }
    round_trip(&c, vec![op]);
    // z from a set value.
    let mut op = apply::update("n");
    if let Op::UpdateNode { z, .. } = &mut op {
        *z = Some(9);
    }
    round_trip(&c, vec![op]);
    // Out of the frame, and into one.
    let mut out = apply::update("i");
    if let Op::UpdateNode { parent, .. } = &mut out {
        *parent = Some(None);
    }
    let x = round_trip(&c, vec![out]);
    assert_eq!(x.node("i").unwrap().parent, None);
    let mut into = apply::update("j");
    if let Op::UpdateNode { parent, .. } = &mut into {
        *parent = Some(Some("f".into()));
    }
    round_trip(&c, vec![into]);
}

#[test]
fn body_updates_are_merge_patches_and_come_back() {
    let c = fixture();
    // A prompt.
    let x = round_trip(&c, vec![apply::patch("c", json!({ "prompt": "slow push in" }))]);
    assert!(matches!(&x.node("c").unwrap().body, NodeBody::Gen { prompt, .. } if prompt == "slow push in"));
    // Nested options: one set, one removed with null, the rest kept.
    let x = round_trip(&c, vec![apply::patch("c", json!({ "options": { "resolution": "720p", "aspect_ratio": null } }))]);
    let NodeBody::Gen { options, .. } = &x.node("c").unwrap().body else { panic!() };
    assert_eq!(options, &json!({ "duration": 5, "resolution": "720p" }));
    // A field that wasn't there (title), on a note, on a frame.
    round_trip(&c, vec![apply::patch("c", json!({ "title": "Keyframe 1", "model": "fal/kling" }))]);
    round_trip(&c, vec![apply::patch("n", json!({ "text": "beat two" }))]);
    round_trip(&c, vec![apply::patch("f", json!({ "title": "Scene 1", "role": "scene", "order": 1 }))]);
    // `kind` can't change.
    let mut x = c.clone();
    apply(&mut x, &[apply::patch("n", json!({ "kind": "frame", "text": "t" }))], None, &mut ids());
    assert!(matches!(&x.node("n").unwrap().body, NodeBody::Note { text } if text == "t"));
}

#[test]
fn merge_and_inverse_patch_follow_rfc_7396() {
    let mut t = json!({ "a": 1, "b": { "c": 2, "d": 3 } });
    let p = json!({ "a": null, "b": { "c": 4 }, "e": [1] });
    let inv = inverse_patch(&t, &p);
    let orig = t.clone();
    merge(&mut t, &p);
    assert_eq!(t, json!({ "b": { "c": 4, "d": 3 }, "e": [1] }));
    merge(&mut t, &inv);
    assert_eq!(t, orig);
}

#[test]
fn removing_a_node_takes_its_wires_and_comes_back() {
    let c = fixture();
    let x = round_trip(&c, vec![Op::RemoveNode { id: "n".into() }]);
    assert!(x.node("n").is_none());
    assert!(!x.edges.iter().any(|e| e.id == "e2"));
    let x = round_trip(&c, vec![Op::RemoveNode { id: "c".into() }]);
    assert!(x.edges.is_empty());
}

#[test]
fn removing_a_frame_frees_its_children_and_comes_back() {
    let c = fixture();
    let x = round_trip(&c, vec![Op::RemoveNode { id: "f".into() }]);
    assert!(x.node("f").is_none());
    assert_eq!(x.node("i").unwrap().parent, None);
    assert_eq!(x.node("c").unwrap().parent, None);
    // The children's wires stay.
    assert_eq!(x.edges.len(), 3);
    // A frame and its card together.
    round_trip(&c, vec![Op::RemoveNode { id: "c".into() }, Op::RemoveNode { id: "f".into() }]);
}

#[test]
fn a_removed_id_can_be_added_again_but_not_twice() {
    let c = fixture();
    let mut x = c.clone();
    let mut make = ids();
    let gone = x.node("n").unwrap().clone();
    apply(&mut x, &[Op::RemoveNode { id: "n".into() }], None, &mut make);
    let a = apply(&mut x, &[apply::add_node(&gone)], None, &mut make);
    assert_eq!(a.ids, ["n"]);
    assert!(a.failed.is_empty());
    // Again: it exists; the op fails alone with an empty id, the next applies.
    let a = apply(&mut x, &[apply::add_node(&gone), apply::moved("j", 1.0, 2.0)], None, &mut make);
    assert_eq!(a.ids, [""]);
    assert_eq!(a.failed.len(), 1);
    assert_eq!(a.failed[0].0, 0);
    assert_eq!(x.node("j").unwrap().x, 1.0);
    // Ops on missing ids fail alone.
    let a = apply(&mut x, &[apply::moved("nope", 0.0, 0.0), Op::RemoveEdge { id: "nope".into() }], None, &mut make);
    assert_eq!(a.failed.len(), 2);
    assert!(a.inverse.is_empty());
}

#[test]
fn edges_add_remove_and_replace_and_come_back() {
    let c = fixture();
    let new_edge =
        |id: &str, from: &str, role| Op::AddEdge { edge: NewEdge { id: Some(id.into()), from: from.into(), to: "c".into(), role, label: None } };
    round_trip(&c, vec![new_edge("e9", "j", EdgeRole::LastFrame)]);
    round_trip(&c, vec![Op::RemoveEdge { id: "e3".into() }]);
    // Into a taken frame port: the old wire is replaced.
    let x = round_trip(&c, vec![new_edge("e9", "j", EdgeRole::FirstFrame)]);
    assert!(!x.edges.iter().any(|e| e.id == "e1"));
    // Refused wires fail with the reason.
    let mut x = c.clone();
    let a = apply(&mut x, &[new_edge("e8", "n", EdgeRole::Reference)], None, &mut ids());
    assert_eq!(a.ids, [""]);
    assert!(a.failed[0].1.contains("takes an image"), "{:?}", a.failed);
}

#[test]
fn a_batch_comes_back_in_reverse_order() {
    let c = fixture();
    let mut card = new_node(Some("k"), super::fixtures::gen_body(GenKind::Image, ""));
    (card.x, card.y) = (Some(2000.0), Some(0.0));
    round_trip(
        &c,
        vec![
            Op::AddNode { node: card },
            Op::AddEdge { edge: NewEdge { id: Some("ek".into()), from: "j".into(), to: "k".into(), role: EdgeRole::Reference, label: None } },
            apply::moved("k", 2100.0, 10.0),
            apply::patch("k", json!({ "prompt": "x" })),
        ],
    );
}

#[test]
fn layout_and_unknown_ops_are_left_to_the_server() {
    let c = fixture();
    let mut x = c.clone();
    let a =
        apply(&mut x, &[Op::Layout { ids: vec!["n".into()], arrange: Default::default(), frame: None, gap: None }, Op::Unknown], None, &mut ids());
    assert!(a.failed.is_empty() && a.inverse.is_empty());
    assert_eq!(x, c);
}

#[test]
fn tidy_is_left_to_the_server_and_its_inverse_moves_everything_back() {
    let c = fixture();
    let mut x = c.clone();
    let a = apply(&mut x, &[Op::Tidy { frame: None }], None, &mut ids());
    assert!(a.failed.is_empty());
    assert_eq!(x, c, "nothing moves here");
    assert_eq!(a.inverse.len(), c.nodes.len());
    assert_eq!(
        a.inverse[0],
        Op::UpdateNode { id: "f".into(), x: Some(0.0), y: Some(0.0), w: Some(1200.0), h: Some(800.0), z: None, parent: None, body: None }
    );
    assert_eq!(a.inverse[3], apply::moved("n", -400.0, 0.0), "a node's size never changes");
    // A frame that isn't one fails here as on the server.
    assert_eq!(apply(&mut x, &[Op::Tidy { frame: Some("n".into()) }], None, &mut ids()).failed.len(), 1);
    let mut ops = vec![Op::Tidy { frame: Some("f".into()) }];
    apply::rename(&mut ops, "f", "g");
    assert_eq!(ops, [Op::Tidy { frame: Some("g".into()) }]);
}

#[test]
fn tidy_up_sends_tidy_and_undo_puts_things_back() {
    let c = fixture();
    let mut b = board(&c);
    right_click(&mut b, pt(3000.0, 3000.0));
    let (batch, ops) = sent(&b.choose(&Action::Tidy));
    assert_eq!(ops, [Op::Tidy { frame: None }]);
    assert!(b.can_undo());
    // The server's canvas: the note moved, the frame shrunk.
    b.acked(batch, &edited(2, &[]));
    let mut s = c.clone();
    s.rev = 2;
    s.nodes[3].x = 5000.0;
    s.nodes[0].w = 900.0;
    b.set_canvas(&s);
    assert_eq!(x_of(&b, "n"), 5000.0);
    let (_, ops) = sent(&ch(&mut b, 'z', CMD));
    assert!(ops.contains(&apply::moved("n", -400.0, 0.0)), "{ops:?}");
    assert_eq!(x_of(&b, "n"), -400.0);
    assert_eq!(b.canvas().node("f").unwrap().w, 1200.0);
}

#[test]
fn history_keeps_a_redo_branch_until_a_new_edit() {
    let mut h = History::default();
    let step = |b| Step { ops: vec![apply::moved("n", 0.0, 0.0)], batch: b };
    h.push(step(1));
    h.push(step(2));
    let s = h.undo.pop().unwrap();
    h.push_redo(s);
    assert_eq!(h.redo.len(), 1);
    h.push(step(3));
    assert!(h.redo.is_empty());
    // Empty steps aren't kept; failed batches are forgotten.
    h.push(Step { ops: vec![], batch: 4 });
    assert_eq!(h.undo.len(), 2);
    h.forget(3);
    assert_eq!(h.undo.iter().map(|s| s.batch).collect::<Vec<_>>(), [1]);
}

// ---------------------------------------------------------------- through the board

fn x_of(b: &Board, id: &str) -> f64 {
    b.canvas().node(id).unwrap().x
}

#[test]
fn undo_and_redo_send_the_inverse_and_a_new_edit_ends_redo() {
    let mut b = board(&fixture());
    b.select(&["n".to_owned()]);
    sent(&key(&mut b, Key::Right, NONE));
    assert_eq!(x_of(&b, "n"), -399.0);
    assert!(b.can_undo());

    let (_, ops) = sent(&ch(&mut b, 'z', CMD));
    assert_eq!(ops, [apply::moved("n", -400.0, 0.0)]);
    assert_eq!(x_of(&b, "n"), -400.0);
    assert!(b.can_redo() && !b.can_undo());

    sent(&ch(&mut b, 'Z', CMD_SHIFT));
    assert_eq!(x_of(&b, "n"), -399.0);
    sent(&ch(&mut b, 'z', CMD));
    sent(&ch(&mut b, 'y', CMD));
    assert_eq!(x_of(&b, "n"), -399.0);
    sent(&ch(&mut b, 'z', CMD));
    assert!(b.can_redo());
    // A new edit after undo: no redo.
    sent(&key(&mut b, Key::Down, SHIFT));
    assert!(!b.can_redo());
    assert!(ch(&mut b, 'y', CMD).is_empty());
}

#[test]
fn undoing_a_delete_brings_back_the_node_and_its_wires() {
    let c = fixture();
    let mut b = board(&c);
    b.select(&["c".to_owned()]);
    let (_, ops) = sent(&key(&mut b, Key::Delete, NONE));
    assert_eq!(ops, [Op::RemoveNode { id: "c".into() }]);
    assert!(b.canvas().edges.is_empty());
    sent(&ch(&mut b, 'z', CMD));
    assert_eq!(sorted(b.canvas()), sorted(&c));
}

#[test]
fn an_older_rev_is_ignored() {
    let mut c = fixture();
    c.rev = 5;
    let mut b = board(&c);
    let mut old = c.clone();
    old.rev = 4;
    old.nodes.clear();
    assert!(b.set_canvas(&old).is_empty());
    assert_eq!(b.canvas().nodes.len(), c.nodes.len());
}

fn edited(rev: u64, ids: &[&str]) -> CanvasEdited {
    CanvasEdited { rev, ids: ids.iter().map(|s| (*s).to_owned()).collect(), moved: false, failed: Vec::new(), mentions: Vec::new() }
}

#[test]
fn acked_batches_are_dropped_once_the_server_has_them() {
    let c = fixture();
    let mut b = board(&c);
    b.select(&["n".to_owned()]);
    let (batch, _) = sent(&key(&mut b, Key::Right, NONE));
    assert!(b.acked(batch, &edited(2, &[])).is_empty());
    // The server's rev 2 has it; then an agent moves the note at rev 3: the batch isn't applied again.
    let mut s = c.clone();
    s.rev = 2;
    s.nodes[3].x = -399.0;
    b.set_canvas(&s);
    assert_eq!(x_of(&b, "n"), -399.0);
    s.rev = 3;
    s.nodes[3].x = 700.0;
    b.set_canvas(&s);
    assert_eq!(x_of(&b, "n"), 700.0);
}

#[test]
fn unconfirmed_edits_are_applied_again_on_a_newer_server_canvas() {
    let c = fixture();
    let mut b = board(&c);
    b.select(&["n".to_owned()]);
    let (batch, _) = sent(&key(&mut b, Key::Right, NONE));
    // The agent added a node (rev 2) before the server saw ours.
    let mut s = c.clone();
    s.rev = 2;
    s.nodes.push(note("agent", 3000.0, 0.0, "from the agent"));
    b.set_canvas(&s);
    assert!(b.canvas().node("agent").is_some());
    assert_eq!(x_of(&b, "n"), -399.0);
    // Acked at rev 3, and a read at rev 2 again (racing) still shows it.
    b.acked(batch, &edited(3, &[]));
    b.set_canvas(&s);
    assert_eq!(x_of(&b, "n"), -399.0);
}

#[test]
fn ids_the_server_made_instead_are_renamed_everywhere() {
    let c = fixture();
    let mut b = board(&c);
    let fx = b.choose(&Action::NewNote);
    let (batch, ops) = sent(&fx);
    let mine = added_id(&ops[0]);
    assert_eq!(b.selection(), std::slice::from_ref(&mine));
    b.acked(batch, &edited(2, &["n_server"]));
    assert!(b.canvas().node(&mine).is_none());
    assert!(b.canvas().node("n_server").is_some());
    assert_eq!(b.selection(), ["n_server".to_owned()]);
    assert!(b.laid("n_server").is_some());
    // The undo step names the server's id.
    let (_, ops) = sent(&ch(&mut b, 'z', CMD));
    assert_eq!(ops, [Op::RemoveNode { id: "n_server".into() }]);
}

#[test]
fn a_rename_after_the_servers_canvas_arrived_leaves_no_duplicate() {
    let c = fixture();
    let mut b = board(&c);
    let (batch, ops) = sent(&b.choose(&Action::NewNote));
    let Op::AddNode { node } = &ops[0] else { panic!() };
    // `studio.changed` and the re-read beat the `canvas_edit` answer.
    let mut s = c.clone();
    s.rev = 2;
    s.nodes.push(super::fixtures::node("n_server", node.x.unwrap(), node.y.unwrap(), node.w.unwrap(), node.h.unwrap(), node.body.clone()));
    b.set_canvas(&s);
    b.acked(batch, &edited(2, &["n_server"]));
    let count = b.canvas().nodes.iter().filter(|n| n.id == "n_server").count();
    assert_eq!(count, 1, "{:?}", b.canvas().nodes.iter().map(|n| &n.id).collect::<Vec<_>>());
    assert_eq!(b.canvas().nodes.len(), c.nodes.len() + 1);
}

#[test]
fn failed_ops_are_reported_and_a_failed_batch_is_rolled_back() {
    let c = fixture();
    let mut b = board(&c);
    b.select(&["n".to_owned()]);
    let (batch, _) = sent(&key(&mut b, Key::Right, NONE));
    let fx = b.failed(batch, "the canvas is gone");
    assert_eq!(notices(&fx), ["the canvas is gone"]);
    assert_eq!(x_of(&b, "n"), -400.0);
    assert!(!b.can_undo());

    // Some ops of an acked batch failed on the server: a notice.
    let (batch, _) = sent(&key(&mut b, Key::Right, NONE));
    let mut r = edited(2, &[]);
    r.failed.push(OpFailed { op: 0, code: "not_found".into(), message: "no node n".into() });
    assert_eq!(notices(&b.acked(batch, &r)), ["no node n"]);
}

#[test]
fn out_of_date_is_never_sent() {
    let mut c = fixture();
    c.out_of_date = vec!["c".into()];
    c.out_of_date_why.insert("c".into(), vec!["beat one is a new input".into()]);
    let mut b = board(&c);
    let mut fx = Vec::new();
    b.select(&["c".to_owned(), "f".to_owned()]);
    fx.extend(ch(&mut b, 'd', CMD));
    fx.extend(key(&mut b, Key::Delete, NONE));
    fx.extend(ch(&mut b, 'z', CMD));
    fx.extend(ch(&mut b, 'z', CMD));
    fx.extend(ch(&mut b, 'y', CMD));
    b.select(&["c".to_owned()]);
    fx.extend(drag(&mut b, Point::new(600.0, 120.0), Point::new(900.0, 300.0), NONE));
    let all = sends(&fx);
    assert!(all.len() >= 5, "{all:?}");
    for (_, ops) in all {
        let text = serde_json::to_string(&ops).unwrap();
        assert!(!text.contains("out_of_date"), "{text}");
    }
    // The shown canvas keeps the server's list.
    assert_eq!(b.canvas().out_of_date, ["c"]);
}

fn reference_order(c: &Canvas) -> Vec<String> {
    c.inputs_of("c").into_iter().filter(|e| e.role == EdgeRole::Reference).map(|e| e.id.clone()).collect()
}

#[test]
fn undoing_keeps_the_order_of_wires_in_a_port() {
    // Three references: image 1, 2, 3 in the order they were made.
    let mut c = fixture();
    c.nodes.push(image("k", -400.0, 800.0));
    c.nodes.push(image("l", -400.0, 1200.0));
    c.edges.push(edge("e4", "k", "c", EdgeRole::Reference));
    c.edges.push(edge("e5", "l", "c", EdgeRole::Reference));
    assert_eq!(reference_order(&c), ["e3", "e4", "e5"]);
    for ops in [vec![Op::RemoveEdge { id: "e3".into() }], vec![Op::RemoveNode { id: "j".into() }], vec![Op::RemoveNode { id: "k".into() }]] {
        let mut x = c.clone();
        let mut make = ids();
        let a = apply(&mut x, &ops, None, &mut make);
        apply(&mut x, &a.inverse, None, &mut make);
        assert_eq!(reference_order(&x), ["e3", "e4", "e5"], "undoing {ops:?}");
        assert_eq!(sorted(&x), sorted(&c));
        // And redo, then undo again.
        let mut y = c.clone();
        let a = apply(&mut y, &ops, None, &mut make);
        let back = apply(&mut y, &a.inverse, None, &mut make);
        let again = apply(&mut y, &back.inverse, None, &mut make);
        assert!(again.failed.is_empty());
        apply(&mut y, &again.inverse, None, &mut make);
        assert_eq!(reference_order(&y), ["e3", "e4", "e5"]);
    }
}

// ---------------------------------------------------------------- mentions (docs/STUDIO.md §3, "Mentions")

fn prompt_of(c: &Canvas, id: &str) -> String {
    match &c.node(id).unwrap().body {
        NodeBody::Gen { prompt, .. } => prompt.clone(),
        other => panic!("not a card: {other:?}"),
    }
}

fn with_prompt(mut c: Canvas, id: &str, text: &str) -> Canvas {
    if let Some(NodeBody::Gen { prompt, .. }) = c.nodes.iter_mut().find(|n| n.id == id).map(|n| &mut n.body) {
        text.clone_into(prompt);
    }
    c
}

fn mention_edge(id: &str, from: &str, to: &str) -> Edge {
    Edge { mention: true, ..edge(id, from, to, EdgeRole::Reference) }
}

fn synced(did: MentionDid, token: &str, edge: Option<Edge>, node: Option<&str>, why: Option<&str>) -> MentionSynced {
    MentionSynced { card: "c".into(), token: token.into(), did, edge, node: node.map(str::to_owned), why: why.map(str::to_owned) }
}

/// The patch undoing a prompt set on an empty one (an empty prompt isn't stored: `null` removes it).
fn unprompted() -> Op {
    apply::patch("c", json!({ "prompt": null }))
}

/// Nodes and edges, sorted by id (the rev aside).
fn shape(c: &Canvas) -> (Vec<silo_proto::studio::Node>, Vec<Edge>) {
    let c = sorted(c);
    (c.nodes, c.edges)
}

#[test]
fn a_prompt_edit_takes_away_the_wires_its_removed_mentions_made() {
    let mut c = canvas(
        vec![card("c", GenKind::Image, 0.0, 0.0), image("j", -400.0, 0.0), image("k", -400.0, 300.0), image("i", -400.0, 600.0)],
        vec![mention_edge("e1", "j", "c"), mention_edge("e2", "k", "c"), edge("e3", "i", "c", EdgeRole::Reference)],
    );
    c = with_prompt(c, "c", "@#j and @#k");
    let mut x = c.clone();
    let a = apply(&mut x, &[create::set_prompt("c", "only @#k")], None, &mut ids());
    // `j`'s mention wire goes (as the server takes it away); `k`'s stays, and so does the one drawn by hand.
    assert_eq!(x.edges.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e2", "e3"]);
    // The inverse puts the prompt back only: the server wires `j` again from it.
    assert_eq!(a.inverse, [create::set_prompt("c", "@#j and @#k")]);
    // A body patch without the prompt leaves the wires alone.
    let mut y = c.clone();
    apply(&mut y, &[apply::patch("c", json!({ "title": "Hero" }))], None, &mut ids());
    assert_eq!(y.edges.len(), 3);
}

#[test]
fn undoing_a_mention_that_added_an_entity_node_restores_the_canvas_and_redo_brings_it_back() {
    let before = canvas(vec![card("c", GenKind::Image, 0.0, 0.0), image("x", -400.0, 0.0)], Vec::new());
    let mut b = board(&before);
    let text = "@characters/mara on the quay";
    let (batch, ops) = sent(&b.set_prompt("c", text));
    assert_eq!(ops, [create::set_prompt("c", text)]);
    // The server added Mara's Entity node and wired it, in the same edit.
    let wire = mention_edge("e_m", "n_m", "c");
    let mut after = with_prompt(before.clone(), "c", text);
    after.rev = 2;
    after.nodes.push(entity("n_m", -400.0, 300.0, "characters/mara"));
    after.edges.push(wire.clone());
    let fx = b.acked(
        batch,
        &CanvasEdited { mentions: vec![synced(MentionDid::Wired, "@characters/mara", Some(wire), Some("n_m"), None)], ..edited(2, &[]) },
    );
    assert!(notices(&fx).is_empty(), "a wired mention needs nothing but its wire");
    b.set_canvas(&after);

    // Undo: the prompt back (the server takes its wire away) and the node it added gone.
    let (undo_batch, undo) = sent(&ch(&mut b, 'z', CMD));
    assert_eq!(undo, [unprompted(), Op::RemoveNode { id: "n_m".into() }]);
    assert_eq!(shape(b.canvas()), shape(&before));
    b.acked(undo_batch, &edited(3, &[]));
    let mut server = before.clone();
    server.rev = 3;
    b.set_canvas(&server);

    // Redo: the node back, then the prompt, which the server wires again (a mention's wire, not one by hand).
    let (redo_batch, redo) = sent(&ch(&mut b, 'Z', CMD_SHIFT));
    assert_eq!(redo, [apply::add_node(after.node("n_m").unwrap()), create::set_prompt("c", text)]);
    assert!(!redo.iter().any(|op| matches!(op, Op::AddEdge { .. })));
    let wire = mention_edge("e_m2", "n_m", "c");
    let mut again = after.clone();
    again.rev = 4;
    again.edges = vec![wire.clone()];
    b.acked(redo_batch, &CanvasEdited { mentions: vec![synced(MentionDid::Wired, "@characters/mara", Some(wire), None, None)], ..edited(4, &[]) });
    b.set_canvas(&again);
    assert_eq!(prompt_of(b.canvas(), "c"), text);

    // And undo again: exactly as the first time.
    let (_, undo) = sent(&ch(&mut b, 'z', CMD));
    assert_eq!(undo, [unprompted(), Op::RemoveNode { id: "n_m".into() }]);
    assert_eq!(shape(b.canvas()), shape(&before));
}

#[test]
fn a_mention_that_could_not_be_wired_says_why_and_its_node_goes_on_undo() {
    let before = canvas(vec![card("c", GenKind::Image, 0.0, 0.0), video("clip", -400.0, 0.0)], Vec::new());
    let mut b = board(&before);
    let (batch, _) = sent(&b.set_prompt("c", "@#clip @characters/teo"));
    let why = "clip.mp4 outputs video: wire it by hand into the port it is for";
    let mut r = edited(2, &[]);
    r.failed.push(OpFailed { op: 0, code: "bad_request".into(), message: "something else".into() });
    r.mentions = vec![
        synced(MentionDid::NotWired, "@#clip", None, None, Some(why)),
        // Teo's node was added but the wire refused: the node stays on the canvas until undone.
        synced(MentionDid::NotWired, "@characters/teo", None, Some("n_t"), Some("a loop")),
    ];
    assert_eq!(notices(&b.acked(batch, &r)), [format!("something else; {why}; a loop")]);
    let (_, undo) = sent(&ch(&mut b, 'z', CMD));
    assert_eq!(undo, [unprompted(), Op::RemoveNode { id: "n_t".into() }]);
}

#[test]
fn history_extends_the_step_of_a_batch_on_either_stack() {
    let mut h = History::default();
    h.push(Step { ops: vec![apply::moved("a", 0.0, 0.0)], batch: 1 });
    h.push_redo(Step { ops: vec![apply::moved("b", 0.0, 0.0)], batch: 2 });
    h.extend(1, vec![Op::RemoveNode { id: "n".into() }]);
    h.extend(2, vec![Op::RemoveNode { id: "m".into() }]);
    h.extend(9, vec![Op::RemoveNode { id: "z".into() }]);
    assert_eq!(h.undo[0].ops, [apply::moved("a", 0.0, 0.0), Op::RemoveNode { id: "n".into() }]);
    assert_eq!(h.redo[0].ops, [apply::moved("b", 0.0, 0.0), Op::RemoveNode { id: "m".into() }]);
}
