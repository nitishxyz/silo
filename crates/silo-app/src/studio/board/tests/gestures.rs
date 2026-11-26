//! Gestures through `Board::handle`, each to its final ops and effects.

use silo_proto::studio::{Canvas, EdgeRole, GenKind, GenStatus, NodeBody, Op, StudioMedia};

use super::super::apply;
use super::super::{Board, Button, Camera, Cursor, Effect, Event, Key, Mods, Point, Rect, Tool};
use super::fixtures::*;

/// Notes `a`, `b`; image `i`; card `c`; frame `f` holding note `fn`.
fn fixture() -> Canvas {
    canvas(
        vec![
            note("a", 0.0, 0.0, "beat"),
            note("b", 400.0, 0.0, ""),
            image("i", 0.0, 400.0),
            card("c", GenKind::Image, 800.0, 0.0),
            frame("f", 0.0, 800.0, 800.0, 500.0),
            in_frame(note("fn", 40.0, 880.0, ""), "f"),
        ],
        vec![],
    )
}

fn sel(b: &Board) -> Vec<&str> {
    b.selection().iter().map(String::as_str).collect()
}

fn update(id: &str, x: Option<f64>, y: Option<f64>, w: Option<f64>, h: Option<f64>, parent: Option<Option<&str>>) -> Op {
    Op::UpdateNode { id: id.into(), x, y, w, h, z: None, parent: parent.map(|p| p.map(str::to_owned)), body: None }
}

// ---------------------------------------------------------------- selection

#[test]
fn click_shift_click_and_click_on_empty() {
    let mut b = board(&fixture());
    let fx = click(&mut b, pt(100.0, 80.0), NONE);
    assert!(fx.contains(&Effect::Selection));
    assert_eq!(sel(&b), ["a"]);
    click(&mut b, pt(500.0, 80.0), SHIFT);
    assert_eq!(sel(&b), ["a", "b"]);
    assert!(b.is_selected("b"));
    click(&mut b, pt(100.0, 80.0), SHIFT);
    assert_eq!(sel(&b), ["b"]);
    // A click on a selected node among several collapses to it.
    click(&mut b, pt(100.0, 80.0), SHIFT);
    click(&mut b, pt(500.0, 80.0), NONE);
    assert_eq!(sel(&b), ["b"]);
    // Shift-click on empty keeps it; a plain click clears.
    click(&mut b, pt(3000.0, 3000.0), SHIFT);
    assert_eq!(sel(&b), ["b"]);
    let fx = click(&mut b, pt(3000.0, 3000.0), NONE);
    assert!(fx.contains(&Effect::Selection));
    assert!(b.selection().is_empty());
    // A frame by its title, and by its body.
    click(&mut b, pt(400.0, 810.0), NONE);
    assert_eq!(sel(&b), ["f"]);
    click(&mut b, pt(600.0, 1200.0), NONE);
    assert_eq!(sel(&b), ["f"]);
}

#[test]
fn marquee_selects_what_it_touches_and_shift_adds() {
    let mut b = board(&fixture());
    drag_hold(&mut b, pt(-50.0, -50.0), pt(450.0, 50.0), NONE);
    assert_eq!(b.marquee(), Some(Rect::new(-50.0, -50.0, 500.0, 100.0)));
    assert_eq!(b.cursor(), Cursor::Crosshair);
    let mut s = sel(&b);
    s.sort_unstable();
    assert_eq!(s, ["a", "b"]);
    let fx = release(&mut b, pt(450.0, 50.0), NONE);
    assert!(sends(&fx).is_empty());
    assert!(b.marquee().is_none());

    drag(&mut b, pt(-50.0, 350.0), pt(10.0, 420.0), SHIFT);
    let mut s = sel(&b);
    s.sort_unstable();
    assert_eq!(s, ["a", "b", "i"]);
}

#[test]
fn a_marquee_inside_a_frame_selects_its_contents_not_the_frame() {
    let mut b = board(&fixture());
    drag(&mut b, pt(20.0, 860.0), pt(100.0, 950.0), NONE);
    assert_eq!(sel(&b), ["fn"]);
}

#[test]
fn cmd_a_selects_everything() {
    let mut b = board(&fixture());
    ch(&mut b, 'a', CMD);
    assert_eq!(b.selection().len(), 6);
}

#[test]
fn a_right_press_selects_what_is_under_it_and_opens_its_menu() {
    let mut b = board(&fixture());
    let fx = right_click(&mut b, pt(900.0, 100.0));
    assert!(fx.contains(&Effect::Menu));
    assert_eq!(sel(&b), ["c"]);
    assert!(b.menu().is_some());
    let fx = key(&mut b, Key::Escape, NONE);
    assert_eq!(fx, [Effect::CloseMenu]);
    assert!(b.menu().is_none());
}

// ---------------------------------------------------------------- moving

#[test]
fn drag_moves_the_node_under_the_pointer() {
    let mut b = board(&fixture());
    let fx = drag(&mut b, pt(100.0, 80.0), pt(150.0, 135.0), CMD);
    assert_eq!(sel(&b), ["a"]);
    let (_, ops) = sent(&fx);
    assert_eq!(ops, [apply::moved("a", 50.0, 55.0)]);
    assert_eq!(b.canvas().node("a").unwrap().x, 50.0);
}

#[test]
fn drag_moves_the_whole_selection() {
    let mut b = board(&fixture());
    b.select(&["a".to_owned(), "b".to_owned()]);
    let (_, ops) = sent(&drag(&mut b, pt(100.0, 80.0), pt(100.0, 280.0), CMD));
    assert_eq!(ops, [apply::moved("a", 0.0, 200.0), apply::moved("b", 400.0, 200.0)]);
}

#[test]
fn moving_snaps_to_other_nodes_edges_unless_cmd_is_held() {
    let mut b = board(&fixture());
    // a's left edge to 397: three from b's left edge (400).
    drag_hold(&mut b, pt(100.0, 80.0), pt(497.0, 380.0), NONE);
    assert!(!b.guides().is_empty());
    assert!(b.guides().iter().any(|g| g.vertical && g.at == 400.0));
    close(b.laid("a").unwrap().rect.x, 400.0);
    let (_, ops) = sent(&release(&mut b, pt(497.0, 380.0), NONE));
    assert_eq!(ops, [apply::moved("a", 400.0, 300.0)]);
    assert!(b.guides().is_empty());

    sent(&ch(&mut b, 'z', CMD));
    let (_, ops) = sent(&drag(&mut b, pt(100.0, 80.0), pt(497.0, 380.0), CMD));
    assert_eq!(ops, [apply::moved("a", 397.0, 300.0)]);
}

#[test]
fn moving_snaps_centres_too() {
    let mut b = board(&fixture());
    // c's centre x is 960; a (240 wide) dropped with its centre at 963 (x 843): snaps to x 840.
    let (_, ops) = sent(&drag(&mut b, pt(100.0, 80.0), pt(943.0, 480.0), NONE));
    assert_eq!(ops, [apply::moved("a", 840.0, 400.0)]);
}

#[test]
fn a_moved_frame_takes_its_contents() {
    let mut b = board(&fixture());
    let (_, ops) = sent(&drag(&mut b, pt(400.0, 810.0), pt(500.0, 860.0), CMD));
    assert_eq!(sel(&b), ["f"]);
    assert_eq!(ops, [apply::moved("f", 100.0, 850.0), apply::moved("fn", 140.0, 930.0)]);
    // The view draws the contents moving along while dragging.
    drag_hold(&mut b, pt(500.0, 860.0), pt(500.0, 1060.0), CMD);
    close(b.laid("fn").unwrap().rect.y, 930.0 + 200.0);
}

#[test]
fn dropping_into_and_out_of_frames_changes_parent() {
    let mut b = board(&fixture());
    // b's centre lands in f.
    let (_, ops) = sent(&drag(&mut b, pt(500.0, 80.0), pt(400.0, 980.0), CMD));
    assert_eq!(ops, [update("b", Some(300.0), Some(900.0), None, None, Some(Some("f")))]);
    assert_eq!(b.canvas().node("b").unwrap().parent.as_deref(), Some("f"));
    // fn dragged out of it.
    let (_, ops) = sent(&drag(&mut b, pt(100.0, 950.0), pt(100.0, 250.0), CMD));
    assert_eq!(ops, [update("fn", Some(40.0), Some(180.0), None, None, Some(None))]);
    assert_eq!(b.canvas().node("fn").unwrap().parent, None);
    // Moving within its frame: no parent change.
    let (_, ops) = sent(&drag(&mut b, pt(400.0, 1000.0), pt(450.0, 1000.0), CMD));
    assert_eq!(ops, [apply::moved("b", 350.0, 900.0)]);
}

#[test]
fn a_small_wiggle_is_a_click_not_a_move() {
    let mut b = board(&fixture());
    let fx = drag(&mut b, pt(100.0, 80.0), pt(101.0, 81.0), NONE);
    assert!(sends(&fx).is_empty());
    assert_eq!(sel(&b), ["a"]);
}

#[test]
fn arrow_keys_nudge_the_selection() {
    let mut b = board(&fixture());
    b.select(&["a".to_owned()]);
    assert_eq!(sent(&key(&mut b, Key::Left, NONE)).1, [apply::moved("a", -1.0, 0.0)]);
    assert_eq!(sent(&key(&mut b, Key::Up, SHIFT)).1, [apply::moved("a", -1.0, -10.0)]);
    b.select(&["f".to_owned()]);
    assert_eq!(sent(&key(&mut b, Key::Right, SHIFT)).1, [apply::moved("f", 10.0, 800.0), apply::moved("fn", 50.0, 880.0)]);
    b.select(&[]);
    assert!(sends(&key(&mut b, Key::Down, NONE)).is_empty());
}

// ---------------------------------------------------------------- resizing

#[test]
fn pictures_keep_their_aspect_unless_shift_is_held() {
    let mut b = board(&fixture());
    b.select(&["i".to_owned()]);
    let (_, ops) = sent(&drag(&mut b, pt(320.0, 580.0), pt(640.0, 700.0), CMD));
    assert_eq!(ops, [update("i", Some(0.0), Some(400.0), Some(640.0), Some(360.0), None)]);
    sent(&ch(&mut b, 'z', CMD));
    let (_, ops) = sent(&drag(&mut b, pt(320.0, 580.0), pt(640.0, 700.0), CMD_SHIFT));
    assert_eq!(ops, [update("i", Some(0.0), Some(400.0), Some(640.0), Some(300.0), None)]);
}

#[test]
fn a_top_left_corner_keeps_the_bottom_right_in_place() {
    let mut b = board(&fixture());
    b.select(&["i".to_owned()]);
    drag_hold(&mut b, pt(0.0, 400.0), pt(-320.0, 300.0), CMD);
    assert_eq!(b.cursor(), Cursor::Resize(super::super::Corner::TopLeft));
    let (_, ops) = sent(&release(&mut b, pt(-320.0, 300.0), CMD));
    assert_eq!(ops, [update("i", Some(-320.0), Some(220.0), Some(640.0), Some(360.0), None)]);
}

#[test]
fn cards_resize_in_width_and_their_height_follows() {
    let mut b = board(&fixture());
    b.select(&["c".to_owned()]);
    let (_, ops) = sent(&drag(&mut b, pt(1120.0, 292.0), pt(1220.0, 600.0), CMD));
    let h: f64 = 32.0 + 420.0 * 9.0 / 16.0 + 44.0 + 36.0;
    assert_eq!(ops, [update("c", Some(800.0), Some(0.0), Some(420.0), Some(h.round()), None)]);
    close(b.laid("c").unwrap().rect.h, h);
}

#[test]
fn notes_resize_freely_down_to_their_minimum() {
    let mut b = board(&fixture());
    b.select(&["a".to_owned()]);
    let (_, ops) = sent(&drag(&mut b, pt(240.0, 160.0), pt(300.0, 400.0), CMD));
    assert_eq!(ops, [update("a", Some(0.0), Some(0.0), Some(300.0), Some(400.0), None)]);
    let (_, ops) = sent(&drag(&mut b, pt(300.0, 400.0), pt(10.0, 10.0), CMD));
    assert_eq!(ops, [update("a", Some(0.0), Some(0.0), Some(120.0), Some(60.0), None)]);
}

#[test]
fn a_resized_frame_adopts_what_falls_inside_and_lets_go_of_what_falls_out() {
    let mut b = board(&fixture());
    b.select(&["f".to_owned()]);
    // Grow f's top edge up past i (0..320 × 400..580); shrink its right edge past nothing.
    let (_, ops) = sent(&drag(&mut b, pt(800.0, 800.0), pt(800.0, 380.0), CMD));
    assert_eq!(ops[0], update("f", Some(0.0), Some(380.0), Some(800.0), Some(920.0), None));
    assert!(ops.contains(&update("i", None, None, None, None, Some(Some("f")))));
    // Then shrink it so fn falls out.
    let (_, ops) = sent(&drag(&mut b, pt(800.0, 1300.0), pt(800.0, 700.0), CMD));
    assert!(ops.contains(&update("fn", None, None, None, None, Some(None))), "{ops:?}");
}

// ---------------------------------------------------------------- drawing

#[test]
fn the_note_tool_draws_a_note_then_returns_to_select() {
    let mut b = board(&fixture());
    ch(&mut b, 'n', NONE);
    assert_eq!(b.tool(), Tool::Note);
    drag_hold(&mut b, pt(900.0, 400.0), pt(1150.0, 600.0), NONE);
    assert_eq!(b.drawing(), Some((Tool::Note, Rect::new(900.0, 400.0, 250.0, 200.0))));
    let fx = release(&mut b, pt(1150.0, 600.0), NONE);
    let (_, ops) = sent(&fx);
    let id = added_id(&ops[0]);
    let Op::AddNode { node } = &ops[0] else { panic!() };
    assert_eq!((node.x, node.y, node.w, node.h), (Some(900.0), Some(400.0), Some(250.0), Some(200.0)));
    assert!(matches!(node.body, NodeBody::Note { .. }));
    assert!(fx.contains(&Effect::EditNote { node: id.clone() }));
    assert_eq!(sel(&b), [id.as_str()]);
    assert_eq!(b.tool(), Tool::Select);

    // A click: the default size at the point.
    ch(&mut b, 'N', NONE);
    let (_, ops) = sent(&click(&mut b, pt(2000.0, 0.0), NONE));
    let Op::AddNode { node } = &ops[0] else { panic!() };
    assert_eq!((node.x, node.y, node.w, node.h), (Some(2000.0), Some(0.0), Some(240.0), Some(160.0)));
}

#[test]
fn the_frame_tool_draws_a_frame_adopting_what_is_wholly_inside() {
    let mut b = board(&fixture());
    ch(&mut b, 'F', SHIFT);
    assert_eq!(b.tool(), Tool::Frame);
    // Around a and b, across part of i.
    let (_, ops) = sent(&drag(&mut b, pt(-20.0, -20.0), pt(700.0, 450.0), NONE));
    let id = added_id(&ops[0]);
    assert!(matches!(&ops[0], Op::AddNode { node } if matches!(node.body, NodeBody::Frame { .. }) && node.parent.is_none()));
    assert_eq!(&ops[1..], [update("a", None, None, None, None, Some(Some(&id))), update("b", None, None, None, None, Some(Some(&id)))]);
    assert_eq!(b.tool(), Tool::Select);
    assert_eq!(b.canvas().node("a").unwrap().parent.as_deref(), Some(id.as_str()));
    // Drawn inside f: it lands in f and takes fn.
    ch(&mut b, 'F', SHIFT);
    let (_, ops) = sent(&drag(&mut b, pt(20.0, 860.0), pt(400.0, 1100.0), NONE));
    let inner = added_id(&ops[0]);
    assert!(matches!(&ops[0], Op::AddNode { node } if node.parent.as_deref() == Some("f")));
    assert_eq!(&ops[1..], [update("fn", None, None, None, None, Some(Some(&inner)))]);
}

// ---------------------------------------------------------------- duplicate and delete

fn wired() -> Canvas {
    let running =
        with_status(with_takes(card("c", GenKind::Image, 400.0, 0.0), vec![take("t.png", StudioMedia::Image, 100, 100)]), GenStatus::Running);
    let queued = with_status(card("q", GenKind::Image, 400.0, 600.0), GenStatus::Queued);
    canvas(
        vec![note("src", 0.0, 0.0, "a cat"), running, queued, card("down", GenKind::Video, 1000.0, 0.0)],
        vec![edge("w1", "src", "c", EdgeRole::Text), edge("w2", "c", "down", EdgeRole::FirstFrame)],
    )
}

#[test]
fn cmd_d_duplicates_keeping_wires_coming_in_and_copies_dont_run() {
    let mut b = board(&wired());
    b.select(&["c".to_owned(), "q".to_owned()]);
    let fx = ch(&mut b, 'd', CMD);
    let (_, ops) = sent(&fx);
    let adds: Vec<&Op> = ops.iter().filter(|o| matches!(o, Op::AddNode { .. })).collect();
    assert_eq!(adds.len(), 2);
    let copy = added_id(adds[0]);
    let Op::AddNode { node } = adds[0] else { panic!() };
    // A small offset down and right of the original, cards too.
    assert_eq!((node.x, node.y), (Some(400.0 + 32.0), Some(32.0)));
    let NodeBody::Gen { status, job, takes, .. } = &node.body else { panic!() };
    assert_eq!(*status, GenStatus::Done);
    assert!(job.is_none());
    assert_eq!(takes.len(), 1);
    let Op::AddNode { node: q } = adds[1] else { panic!() };
    assert!(matches!(q.body, NodeBody::Gen { status: GenStatus::Draft, .. }));
    // The wire into it is kept (from the same source); the wire out of it isn't.
    let edges: Vec<&Op> = ops.iter().filter(|o| matches!(o, Op::AddEdge { .. })).collect();
    assert_eq!(edges.len(), 1);
    assert!(matches!(edges[0], Op::AddEdge { edge } if edge.from == "src" && edge.to == copy && edge.role == EdgeRole::Text));
    // The copies are selected.
    assert_eq!(b.selection().len(), 2);
    assert!(b.is_selected(&copy) && !b.is_selected("c"));
}

#[test]
fn duplicating_a_frame_copies_its_contents_and_the_wires_between_them() {
    let mut c = wired();
    c.nodes.push(frame("f", -100.0, -100.0, 1600.0, 1200.0));
    for n in &mut c.nodes[..4] {
        n.parent = Some("f".into());
    }
    let mut b = board(&c);
    b.select(&["f".to_owned()]);
    let (_, ops) = sent(&ch(&mut b, 'd', CMD));
    let adds: Vec<String> = ops.iter().filter(|o| matches!(o, Op::AddNode { .. })).map(added_id).collect();
    assert_eq!(adds.len(), 5);
    // The frame first, its copies inside the copy.
    assert!(matches!(&ops[0], Op::AddNode { node } if matches!(node.body, NodeBody::Frame { .. })));
    for op in &ops[1..5] {
        assert!(matches!(op, Op::AddNode { node } if node.parent.as_deref() == Some(adds[0].as_str())));
    }
    let edges = ops.iter().filter(|o| matches!(o, Op::AddEdge { edge } if adds.contains(&edge.from) && adds.contains(&edge.to))).count();
    assert_eq!(edges, 2);
    assert_eq!(b.selection(), [adds[0].clone()]);
}

#[test]
fn delete_removes_nodes_with_their_wires_and_a_selected_wire_unwires() {
    let mut b = board(&wired());
    b.select(&["c".to_owned()]);
    let (_, ops) = sent(&key(&mut b, Key::Delete, NONE));
    assert_eq!(ops, [Op::RemoveNode { id: "c".into() }]);
    assert!(b.canvas().edges.is_empty());
    assert!(b.selection().is_empty());
    sent(&ch(&mut b, 'z', CMD));

    // Click a wire: it is selected; Backspace unwires it.
    let s = b.scene();
    let mid = s.curve(b.canvas(), 0).unwrap().mid();
    let fx = click(&mut b, mid, NONE);
    assert!(fx.contains(&Effect::Selection));
    assert_eq!(b.selected_edge(), Some("w1"));
    assert!(b.selection().is_empty());
    let (_, ops) = sent(&key(&mut b, Key::Backspace, NONE));
    assert_eq!(ops, [Op::RemoveEdge { id: "w1".into() }]);
    assert_eq!(b.selected_edge(), None);
    assert!(b.canvas().node("src").is_some());
}

// ---------------------------------------------------------------- escape

#[test]
fn escape_cancels_a_move_or_resize_and_restores_a_marquees_selection() {
    let mut b = board(&fixture());
    drag_hold(&mut b, pt(100.0, 80.0), pt(300.0, 300.0), NONE);
    assert!(b.busy());
    assert!(sends(&key(&mut b, Key::Escape, NONE)).is_empty());
    assert!(!b.busy());
    assert_eq!(b.laid("a").unwrap().rect.origin(), Point::ZERO);
    assert!(sends(&release(&mut b, pt(300.0, 300.0), NONE)).is_empty());
    assert_eq!(b.canvas().node("a").unwrap().x, 0.0);

    b.select(&["i".to_owned()]);
    drag_hold(&mut b, pt(320.0, 580.0), pt(900.0, 900.0), CMD);
    assert!(b.laid("i").unwrap().rect.w > 320.0);
    key(&mut b, Key::Escape, NONE);
    assert_eq!(b.laid("i").unwrap().rect, Rect::new(0.0, 400.0, 320.0, 180.0));
    assert!(sends(&release(&mut b, pt(900.0, 900.0), NONE)).is_empty());

    drag_hold(&mut b, pt(-50.0, -50.0), pt(700.0, 100.0), NONE);
    assert_eq!(b.selection().len(), 2);
    key(&mut b, Key::Escape, NONE);
    assert_eq!(sel(&b), ["i"]);
    assert!(b.marquee().is_none());

    // Then: back to Select, then the selection clears.
    ch(&mut b, 'n', NONE);
    key(&mut b, Key::Escape, NONE);
    assert_eq!(b.tool(), Tool::Select);
    assert_eq!(sel(&b), ["i"]);
    key(&mut b, Key::Escape, NONE);
    assert!(b.selection().is_empty());
}

// ---------------------------------------------------------------- panning and zooming

#[test]
fn space_drag_and_middle_drag_pan() {
    let mut b = board(&fixture());
    key(&mut b, Key::Space, NONE);
    assert_eq!(b.cursor(), Cursor::Grab);
    // View points (the camera moves under the pointer).
    let mut fx = b.handle(Event::Down { at: pt(100.0, 100.0), button: Button::Left, mods: NONE, clicks: 1 });
    fx.extend(b.handle(Event::Move { at: pt(130.0, 110.0), mods: NONE }));
    fx.extend(b.handle(Event::Move { at: pt(150.0, 130.0), mods: NONE }));
    fx.extend(b.handle(Event::Up { at: pt(150.0, 130.0), button: Button::Left, mods: NONE }));
    assert!(sends(&fx).is_empty());
    assert_eq!(b.camera().pan, Point::new(50.0, 30.0));
    assert!(b.selection().is_empty());
    b.handle(Event::KeyUp { key: Key::Space, mods: NONE });

    let at = Point::new(500.0, 500.0);
    b.handle(Event::Down { at, button: Button::Middle, mods: NONE, clicks: 1 });
    b.handle(Event::Move { at: Point::new(480.0, 520.0), mods: NONE });
    b.handle(Event::Up { at: Point::new(480.0, 520.0), button: Button::Middle, mods: NONE });
    assert_eq!(b.camera().pan, Point::new(30.0, 50.0));
}

#[test]
fn cmd_plus_and_minus_zoom_around_the_centre() {
    let mut b = board(&fixture());
    let centre = Point::new(VIEW.w / 2.0, VIEW.h / 2.0);
    let under = b.camera().to_canvas(centre);
    ch(&mut b, '=', CMD);
    close(b.camera().zoom, 1.25);
    close_pt(b.camera().to_view(under), centre);
    ch(&mut b, '-', CMD);
    close(b.camera().zoom, 1.0);
}

#[test]
fn gestures_work_at_any_zoom_and_pan() {
    let mut b = board(&fixture());
    let _ = b.set_camera(Camera { pan: Point::new(-300.0, 120.0), zoom: 0.5 });
    let (_, ops) = sent(&drag(&mut b, pt(100.0, 80.0), pt(150.0, 135.0), CMD));
    assert_eq!(ops, [apply::moved("a", 50.0, 55.0)]);
}

// ---------------------------------------------------------------- double clicks and keys

fn prompt_at(fx: &[Effect]) -> Option<Point> {
    fx.iter().find_map(|f| match f {
        Effect::OpenPrompt { at, .. } => Some(*at),
        _ => None,
    })
}

#[test]
fn double_click_on_empty_canvas_or_a_frames_body_opens_the_canvas_prompt() {
    let mut b = board(&fixture());
    assert_eq!(prompt_at(&double_click(&mut b, pt(2000.0, 300.0))), Some(pt(2000.0, 300.0)));
    assert_eq!(prompt_at(&double_click(&mut b, pt(600.0, 1200.0))), Some(pt(600.0, 1200.0)));
}

#[test]
fn slash_and_g_open_the_canvas_prompt_at_the_pointer() {
    let mut b = board(&fixture());
    hover(&mut b, pt(1000.0, 600.0), NONE);
    assert_eq!(prompt_at(&ch(&mut b, '/', NONE)), Some(pt(1000.0, 600.0)));
    assert_eq!(prompt_at(&ch(&mut b, 'g', NONE)), Some(pt(1000.0, 600.0)));
    assert_eq!(prompt_at(&ch(&mut b, 'G', NONE)), Some(pt(1000.0, 600.0)));
    // Away from the board: the view's centre.
    b.handle(Event::Leave);
    assert_eq!(prompt_at(&ch(&mut b, 'g', NONE)), Some(pt(VIEW.w / 2.0, VIEW.h / 2.0)));
}

#[test]
fn double_click_on_a_node_edits_or_views_it() {
    let mut c = fixture();
    c.nodes.push(with_takes(card("done", GenKind::Image, 1400.0, 0.0), vec![take("t.png", StudioMedia::Image, 1920, 1080)]));
    c.nodes.push(entity("e", 1400.0, 500.0, "characters/mara"));
    let mut b = board(&c);
    assert!(double_click(&mut b, pt(100.0, 80.0)).contains(&Effect::EditNote { node: "a".into() }));
    assert!(double_click(&mut b, pt(100.0, 500.0)).contains(&Effect::View { node: "i".into() }));
    // A draft card's face, and any card's prompt: focus the prompt.
    assert!(double_click(&mut b, pt(900.0, 100.0)).contains(&Effect::FocusPrompt { node: "c".into() }));
    assert!(double_click(&mut b, pt(1500.0, 230.0)).contains(&Effect::FocusPrompt { node: "done".into() }));
    // A card's face with a take: the viewer.
    assert!(double_click(&mut b, pt(1500.0, 100.0)).contains(&Effect::View { node: "done".into() }));
    assert!(double_click(&mut b, pt(1500.0, 600.0)).contains(&Effect::OpenWorld { entity: "characters/mara".into() }));
    assert!(double_click(&mut b, pt(400.0, 810.0)).contains(&Effect::RenameFrame { node: "f".into() }));
    assert_eq!(sel(&b), ["f"]);
}

#[test]
fn single_clicks_on_a_cards_controls() {
    let two = vec![take("1.png", StudioMedia::Image, 1920, 1080), take("2.png", StudioMedia::Image, 1920, 1080)];
    let mut c = fixture();
    c.nodes.push(with_takes(card("t", GenKind::Image, 1400.0, 0.0), two));
    c.nodes.push(with_status(card("r", GenKind::Image, 1400.0, 500.0), GenStatus::Running));
    let mut b = board(&c);
    let run = |b: &Board, id: &str| b.laid(id).unwrap().card.unwrap().run.center();
    let p = run(&b, "c");
    assert!(click(&mut b, p, NONE).contains(&Effect::Run { node: "c".into() }));
    let p = run(&b, "r");
    assert!(click(&mut b, p, NONE).contains(&Effect::Stop { node: "r".into() }));
    let p = b.laid("c").unwrap().card.unwrap().prompt.center();
    assert!(click(&mut b, p, NONE).contains(&Effect::FocusPrompt { node: "c".into() }));
    // The stepper: back from the last take picks the first; forward from it, nothing more.
    let (back, fwd) = b.laid("t").unwrap().card.unwrap().stepper.unwrap();
    let (_, ops) = sent(&click(&mut b, back.center(), NONE));
    assert_eq!(ops, [apply::patch("t", serde_json::json!({ "pick": 0 }))]);
    assert!(sends(&click(&mut b, back.center(), NONE)).is_empty());
    sent(&click(&mut b, fwd.center(), NONE));
    assert!(sends(&click(&mut b, fwd.center(), NONE)).is_empty());
}

#[test]
fn tool_keys() {
    let mut b = board(&fixture());
    ch(&mut b, 'n', NONE);
    assert_eq!(b.tool(), Tool::Note);
    assert_eq!(b.cursor(), Cursor::Crosshair);
    ch(&mut b, 'v', NONE);
    assert_eq!(b.tool(), Tool::Select);
    // `f` with nothing selected: the Frame tool.
    ch(&mut b, 'f', NONE);
    assert_eq!(b.tool(), Tool::Frame);
    ch(&mut b, 'V', NONE);
    // With a selection `f` fits it and keeps the tool; `⇧F` is always the Frame tool.
    b.select(&["c".to_owned()]);
    let _ = b.set_camera(Camera { pan: Point::new(5000.0, 0.0), zoom: 0.2 });
    ch(&mut b, 'f', NONE);
    assert_eq!(b.tool(), Tool::Select);
    assert!(b.visible_rect().contains_rect(b.laid("c").unwrap().rect));
    ch(&mut b, 'F', SHIFT);
    assert_eq!(b.tool(), Tool::Frame);
    // ⌘ keys aren't tools.
    ch(&mut b, 'v', NONE);
    ch(&mut b, 'n', Mods { cmd: true, ..NONE });
    assert_eq!(b.tool(), Tool::Select);
}

#[test]
fn enter_edits_or_views_the_single_selected_node_and_cmd_c_copies() {
    let mut b = board(&fixture());
    b.select(&["a".to_owned()]);
    assert_eq!(key(&mut b, Key::Enter, NONE), [Effect::EditNote { node: "a".into() }]);
    b.select(&["i".to_owned()]);
    assert_eq!(key(&mut b, Key::Enter, NONE), [Effect::View { node: "i".into() }]);
    b.select(&["c".to_owned(), "a".to_owned()]);
    assert!(key(&mut b, Key::Enter, NONE).is_empty());
    assert_eq!(ch(&mut b, 'c', CMD), [Effect::Copy { nodes: vec!["c".into(), "a".into()] }]);
    let alt_cmd = Mods { cmd: true, alt: true, ..NONE };
    assert_eq!(key(&mut b, Key::Enter, alt_cmd), [Effect::AskAgent { nodes: vec!["c".into(), "a".into()] }]);
}

#[test]
fn moving_snaps_to_a_frames_padded_inside() {
    let mut b = board(&fixture());
    // b to (21, 853): three off f's padded inside (24, 800 + 32 + 24).
    let (_, ops) = sent(&drag(&mut b, pt(500.0, 80.0), pt(121.0, 933.0), NONE));
    assert_eq!(ops, [update("b", Some(24.0), Some(856.0), None, None, Some(Some("f")))]);
}

#[test]
fn resizing_snaps_the_corner_unless_cmd_is_held() {
    let mut b = board(&fixture());
    b.select(&["a".to_owned()]);
    let (_, ops) = sent(&drag(&mut b, pt(240.0, 160.0), pt(397.0, 300.0), NONE));
    assert_eq!(ops, [update("a", Some(0.0), Some(0.0), Some(400.0), Some(300.0), None)]);
    sent(&ch(&mut b, 'z', CMD));
    let (_, ops) = sent(&drag(&mut b, pt(240.0, 160.0), pt(397.0, 300.0), CMD));
    assert_eq!(ops, [update("a", Some(0.0), Some(0.0), Some(397.0), Some(300.0), None)]);
}

#[test]
fn escape_cancels_a_wire_being_dragged() {
    let mut c = fixture();
    c.edges.clear();
    let mut b = board(&c);
    let from = b.laid("i").unwrap().output_center();
    let to = b.laid("c").unwrap().port_center(0);
    drag_hold(&mut b, from, to, NONE);
    assert!(matches!(b.wiring().unwrap().drop, super::super::WireDrop::Wire { .. }));
    assert!(b.port_state(&b.laid("c").unwrap(), 0).target);
    key(&mut b, Key::Escape, NONE);
    assert!(b.wiring().is_none());
    assert!(sends(&release(&mut b, to, NONE)).is_empty());
    assert!(b.canvas().edges.is_empty());
}

#[test]
fn duplicating_wired_nodes_wires_the_copies_together() {
    let mut b = board(&wired());
    b.select(&["src".to_owned(), "c".to_owned()]);
    let (_, ops) = sent(&ch(&mut b, 'd', CMD));
    let adds: Vec<String> = ops.iter().filter(|o| matches!(o, Op::AddNode { .. })).map(added_id).collect();
    let edges: Vec<(String, String)> = ops
        .iter()
        .filter_map(|o| match o {
            Op::AddEdge { edge } => Some((edge.from.clone(), edge.to.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(edges, [(adds[0].clone(), adds[1].clone())]);
}

#[test]
fn another_canvas_resets_the_board() {
    let c = fixture();
    let mut b = board(&c);
    b.select(&["a".to_owned()]);
    ch(&mut b, 'n', NONE);
    let mut other = canvas(vec![note("z", 0.0, 0.0, "")], vec![]);
    other.name = "ep02".into();
    let fx = b.set_canvas(&other);
    assert!(fx.contains(&Effect::Selection));
    assert!(b.selection().is_empty());
    assert_eq!(b.tool(), Tool::Select);
    assert!(!b.can_undo());
    // A newer read of the same canvas drops a selected node that is gone.
    b.select(&["z".to_owned()]);
    let mut gone = other.clone();
    gone.rev = 2;
    gone.nodes.clear();
    assert!(b.set_canvas(&gone).contains(&Effect::Selection));
    assert!(b.selection().is_empty());
}

#[test]
fn hovering_sets_the_cursor() {
    let mut b = board(&fixture());
    assert!(!hover(&mut b, pt(100.0, 80.0), NONE).is_empty());
    assert_eq!(b.cursor(), Cursor::Grab);
    assert!(hover(&mut b, pt(101.0, 80.0), NONE).is_empty(), "same target: no redraw");
    let run = b.laid("c").unwrap().card.unwrap().run.center();
    hover(&mut b, run, NONE);
    assert_eq!(b.cursor(), Cursor::Pointer);
    let prompt = b.laid("c").unwrap().card.unwrap().prompt.center();
    hover(&mut b, prompt, NONE);
    assert_eq!(b.cursor(), Cursor::Text);
    let port = b.laid("c").unwrap().port_center(1);
    hover(&mut b, port, NONE);
    assert_eq!(b.hovered(), &super::super::Hit::Port { id: "c".into(), port: EdgeRole::Text });
    assert!(b.port_state(&b.laid("c").unwrap(), 1).hovered);
    hover(&mut b, pt(3000.0, 3000.0), NONE);
    assert_eq!(b.cursor(), Cursor::Default);
}

#[test]
fn clicking_an_entitys_name_strip_opens_it_in_the_world() {
    let mut c = fixture();
    c.nodes.push(entity("e", 1400.0, 500.0, "characters/mara"));
    let mut b = board(&c);
    // The sheet selects it; the name strip below opens the World view.
    let fx = click(&mut b, pt(1500.0, 600.0), NONE);
    assert!(!fx.iter().any(|f| matches!(f, Effect::OpenWorld { .. })));
    assert_eq!(sel(&b), ["e"]);
    assert!(click(&mut b, pt(1500.0, 714.0), NONE).contains(&Effect::OpenWorld { entity: "characters/mara".into() }));
}
