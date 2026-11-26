//! Canvases, projects, sections and boards built by hand, and helpers driving a [`Board`] with events.

use std::collections::BTreeMap;

use serde_json::Value;
use silo_proto::studio::{
    Canvas, Edge, EdgeRole, EntityInfo, Format, FrameRole, GenKind, GenStatus, Node, NodeBody, Op, Project, Section, SectionKind, StudioMedia, Take,
};

use super::super::{Board, Button, Camera, Effect, Event, Key, Mods, Point, Size};

pub const VIEW: Size = Size::new(1200.0, 800.0);

pub fn canvas(nodes: Vec<Node>, edges: Vec<Edge>) -> Canvas {
    Canvas {
        name: "ep01".into(),
        title: "Pilot".into(),
        episode: None,
        format: None,
        rev: 1,
        nodes,
        edges,
        out_of_date: Vec::new(),
        out_of_date_why: BTreeMap::new(),
    }
}

pub fn node(id: &str, x: f64, y: f64, w: f64, h: f64, body: NodeBody) -> Node {
    Node { id: id.into(), x, y, w, h, z: None, parent: None, body }
}

pub fn gen_body(makes: GenKind, prompt: &str) -> NodeBody {
    NodeBody::Gen {
        makes,
        title: None,
        prompt: prompt.into(),
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

/// A draft card 320 wide (its height follows its content).
pub fn card(id: &str, makes: GenKind, x: f64, y: f64) -> Node {
    node(id, x, y, 320.0, 300.0, gen_body(makes, ""))
}

pub fn take(path: &str, media: StudioMedia, w: u32, h: u32) -> Take {
    Take { path: path.into(), media, width: Some(w), height: Some(h), seconds: None, model: None, prompt: None, at: 1 }
}

/// `n` with `takes`, done.
pub fn with_takes(mut n: Node, new: Vec<Take>) -> Node {
    if let NodeBody::Gen { takes, status, .. } = &mut n.body {
        *takes = new;
        *status = GenStatus::Done;
    }
    n
}

pub fn with_status(mut n: Node, s: GenStatus) -> Node {
    if let NodeBody::Gen { status, job, .. } = &mut n.body {
        *status = s;
        *job = Some("job_1".into());
    }
    n
}

pub fn with_options(mut n: Node, o: Value) -> Node {
    if let NodeBody::Gen { options, .. } = &mut n.body {
        *options = o;
    }
    n
}

pub fn in_frame(mut n: Node, parent: &str) -> Node {
    n.parent = Some(parent.into());
    n
}

pub fn note(id: &str, x: f64, y: f64, text: &str) -> Node {
    node(id, x, y, 240.0, 160.0, NodeBody::Note { text: text.into() })
}

pub fn asset(id: &str, media: StudioMedia, x: f64, y: f64, w: f64, h: f64) -> Node {
    let path = match media {
        StudioMedia::Video => format!("canvases/ep01/assets/{id}.mp4"),
        StudioMedia::Audio => format!("canvases/ep01/assets/{id}.mp3"),
        _ => format!("canvases/ep01/assets/{id}.png"),
    };
    node(id, x, y, w, h, NodeBody::Asset { path, media, width: None, height: None, seconds: None, prompt: None })
}

pub fn image(id: &str, x: f64, y: f64) -> Node {
    asset(id, StudioMedia::Image, x, y, 320.0, 180.0)
}

pub fn video(id: &str, x: f64, y: f64) -> Node {
    asset(id, StudioMedia::Video, x, y, 320.0, 180.0)
}

pub fn entity(id: &str, x: f64, y: f64, entity: &str) -> Node {
    node(id, x, y, 200.0, 228.0, NodeBody::Entity { id: entity.into() })
}

pub fn frame(id: &str, x: f64, y: f64, w: f64, h: f64) -> Node {
    node(id, x, y, w, h, NodeBody::Frame { title: id.into(), role: FrameRole::Group, order: None, format: None })
}

pub fn frame_with_format(id: &str, x: f64, y: f64, w: f64, h: f64, format: &str) -> Node {
    node(id, x, y, w, h, NodeBody::Frame { title: id.into(), role: FrameRole::Shot, order: Some(1), format: Some(format.into()) })
}

pub fn edge(id: &str, from: &str, to: &str, role: EdgeRole) -> Edge {
    Edge { id: id.into(), from: from.into(), to: to.into(), role, label: None, mention: false }
}

fn preset(aspect: &str) -> Format {
    Format { aspect: aspect.into(), resolution: Some("1080p".into()), fps: None }
}

/// Default `landscape` (16:9); `vertical` (9:16) and `square` (1:1) presets.
pub fn project() -> Project {
    Project {
        name: "Harbor Lights".into(),
        format: "landscape".into(),
        formats: BTreeMap::from([
            ("landscape".to_owned(), preset("16:9")),
            ("vertical".to_owned(), preset("9:16")),
            ("square".to_owned(), preset("1:1")),
        ]),
        canvases: Vec::new(),
        sections: Vec::new(),
        models: BTreeMap::new(),
    }
}

/// Characters: Mara Venn (with a sheet) and Teo (none).
pub fn sections() -> Vec<Section> {
    let ent = |id: &str, name: &str, sheet: Option<&str>| EntityInfo {
        id: id.into(),
        name: name.into(),
        file: format!("world/{id}/x.md"),
        summary: None,
        sheet: sheet.map(str::to_owned),
        refs: Vec::new(),
        looks: Vec::new(),
        links: Vec::new(),
        tags: Vec::new(),
        approval: Default::default(),
        voice: false,
    };
    vec![Section {
        id: "characters".into(),
        title: "Characters".into(),
        kind: SectionKind::Entities,
        entities: vec![ent("characters/mara", "Mara Venn", Some("world/characters/mara/sheet.png")), ent("characters/teo", "Teo", None)],
        refs: Vec::new(),
    }]
}

/// A board showing `c` with the project and sections, at 100 % with the canvas origin at the view's
/// top-left (view points are canvas points).
pub fn board(c: &Canvas) -> Board {
    let mut b = Board::new(7);
    b.set_view_size(VIEW);
    b.set_context(Some(&project()), &sections());
    b.set_canvas(c);
    b.set_camera(Camera::default());
    b
}

// ---------------------------------------------------------------- events

pub const NONE: Mods = Mods::NONE;
pub const SHIFT: Mods = Mods { shift: true, cmd: false, alt: false, ctrl: false };
pub const CMD: Mods = Mods { shift: false, cmd: true, alt: false, ctrl: false };
pub const CMD_SHIFT: Mods = Mods { shift: true, cmd: true, alt: false, ctrl: false };

fn v(b: &Board, p: Point) -> Point {
    b.camera().to_view(p)
}

pub fn pt(x: f64, y: f64) -> Point {
    Point::new(x, y)
}

/// A press at canvas point `p`.
pub fn press(b: &mut Board, p: Point, mods: Mods) -> Vec<Effect> {
    let at = v(b, p);
    b.handle(Event::Down { at, button: Button::Left, mods, clicks: 1 })
}

pub fn release(b: &mut Board, p: Point, mods: Mods) -> Vec<Effect> {
    let at = v(b, p);
    b.handle(Event::Up { at, button: Button::Left, mods })
}

pub fn hover(b: &mut Board, p: Point, mods: Mods) -> Vec<Effect> {
    let at = v(b, p);
    b.handle(Event::Move { at, mods })
}

/// A click at canvas point `p`.
pub fn click(b: &mut Board, p: Point, mods: Mods) -> Vec<Effect> {
    let mut fx = press(b, p, mods);
    fx.extend(release(b, p, mods));
    fx
}

/// A double click at canvas point `p`: its second press.
pub fn double_click(b: &mut Board, p: Point) -> Vec<Effect> {
    click(b, p, NONE);
    let at = v(b, p);
    let mut fx = b.handle(Event::Down { at, button: Button::Left, mods: NONE, clicks: 2 });
    fx.extend(b.handle(Event::Up { at, button: Button::Left, mods: NONE }));
    fx
}

pub fn right_click(b: &mut Board, p: Point) -> Vec<Effect> {
    let at = v(b, p);
    let mut fx = b.handle(Event::Down { at, button: Button::Right, mods: NONE, clicks: 1 });
    fx.extend(b.handle(Event::Up { at, button: Button::Right, mods: NONE }));
    fx
}

/// Press at `from`, move through the midpoint to `to`, release there; `mods` held throughout.
pub fn drag(b: &mut Board, from: Point, to: Point, mods: Mods) -> Vec<Effect> {
    let mut fx = press(b, from, mods);
    fx.extend(hover(b, (from + to) * 0.5, mods));
    fx.extend(hover(b, to, mods));
    fx.extend(release(b, to, mods));
    fx
}

/// Press and move without releasing.
pub fn drag_hold(b: &mut Board, from: Point, to: Point, mods: Mods) -> Vec<Effect> {
    let mut fx = press(b, from, mods);
    fx.extend(hover(b, (from + to) * 0.5, mods));
    fx.extend(hover(b, to, mods));
    fx
}

pub fn key(b: &mut Board, key: Key, mods: Mods) -> Vec<Effect> {
    b.handle(Event::KeyDown { key, mods })
}

pub fn ch(b: &mut Board, c: char, mods: Mods) -> Vec<Effect> {
    key(b, Key::Char(c), mods)
}

// ---------------------------------------------------------------- effects

/// Every `Send` in `fx`: its batch and ops.
pub fn sends(fx: &[Effect]) -> Vec<(u64, Vec<Op>)> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Send { batch, ops, .. } => Some((*batch, ops.clone())),
            _ => None,
        })
        .collect()
}

/// The one `Send` in `fx`.
#[track_caller]
pub fn sent(fx: &[Effect]) -> (u64, Vec<Op>) {
    let s = sends(fx);
    assert_eq!(s.len(), 1, "expected one Send in {fx:?}");
    s.into_iter().next().unwrap()
}

pub fn notices(fx: &[Effect]) -> Vec<String> {
    fx.iter()
        .filter_map(|f| match f {
            Effect::Notice(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// The id of the node an `add_node` op adds.
#[track_caller]
pub fn added_id(op: &Op) -> String {
    match op {
        Op::AddNode { node } => node.id.clone().expect("add_node with an id"),
        other => panic!("not add_node: {other:?}"),
    }
}

/// The canvas with nodes and edges sorted by id (removing and re-adding moves them to the end).
pub fn sorted(c: &Canvas) -> Canvas {
    let mut c = c.clone();
    c.nodes.sort_by(|a, b| a.id.cmp(&b.id));
    c.edges.sort_by(|a, b| a.id.cmp(&b.id));
    c
}

#[track_caller]
pub fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-6, "{a} != {b}");
}

#[track_caller]
pub fn close_pt(a: Point, b: Point) {
    assert!(a.dist(b) < 1e-6, "{a:?} != {b:?}");
}
