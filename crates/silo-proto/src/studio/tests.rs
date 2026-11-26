//! The pure helpers of docs/STUDIO.md §3 (ports, wires, sources, keys, out
//! of date, formats) and the wire forms of the studio types.

use serde_json::json;

use super::*;

fn node(id: &str, body: NodeBody) -> Node {
    Node { id: id.into(), x: 0.0, y: 0.0, w: 10.0, h: 10.0, z: None, parent: None, body }
}

fn framed(id: &str, parent: &str, body: NodeBody) -> Node {
    Node { parent: Some(parent.into()), ..node(id, body) }
}

fn card(makes: GenKind) -> NodeBody {
    serde_json::from_value(json!({"kind": "gen", "makes": makes})).unwrap()
}

fn titled(makes: GenKind, title: &str) -> NodeBody {
    serde_json::from_value(json!({"kind": "gen", "makes": makes, "title": title})).unwrap()
}

fn take(path: &str, media: StudioMedia) -> Take {
    Take { path: path.into(), media, width: None, height: None, seconds: None, model: None, prompt: None, at: 1 }
}

/// A card with `takes` (`done`), `pick` and `inputs_used`.
fn made(makes: GenKind, title: &str, takes: &[&str], pick: Option<u32>, used: &[&str]) -> NodeBody {
    let mut body = titled(makes, title);
    let media = match makes {
        GenKind::Video => StudioMedia::Video,
        GenKind::Speech | GenKind::Sound | GenKind::Music => StudioMedia::Audio,
        _ => StudioMedia::Image,
    };
    if let NodeBody::Gen { takes: t, pick: p, status, inputs_used, .. } = &mut body {
        *t = takes.iter().map(|path| take(path, media)).collect();
        *p = pick;
        *status = if takes.is_empty() { GenStatus::Draft } else { GenStatus::Done };
        *inputs_used = used.iter().map(|s| (*s).to_owned()).collect();
    }
    body
}

fn with_status(mut body: NodeBody, to: GenStatus) -> NodeBody {
    if let NodeBody::Gen { status, .. } = &mut body {
        *status = to;
    }
    body
}

fn asset(path: &str, media: StudioMedia) -> NodeBody {
    NodeBody::Asset { path: path.into(), media, width: None, height: None, seconds: None, prompt: None }
}

fn note(text: &str) -> NodeBody {
    NodeBody::Note { text: text.into() }
}

fn entity(id: &str) -> NodeBody {
    NodeBody::Entity { id: id.into() }
}

fn frame(role: FrameRole, format: Option<&str>) -> NodeBody {
    NodeBody::Frame { title: "F".into(), role, order: None, format: format.map(str::to_owned) }
}

fn edge(id: &str, from: &str, to: &str, role: EdgeRole) -> Edge {
    Edge { id: id.into(), from: from.into(), to: to.into(), role, label: None, mention: false }
}

fn canvas(nodes: Vec<Node>, edges: Vec<Edge>) -> Canvas {
    Canvas {
        name: "c".into(),
        title: "C".into(),
        episode: None,
        format: None,
        rev: 1,
        nodes,
        edges,
        out_of_date: Vec::new(),
        out_of_date_why: BTreeMap::new(),
    }
}

fn sections() -> Vec<Section> {
    let mara = EntityInfo {
        id: "characters/mara".into(),
        name: "Mara Venn".into(),
        file: "world/characters/mara/mara.md".into(),
        summary: None,
        sheet: Some("world/characters/mara/sheet.png".into()),
        refs: Vec::new(),
        looks: Vec::new(),
        links: Vec::new(),
        tags: Vec::new(),
        approval: Approval::None,
        voice: false,
    };
    let teo = EntityInfo { id: "characters/teo".into(), name: "Teo".into(), file: "world/characters/teo/teo.md".into(), sheet: None, ..mara.clone() };
    vec![Section { id: "characters".into(), title: "Characters".into(), kind: SectionKind::Entities, entities: vec![mara, teo], refs: Vec::new() }]
}

const ALL_ROLES: [EdgeRole; 9] = [
    EdgeRole::FirstFrame,
    EdgeRole::MiddleFrame,
    EdgeRole::LastFrame,
    EdgeRole::Reference,
    EdgeRole::Video,
    EdgeRole::Audio,
    EdgeRole::Text,
    EdgeRole::Next,
    EdgeRole::Unknown,
];

#[test]
fn ports_per_kind() {
    use EdgeRole as R;
    assert_eq!(GenKind::Image.ports(), [R::Reference, R::Text]);
    assert_eq!(GenKind::Video.ports(), [R::FirstFrame, R::MiddleFrame, R::LastFrame, R::Reference, R::Video, R::Audio, R::Text]);
    for audio in [GenKind::Speech, GenKind::Sound, GenKind::Music] {
        assert_eq!(audio.ports(), [R::Text]);
        assert_eq!(audio.media(), SourceMedia::Audio);
    }
    assert!(GenKind::Unknown.ports().is_empty());
    assert_eq!(
        (GenKind::Image.media(), GenKind::Video.media(), GenKind::Unknown.media()),
        (SourceMedia::Image, SourceMedia::Video, SourceMedia::Unknown)
    );
    assert_eq!(GenKind::Unknown.label(), "Card");
    // Every port of every card is a port role that takes something.
    for kind in [GenKind::Image, GenKind::Video, GenKind::Speech, GenKind::Sound, GenKind::Music] {
        assert!(kind.ports().iter().all(|p| p.is_port() && p.accepts().is_some()), "{kind:?}");
    }
}

#[test]
fn edge_roles_accept_hold_and_name() {
    use EdgeRole as R;
    for role in ALL_ROLES {
        let expected = match role {
            R::FirstFrame | R::MiddleFrame | R::LastFrame | R::Reference => Some(SourceMedia::Image),
            R::Video => Some(SourceMedia::Video),
            R::Audio => Some(SourceMedia::Audio),
            R::Text => Some(SourceMedia::Text),
            R::Next | R::Unknown => None,
        };
        assert_eq!(role.accepts(), expected, "{role:?}");
        assert_eq!(role.single(), matches!(role, R::FirstFrame | R::MiddleFrame | R::LastFrame), "{role:?}");
        assert_eq!(role.is_port(), !matches!(role, R::Next | R::Unknown), "{role:?}");
        // The display name is the wire name with spaces.
        let wire = serde_json::to_value(role).unwrap();
        assert_eq!(role.name(), wire.as_str().unwrap().replace('_', " "), "{role:?}");
    }
}

#[test]
fn default_port_is_the_first_free_one_that_fits() {
    use EdgeRole as R;
    use SourceMedia as M;
    // An image onto a video card: first frame, then reference (never the
    // middle or last frame on its own).
    assert_eq!(default_port(GenKind::Video, M::Image, &[]), Some(R::FirstFrame));
    assert_eq!(default_port(GenKind::Video, M::Image, &[R::FirstFrame]), Some(R::Reference));
    assert_eq!(default_port(GenKind::Video, M::Image, &[R::FirstFrame, R::Reference]), Some(R::Reference), "reference holds many");
    assert_eq!(default_port(GenKind::Video, M::Video, &[R::Video]), Some(R::Video));
    assert_eq!(default_port(GenKind::Video, M::Audio, &[]), Some(R::Audio));
    assert_eq!(default_port(GenKind::Video, M::Text, &[]), Some(R::Text));
    assert_eq!(default_port(GenKind::Image, M::Image, &[]), Some(R::Reference));
    assert_eq!(default_port(GenKind::Image, M::Video, &[]), None);
    assert_eq!(default_port(GenKind::Speech, M::Text, &[]), Some(R::Text));
    assert_eq!(default_port(GenKind::Music, M::Image, &[]), None);
    assert_eq!(default_port(GenKind::Unknown, M::Text, &[]), None);
    assert_eq!(default_port(GenKind::Image, M::Unknown, &[]), None);
}

/// a: image asset, v: video asset, s: audio asset, t: note, e: entity,
/// f: frame, img/vid/spk: cards.
fn board(edges: Vec<Edge>) -> Canvas {
    canvas(
        vec![
            node("a", asset("canvases/c/assets/a.png", StudioMedia::Image)),
            node("v", asset("canvases/c/assets/v.mp4", StudioMedia::Video)),
            node("s", asset("canvases/c/assets/s.mp3", StudioMedia::Audio)),
            node("t", note("a beat")),
            node("e", entity("characters/mara")),
            node("f", frame(FrameRole::Shot, None)),
            node("img", card(GenKind::Image)),
            node("img2", card(GenKind::Image)),
            node("img3", card(GenKind::Image)),
            node("vid", card(GenKind::Video)),
            node("spk", card(GenKind::Speech)),
            node("odd", NodeBody::Unknown),
        ],
        edges,
    )
}

#[test]
fn wire_check_refuses_what_cant_be_and_says_why() {
    use EdgeRole as R;
    let c = board(vec![edge("e1", "a", "vid", R::FirstFrame), edge("e2", "img", "img2", R::Reference)]);
    let err = |from: &str, to: &str, role: EdgeRole| c.wire_check(from, to, role).unwrap_err();

    assert!(err("img", "img", R::Reference).contains("itself"));
    assert_eq!(err("nope", "img", R::Reference), "no node nope");
    assert_eq!(err("a", "nope", R::Reference), "no node nope");
    assert_eq!(err("a", "img", R::Unknown), "unknown edge role");
    assert!(err("a", "t", R::Reference).contains("is not a card"), "wires go into cards");
    assert_eq!(err("a", "img", R::FirstFrame), "an image card has no first frame port (its ports: reference, text)");
    assert_eq!(err("t", "spk", R::Reference), "a speech card has no reference port (its ports: text)");
    assert!(err("v", "img", R::Reference).contains("the reference port takes an image; v outputs a video"));
    assert!(err("a", "vid", R::Audio).contains("takes audio"));
    assert!(err("t", "vid", R::FirstFrame).contains("outputs text"));
    assert!(err("s", "spk", R::Text).contains("takes text"));
    assert!(err("f", "img", R::Reference).contains("has no output"), "a frame outputs nothing");
    assert!(err("odd", "img", R::Reference).contains("has no output"), "nor does an unknown kind");
    // A video card's take is a video: not a reference.
    assert!(err("vid", "img", R::Reference).contains("outputs a video"));
    assert!(err("img", "img2", R::Reference).contains("already wired"), "a duplicate");

    // What fits: every source kind, a draft card by what it makes.
    assert_eq!(c.wire_check("a", "img", R::Reference), Ok(vec![]));
    assert_eq!(c.wire_check("e", "vid", R::Reference), Ok(vec![]));
    assert_eq!(c.wire_check("t", "spk", R::Text), Ok(vec![]));
    assert_eq!(c.wire_check("img", "vid", R::LastFrame), Ok(vec![]));
    assert_eq!(c.wire_check("v", "vid", R::Video), Ok(vec![]));
    assert_eq!(c.wire_check("s", "vid", R::Audio), Ok(vec![]));
    // `next` is no wire: it goes between anything (frames too).
    assert_eq!(c.wire_check("f", "t", R::Next), Ok(vec![]));
    // A frame port holds one wire: the new one replaces it; others hold many.
    assert_eq!(c.wire_check("img", "vid", R::FirstFrame), Ok(vec!["e1".to_owned()]));
    assert_eq!(c.wire_check("e", "img2", R::Reference), Ok(vec![]));
}

#[test]
fn wire_check_refuses_loops_direct_and_indirect() {
    use EdgeRole as R;
    // img → img2 → img3 (references), vid ← img3 (first frame).
    let c = board(vec![edge("e1", "img", "img2", R::Reference), edge("e2", "img2", "img3", R::Reference), edge("e3", "img3", "vid", R::FirstFrame)]);
    assert!(c.wire_check("img2", "img", R::Reference).unwrap_err().contains("loop"), "direct");
    assert!(c.wire_check("img3", "img", R::Reference).unwrap_err().contains("loop"), "through one card");
    assert!(c.wire_check("t", "img", R::Text).is_ok());
    // Downstream of nothing: fine.
    assert!(c.wire_check("img", "img3", R::Reference).is_ok(), "a second path is no loop");
    // `next` edges are not followed: they make no loop.
    let c = board(vec![edge("n1", "img2", "img", R::Next)]);
    assert!(c.wire_check("img", "img2", R::Reference).is_ok());
    // A loop already in a hand-edited file doesn't hang the check.
    let c = board(vec![edge("x1", "img", "img2", R::Reference), edge("x2", "img2", "img", R::Reference)]);
    assert!(c.wire_check("img2", "img3", R::Reference).is_ok());
    assert!(c.wire_check("img3", "img", R::Reference).is_ok());
}

#[test]
fn inputs_come_by_port_then_in_edge_order() {
    use EdgeRole as R;
    let c = board(vec![
        edge("t1", "t", "vid", R::Text),
        edge("r1", "e", "vid", R::Reference),
        edge("ff", "img", "vid", R::FirstFrame),
        edge("r2", "a", "vid", R::Reference),
        edge("nx", "img2", "vid", R::Next),
        edge("au", "s", "vid", R::Audio),
        edge("lf", "img3", "vid", R::LastFrame),
        edge("x", "a", "img", R::FirstFrame), // hand-edited: a port the image card lacks
        edge("ir", "a", "img", R::Reference),
    ]);
    let ids = |id: &str| c.inputs_of(id).iter().map(|e| e.id.as_str()).collect::<Vec<_>>();
    assert_eq!(ids("vid"), ["ff", "lf", "r1", "r2", "au", "t1"]);
    assert_eq!(ids("img"), ["ir"], "only its own ports");
    assert!(ids("t").is_empty() && ids("nope").is_empty(), "no card: no inputs");
}

#[test]
fn what_each_node_outputs() {
    let c = canvas(
        vec![
            node("a", asset("canvases/c/assets/a.png", StudioMedia::Image)),
            node("e", entity("characters/mara")),
            node("bare", entity("characters/teo")),
            node("gone", entity("places/nowhere")),
            node("draft", titled(GenKind::Image, "Keyframe 3")),
            node("running", with_status(card(GenKind::Video), GenStatus::Running)),
            node("queued", with_status(made(GenKind::Image, "Q", &["canvases/c/assets/q1.png"], None, &[]), GenStatus::Queued)),
            node(
                "done",
                made(GenKind::Image, "Done", &["canvases/c/assets/k1.png", "canvases/c/assets/k2.png", "canvases/c/assets/k3.png"], Some(1), &[]),
            ),
            node("last", made(GenKind::Image, "Last", &["canvases/c/assets/l1.png", "canvases/c/assets/l2.png"], None, &[])),
            node("badpick", made(GenKind::Image, "", &["canvases/c/assets/b1.png"], Some(7), &[])),
            node("t", note("\n## Mara hears the bell\nshe turns")),
            node("f", frame(FrameRole::Scene, None)),
            node("u", NodeBody::Unknown),
        ],
        vec![],
    );
    let s = sections();
    let out = |id: &str| c.output_of(id, &s).unwrap();

    let a = out("a");
    assert_eq!(
        (a.media, a.path.as_deref(), a.label.as_str(), a.pending, a.problem),
        (SourceMedia::Image, Some("canvases/c/assets/a.png"), "a.png", false, None)
    );
    let e = out("e");
    assert_eq!((e.media, e.path.as_deref(), e.label.as_str()), (SourceMedia::Image, Some("world/characters/mara/sheet.png"), "Mara Venn"));
    let bare = out("bare");
    assert_eq!((bare.path, bare.problem.as_deref()), (None, Some("Teo has no sheet yet")));
    assert_eq!(out("gone").label, "places/nowhere", "an entity not in the world: its id");
    assert!(out("gone").problem.is_some());

    let draft = out("draft");
    assert_eq!((draft.media, draft.path, draft.pending, draft.problem.as_deref()), (SourceMedia::Image, None, false, Some("Run Keyframe 3 first")));
    let running = out("running");
    assert_eq!((running.media, running.pending, running.problem, running.label.as_str()), (SourceMedia::Video, true, None, "Video running"));
    let queued = out("queued");
    assert_eq!((queued.pending, queued.path.as_deref()), (true, Some("canvases/c/assets/q1.png")), "an older take while it runs again");
    assert_eq!(out("done").path.as_deref(), Some("canvases/c/assets/k2.png"), "the pick");
    assert_eq!(out("last").path.as_deref(), Some("canvases/c/assets/l2.png"), "no pick: the last take");
    assert_eq!(out("badpick").path.as_deref(), Some("canvases/c/assets/b1.png"), "a pick past the takes: the last");
    assert_eq!(out("badpick").label, "Image badpick", "an empty title: kind and id");

    let t = out("t");
    assert_eq!(
        (t.media, t.text.as_deref(), t.path, t.label.as_str()),
        (SourceMedia::Text, Some("\n## Mara hears the bell\nshe turns"), None, "Mara hears the bell")
    );
    assert_eq!(c.label_of("f", &s), "F");
    for none in ["f", "u", "nope"] {
        assert!(c.output_of(none, &s).is_none(), "{none}");
        assert!(c.source_media(none).is_none(), "{none}");
    }
    // A video card whose picked take is an image (a still from it): the take decides.
    let mut body = made(GenKind::Video, "V", &["x.png"], None, &[]);
    if let NodeBody::Gen { takes, .. } = &mut body {
        takes[0].media = StudioMedia::Image;
    }
    let odd = canvas(vec![node("v", body)], vec![]);
    assert_eq!(odd.source_media("v"), Some(SourceMedia::Image));
}

#[test]
fn keys_and_out_of_date() {
    use EdgeRole as R;
    let s = sections();
    let mtimes = |rel: &str| match rel {
        "canvases/c/assets/a.png" => Some(1000),
        "world/characters/mara/sheet.png" => Some(2000),
        _ => None,
    };
    let c = canvas(
        vec![
            node("a", asset("canvases/c/assets/a.png", StudioMedia::Image)),
            node("x", asset("canvases/c/assets/missing.png", StudioMedia::Image)),
            node("e", entity("characters/mara")),
            node("bare", entity("characters/teo")),
            node("k", made(GenKind::Image, "Keyframe", &["canvases/c/assets/k1.png", "canvases/c/assets/k2.png"], Some(0), &[])),
            node("draft", card(GenKind::Image)),
            node("t", note("a beat")),
            node("vid", made(GenKind::Video, "Shot", &["canvases/c/assets/v1.mp4"], None, &[])),
            node("f", frame(FrameRole::Shot, None)),
        ],
        vec![
            edge("1", "t", "vid", R::Text),
            edge("2", "e", "vid", R::Reference),
            edge("3", "k", "vid", R::FirstFrame),
            edge("4", "bare", "vid", R::Reference),
            edge("5", "a", "vid", R::Reference),
            edge("6", "draft", "vid", R::LastFrame),
        ],
    );
    let key = |id: &str| c.source_key(id, &s, mtimes);
    assert_eq!(key("a").as_deref(), Some("canvases/c/assets/a.png@1000"));
    assert_eq!(key("x").as_deref(), Some("canvases/c/assets/missing.png"), "no mtime: the path alone");
    assert_eq!(key("e").as_deref(), Some("world/characters/mara/sheet.png@2000"));
    assert_eq!(key("bare"), None, "no sheet: no key");
    assert_eq!(key("k").as_deref(), Some("canvases/c/assets/k1.png"), "the picked take's path");
    assert_eq!(key("draft"), None);
    assert_eq!(key("f"), None);
    let note_key = key("t").unwrap();
    assert!(note_key.starts_with("note:") && note_key.len() == "note:".len() + 16, "{note_key}");
    assert_eq!(note_key, c.source_key("t", &s, |_| None).unwrap(), "a note's key has no mtime");
    let other = canvas(vec![node("t", note("another beat"))], vec![]);
    assert_ne!(other.source_key("t", &s, mtimes).unwrap(), note_key, "the text changes the key");

    let keys = c.input_keys("vid", &s, mtimes);
    assert_eq!(
        keys,
        ["canvases/c/assets/k1.png", "world/characters/mara/sheet.png@2000", "canvases/c/assets/a.png@1000", note_key.as_str()],
        "by port (first frame, references in edge order, text); sources without a key left out"
    );

    // Out of date: inputs_used differs from the keys now, in any order.
    let with_used = |used: Vec<String>| {
        let mut c = c.clone();
        if let Some(NodeBody::Gen { inputs_used, .. }) = c.nodes.iter_mut().find(|n| n.id == "vid").map(|n| &mut n.body) {
            *inputs_used = used;
        }
        c
    };
    let mut reversed = keys.clone();
    reversed.reverse();
    let fresh = with_used(reversed);
    assert!(!fresh.out_of_date("vid", &keys), "the same keys in another order");
    assert!(fresh.out_of_date_why("vid", &s, mtimes).is_empty());
    assert!(with_used(keys[..3].to_vec()).out_of_date("vid", &keys));
    assert!(!c.out_of_date("draft", &keys), "a draft never is");
    assert!(!c.out_of_date("a", &keys) && !c.out_of_date("nope", &keys), "not a card");
    let running = canvas(vec![node("r", with_status(made(GenKind::Image, "R", &["r.png"], None, &["old"]), GenStatus::Running))], vec![]);
    assert!(!running.out_of_date("r", &[]), "a running card never is");
    let failed = canvas(vec![node("r", with_status(made(GenKind::Image, "R", &["r.png"], None, &["old"]), GenStatus::Failed))], vec![]);
    assert!(failed.out_of_date("r", &[]), "a failed rerun keeps its old take, out of date");

    // Why: the sheet changed (same file, another mtime), a new wire, the
    // keyframe picked another take, and inputs no longer wired.
    let mut used = keys.clone();
    used[1] = "world/characters/mara/sheet.png@1500".into();
    let stale = with_used(used);
    assert_eq!(stale.out_of_date_why("vid", &s, mtimes), ["Mara Venn changed"]);

    let stale = with_used(vec![keys[0].clone(), keys[1].clone(), keys[3].clone()]);
    assert_eq!(stale.out_of_date_why("vid", &s, mtimes), ["a.png is a new input"]);

    let mut used = keys.clone();
    used[0] = "canvases/c/assets/k2.png".into();
    let stale = with_used(used);
    assert_eq!(stale.out_of_date_why("vid", &s, mtimes), ["Keyframe changed"], "another take of the same card");

    // The note's text edited: that note changed (not a new input and a gone one).
    let mut used = keys.clone();
    used[3] = "note:0000000000000000".into();
    assert_eq!(with_used(used).out_of_date_why("vid", &s, mtimes), ["a beat changed"]);

    let mut used = keys.clone();
    used.push("canvases/c/assets/gone.png@5".into());
    used.push("note:0000000000000000".into());
    let stale = with_used(used);
    assert_eq!(stale.out_of_date_why("vid", &s, mtimes), ["2 inputs no longer wired"]);
    let mut used = keys.clone();
    used.push("canvases/c/assets/gone.png@5".into());
    assert_eq!(with_used(used).out_of_date_why("vid", &s, mtimes), ["1 input no longer wired"]);
}

fn presets() -> Project {
    let f = |aspect: &str| Format { aspect: aspect.into(), resolution: Some("1080p".into()), fps: None };
    Project {
        name: "P".into(),
        format: "landscape".into(),
        formats: BTreeMap::from([
            ("landscape".to_owned(), f("16:9")),
            ("vertical".to_owned(), f("9:16")),
            ("square".to_owned(), f("1:1")),
            ("cinema".to_owned(), f("2.39:1")),
        ]),
        canvases: Vec::new(),
        sections: Vec::new(),
        models: BTreeMap::new(),
    }
}

#[test]
fn format_resolution_shot_scene_canvas_project() {
    let p = presets();
    let name = |c: &Canvas, frame: Option<&str>| p.format_for(c, frame).map(|(n, _)| n);
    let mut c = canvas(
        vec![
            node("scene", frame(FrameRole::Scene, Some("square"))),
            framed("shot", "scene", frame(FrameRole::Shot, Some("cinema"))),
            framed("plain", "scene", frame(FrameRole::Shot, None)),
            framed("typo", "scene", frame(FrameRole::Shot, Some("nope"))),
            node("lone", frame(FrameRole::Shot, None)),
            framed("card", "shot", card(GenKind::Image)),
        ],
        vec![],
    );
    assert_eq!(name(&c, Some("shot")).as_deref(), Some("cinema"), "the shot's own");
    assert_eq!(name(&c, Some("plain")).as_deref(), Some("square"), "else its scene's");
    assert_eq!(name(&c, Some("typo")).as_deref(), Some("square"), "a name with no preset is skipped");
    assert_eq!(name(&c, Some("lone")).as_deref(), Some("landscape"), "else the project's");
    assert_eq!(name(&c, None).as_deref(), Some("landscape"));
    assert_eq!(name(&c, Some("nope")).as_deref(), Some("landscape"), "a missing frame");
    assert_eq!(name(&c, Some("card")).as_deref(), Some("landscape"), "a card is no frame");
    c.format = Some("vertical".into());
    assert_eq!(name(&c, Some("lone")).as_deref(), Some("vertical"), "the canvas's before the project's");
    assert_eq!(name(&c, Some("shot")).as_deref(), Some("cinema"), "a frame's before the canvas's");
    c.format = Some("gone".into());
    assert_eq!(name(&c, None).as_deref(), Some("landscape"), "a canvas format with no preset is skipped");
    let (_, f) = p.format_for(&c, Some("shot")).unwrap();
    assert_eq!(f.aspect, "2.39:1");
    // A project default with no preset: nothing.
    let mut broken = p.clone();
    broken.format = "nope".into();
    assert_eq!(broken.format_for(&canvas(vec![], vec![]), None), None);
    // Frames whose parents loop end the walk.
    let looped = canvas(vec![framed("x", "y", frame(FrameRole::Shot, None)), framed("y", "x", frame(FrameRole::Scene, None))], vec![]);
    assert_eq!(name(&looped, Some("x")).as_deref(), Some("landscape"));
    let selfish = canvas(vec![framed("x", "x", frame(FrameRole::Shot, None))], vec![]);
    assert_eq!(name(&selfish, Some("x")).as_deref(), Some("landscape"));
}

#[test]
fn aspect_ratios() {
    let ratio = |aspect: &str| Format { aspect: aspect.into(), resolution: None, fps: None }.aspect_ratio();
    assert_eq!(ratio("16:9"), Some((16.0, 9.0)));
    assert_eq!(ratio("2.39:1"), Some((2.39, 1.0)));
    assert_eq!(ratio(" 4 : 5 "), Some((4.0, 5.0)));
    for bad in ["", "16x9", "16:", ":9", "0:1", "1:0", "-16:9", "a:b", "inf:1", "NaN:1", "16:9:1"] {
        assert_eq!(ratio(bad), None, "{bad:?}");
    }
}

#[test]
fn unknown_variants_read_as_unknown() {
    assert_eq!(serde_json::from_value::<StudioView>(json!("timeline")).unwrap(), StudioView::Unknown);
    assert_eq!(serde_json::from_value::<SectionKind>(json!("map")).unwrap(), SectionKind::Unknown);
    assert_eq!(serde_json::from_value::<StudioMedia>(json!("model3d")).unwrap(), StudioMedia::Unknown);
    assert_eq!(serde_json::from_value::<GenStatus>(json!("paused")).unwrap(), GenStatus::Unknown);
    assert_eq!(serde_json::from_value::<FrameRole>(json!("act")).unwrap(), FrameRole::Unknown);
    assert_eq!(serde_json::from_value::<EdgeRole>(json!("output")).unwrap(), EdgeRole::Unknown);
    assert_eq!(serde_json::from_value::<Arrange>(json!("circle")).unwrap(), Arrange::Unknown);
    assert_eq!(serde_json::from_value::<StudioChange>(json!("cut")).unwrap(), StudioChange::Unknown);
    assert_eq!(serde_json::from_value::<GenKind>(json!("model3d")).unwrap(), GenKind::Unknown);
    assert_eq!(serde_json::from_value::<SourceMedia>(json!("mesh")).unwrap(), SourceMedia::Unknown);
    // Tagged enums with fields of a newer peer.
    assert_eq!(serde_json::from_value::<NodeBody>(json!({"kind": "sticker", "emoji": "x"})).unwrap(), NodeBody::Unknown);
    assert_eq!(serde_json::from_value::<Op>(json!({"op": "group", "ids": ["a"]})).unwrap(), Op::Unknown);
    // A node of a newer kind inside a canvas, and an edge of a newer role.
    let c: Canvas = serde_json::from_value(json!({
        "name": "c", "title": "C",
        "nodes": [{"id": "n", "x": 1, "y": 2, "w": 3, "h": 4, "body": {"kind": "sticker"}}],
        "edges": [{"id": "e", "from": "n", "to": "n", "role": "variant"}],
    }))
    .unwrap();
    assert_eq!((c.nodes[0].body.clone(), c.edges[0].role), (NodeBody::Unknown, EdgeRole::Unknown));
    assert!(!c.edges[0].role.is_port());
}

#[test]
fn defaults_are_skipped_and_round_trip() {
    let draft: NodeBody = serde_json::from_value(json!({"kind": "gen"})).unwrap();
    let NodeBody::Gen { makes, status, ref options, ref takes, pick, .. } = draft else { panic!("a card") };
    assert_eq!((makes, status, options.is_null(), takes.len(), pick), (GenKind::Image, GenStatus::Draft, true, 0, None));
    assert_eq!(serde_json::to_value(&draft).unwrap(), json!({"kind": "gen", "makes": "image", "status": "draft"}));
    assert_eq!(serde_json::to_value(note("")).unwrap(), json!({"kind": "note", "text": ""}));
    assert_eq!(serde_json::from_value::<NodeBody>(json!({"kind": "note"})).unwrap(), note(""));
    assert_eq!(
        serde_json::from_value::<NodeBody>(json!({"kind": "frame"})).unwrap(),
        NodeBody::Frame { title: String::new(), role: FrameRole::Group, order: None, format: None }
    );
    let n = node("n_1", note("x"));
    assert_eq!(
        serde_json::to_value(&n).unwrap(),
        json!({"id": "n_1", "x": 0.0, "y": 0.0, "w": 10.0, "h": 10.0, "body": {"kind": "note", "text": "x"}})
    );
    let state = StudioState::new("/r".into());
    assert_eq!(serde_json::to_value(&state).unwrap(), json!({"root": "/r", "view": "canvas"}));
    assert_eq!(serde_json::from_value::<StudioState>(json!({"root": "/r"})).unwrap(), state);
    let edited = CanvasEdited { rev: 2, ids: vec![], moved: false, failed: vec![], mentions: vec![] };
    assert_eq!(serde_json::to_value(&edited).unwrap(), json!({"rev": 2, "ids": [], "moved": false}));
    // A hand-drawn wire has no `mention`; one a mention made says so.
    let hand = edge("e_1", "a", "b", EdgeRole::Reference);
    assert_eq!(serde_json::to_value(&hand).unwrap(), json!({"id": "e_1", "from": "a", "to": "b", "role": "reference"}));
    let made = Edge { mention: true, ..hand };
    assert_eq!(serde_json::to_value(&made).unwrap()["mention"], json!(true));
    assert_eq!(serde_json::from_value::<Edge>(serde_json::to_value(&made).unwrap()).unwrap(), made);

    // A full card survives a round trip.
    let full = NodeBody::Gen {
        makes: GenKind::Video,
        title: Some("Shot 1".into()),
        prompt: "She turns".into(),
        model: Some("kling-3.0-pro".into()),
        mode: Some("image-to-video".into()),
        options: json!({"duration": 5, "style": false}),
        status: GenStatus::Failed,
        progress: Some("40%".into()),
        job: Some("job-1".into()),
        error: Some("no key".into()),
        error_code: Some("no_provider".into()),
        takes: vec![Take {
            width: Some(1080),
            height: Some(1920),
            seconds: Some(5.0),
            model: Some("m".into()),
            prompt: Some("p".into()),
            ..take("t.mp4", StudioMedia::Video)
        }],
        pick: Some(0),
        inputs_used: vec!["k@1".into()],
        tool: Some("generate_media".into()),
        input: Some(json!({"model": "m"})),
    };
    let back: NodeBody = serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
    assert_eq!(back, full);

    // Ops: optional fields, `parent: null` and the renamed layout keys.
    let op: Op = serde_json::from_value(json!({"op": "update_node", "id": "n", "parent": null})).unwrap();
    assert!(matches!(op, Op::UpdateNode { parent: Some(None), body: None, x: None, .. }));
    let op: Op = serde_json::from_value(json!({"op": "update_node", "id": "n"})).unwrap();
    assert!(matches!(op, Op::UpdateNode { parent: None, .. }));
    assert_eq!(serde_json::to_value(&op).unwrap(), json!({"op": "update_node", "id": "n"}));
    let op: Op = serde_json::from_value(json!({"op": "layout", "ids": ["a"], "as": "grid", "in": "f"})).unwrap();
    assert_eq!(op, Op::Layout { ids: vec!["a".into()], arrange: Arrange::Grid, frame: Some("f".into()), gap: None });
    let op: Op = serde_json::from_value(json!({"op": "layout", "ids": ["a"]})).unwrap();
    assert!(matches!(op, Op::Layout { arrange: Arrange::Row, frame: None, .. }), "row by default");
    // tidy: frame optional, round-trips.
    let op: Op = serde_json::from_value(json!({"op": "tidy"})).unwrap();
    assert_eq!(op, Op::Tidy { frame: None });
    assert_eq!(serde_json::to_value(&op).unwrap(), json!({"op": "tidy"}));
    let op = Op::Tidy { frame: Some("s1".into()) };
    assert_eq!(serde_json::to_value(&op).unwrap(), json!({"op": "tidy", "frame": "s1"}));
    assert_eq!(serde_json::from_value::<Op>(serde_json::to_value(&op).unwrap()).unwrap(), op);
    let op: Op = serde_json::from_value(json!({"op": "add_node", "node": {"body": {"kind": "gen", "makes": "video", "prompt": "p"}}})).unwrap();
    let Op::AddNode { node } = op else { panic!() };
    assert_eq!((node.id, node.x, node.w), (None, None, None));
    let op: Op = serde_json::from_value(json!({"op": "add_edge", "edge": {"from": "a", "to": "b", "role": "first_frame"}})).unwrap();
    assert!(matches!(op, Op::AddEdge { edge: NewEdge { id: None, role: EdgeRole::FirstFrame, .. } }));

    // studio.set and canvas_update: absent keeps, null clears.
    let set: StudioSetParams = serde_json::from_value(json!({"block_id": "b", "active": null, "agent": null})).unwrap();
    assert_eq!((set.active, set.world_section, set.agent), (Some(None), None, Some(None)));
    let set: StudioSetParams = serde_json::from_value(json!({"block_id": "b", "active": "ep01"})).unwrap();
    assert_eq!(set.active, Some(Some("ep01".into())));
    let up: StudioCanvasUpdateParams = serde_json::from_value(json!({"block_id": "b", "name": "c", "episode": null})).unwrap();
    assert_eq!((&up.episode, &up.format), (&Some(None), &None));
    let roundtrip = serde_json::to_value(&up).unwrap();
    assert_eq!(roundtrip, json!({"block_id": "b", "name": "c", "episode": null}));

    // Entity frontmatter fields of any shape are kept as JSON.
    let e: Entity = serde_json::from_value(json!({
        "info": {"id": "characters/mara", "name": "Mara", "file": "world/characters/mara/mara.md"},
        "fields": {"name": "Mara", "rig": {"height": 170}, "voice": "elevenlabs/x"},
    }))
    .unwrap();
    assert_eq!(e.fields["rig"], json!({"height": 170}));
    assert!(e.body.is_empty() && e.used_in.is_empty());
}

#[test]
fn production_and_approval_on_the_wire() {
    assert_eq!(serde_json::to_value(StudioView::Production).unwrap(), json!("production"));
    assert_eq!(serde_json::from_value::<StudioView>(json!("production")).unwrap(), StudioView::Production);
    for (a, text) in [(Approval::None, "none"), (Approval::Approved, "approved"), (Approval::Changed, "changed")] {
        assert_eq!(serde_json::to_value(a).unwrap(), json!(text));
        assert_eq!(serde_json::from_value::<Approval>(json!(text)).unwrap(), a);
    }
    assert_eq!(serde_json::from_value::<Approval>(json!("locked")).unwrap(), Approval::Unknown);
    assert_eq!(serde_json::from_value::<StepKind>(json!("subtitles")).unwrap(), StepKind::Unknown);

    // EntityInfo: approval and voice skipped at their defaults, read when absent.
    let mut info = sections()[0].entities[0].clone();
    let bare = serde_json::to_value(&info).unwrap();
    assert!(bare.get("approval").is_none() && bare.get("voice").is_none(), "{bare}");
    assert_eq!(serde_json::from_value::<EntityInfo>(bare).unwrap(), info);
    (info.approval, info.voice) = (Approval::Changed, true);
    let v = serde_json::to_value(&info).unwrap();
    assert_eq!((&v["approval"], &v["voice"]), (&json!("changed"), &json!(true)));
    assert_eq!(serde_json::from_value::<EntityInfo>(v).unwrap(), info);

    // Project.models: omitted when empty.
    let mut p = presets();
    assert!(serde_json::to_value(&p).unwrap().get("models").is_none());
    p.models.insert("video".into(), "kling-3.0-pro".into());
    assert_eq!(serde_json::to_value(&p).unwrap()["models"], json!({"video": "kling-3.0-pro"}));

    let production = Production {
        assets: vec![AssetState {
            id: "characters/mara".into(),
            name: "Mara Venn".into(),
            section: "characters".into(),
            sheet: Some("world/characters/mara/sheet.png".into()),
            approval: Approval::Approved,
            voice: Some(false),
            video_sheet: Some(true),
            used: 2,
        }],
        episodes: vec![EpisodeState {
            canvas: "ep01-pilot".into(),
            title: "Pilot".into(),
            episode: Some(1),
            script: true,
            shots: 8,
            keyframes: 5,
            videos: 2,
            sound: 1,
            out_of_date: 1,
            failed: 0,
            running: 0,
            cut: Some(Cut {
                path: "canvases/ep01-pilot/out/ep01-pilot-v2.mp4".into(),
                version: 2,
                older: vec!["canvases/ep01-pilot/out/ep01-pilot-v1.mp4".into()],
                at: 7,
                stale: true,
            }),
        }],
        models: BTreeMap::from([("image".to_owned(), "soul-2".to_owned())]),
        next: vec![Step {
            kind: StepKind::Approve,
            text: "Review and approve Teo's sheet".into(),
            ask: None,
            entity: Some("characters/teo".into()),
            canvas: None,
        }],
    };
    let v = serde_json::to_value(&production).unwrap();
    assert_eq!(
        v["assets"][0],
        json!({"id": "characters/mara", "name": "Mara Venn", "section": "characters", "sheet": "world/characters/mara/sheet.png",
               "approval": "approved", "voice": false, "video_sheet": true, "used": 2})
    );
    assert_eq!(v["episodes"][0]["cut"]["stale"], json!(true));
    assert_eq!(v["next"][0], json!({"kind": "approve", "text": "Review and approve Teo's sheet", "entity": "characters/teo"}));
    assert_eq!(serde_json::from_value::<Production>(v).unwrap(), production);
    // A place: no voice, no video sheet; an empty production reads from {}.
    let place: AssetState = serde_json::from_value(json!({"id": "places/quay", "name": "Quay", "section": "places"})).unwrap();
    assert_eq!((place.approval, place.voice, place.video_sheet, place.used), (Approval::None, None, None, 0));
    assert_eq!(serde_json::from_value::<Production>(json!({})).unwrap(), Production::default());

    // The params.
    let approve: StudioEntityApproveParams = serde_json::from_value(json!({"block_id": "b", "id": "characters/mara", "approved": false})).unwrap();
    assert!(!approve.approved);
    let models: StudioSetModelsParams =
        serde_json::from_value(json!({"block_id": "b", "models": {"video": "kling-3.0-pro", "image": null}})).unwrap();
    assert_eq!(models.models, BTreeMap::from([("image".to_owned(), None), ("video".to_owned(), Some("kling-3.0-pro".to_owned()))]));
}

#[test]
fn named_references_links_and_their_methods_serialise() {
    let info = EntityInfo {
        id: "characters/dhritarashtra".into(),
        name: "Dhritarashtra".into(),
        file: "world/characters/dhritarashtra/dhritarashtra.md".into(),
        refs: vec!["world/characters/dhritarashtra/refs/crown-off.png".into(), "world/characters/dhritarashtra/refs/walk.mp4".into()],
        looks: vec![
            RefInfo {
                id: "crown-off".into(),
                path: "world/characters/dhritarashtra/refs/crown-off.png".into(),
                name: "Crown off".into(),
                defines: Some("his bare head and hair, without the crown".into()),
                media: StudioMedia::Image,
            },
            RefInfo {
                id: "walk".into(),
                path: "world/characters/dhritarashtra/refs/walk.mp4".into(),
                name: "walk".into(),
                defines: None,
                media: StudioMedia::Video,
            },
        ],
        links: vec!["props/kuru-crown".into()],
        ..EntityInfo::default()
    };
    let v = serde_json::to_value(&info).unwrap();
    assert_eq!(
        v["looks"],
        json!([
            {"id": "crown-off", "path": "world/characters/dhritarashtra/refs/crown-off.png", "name": "Crown off",
             "defines": "his bare head and hair, without the crown", "media": "image"},
            {"id": "walk", "path": "world/characters/dhritarashtra/refs/walk.mp4", "name": "walk", "media": "video"},
        ])
    );
    assert_eq!(v["links"], json!(["props/kuru-crown"]));
    assert_eq!(serde_json::from_value::<EntityInfo>(v).unwrap(), info);
    assert_eq!(info.look("walk").map(|l| l.media), Some(StudioMedia::Video));
    // An older server's listing reads without them; empty ones are left out.
    let old: EntityInfo = serde_json::from_value(json!({"id": "characters/teo", "name": "Teo", "file": "f.md"})).unwrap();
    assert!(old.looks.is_empty() && old.links.is_empty());
    assert!(serde_json::to_value(&old).unwrap().get("looks").is_none());

    // The ref changes, tagged by `op`, the reference as `ref`.
    let p: StudioEntityRefParams = serde_json::from_value(json!({"block_id": "b", "id": "characters/dhritarashtra",
        "change": {"op": "add", "path": "~/Downloads/crown.png", "name": "Crown off"}}))
    .unwrap();
    assert_eq!(p.change, RefChange::Add { path: "~/Downloads/crown.png".into(), name: Some("Crown off".into()), defines: None, at: None });
    for (change, wire) in [
        (RefChange::Rename { ref_id: "face".into(), name: Some("Face".into()) }, json!({"op": "rename", "ref": "face", "name": "Face"})),
        (RefChange::Rename { ref_id: "face".into(), name: None }, json!({"op": "rename", "ref": "face", "name": null})),
        (
            RefChange::Describe { ref_id: "face".into(), defines: Some("the face".into()) },
            json!({"op": "describe", "ref": "face", "defines": "the face"}),
        ),
        (RefChange::Move { ref_id: "face".into(), to: 0 }, json!({"op": "move", "ref": "face", "to": 0})),
        (RefChange::Remove { ref_id: "face".into() }, json!({"op": "remove", "ref": "face"})),
    ] {
        assert_eq!(serde_json::to_value(&change).unwrap(), wire);
        assert_eq!(serde_json::from_value::<RefChange>(wire).unwrap(), change);
    }
    assert_eq!(
        serde_json::from_value::<RefChange>(json!({"op": "rename", "ref": "face"})).unwrap(),
        RefChange::Rename { ref_id: "face".into(), name: None }
    );
    let done = EntityRefEdited { info: info.clone(), ref_id: Some("crown-off".into()) };
    let v = serde_json::to_value(&done).unwrap();
    assert_eq!(v["ref"], json!("crown-off"));
    assert_eq!(serde_json::from_value::<EntityRefEdited>(v).unwrap(), done);
    assert!(serde_json::to_value(EntityRefEdited { info, ref_id: None }).unwrap().get("ref").is_none());
    let link: StudioEntityLinkParams =
        serde_json::from_value(json!({"block_id": "b", "id": "characters/bhima", "other": "props/bhima-mace", "linked": false})).unwrap();
    assert!(!link.linked && link.other == "props/bhima-mace");
}

#[test]
fn reference_ids_are_slugs_made_unique() {
    assert_eq!(ref_slug("Crown off!"), "crown-off");
    assert_eq!(ref_slug("  Réverse  angle "), "r-verse-angle");
    assert_eq!(ref_slug("कृष्ण"), "");
    assert_eq!(file_stem("world/a/refs/crown-off.png"), "crown-off");
    assert_eq!(file_stem("refs/.hidden"), ".hidden");
    assert_eq!(file_stem("refs/a.b.png"), "a.b");
    // The file's stem, whatever the name (so a rename keeps it); the name only when the stem leaves no slug.
    let ids = ref_ids(&[
        (None, "refs/face.png"),
        (Some("Crown off"), "refs/crown-off.png"),
        (Some("Bare head"), "refs/Crown Off.jpg"),
        (Some("Crown off"), "refs/c1.png"),
        (Some("Flute"), "refs/बाँसुरी.png"),
        (Some("कृष्ण"), "refs/कृष्ण.png"),
        (Some(""), "refs/face.jpg"),
        (Some("Flute"), "refs/वेणु.png"),
    ]);
    assert_eq!(ids, ["face", "crown-off", "crown-off-2", "c1", "flute", "ref", "face-2", "flute-2"]);
    let renamed = ref_ids(&[(None, "refs/face.png"), (Some("Bare head"), "refs/crown-off.png")]);
    assert_eq!(renamed, ["face", "crown-off"], "a rename leaves the id alone");
}

#[test]
fn cards_are_sized_and_spaced_from_one_place() {
    use size::*;
    // Landscape, square and audio cards are CARD_W; tall ones narrower, never below TALL_MIN_W.
    assert_eq!(card_width(GenKind::Image, 16.0 / 9.0), 340.0);
    assert_eq!(card_width(GenKind::Video, 1.0), 340.0);
    assert_eq!(card_width(GenKind::Speech, 9.0 / 16.0), 340.0);
    assert_eq!(card_width(GenKind::Image, 9.0 / 16.0), 264.0);
    assert_eq!(card_width(GenKind::Image, 4.0 / 5.0), 304.0);
    assert_eq!(card_width(GenKind::Image, f64::NAN), 340.0, "no aspect: 16:9");
    // Ports on the left, the output on the right, and a gap between them.
    assert_eq!(PORT_GUTTER, 38.0);
    assert_eq!(OUTPUT_GUTTER, 37.0);
    assert_eq!(CARD_GAP, GAP + PORT_GUTTER + OUTPUT_GUTTER);
    assert_eq!(CARD_GAP, 115.0);
    // Height: header, face, the collapsed prompt (1 to 4 lines), footer.
    assert_eq!(card_height(GenKind::Image, 340.0, 16.0 / 9.0, 1), 32.0 + 191.25 + PROMPT_MIN + 36.0);
    assert_eq!(card_height(GenKind::Image, 264.0, 9.0 / 16.0, 20), 32.0 + 264.0 * 16.0 / 9.0 + 92.0 + 36.0);
    assert_eq!(card_height(GenKind::Music, 340.0, 1.0, 2), 32.0 + AUDIO_FACE + 56.0 + 36.0);
    assert_eq!((PROMPT_MAX, PROMPT_OPEN_MAX), (92.0, 236.0));
    assert_eq!(prompt_height(20, true), PROMPT_OPEN_MAX);
    assert_eq!(prompt_height(6, true), 6.0 * 18.0 + 20.0);
    assert_eq!(prompt_height(6, false), PROMPT_MAX);
    // Estimated lines: wrapped at the card's width, every line at least one.
    assert_eq!(estimate_lines("", 340.0), 1);
    assert_eq!(estimate_lines("a\nb", 340.0), 2);
    assert_eq!(estimate_lines(&"x".repeat(200), 340.0), 5);
    assert_eq!(parse_aspect("9:16"), Some(9.0 / 16.0));
    assert_eq!(parse_aspect("9:0"), None);
}

#[test]
fn documents_are_text_assets_inferred_from_their_path() {
    for (path, media) in [
        ("canvases/c/script.md", StudioMedia::Text),
        ("notes.MARKDOWN", StudioMedia::Text),
        ("a/b.txt", StudioMedia::Text),
        ("x.png", StudioMedia::Image),
        ("x.MOV", StudioMedia::Video),
        ("x.wav", StudioMedia::Audio),
        ("canvas.json", StudioMedia::Unknown),
        ("README", StudioMedia::Unknown),
    ] {
        assert_eq!(StudioMedia::of_path(path), media, "{path}");
    }
    assert_eq!(SourceMedia::from(StudioMedia::Text), SourceMedia::Text);
    // An asset without media reads (the server infers it).
    let body: NodeBody = serde_json::from_value(json!({"kind": "asset", "path": "canvases/c/script.md"})).unwrap();
    assert!(matches!(body, NodeBody::Asset { media: StudioMedia::Unknown, .. }));
    let text: NodeBody = serde_json::from_value(json!({"kind": "asset", "path": "s.md", "media": "text"})).unwrap();
    assert_eq!(serde_json::to_value(&text).unwrap()["media"], "text");
}

#[test]
fn documents_list_script_first_then_by_name() {
    let names = ["b.md", "canvas.json", "Notes.txt", ".hidden.md", "script.md", "a.markdown", "pic.png", "assets"].map(String::from);
    assert_eq!(documents_of("ep1", names), ["canvases/ep1/script.md", "canvases/ep1/a.markdown", "canvases/ep1/b.md", "canvases/ep1/Notes.txt"]);
    assert!(!is_document("x/y.md"));
}

#[test]
fn a_document_outputs_its_path_and_goes_out_of_date_with_its_file() {
    use EdgeRole as R;
    let doc = "canvases/c/script.md";
    let mut used = canvas(
        vec![node("s", asset(doc, StudioMedia::Text)), node("k", made(GenKind::Image, "K", &["canvases/c/assets/k.png"], None, &[]))],
        vec![edge("1", "s", "k", R::Text)],
    );
    assert_eq!(used.wire_check("s", "k", R::Reference).unwrap_err(), "the reference port takes an image; s outputs text");
    assert_eq!(used.document_of("s"), Some(doc));
    assert_eq!(used.document_of("k"), None);
    let out = used.output_of("s", &[]).unwrap();
    assert_eq!((out.media, out.path.as_deref(), out.text, out.label.as_str()), (SourceMedia::Text, Some(doc), None, "script.md"));
    let at = |m: u64| move |p: &str| (p == doc).then_some(m);
    let keys = used.input_keys("k", &[], at(5));
    assert_eq!(keys, [format!("{doc}@5")]);
    if let NodeBody::Gen { inputs_used, .. } = &mut used.nodes[1].body {
        inputs_used.clone_from(&keys);
    }
    assert!(used.out_of_date_why("k", &[], at(5)).is_empty());
    assert_eq!(used.out_of_date_why("k", &[], at(9)), ["script.md changed"]);
}

#[test]
fn mentions_resolve_to_entities_and_nodes_of_the_canvas() {
    use mention::Target;
    let c = canvas(
        vec![
            node("card", titled(GenKind::Image, "Key")),
            node("e_mara", entity("characters/mara")),
            node("e_mara2", entity("characters/mara")),
            node("doc", asset("canvases/c/script.md", StudioMedia::Text)),
            node("f", frame(FrameRole::Shot, None)),
            node("other", titled(GenKind::Video, "Shot 2")),
        ],
        vec![],
    );
    let s = sections();
    let mara = Target::Entity("characters/mara".into());
    assert_eq!(c.mention_label(&mara, "card", &s).as_deref(), Some("Mara Venn"));
    assert_eq!(c.mention_sources(&mara, "card", &s), ["e_mara", "e_mara2"]);
    assert!(c.mention_names(&mara, "e_mara2", &s) && !c.mention_names(&mara, "doc", &s));
    assert_eq!(c.mention_label(&Target::Entity("characters/nobody".into()), "card", &s), None, "no such entity: text");
    let teo = Target::Entity("characters/teo".into());
    assert_eq!(c.mention_label(&teo, "card", &s).as_deref(), Some("Teo"), "resolves without a node on the canvas");
    assert!(c.mention_sources(&teo, "card", &s).is_empty());
    assert_eq!(c.mention_label(&Target::Node("doc".into()), "card", &s).as_deref(), Some("script.md"));
    assert_eq!(c.mention_label(&Target::Node("other".into()), "card", &s).as_deref(), Some("Shot 2"));
    assert_eq!(c.mention_label(&Target::Node("f".into()), "card", &s), None, "a frame");
    assert_eq!(c.mention_label(&Target::Node("card".into()), "card", &s), None, "the card itself");
    assert_eq!(c.mention_label(&Target::Node("gone".into()), "card", &s), None);
    let mut c = c;
    if let NodeBody::Gen { prompt, .. } = &mut c.nodes[0].body {
        *prompt = "@characters/mara and @#doc, @characters/mara again, @#f, @characters/nobody, me@x.com".into();
    }
    assert_eq!(c.mentions_of("card", &s), [(mara, "Mara Venn".to_owned()), (Target::Node("doc".into()), "script.md".to_owned())]);
    assert!(c.mentions_of("doc", &s).is_empty(), "not a card");
}
