//! Wires: curves, what a drop does (a wire, a refusal and why, the drag-to-create menu), and wiring by
//! dragging on the board.

use silo_proto::studio::{Canvas, EdgeRole, GenKind, Op, StudioMedia};

use super::super::wire::{CreateOption, create_options, resolve};
use super::super::{Action, Effect, Hit, MenuTarget, Part, Point, WireDrop, WireEnd};
use super::fixtures::*;

/// An image `i`, a video `vid`, audio `aud`, a note `n`, a video card `v`, image cards `a` → `b` wired.
fn fixture() -> Canvas {
    canvas(
        vec![
            image("i", -600.0, 0.0),
            video("vid", -600.0, 300.0),
            asset("aud", StudioMedia::Audio, -600.0, 600.0, 320.0, 72.0),
            note("n", -600.0, 800.0, "a harbour at dawn"),
            card("v", GenKind::Video, 0.0, 0.0),
            card("a", GenKind::Image, 600.0, 0.0),
            card("b", GenKind::Image, 1200.0, 0.0),
            frame("f", 0.0, 1400.0, 600.0, 400.0),
        ],
        vec![edge("ab", "a", "b", EdgeRole::Reference)],
    )
}

fn out(from: &str) -> WireEnd {
    WireEnd::Out { from: from.into() }
}

fn port(id: &str, port: EdgeRole) -> Hit {
    Hit::Port { id: id.into(), port }
}

fn body(id: &str) -> Hit {
    Hit::Node { id: id.into(), part: Part::Face }
}

#[track_caller]
fn refused(d: &WireDrop, want: &str) {
    match d {
        WireDrop::Refused { reason, .. } => assert!(reason.contains(want), "{reason:?} should say {want:?}"),
        other => panic!("expected a refusal ({want}), got {other:?}"),
    }
}

fn wired(from: &str, to: &str, role: EdgeRole) -> WireDrop {
    WireDrop::Wire { from: from.into(), to: to.into(), port: role, replaces: Vec::new() }
}

#[test]
fn a_wire_runs_from_the_output_dot_to_its_port() {
    let mut c = fixture();
    c.edges.push(edge("w", "i", "v", EdgeRole::Reference));
    let mut b = board(&c);
    b.fit_all();
    let s = b.scene();
    let curve = s.curve(&c, 1).unwrap();
    close_pt(curve.a, s.get("i").unwrap().output_center());
    close_pt(curve.b, s.get("v").unwrap().port_center(3));
    close_pt(curve.point(0.0), curve.a);
    close_pt(curve.point(1.0), curve.b);
    assert!(curve.distance(curve.mid()) < 1e-6);
    // The board draws the same curve.
    let drawn: Vec<_> = b.wires().filter(|w| w.id == "w").collect();
    assert_eq!(drawn.len(), 1);
    assert_eq!(drawn[0].curve, curve);
    // `next` edges are not wires.
    c.edges.push(edge("nx", "a", "b", EdgeRole::Next));
    assert!(board(&c).scene().curve(&c, 2).is_none());
}

#[test]
fn dropping_onto_a_port() {
    let c = fixture();
    assert_eq!(resolve(&c, &out("i"), &port("v", EdgeRole::Reference)), wired("i", "v", EdgeRole::Reference));
    assert_eq!(resolve(&c, &out("n"), &port("v", EdgeRole::Text)), wired("n", "v", EdgeRole::Text));
    // A port the card lacks.
    refused(&resolve(&c, &out("i"), &port("a", EdgeRole::FirstFrame)), "An image card has no first frame port");
    // The wrong media.
    refused(&resolve(&c, &out("vid"), &port("v", EdgeRole::Reference)), "The reference port takes an image; vid outputs a video");
    refused(&resolve(&c, &out("aud"), &port("v", EdgeRole::Text)), "takes text");
    // A loop: b is fed by a.
    refused(&resolve(&c, &out("b"), &port("a", EdgeRole::Reference)), "would make a loop");
    // Already there.
    refused(&resolve(&c, &out("a"), &port("b", EdgeRole::Reference)), "already wired");
    // Back onto itself: nothing.
    assert_eq!(resolve(&c, &out("a"), &port("a", EdgeRole::Reference)), WireDrop::Nothing);
}

#[test]
fn a_frame_port_holds_one_wire_the_new_one_replaces_it() {
    let mut c = fixture();
    c.nodes.push(image("i2", -600.0, -400.0));
    c.edges.push(edge("ff", "i", "v", EdgeRole::FirstFrame));
    assert_eq!(
        resolve(&c, &out("i2"), &port("v", EdgeRole::FirstFrame)),
        WireDrop::Wire { from: "i2".into(), to: "v".into(), port: EdgeRole::FirstFrame, replaces: vec!["ff".into()] }
    );
    // Through the board: the old wire is removed explicitly, in the same batch.
    let mut b = board(&c);
    let from = b.laid("i2").unwrap().output_center();
    let to = b.laid("v").unwrap().port_center(0);
    let fx = drag(&mut b, from, to, NONE);
    let (_, ops) = sent(&fx);
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0], Op::RemoveEdge { id: "ff".into() });
    assert!(matches!(&ops[1], Op::AddEdge { edge } if edge.from == "i2" && edge.to == "v" && edge.role == EdgeRole::FirstFrame));
    let into_ff: Vec<_> = b.canvas().edges.iter().filter(|e| e.to == "v" && e.role == EdgeRole::FirstFrame).collect();
    assert_eq!(into_ff.len(), 1);
    assert_eq!(into_ff[0].from, "i2");
}

#[test]
fn dropping_onto_a_cards_body_takes_its_first_free_port_that_fits() {
    let mut c = fixture();
    // An image onto a video card: first frame…
    assert_eq!(resolve(&c, &out("i"), &body("v")), wired("i", "v", EdgeRole::FirstFrame));
    // …then reference once the first frame is taken.
    c.edges.push(edge("ff", "i", "v", EdgeRole::FirstFrame));
    c.nodes.push(image("i2", -600.0, -400.0));
    assert_eq!(resolve(&c, &out("i2"), &body("v")), wired("i2", "v", EdgeRole::Reference));
    // A video into `video`, audio into `audio`, a note into `text`.
    assert_eq!(resolve(&c, &out("vid"), &body("v")), wired("vid", "v", EdgeRole::Video));
    assert_eq!(resolve(&c, &out("aud"), &body("v")), wired("aud", "v", EdgeRole::Audio));
    assert_eq!(resolve(&c, &out("n"), &body("a")), wired("n", "a", EdgeRole::Text));
    // What the card can't take at all, and what isn't a card.
    refused(&resolve(&c, &out("vid"), &body("a")), "An image card can't take a video");
    refused(&resolve(&c, &out("i"), &body("n")), "Wires go into cards");
    assert_eq!(resolve(&c, &out("i"), &body("i")), WireDrop::Nothing);
}

#[test]
fn dropping_onto_empty_canvas_offers_drag_to_create() {
    let c = fixture();
    assert_eq!(resolve(&c, &out("i"), &Hit::Empty), WireDrop::Create);
    assert_eq!(resolve(&c, &out("i"), &Hit::Node { id: "f".into(), part: Part::FrameBody }), WireDrop::Create);
    let opt = |makes, port| CreateOption { makes: Some(makes), port };
    // From an image: Image (reference) or Video (first frame).
    assert_eq!(create_options(&c, &out("i")), [opt(GenKind::Image, EdgeRole::Reference), opt(GenKind::Video, EdgeRole::FirstFrame)]);
    // From a video: Video.
    assert_eq!(create_options(&c, &out("vid")), [opt(GenKind::Video, EdgeRole::Video)]);
    // From audio: a video card's audio.
    assert_eq!(create_options(&c, &out("aud")), [opt(GenKind::Video, EdgeRole::Audio)]);
    // From a note: any kind, through text.
    let from_note = create_options(&c, &out("n"));
    assert_eq!(from_note.len(), 5);
    assert!(from_note.iter().all(|o| o.port == EdgeRole::Text));
    // An image card's draft output is an image.
    assert_eq!(create_options(&c, &out("a")).len(), 2);
}

#[test]
fn dragging_from_a_port_looks_for_a_source() {
    let c = fixture();
    let into = WireEnd::In { to: "v".into(), port: EdgeRole::Reference };
    assert_eq!(resolve(&c, &into, &body("i")), wired("i", "v", EdgeRole::Reference));
    assert_eq!(resolve(&c, &into, &Hit::Output { id: "i".into() }), wired("i", "v", EdgeRole::Reference));
    refused(&resolve(&c, &into, &body("vid")), "takes an image");
    refused(&resolve(&c, &into, &Hit::Node { id: "f".into(), part: Part::Title }), "no output");
    assert_eq!(resolve(&c, &into, &Hit::Empty), WireDrop::Create);
    // Drag to create from a port: what makes its media (and a note into text).
    let kinds = |end: WireEnd| create_options(&c, &end).into_iter().map(|o| o.makes).collect::<Vec<_>>();
    assert_eq!(kinds(into), [Some(GenKind::Image)]);
    assert_eq!(kinds(WireEnd::In { to: "v".into(), port: EdgeRole::Text }), [None]);
    assert_eq!(kinds(WireEnd::In { to: "v".into(), port: EdgeRole::Audio }), [Some(GenKind::Speech), Some(GenKind::Sound), Some(GenKind::Music)]);
}

#[test]
fn a_refused_drop_says_why_and_sends_nothing() {
    let c = fixture();
    let mut b = board(&c);
    let from = b.laid("vid").unwrap().output_center();
    let to = b.laid("v").unwrap().port_center(3); // reference
    drag_hold(&mut b, from, to, NONE);
    let draft = b.wiring().expect("a wire is being dragged");
    assert!(matches!(draft.drop, WireDrop::Refused { node, .. } if node == "v"));
    let v = b.laid("v").unwrap();
    assert!(b.port_state(&v, 3).refused);
    assert!(b.port_state(&v, 4).fits, "the video port takes a video");
    let fx = release(&mut b, to, NONE);
    assert!(sends(&fx).is_empty());
    assert_eq!(notices(&fx).len(), 1);
    assert!(notices(&fx)[0].contains("takes an image"));
    assert!(b.wiring().is_none());
}

#[test]
fn a_card_made_from_the_drag_to_create_menu_is_wired_in_the_same_batch() {
    let c = fixture();
    let mut b = board(&c);
    let from = b.laid("i").unwrap().output_center();
    let drop_at = Point::new(-300.0, 2400.0);
    let fx = drag(&mut b, from, drop_at, NONE);
    assert!(fx.contains(&Effect::Menu));
    let menu = b.menu().unwrap();
    assert_eq!(menu.target, MenuTarget::Create(out("i")));
    assert_eq!(menu.title.as_deref(), Some("Make from i.png"));
    let actions: Vec<&Action> = menu.groups.iter().flat_map(|g| &g.entries).map(|e| &e.action).collect();
    assert_eq!(
        actions,
        [&Action::Make { makes: GenKind::Image, port: EdgeRole::Reference }, &Action::Make { makes: GenKind::Video, port: EdgeRole::FirstFrame }]
    );

    let fx = b.choose(&Action::Make { makes: GenKind::Video, port: EdgeRole::FirstFrame });
    let (_, ops) = sent(&fx);
    assert_eq!(ops.len(), 2);
    let id = added_id(&ops[0]);
    assert!(matches!(&ops[1], Op::AddEdge { edge } if edge.from == "i" && edge.to == id && edge.role == EdgeRole::FirstFrame));
    assert_eq!(b.selection(), std::slice::from_ref(&id));
    assert!(fx.contains(&Effect::FocusPrompt { node: id.clone() }));
    // Its first-frame port lands where the wire was dropped.
    close_pt(b.laid(&id).unwrap().port_center(0), drop_at);
    assert!(b.menu().is_none());
}

#[test]
fn a_note_made_into_a_text_port() {
    let c = fixture();
    let mut b = board(&c);
    let from = b.laid("v").unwrap().port_center(6); // text
    let drop_at = Point::new(-300.0, 2400.0);
    drag(&mut b, from, drop_at, NONE);
    let menu = b.menu().unwrap();
    assert_eq!(menu.title.as_deref(), Some("Into text"));
    let fx = b.choose(&Action::MakeNote);
    let (_, ops) = sent(&fx);
    let id = added_id(&ops[0]);
    assert!(matches!(&ops[1], Op::AddEdge { edge } if edge.from == id && edge.to == "v" && edge.role == EdgeRole::Text));
    assert!(fx.contains(&Effect::EditNote { node: id.clone() }));
    // Its output dot is at the drop point.
    close_pt(b.laid(&id).unwrap().output_center(), drop_at);
}
