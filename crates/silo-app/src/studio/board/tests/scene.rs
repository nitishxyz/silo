//! Layout (each node kind's rect and parts, a card's face and height, ports, output dots, draw order) and
//! hit testing.

use std::collections::{HashMap, HashSet};

use serde_json::json;
use silo_proto::studio::{EdgeRole, GenKind, GenStatus, StudioMedia, size::AUDIO_FACE};

use super::super::layout::{
    self, CARD_FOOTER, CARD_HEADER, CardAction, ENTITY_NAME, FRAME_TITLE, PORT_GAP, PORT_SIZE, PORT_STEP, PORT_TOP, PROMPT_MAX, PROMPT_MIN,
    PROMPT_OPEN_MAX,
};
use super::super::{Corner, Hit, Part, Point, Rect, Scene, Shape};
use super::fixtures::*;

fn scene(c: &silo_proto::studio::Canvas) -> Scene {
    Scene::build(c, Some(&project()), &HashMap::new(), &HashSet::new(), &HashSet::new())
}

fn hit(s: &Scene, c: &silo_proto::studio::Canvas, p: Point, selected: &[&str]) -> Hit {
    let sel: Vec<String> = selected.iter().map(|s| (*s).to_owned()).collect();
    s.hit(c, p, 1.0, &sel)
}

fn node_hit(id: &str, part: Part) -> Hit {
    Hit::Node { id: id.into(), part }
}

// ---------------------------------------------------------------- layout

#[test]
fn a_draft_card_is_header_face_at_the_project_format_prompt_and_footer() {
    let c = canvas(vec![card("c", GenKind::Image, 100.0, 100.0)], vec![]);
    let l = *scene(&c).get("c").unwrap();
    assert_eq!(l.shape, Shape::Card(GenKind::Image));
    let p = l.card.unwrap();
    assert_eq!(p.header, Rect::new(100.0, 100.0, 320.0, CARD_HEADER));
    // 16:9 (the project's landscape) at 320 wide.
    assert_eq!(p.face, Rect::new(100.0, 132.0, 320.0, 180.0));
    close(p.aspect, 16.0 / 9.0);
    assert_eq!(p.prompt, Rect::new(100.0, 312.0, 320.0, PROMPT_MIN));
    assert_eq!(p.footer, Rect::new(100.0, 356.0, 320.0, CARD_FOOTER));
    assert!(p.footer.contains_rect(p.run) && p.run.center().x > p.footer.center().x, "Run sits right in the footer");
    assert!(p.stepper.is_none(), "no stepper without two takes");
    // The node's stored height is ignored: it follows the content.
    assert_eq!(l.rect, Rect::new(100.0, 100.0, 320.0, 32.0 + 180.0 + PROMPT_MIN + 36.0));
}

#[test]
fn a_cards_face_takes_the_picked_takes_shape_else_its_own_aspect_else_the_effective_format() {
    let tall = with_takes(card("t", GenKind::Image, 0.0, 0.0), vec![take("a.png", StudioMedia::Image, 1080, 1920)]);
    let own = with_options(card("o", GenKind::Image, 0.0, 0.0), json!({ "aspect_ratio": "1:1" }));
    let plain = card("p", GenKind::Video, 0.0, 0.0);
    let shot = in_frame(card("s", GenKind::Video, 0.0, 0.0), "shot");
    let mut c = canvas(vec![tall, own, plain, frame_with_format("shot", -50.0, -50.0, 2000.0, 2000.0, "square"), shot], vec![]);
    c.format = Some("vertical".into());
    let s = scene(&c);
    let face = |id: &str| s.get(id).unwrap().card.unwrap().face;
    close(face("t").h, 320.0 * 1920.0 / 1080.0);
    close(face("o").h, 320.0);
    // The canvas's vertical override beats the project's landscape…
    close(face("p").h, 320.0 * 16.0 / 9.0);
    // …and a shot frame's format beats the canvas's.
    close(face("s").h, 320.0);

    // No project: 16:9.
    let s = Scene::build(&c, None, &HashMap::new(), &HashSet::new(), &HashSet::new());
    close(s.get("p").unwrap().card.unwrap().face.h, 180.0);
}

#[test]
fn a_cards_height_follows_its_prompt() {
    let long = "word ".repeat(20); // 100 characters: three lines at 320 wide
    let mut n = card("c", GenKind::Image, 0.0, 0.0);
    n.body = gen_body(GenKind::Image, &long);
    let c = canvas(vec![n], vec![]);
    let s = scene(&c);
    let l = s.get("c").unwrap();
    assert_eq!(l.card.unwrap().prompt.h, layout::estimate_prompt(&long, 320.0, false));
    assert!(l.card.unwrap().prompt.h > PROMPT_MIN);

    let mut b = board(&c);
    let before = b.laid("c").unwrap().rect.h;
    assert!(!b.measure_prompt("c", 80.0).is_empty());
    let l = b.laid("c").unwrap();
    close(l.card.unwrap().prompt.h, 80.0);
    close(l.rect.h, CARD_HEADER + 180.0 + 80.0 + CARD_FOOTER);
    assert!(l.rect.h != before);
    // The same measure again changes nothing; collapsed, it shows 4 lines at most.
    assert!(b.measure_prompt("c", 80.0).is_empty());
    b.measure_prompt("c", 5000.0);
    close(b.laid("c").unwrap().card.unwrap().prompt.h, PROMPT_MAX);
    close(PROMPT_MAX, 4.0 * 18.0 + 20.0);
}

/// Two cards with long prompts (measured at 200), side by side.
fn long_prompts() -> super::super::Board {
    let mut a = card("a", GenKind::Image, 0.0, 0.0);
    a.body = gen_body(GenKind::Image, &"word ".repeat(200));
    let mut c2 = card("b", GenKind::Image, 600.0, 0.0);
    c2.body = gen_body(GenKind::Image, &"word ".repeat(200));
    let mut b = board(&canvas(vec![a, c2], vec![]));
    b.measure_prompt("a", 200.0);
    b.measure_prompt("b", 200.0);
    b
}

#[test]
fn a_prompt_is_collapsed_unless_its_card_is_edited_or_the_single_selected_one() {
    let mut b = long_prompts();
    let shown = |b: &super::super::Board, id: &str| b.laid(id).unwrap().card.unwrap();
    assert!(!shown(&b, "a").open);
    close(shown(&b, "a").prompt.h, PROMPT_MAX);
    // The single selected card opens (its card grows); a multi-selection stays collapsed.
    b.select(&["a".to_owned()]);
    assert!(shown(&b, "a").open && !shown(&b, "b").open);
    close(shown(&b, "a").prompt.h, 200.0);
    close(b.laid("a").unwrap().rect.h, CARD_HEADER + 180.0 + 200.0 + CARD_FOOTER);
    b.select(&["a".to_owned(), "b".to_owned()]);
    assert!(!shown(&b, "a").open && !shown(&b, "b").open);
    close(shown(&b, "a").prompt.h, PROMPT_MAX);
    // The edited one opens, whatever is selected; to 12 lines at most (the editor scrolls beyond).
    assert!(!b.set_editing(Some("b")).is_empty());
    assert!(shown(&b, "b").open && !shown(&b, "a").open);
    b.measure_prompt("b", 5000.0);
    close(shown(&b, "b").prompt.h, PROMPT_OPEN_MAX);
    close(PROMPT_OPEN_MAX, 12.0 * 18.0 + 20.0);
    assert!(b.set_editing(Some("b")).is_empty(), "no change");
    b.set_editing(None);
    close(shown(&b, "b").prompt.h, PROMPT_MAX);
    b.select(&[]);
    assert!(!shown(&b, "a").open);
}

#[test]
fn run_and_the_prompt_are_hit_where_they_are_drawn_collapsed_or_open() {
    let mut b = long_prompts();
    for open in [false, true] {
        if open {
            b.select(&["a".to_owned()]);
        }
        let p = b.laid("a").unwrap().card.unwrap();
        assert_eq!(p.open, open);
        assert_eq!(p.footer.y, p.prompt.bottom(), "nothing under the footer");
        let s = Scene::build(
            b.canvas(),
            Some(&project()),
            &HashMap::from([("a".to_owned(), 200.0)]),
            &HashSet::new(),
            &HashSet::from_iter(open.then(|| "a".to_owned())),
        );
        assert_eq!(s.get("a").unwrap().card.unwrap(), p, "the same layout from the same inputs");
        assert_eq!(hit(&s, b.canvas(), p.run.center(), &[]), node_hit("a", Part::Run));
        assert_eq!(hit(&s, b.canvas(), Point::new(p.prompt.x + 20.0, p.prompt.bottom() - 2.0), &[]), node_hit("a", Part::Prompt));
        assert_ne!(hit(&s, b.canvas(), Point::new(p.prompt.x + 20.0, p.prompt.bottom() + 2.0), &[]), node_hit("a", Part::Prompt));
    }
}

#[test]
fn a_card_refused_for_want_of_a_provider_offers_to_connect_one() {
    let mut failed = with_status(card("f", GenKind::Image, 0.0, 0.0), GenStatus::Failed);
    let mut refused = with_status(card("n", GenKind::Video, 600.0, 0.0), GenStatus::Failed);
    if let silo_proto::studio::NodeBody::Gen { error_code, error, .. } = &mut refused.body {
        *error_code = Some("no_provider".into());
        *error = Some("No video provider".into());
    }
    if let silo_proto::studio::NodeBody::Gen { error_code, .. } = &mut failed.body {
        *error_code = Some("model".into());
    }
    let running = with_status(card("r", GenKind::Image, 0.0, 600.0), GenStatus::Running);
    let c = canvas(vec![failed, refused, running, card("d", GenKind::Image, 600.0, 600.0)], vec![]);
    let s = scene(&c);
    let action = |id: &str| layout::card_action(&c.node(id).unwrap().body);
    assert_eq!([action("f"), action("n"), action("r"), action("d")], [CardAction::Retry, CardAction::Connect, CardAction::Stop, CardAction::Run]);
    let run = |id: &str| s.get(id).unwrap().card.unwrap().run;
    assert_eq!(run("n").w, layout::CONNECT_W, "wide enough for its label");
    assert_eq!(run("f").w, layout::RUN_W);
    assert_eq!(run("n").right(), run("f").right() + 600.0, "right in the footer");
    assert_eq!(hit(&s, &c, Point::new(run("n").x + 4.0, run("n").center().y), &[]), node_hit("n", Part::Run));
}

#[test]
fn a_narrow_card_from_an_older_canvas_is_drawn_as_stored() {
    let mut n = card("c", GenKind::Image, 0.0, 0.0);
    n.w = 180.0;
    let c = canvas(vec![n], vec![]);
    assert_eq!(scene(&c).get("c").unwrap().rect.w, 180.0);
}

#[test]
fn audio_cards_have_the_compact_player_for_a_face() {
    let c = canvas(vec![card("s", GenKind::Speech, 0.0, 0.0)], vec![]);
    let l = *scene(&c).get("s").unwrap();
    close(l.card.unwrap().face.h, AUDIO_FACE);
    assert!(l.shape.is_audio());
}

#[test]
fn one_port_module_per_port_in_order_with_wired_ones_marked() {
    let c = canvas(
        vec![image("i", -600.0, 0.0), note("n", -600.0, 400.0, "x"), card("v", GenKind::Video, 0.0, 0.0)],
        vec![edge("e1", "i", "v", EdgeRole::Reference), edge("e2", "n", "v", EdgeRole::Text)],
    );
    let s = scene(&c);
    let l = s.get("v").unwrap();
    assert_eq!(l.ports, GenKind::Video.ports());
    for (k, role) in l.ports.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let y = PORT_TOP + k as f64 * PORT_STEP;
        close_pt(l.port_center(k), Point::new(-PORT_GAP - PORT_SIZE / 2.0, y));
        assert_eq!(l.is_wired(k), matches!(role, EdgeRole::Reference | EdgeRole::Text), "{role:?}");
    }
    assert_eq!(l.port_index(EdgeRole::Reference), Some(3));
    close_pt(l.input_point(EdgeRole::Reference), l.port_center(3));
    let img = s.get("i").unwrap();
    assert!(img.ports.is_empty());
    assert_eq!(scene(&canvas(vec![card("x", GenKind::Music, 0.0, 0.0)], vec![])).get("x").unwrap().ports, &[EdgeRole::Text]);
}

#[test]
fn output_dots_only_on_nodes_with_an_output() {
    let c = canvas(
        vec![
            card("c", GenKind::Image, 0.0, 0.0),
            note("n", 0.0, 500.0, ""),
            image("i", 0.0, 800.0),
            entity("e", 0.0, 1100.0, "characters/mara"),
            frame("f", 1000.0, 0.0, 400.0, 300.0),
        ],
        vec![],
    );
    let s = scene(&c);
    for id in ["c", "n", "i", "e"] {
        assert!(s.get(id).unwrap().output, "{id}");
    }
    assert!(!s.get("f").unwrap().output);
    // A card's dot is level with its first port; others' halfway down; right of the rect.
    let card = s.get("c").unwrap();
    close_pt(card.output_center(), Point::new(320.0 + PORT_GAP + PORT_SIZE / 2.0, PORT_TOP));
    let n = s.get("n").unwrap();
    close_pt(n.output_center(), Point::new(240.0 + PORT_GAP + PORT_SIZE / 2.0, 580.0));
    assert!(card.outer().contains_rect(card.output_rect()) && card.outer().contains_rect(card.port_rect(1)));
}

#[test]
fn asset_entity_and_frame_parts() {
    let c = canvas(vec![image("i", 0.0, 0.0), entity("e", 400.0, 0.0, "characters/mara"), frame("f", 0.0, 400.0, 500.0, 300.0)], vec![]);
    let s = scene(&c);
    assert_eq!(s.get("i").unwrap().face, Some(Rect::new(0.0, 0.0, 320.0, 180.0)));
    assert_eq!(s.get("e").unwrap().face, Some(Rect::new(400.0, 0.0, 200.0, 228.0 - ENTITY_NAME)));
    let f = s.get("f").unwrap();
    assert_eq!(f.title, Some(Rect::new(0.0, 400.0, 500.0, FRAME_TITLE)));
    assert_eq!(f.body(), Rect::new(0.0, 400.0 + FRAME_TITLE, 500.0, 300.0 - FRAME_TITLE));
    assert!(f.ports.is_empty() && f.card.is_none());
}

#[test]
fn draw_order_puts_frames_below_outer_first_and_the_selection_on_top() {
    let mut a = note("a", 0.0, 0.0, "");
    a.z = Some(5);
    let c = canvas(
        vec![a, note("b", 10.0, 10.0, ""), in_frame(frame("inner", 0.0, 0.0, 300.0, 300.0), "outer"), frame("outer", -10.0, -10.0, 900.0, 900.0)],
        vec![],
    );
    let ids = |s: &Scene| s.order.iter().map(|&i| c.nodes[i].id.clone()).collect::<Vec<_>>();
    let s = scene(&c);
    // Higher `z` above.
    assert_eq!(ids(&s), ["outer", "inner", "b", "a"]);
    assert_eq!(s.get("inner").unwrap().depth, 1);
    // The selection is drawn last, above a higher `z`.
    let s = Scene::build(&c, Some(&project()), &HashMap::new(), &HashSet::from(["b".to_owned()]), &HashSet::new());
    assert_eq!(ids(&s), ["outer", "inner", "a", "b"]);
}

// ---------------------------------------------------------------- hit testing

#[test]
fn every_part_of_a_card() {
    let n = with_takes(
        card("c", GenKind::Image, 100.0, 100.0),
        vec![take("1.png", StudioMedia::Image, 1920, 1080), take("2.png", StudioMedia::Image, 1920, 1080)],
    );
    let c = canvas(vec![n], vec![]);
    let s = scene(&c);
    let p = s.get("c").unwrap().card.unwrap();
    let (back, fwd) = p.stepper.unwrap();
    assert_eq!(hit(&s, &c, back.center(), &[]), node_hit("c", Part::TakeBack));
    assert_eq!(hit(&s, &c, fwd.center(), &[]), node_hit("c", Part::TakeForward));
    assert_eq!(hit(&s, &c, Point::new(150.0, 110.0), &[]), node_hit("c", Part::Header));
    assert_eq!(hit(&s, &c, p.face.center(), &[]), node_hit("c", Part::Face));
    assert_eq!(hit(&s, &c, p.prompt.center(), &[]), node_hit("c", Part::Prompt));
    assert_eq!(hit(&s, &c, p.run.center(), &[]), node_hit("c", Part::Run));
    assert_eq!(hit(&s, &c, Point::new(110.0, p.footer.center().y), &[]), node_hit("c", Part::Body));
    // Ports and the output dot, a little off-centre too.
    let l = s.get("c").unwrap();
    assert_eq!(hit(&s, &c, l.port_center(0) + Point::new(3.0, 3.0), &[]), Hit::Port { id: "c".into(), port: EdgeRole::Reference });
    assert_eq!(hit(&s, &c, l.port_center(1), &[]), Hit::Port { id: "c".into(), port: EdgeRole::Text });
    assert_eq!(hit(&s, &c, l.output_center() - Point::new(4.0, 0.0), &[]), Hit::Output { id: "c".into() });
}

#[test]
fn hits_follow_the_priority_order() {
    // A note on top of the card's ports; a frame under everything; a wire across the frame.
    let c = canvas(
        vec![
            frame("f", -200.0, -200.0, 2000.0, 1200.0),
            in_frame(image("i", -100.0, 400.0), "f"),
            in_frame(card("c", GenKind::Image, 900.0, 100.0), "f"),
            note("top", 860.0, 110.0, ""),
            in_frame(note("n", 0.0, 0.0, ""), "f"),
        ],
        vec![edge("w", "i", "c", EdgeRole::Reference)],
    );
    let s = scene(&c);
    let card = *s.get("c").unwrap();

    // Resize corners of the selection first, even over a node on top.
    assert_eq!(hit(&s, &c, Point::new(901.0, 101.0), &["c"]), Hit::Resize { id: "c".into(), corner: Corner::TopLeft });
    assert_eq!(hit(&s, &c, Point::new(0.0, 160.0), &["n"]), Hit::Resize { id: "n".into(), corner: Corner::BottomLeft });
    // Then ports, even under a node drawn above.
    assert_eq!(hit(&s, &c, card.port_center(0), &[]), Hit::Port { id: "c".into(), port: EdgeRole::Reference });
    // Then nodes, top first.
    assert_eq!(hit(&s, &c, Point::new(950.0, 200.0), &[]), node_hit("top", Part::Body));
    assert_eq!(hit(&s, &c, Point::new(1150.0, 200.0), &[]), node_hit("c", Part::Face));
    // Then wires (over a frame's body).
    let mid = s.curve(&c, 0).unwrap().mid();
    assert_eq!(hit(&s, &c, mid, &[]), Hit::Wire { id: "w".into() });
    // Then frame bodies; a frame's title bar is the frame.
    assert_eq!(hit(&s, &c, Point::new(100.0, 900.0), &[]), node_hit("f", Part::FrameBody));
    assert_eq!(hit(&s, &c, Point::new(1500.0, -190.0), &[]), node_hit("f", Part::Title));
    // Then nothing.
    assert_eq!(hit(&s, &c, Point::new(5000.0, 5000.0), &[]), Hit::Empty);
}

#[test]
fn a_frames_title_bar_beats_a_wire_across_it() {
    // A wire crossing the title bar: the title is the frame (it moves it), the body below lets the wire win.
    let c = canvas(
        vec![frame("f", 0.0, 0.0, 1000.0, 600.0), image("i", -600.0, -300.0), card("c", GenKind::Image, 1300.0, 200.0)],
        vec![edge("w", "i", "c", EdgeRole::Reference)],
    );
    let s = scene(&c);
    let curve = s.curve(&c, 0).unwrap();
    let on_title = curve.points(400).find(|p| p.x > 10.0 && p.x < 990.0 && p.y > 4.0 && p.y < FRAME_TITLE - 4.0);
    let on_title = on_title.expect("the wire crosses the title bar");
    assert_eq!(hit(&s, &c, on_title, &[]), node_hit("f", Part::Title));
    let in_body = curve.points(400).find(|p| p.x > 10.0 && p.x < 990.0 && p.y > FRAME_TITLE + 20.0 && p.y < 590.0).unwrap();
    assert_eq!(hit(&s, &c, in_body, &[]), Hit::Wire { id: "w".into() });
}

#[test]
fn hits_reach_small_targets_at_any_zoom() {
    let c = canvas(vec![card("c", GenKind::Image, 0.0, 0.0)], vec![]);
    let s = scene(&c);
    let port = s.get("c").unwrap().port_center(0);
    // At 10 % the slop is 6 view points = 60 canvas units around the module.
    let far = port + Point::new(0.0, -40.0);
    assert_eq!(s.hit(&c, far, 1.0, &[]), Hit::Empty);
    assert_eq!(s.hit(&c, far, 0.1, &[]), Hit::Port { id: "c".into(), port: EdgeRole::Reference });
}

#[test]
fn marquee_selects_what_it_touches_but_frames_only_wholly_inside() {
    let c = canvas(
        vec![frame("scene", 0.0, 0.0, 1000.0, 600.0), in_frame(note("a", 50.0, 100.0, ""), "scene"), in_frame(note("b", 600.0, 100.0, ""), "scene")],
        vec![],
    );
    let s = scene(&c);
    let ids = |r: Rect| -> Vec<String> {
        let mut v: Vec<String> = s.marquee(r).into_iter().map(|i| c.nodes[i].id.clone()).collect();
        v.sort();
        v
    };
    // Inside the scene: its shots, not the scene.
    assert_eq!(ids(Rect::new(40.0, 50.0, 700.0, 100.0)), ["a", "b"]);
    // Touching a corner is enough for a node.
    assert_eq!(ids(Rect::new(280.0, 250.0, 10.0, 10.0)), ["a"]);
    // Around everything: the frame too.
    assert_eq!(ids(Rect::new(-10.0, -10.0, 1100.0, 700.0)), ["a", "b", "scene"]);
}

#[test]
fn frame_at_is_the_innermost_frame_holding_the_centre() {
    let c = canvas(vec![frame("outer", 0.0, 0.0, 1000.0, 1000.0), in_frame(frame("inner", 100.0, 100.0, 400.0, 400.0), "outer")], vec![]);
    let s = scene(&c);
    let at = |r: Rect, skip: Option<usize>| s.frame_at(r, |i| Some(i) == skip).map(|i| c.nodes[i].id.clone());
    assert_eq!(at(Rect::around(Point::new(200.0, 200.0), 50.0, 50.0), None).as_deref(), Some("inner"));
    assert_eq!(at(Rect::around(Point::new(700.0, 700.0), 50.0, 50.0), None).as_deref(), Some("outer"));
    assert_eq!(at(Rect::around(Point::new(200.0, 200.0), 50.0, 50.0), s.index("inner")).as_deref(), Some("outer"));
    // Larger than the inner frame: not in it.
    assert_eq!(at(Rect::around(Point::new(300.0, 300.0), 450.0, 450.0), None).as_deref(), Some("outer"));
    assert_eq!(at(Rect::around(Point::new(2000.0, 200.0), 50.0, 50.0), None), None);
}

#[test]
fn frame_nesting_and_descendants() {
    let c = canvas(
        vec![
            frame("outer", 0.0, 0.0, 1000.0, 1000.0),
            in_frame(frame("inner", 100.0, 100.0, 400.0, 400.0), "outer"),
            in_frame(note("n", 150.0, 150.0, ""), "inner"),
            in_frame(note("x", 0.0, 0.0, ""), "n"),
        ],
        vec![],
    );
    let s = scene(&c);
    let outer = s.index("outer").unwrap();
    let mut d: Vec<&str> = s.descendants(outer).into_iter().map(|i| c.nodes[i].id.as_str()).collect();
    d.sort_unstable();
    assert_eq!(d, ["inner", "n"]);
    assert!(s.within(s.index("n").unwrap(), outer));
    // A parent that isn't a frame is no parent.
    assert_eq!(s.parent[s.index("x").unwrap()], None);
}
