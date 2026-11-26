//! Document cards: a text asset (`script.md`) laid out as a title bar over its preview, wired from like a note,
//! opened in the document panel, pinned from the Documents list.

use std::collections::{HashMap, HashSet};

use silo_proto::studio::{EdgeRole, GenKind, NodeBody, Op, StudioMedia};

use super::super::layout::{DOC_HEADER, DOC_SIZE, new_size};
use super::super::menu::{node_menu, selection_actions};
use super::super::wire::{WireEnd, create_options};
use super::super::{Action, Effect, Hit, Key, Part, Rect, Scene, Shape};
use super::fixtures::*;

const DOC: &str = "canvases/ep01/script.md";

fn doc(id: &str, x: f64, y: f64) -> silo_proto::studio::Node {
    let body = NodeBody::Asset { path: DOC.into(), media: StudioMedia::Text, width: None, height: None, seconds: None, prompt: None };
    node(id, x, y, DOC_SIZE.w, DOC_SIZE.h, body)
}

fn fixture() -> silo_proto::studio::Canvas {
    canvas(vec![doc("d", 0.0, 0.0), card("k", GenKind::Image, 800.0, 0.0)], vec![edge("w", "d", "k", EdgeRole::Text)])
}

#[test]
fn a_document_is_a_title_bar_over_its_preview_with_an_output() {
    let c = fixture();
    let s = Scene::build(&c, Some(&project()), &HashMap::new(), &HashSet::new(), &HashSet::new());
    let l = s.get("d").unwrap();
    assert_eq!(l.shape, Shape::Asset(StudioMedia::Text));
    assert!(l.shape.is_document() && !l.shape.keeps_aspect());
    assert_eq!(l.rect, Rect::new(0.0, 0.0, 320.0, 400.0));
    assert_eq!(l.face, Some(Rect::new(0.0, DOC_HEADER, 320.0, 400.0 - DOC_HEADER)));
    assert!(l.output && l.ports.is_empty());
    let sel: Vec<String> = Vec::new();
    assert_eq!(s.hit(&c, pt(100.0, 10.0), 1.0, &sel), Hit::Node { id: "d".into(), part: Part::Header });
    assert_eq!(s.hit(&c, pt(100.0, 200.0), 1.0, &sel), Hit::Node { id: "d".into(), part: Part::Face });
    assert_eq!(s.hit(&c, l.output_center(), 1.0, &sel), Hit::Output { id: "d".into() });
    // New ones are the server's document size.
    assert_eq!(new_size(&c.nodes[0].body, 1.0), DOC_SIZE);
}

#[test]
fn its_menu_opens_it_and_makes_cards_through_text() {
    let c = fixture();
    let groups = node_menu(&c, &sections(), "d");
    let labels: Vec<&str> = groups.iter().flat_map(|g| &g.entries).map(|e| e.label.as_str()).collect();
    assert_eq!(&labels[..2], ["Open", "Open in Files"]);
    assert!(labels.contains(&"Delete"));
    assert!(!labels.contains(&"Delete File…") && !labels.contains(&"Save…") && !labels.contains(&"View"), "{labels:?}");
    let make = groups.iter().find(|g| g.title.as_deref() == Some("Make from this")).unwrap();
    assert_eq!(make.entries.len(), 5);
    assert!(make.entries.iter().all(|e| matches!(e.action, Action::Make { port: EdgeRole::Text, .. })));
    // Its selection action: Visualize, as a note's.
    let acts = selection_actions(&c, &sections(), "d");
    assert_eq!(acts.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["Visualize"]);
    // Drag to create: any kind, into text.
    let opts = create_options(&c, &WireEnd::Out { from: "d".into() });
    assert!(opts.len() == 5 && opts.iter().all(|o| o.port == EdgeRole::Text));
}

#[test]
fn double_click_enter_and_open_ask_for_the_document_panel() {
    let c = fixture();
    let mut b = board(&c);
    let want = Effect::OpenDocument { path: DOC.into() };
    assert!(double_click(&mut b, pt(100.0, 200.0)).contains(&want));
    assert!(double_click(&mut b, pt(100.0, 10.0)).contains(&want));
    b.select(&["d".to_owned()]);
    assert_eq!(key(&mut b, Key::Enter, NONE), std::slice::from_ref(&want));
    assert!(b.choose(&Action::OpenDocument).contains(&want));
    assert_eq!(b.choose(&Action::RevealInFiles), [Effect::RevealInFiles { path: DOC.into() }]);
}

#[test]
fn add_document_pins_a_card_at_the_views_centre_off_what_is_there() {
    let mut b = board(&canvas(Vec::new(), Vec::new()));
    let fx = b.add_document("canvases/ep01/references.md", Some(pt(400.0, 300.0)));
    let (_, ops) = sent(&fx);
    let Op::AddNode { node } = &ops[0] else { panic!("{ops:?}") };
    assert!(matches!(&node.body, NodeBody::Asset { path, media: StudioMedia::Text, .. } if path == "canvases/ep01/references.md"));
    assert_eq!((node.x, node.y, node.w, node.h), (Some(240.0), Some(100.0), Some(320.0), Some(400.0)));
    assert_eq!(b.selection(), [added_id(&ops[0])]);
    // A second one goes below the first, not on it.
    let (_, ops) = sent(&b.add_document(DOC, Some(pt(400.0, 300.0))));
    let Op::AddNode { node } = &ops[0] else { panic!() };
    assert!(node.y.unwrap() >= 500.0, "{node:?}");
}
