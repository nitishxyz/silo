//! Making things: the canvas prompt and its Run, Make from this and the selection actions, imports,
//! entities, and what the empty-canvas menu makes.

use silo_proto::studio::{Canvas, EdgeRole, GenKind, NodeBody, Op, StudioMedia};

use super::super::layout::{CARD_GAP, CARD_W, GAP, PORT_HIT};
use super::super::{Action, Board, Effect, Imported, Rect, media_of_path};
use super::fixtures::*;

fn fixture() -> Canvas {
    canvas(
        vec![
            note("n", 0.0, 0.0, "a lighthouse at night"),
            image("i", 0.0, 400.0),
            video("vid", 0.0, 800.0),
            with_takes(card("done", GenKind::Image, 0.0, 1200.0), vec![take("t.png", StudioMedia::Image, 1920, 1080)]),
            frame_with_format("tall", 3000.0, 0.0, 1000.0, 1000.0, "vertical"),
        ],
        vec![],
    )
}

fn add_rect(op: &Op) -> Rect {
    let Op::AddNode { node } = op else { panic!("{op:?}") };
    Rect::new(node.x.unwrap(), node.y.unwrap(), node.w.unwrap(), node.h.unwrap())
}

fn runs(fx: &[Effect]) -> Vec<String> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Run { node } => Some(node.clone()),
            _ => None,
        })
        .collect()
}

fn ack(b: &mut Board, batch: u64, ids: &[&str]) -> Vec<Effect> {
    b.acked(
        batch,
        &silo_proto::studio::CanvasEdited {
            rev: 9,
            ids: ids.iter().map(|s| (*s).to_owned()).collect(),
            moved: false,
            failed: vec![],
            mentions: vec![],
        },
    )
}

// ---------------------------------------------------------------- the canvas prompt

#[test]
fn the_canvas_prompt_makes_a_card_at_the_point_and_runs_it_once_the_server_has_it() {
    let mut b = board(&fixture());
    double_click(&mut b, pt(2000.0, 300.0));
    let fx = b.submit_prompt(GenKind::Image, "a cat on a boat", true);
    let (batch, ops) = sent(&fx);
    assert_eq!(ops.len(), 1);
    let id = added_id(&ops[0]);
    let r = add_rect(&ops[0]);
    // Centred on the point, its top a little above it.
    assert_eq!((r.x + r.w / 2.0, r.y), (2000.0, 260.0));
    assert!(
        matches!(&ops[0], Op::AddNode { node } if matches!(&node.body, NodeBody::Gen { makes: GenKind::Image, prompt, .. } if prompt == "a cat on a boat"))
    );
    assert!(runs(&fx).is_empty(), "no Run before the server has the card");
    assert_eq!(b.selection(), std::slice::from_ref(&id));
    assert_eq!(runs(&ack(&mut b, batch, &[&id])), [id]);
}

#[test]
fn the_run_follows_an_id_the_server_changed() {
    let mut b = board(&fixture());
    ch(&mut b, 'g', NONE);
    let (batch, _) = sent(&b.submit_prompt(GenKind::Video, "waves", true));
    assert_eq!(runs(&ack(&mut b, batch, &["n_server"])), ["n_server"]);
    assert_eq!(b.selection(), ["n_server".to_owned()]);
}

#[test]
fn card_only_focuses_its_prompt_and_never_runs() {
    let mut b = board(&fixture());
    double_click(&mut b, pt(2000.0, 300.0));
    let fx = b.submit_prompt(GenKind::Music, "sea shanty", false);
    let (batch, ops) = sent(&fx);
    let id = added_id(&ops[0]);
    assert!(fx.contains(&Effect::FocusPrompt { node: id.clone() }));
    assert!(runs(&ack(&mut b, batch, &[&id])).is_empty());
    // Enter with an empty prompt makes the card only too.
    let fx = b.submit_prompt(GenKind::Image, "  ", true);
    let (batch, ops) = sent(&fx);
    assert!(runs(&ack(&mut b, batch, &[&added_id(&ops[0])])).is_empty());
}

#[test]
fn a_failed_batch_never_runs() {
    let mut b = board(&fixture());
    let (batch, ops) = sent(&b.submit_prompt(GenKind::Image, "x", true));
    b.failed(batch, "no");
    assert!(b.canvas().node(&added_id(&ops[0])).is_none());
    assert!(runs(&ack(&mut b, batch, &["whatever"])).is_empty());
}

#[test]
fn a_card_made_in_a_vertical_frame_is_in_it_at_its_format() {
    let mut b = board(&fixture());
    double_click(&mut b, pt(3500.0, 500.0));
    let (_, ops) = sent(&b.submit_prompt(GenKind::Image, "x", false));
    let Op::AddNode { node } = &ops[0] else { panic!() };
    assert_eq!(node.parent.as_deref(), Some("tall"));
    // Narrower for a tall face: 340 · √(9/16) = 255, but never below 264.
    assert_eq!(node.w, Some(264.0));
    let id = added_id(&ops[0]);
    close(b.laid(&id).unwrap().card.unwrap().face.h, 264.0 * 16.0 / 9.0);
}

// ---------------------------------------------------------------- made from a source

/// Chooses the selection action labelled `label` over `id`: the batch's ops, the new card and the effects.
fn act(b: &mut Board, id: &str, label: &str) -> (Vec<Op>, String, Vec<Effect>) {
    b.select(&[id.to_owned()]);
    let bar = b.selection_bar().expect("a selection bar");
    let entry = bar.entries.iter().find(|e| e.label == label).unwrap_or_else(|| panic!("no {label} in {:?}", bar.entries));
    assert!(entry.enabled, "{label}: {:?}", entry.reason);
    let action = entry.action.clone();
    let fx = b.choose(&action);
    let (_, ops) = sent(&fx);
    let new = added_id(&ops[0]);
    (ops, new, fx)
}

#[track_caller]
fn made(ops: &[Op], new: &str, from: &str, makes: GenKind, port: EdgeRole) {
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert!(matches!(&ops[0], Op::AddNode { node } if matches!(node.body, NodeBody::Gen { makes: m, .. } if m == makes)), "{ops:?}");
    assert!(matches!(&ops[1], Op::AddEdge { edge } if edge.from == from && edge.to == new && edge.role == port), "{ops:?}");
}

#[test]
fn visualize_animate_edit_and_extend() {
    let mut b = board(&fixture());
    let (ops, new, fx) = act(&mut b, "n", "Visualize");
    made(&ops, &new, "n", GenKind::Image, EdgeRole::Text);
    assert!(fx.contains(&Effect::FocusPrompt { node: new.clone() }));
    assert_eq!(b.selection(), std::slice::from_ref(&new));
    // To the right of the source, level with it, CARD_GAP away: the source's output and the card's ports clear.
    let r = add_rect(&ops[0]);
    assert_eq!((r.x, r.y, r.w), (240.0 + CARD_GAP, 0.0, CARD_W));
    let (src, card) = (b.laid("n").unwrap().outer(), b.laid(&new).unwrap().outer());
    assert!(card.x - src.right() >= GAP - 2.0 * PORT_HIT, "{src:?} {card:?}");

    let (ops, new, _) = act(&mut b, "i", "Animate");
    made(&ops, &new, "i", GenKind::Video, EdgeRole::FirstFrame);
    let (ops, new, _) = act(&mut b, "i", "Edit");
    made(&ops, &new, "i", GenKind::Image, EdgeRole::Reference);
    let (ops, new, _) = act(&mut b, "vid", "Extend");
    made(&ops, &new, "vid", GenKind::Video, EdgeRole::Video);
    // A card with a take is an image source too.
    let (ops, new, _) = act(&mut b, "done", "Animate");
    made(&ops, &new, "done", GenKind::Video, EdgeRole::FirstFrame);
}

#[test]
fn made_cards_go_into_free_space() {
    let mut b = board(&fixture());
    // Two Animates from the same image: the second goes below the first.
    let (ops1, first, _) = act(&mut b, "i", "Animate");
    let (ops2, _, _) = act(&mut b, "i", "Animate");
    let (r1, r2) = (add_rect(&ops1[0]), add_rect(&ops2[0]));
    assert_eq!(r1.x, r2.x);
    assert!(r2.y > r1.y);
    let outer1 = b.laid(&first).unwrap().outer();
    assert!(!outer1.intersects(r2), "{r1:?} {r2:?}");
}

#[test]
fn run_again_runs_the_card() {
    let mut b = board(&fixture());
    b.select(&["done".to_owned()]);
    let bar = b.selection_bar().unwrap();
    let run = bar.entries.iter().find(|e| e.label == "Run again").unwrap();
    assert_eq!(b.choose(&run.action.clone()), [Effect::Run { node: "done".into() }]);
}

#[test]
fn make_from_this_in_the_context_menu() {
    let mut b = board(&fixture());
    right_click(&mut b, pt(100.0, 450.0));
    let menu = b.menu().unwrap();
    let make = menu.groups.iter().find(|g| g.title.as_deref() == Some("Make from this")).unwrap();
    assert_eq!(make.entries.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["Image", "Video"]);
    let action = make.entries[1].action.clone();
    let fx = b.choose(&action);
    assert!(fx.contains(&Effect::CloseMenu));
    let (_, ops) = sent(&fx);
    let new = added_id(&ops[0]);
    made(&ops, &new, "i", GenKind::Video, EdgeRole::FirstFrame);
}

// ---------------------------------------------------------------- the empty-canvas menu

#[test]
fn the_empty_canvas_menu_makes_things_at_its_point() {
    let mut b = board(&fixture());
    let at = pt(2000.0, 300.0);
    right_click(&mut b, at);
    let fx = b.choose(&Action::Generate(GenKind::Speech));
    let (_, ops) = sent(&fx);
    let id = added_id(&ops[0]);
    let r = add_rect(&ops[0]);
    assert_eq!(r.x + r.w / 2.0, 2000.0);
    assert!(fx.contains(&Effect::FocusPrompt { node: id }));

    right_click(&mut b, pt(2000.0, 2000.0));
    let fx = b.choose(&Action::NewNote);
    let (_, ops) = sent(&fx);
    assert_eq!(add_rect(&ops[0]).origin(), pt(2000.0, 2000.0));
    assert!(fx.contains(&Effect::EditNote { node: added_id(&ops[0]) }));

    right_click(&mut b, pt(5000.0, 0.0));
    let (_, ops) = sent(&b.choose(&Action::NewFrame));
    assert!(matches!(&ops[0], Op::AddNode { node } if matches!(node.body, NodeBody::Frame { .. })));

    right_click(&mut b, pt(-900.0, 0.0));
    assert_eq!(b.choose(&Action::AddFromWorld), [Effect::CloseMenu, Effect::PickEntity { at: pt(-900.0, 0.0) }]);
    // Without a menu: the view's centre.
    let centre = b.visible_rect().center();
    assert_eq!(b.choose(&Action::Import), [Effect::Import { at: centre }]);
}

// ---------------------------------------------------------------- imports and entities

#[test]
fn imports_are_placed_in_a_row_at_the_point_by_media_kind() {
    let mut b = board(&fixture());
    let files = [
        Imported { path: "canvases/ep01/assets/a.png".into(), media: StudioMedia::Image, width: Some(1000), height: Some(1000), seconds: None },
        Imported { path: "canvases/ep01/assets/b.mp3".into(), media: StudioMedia::Audio, width: None, height: None, seconds: Some(3.0) },
        Imported { path: "canvases/ep01/assets/c.mp4".into(), media: StudioMedia::Video, width: None, height: None, seconds: Some(5.0) },
    ];
    let fx = b.place_imports(&files, Some(pt(2000.0, 500.0)));
    let (_, ops) = sent(&fx);
    let rects: Vec<Rect> = ops.iter().map(add_rect).collect();
    // GAP apart (no ports between assets).
    assert_eq!(rects[0], Rect::new(1840.0, 340.0, 320.0, 320.0));
    assert_eq!(rects[1], Rect::new(2200.0, 464.0, 320.0, 72.0));
    assert_eq!(rects[2], Rect::new(2560.0, 410.0, 320.0, 180.0));
    for (op, f) in ops.iter().zip(&files) {
        assert!(
            matches!(op, Op::AddNode { node } if matches!(&node.body, NodeBody::Asset { path, media, .. } if *path == f.path && *media == f.media))
        );
    }
    assert_eq!(b.selection().len(), 3);
    assert_eq!(media_of_path("x/y.JPG"), StudioMedia::Image);
    assert_eq!(media_of_path("y.mov"), StudioMedia::Video);
    assert_eq!(media_of_path("y.wav"), StudioMedia::Audio);
    assert_eq!(media_of_path("notes.txt"), StudioMedia::Text);
    assert_eq!(media_of_path("canvases/c/script.MD"), StudioMedia::Text);
    assert_eq!(media_of_path("canvas.json"), StudioMedia::Unknown);
}

#[test]
fn an_entity_from_the_world_is_placed_centred_at_the_point() {
    let mut b = board(&fixture());
    let (_, ops) = sent(&b.add_entity("characters/mara", Some(pt(2000.0, 500.0)), Some(1.0)));
    assert!(matches!(&ops[0], Op::AddNode { node } if node.body == NodeBody::Entity { id: "characters/mara".into() }));
    assert_eq!(add_rect(&ops[0]), Rect::new(1900.0, 386.0, 200.0, 228.0));
    // Into the frame it lands in.
    let (_, ops) = sent(&b.add_entity("characters/teo", Some(pt(3500.0, 500.0)), None));
    assert!(matches!(&ops[0], Op::AddNode { node } if node.parent.as_deref() == Some("tall")));
}
