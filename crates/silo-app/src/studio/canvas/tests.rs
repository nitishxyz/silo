//! The canvas view's pure glue (docs/STUDIO.md §4.2, §6.2): the edit pump, keys, menus, the settings panel's
//! models and preview, the setup card's kind, reveals, the minimap's place, hover playback.

use std::time::{Duration, Instant};

use otto_proto::{CardFile, CardPreview, CardProblem, CompiledCard, MediaKind, MediaModeSchema, MediaModelInfo, MediaModelSchema, ModelFit};
use serde_json::json;
use silo_proto::studio::{CanvasEdited, GenKind, Op, OpFailed};

use super::{
    Pick,
    board::{
        Action, Board, Effect, Key, Menu, MenuEntry, MenuGroup, MenuTarget, Point, Size,
        settings::Choice,
        tests::fixtures::{self, canvas, card, image, note, sent},
    },
    board_key,
    media::keep_playing,
    overlays::{menu_items, minimap_frame},
    pump::Pump,
    reveal_wanted, reveal_when_placed,
    run::{failure_text, footer_button, no_provider_text, setup_kind},
    settings_panel::{choice_label, dim_unfit, media_kind, model_infos, settings_preview},
};
use crate::context_menu::Item;

fn edited(rev: u64) -> CanvasEdited {
    CanvasEdited { rev, ids: Vec::new(), moved: false, failed: Vec::new(), mentions: Vec::new() }
}

/// A board with two notes, sized and fitted.
fn two_notes() -> Board {
    fixtures::board(&canvas(vec![note("a", 0.0, 0.0, "one"), note("b", 400.0, 0.0, "two")], Vec::new()))
}

#[test]
fn edits_go_one_batch_at_a_time_in_order() {
    let mut b = two_notes();
    let mut pump = Pump::default();
    let (first, _) = sent(&b.set_note("a", "first"));
    let (second, _) = sent(&b.set_note("b", "second"));
    pump.push((first, vec![Op::RemoveNode { id: "x".into() }], 1));
    pump.push((second, Vec::new(), 1));
    let out = pump.next().unwrap();
    assert_eq!(out.0, first);
    assert!(pump.next().is_none(), "one out at a time");
    // A run asked meanwhile waits for both.
    pump.run("a".into());
    assert!(pump.ready_runs().is_empty());
    pump.answered(&mut b, first, Ok(edited(2)));
    assert!(pump.ready_runs().is_empty(), "the second batch still waits");
    assert_eq!(pump.next().map(|b| b.0), Some(second), "then the next, in order");
    assert!(pump.ready_runs().is_empty(), "and is out");
    pump.answered(&mut b, second, Ok(edited(3)));
    assert!(pump.next().is_none());
    assert_eq!(pump.ready_runs(), ["a"], "released once every edit before it is saved");
    assert!(pump.ready_runs().is_empty(), "once");
    // Nothing pending: a run goes at once.
    pump.run("b".into());
    assert_eq!(pump.ready_runs(), ["b"]);
}

#[test]
fn a_failed_batch_is_undone_and_said_and_the_next_goes() {
    let mut b = two_notes();
    let mut pump = Pump::default();
    let (batch, ops) = sent(&b.set_note("a", "changed"));
    pump.push((batch, ops, 1));
    let out = pump.next().unwrap();
    pump.run("a".into());
    let fx = pump.answered(&mut b, out.0, Err("Not connected".into()));
    assert!(fx.contains(&Effect::Notice("Not connected".into())), "{fx:?}");
    match &b.canvas().node("a").unwrap().body {
        silo_proto::studio::NodeBody::Note { text } => assert_eq!(text, "one", "rolled back"),
        other => panic!("{other:?}"),
    }
    assert_eq!(pump.ready_runs(), ["a"], "nothing left to wait for");
    // An answer that names ops that failed: reported too.
    let (batch, ops) = sent(&b.set_note("b", "x"));
    pump.push((batch, ops, 1));
    pump.next().unwrap();
    let why = OpFailed { op: 0, code: "not_found".into(), message: "no node b".into() };
    let fx = pump.answered(&mut b, batch, Ok(CanvasEdited { failed: vec![why], ..edited(2) }));
    assert!(fx.contains(&Effect::Notice("no node b".into())), "{fx:?}");
}

#[test]
fn keys_map_to_the_boards() {
    assert_eq!(board_key("delete", false), Some(Key::Delete));
    assert_eq!(board_key("backspace", false), Some(Key::Backspace));
    assert_eq!(board_key("escape", true), Some(Key::Escape));
    assert_eq!(board_key("enter", false), Some(Key::Enter));
    assert_eq!(board_key("space", false), Some(Key::Space));
    assert_eq!(
        [board_key("left", false), board_key("right", false), board_key("up", false), board_key("down", false)],
        [Some(Key::Left), Some(Key::Right), Some(Key::Up), Some(Key::Down)]
    );
    assert_eq!(board_key("f", false), Some(Key::Char('f')));
    assert_eq!(board_key("f", true), Some(Key::Char('F')), "shift: upper case");
    assert_eq!(board_key("/", true), Some(Key::Char('/')));
    assert_eq!(board_key("0", false), Some(Key::Char('0')));
    assert_eq!(board_key("é", false), Some(Key::Char('é')), "one character, not one byte");
    assert_eq!(board_key("f1", false), None, "longer names aren't the board's");
    assert_eq!(board_key("tab", false), None);
    assert_eq!(board_key("", false), None);
}

fn labels(items: &[Item<Pick>]) -> Vec<(String, bool)> {
    items
        .iter()
        .map(|i| match i {
            Item::Action { label, enabled, .. } => (label.to_string(), *enabled),
            Item::Separator => ("—".to_owned(), false),
        })
        .collect()
}

#[test]
fn board_menus_become_context_menus() {
    let m = Menu {
        at: Point::ZERO,
        canvas_at: Point::ZERO,
        target: MenuTarget::Node("k".into()),
        title: Some("Keyframe".into()),
        groups: vec![
            MenuGroup {
                title: None,
                entries: vec![
                    MenuEntry::new(Action::Run, "Run").unless(Some("Write a prompt first".into())),
                    MenuEntry::new(Action::Settings, "Settings"),
                ],
            },
            MenuGroup { title: Some("Make from this".into()), entries: vec![MenuEntry::new(Action::NewNote, "Note").key("N")] },
        ],
    };
    let items = menu_items(&m);
    assert_eq!(
        labels(&items),
        [
            ("Keyframe".into(), false),
            ("—".into(), false),
            ("Run — Write a prompt first".into(), false),
            ("Settings".into(), true),
            ("—".into(), false),
            ("Make from this".into(), false),
            ("Note".into(), true),
            ("—".into(), false),
        ]
    );
    let Item::Action { action, keys, .. } = &items[6] else { panic!() };
    assert_eq!((action, *keys), (&Pick::Board(Action::NewNote), "N"));
    let Item::Action { action, .. } = &items[0] else { panic!() };
    assert_eq!(action, &Pick::Nothing, "titles do nothing");
    // The menu tidies the trailing separator.
    let menu = crate::context_menu::Menu::new(gpui::Point::default(), items);
    assert!(!matches!(menu.items().last(), Some(Item::Separator)));
    // A real board menu: the empty canvas's.
    let mut b = two_notes();
    fixtures::right_click(&mut b, Point::new(900.0, 600.0));
    let rows = labels(&menu_items(b.menu().unwrap()));
    assert_eq!(rows[0], ("Generate here".into(), false));
    assert_eq!(rows[1], ("Image".into(), true));
}

fn model(handle: &str, name: &str, provider: &str, connected: bool) -> MediaModelInfo {
    MediaModelInfo {
        handle: handle.into(),
        id: handle.into(),
        name: name.into(),
        about: String::new(),
        kind: MediaKind::Video,
        provider: "p".into(),
        provider_name: provider.into(),
        connected,
        default: false,
        chosen: false,
        price: None,
        inputs: Vec::new(),
        takes: Vec::new(),
        modes: Vec::new(),
    }
}

fn schema(handle: &str, props: serde_json::Value) -> MediaModelSchema {
    MediaModelSchema {
        handle: handle.into(),
        name: handle.into(),
        kind: MediaKind::Video,
        provider: "p".into(),
        connected: true,
        modes: vec![MediaModeSchema {
            mode: "text-to-video".into(),
            endpoint: None,
            price: None,
            schema: json!({ "properties": props }),
            fields: Vec::new(),
        }],
    }
}

#[test]
fn models_come_from_otto_with_what_their_schema_offers() {
    let models = [model("kling-3.0", "Kling 3.0", "Higgsfield", true), model("veo-3", "Veo 3", "", false)];
    let kling = schema(
        "kling-3.0",
        json!({
            "aspect_ratio": { "enum": ["16:9", "9:16"] },
            "duration": { "enum": ["5s", "10", 7.5] },
            "resolution": { "enum": ["720p", "1080p"] },
        }),
    );
    let infos = model_infos(GenKind::Video, &models, |h| (h == "kling-3.0").then_some(&kling));
    assert_eq!(infos.len(), 2);
    let k = &infos[0];
    assert_eq!(
        (k.id.as_str(), k.label.as_str(), k.makes, k.available, k.requires.as_deref()),
        ("kling-3.0", "Kling 3.0 · Higgsfield", GenKind::Video, true, None)
    );
    assert_eq!(k.aspects, ["16:9", "9:16"]);
    assert_eq!(k.durations, [5.0, 10.0, 7.5]);
    assert_eq!(k.resolutions, ["720p", "1080p"]);
    assert_eq!(k.ports, GenKind::Video.ports());
    let v = &infos[1];
    assert_eq!((v.label.as_str(), v.available, v.requires.as_deref()), ("Veo 3", false, Some("")), "no schema: offers anything");
    assert!(v.aspects.is_empty() && v.durations.is_empty() && v.resolutions.is_empty());
    assert_eq!(
        [media_kind(GenKind::Video), media_kind(GenKind::Image), media_kind(GenKind::Speech), media_kind(GenKind::Music)],
        [MediaKind::Video, MediaKind::Image, MediaKind::Audio, MediaKind::Audio]
    );
}

#[test]
fn the_preview_reads_otto_compiled_card() {
    let compiled = CompiledCard {
        tool: "generate_video".into(),
        input: json!({ "prompt": "p", "aspect_ratio": "9:16", "resolution": "1080p", "duration": 5 }),
        model: "kling-3.0".into(),
        model_name: "Kling 3.0".into(),
        provider: "higgsfield".into(),
        mode: None,
        prompt: "Mara at the bell. Style: grainy 16mm".into(),
        files: vec![
            CardFile { port: "first_frame".into(), index: 1, path: "k.png".into(), label: "Keyframe".into(), field: "first_frame".into() },
            CardFile { port: "reference".into(), index: 2, path: "s.png".into(), label: "Mara Venn".into(), field: "references".into() },
        ],
        notes: Vec::new(),
    };
    let p = settings_preview(&CardPreview { compiled: Some(compiled.clone()), ..CardPreview::default() });
    assert_eq!(p.prompt, "Mara at the bell. Style: grainy 16mm");
    assert_eq!(p.model.as_deref(), Some("Kling 3.0"));
    assert_eq!(p.size.as_deref(), Some("9:16 · 1080p · 5"));
    assert_eq!(p.files.len(), 2);
    assert!(p.files[0].ends_with(" 1 · Keyframe") && p.files[1].ends_with(" 2 · Mara Venn"), "{:?}", p.files);
    assert_eq!(p.error, None);
    // No display name: the id; nothing sized: no size.
    let bare = CompiledCard { model_name: String::new(), input: json!({ "prompt": "p" }), files: Vec::new(), ..compiled };
    let p = settings_preview(&CardPreview { compiled: Some(bare), ..CardPreview::default() });
    assert_eq!((p.model.as_deref(), p.size), (Some("kling-3.0"), None));
    // Nothing compiled: what stops it.
    let problem = CardProblem { code: "input".into(), message: "Run Keyframe 3 first".into(), kind: None, provider: None };
    let p = settings_preview(&CardPreview { error: Some(problem), ..CardPreview::default() });
    assert_eq!((p.prompt.as_str(), p.model, p.error.as_deref()), ("", None, Some("Run Keyframe 3 first")));
}

fn choice(value: Option<&str>, enabled: bool) -> Choice {
    Choice { value: value.map(str::to_owned), label: value.unwrap_or("Automatic").to_owned(), enabled, reason: None, selected: false }
}

#[test]
fn models_that_cant_take_what_is_wired_are_dimmed_with_the_reason() {
    let mut choices = vec![choice(None, true), choice(Some("a"), true), choice(Some("b"), true), choice(Some("c"), true), choice(Some("d"), false)];
    choices[4].reason = Some("Connect fal".into());
    let fit = |m: &str, fits: bool, reason: Option<&str>| ModelFit { model: m.into(), mode: None, fits, reason: reason.map(str::to_owned) };
    let fits = [fit("a", true, None), fit("b", false, Some("Takes no first frame")), fit("c", false, None), fit("d", false, Some("x"))];
    dim_unfit(&mut choices, &fits);
    let got: Vec<(bool, Option<&str>)> = choices.iter().map(|c| (c.enabled, c.reason.as_deref())).collect();
    assert_eq!(
        got,
        [(true, None), (true, None), (false, Some("Takes no first frame")), (false, Some("Can't take what is wired")), (false, Some("Connect fal")),],
        "a choice already off keeps its own reason"
    );
    assert_eq!(choice_label(&choices[2]), "b — Takes no first frame");
    assert_eq!(choice_label(&choices[1]), "a");
}

/// A failed `makes` card with `error` and `error_code`.
fn failed(makes: GenKind, error: Option<&str>, code: Option<&str>) -> silo_proto::studio::NodeBody {
    let mut body = fixtures::gen_body(makes, "x");
    if let silo_proto::studio::NodeBody::Gen { status, error: e, error_code, .. } = &mut body {
        *status = silo_proto::studio::GenStatus::Failed;
        *e = error.map(str::to_owned);
        *error_code = code.map(str::to_owned);
    }
    body
}

#[test]
fn a_failed_card_says_why_on_its_face_and_offers_retry_or_to_connect_a_provider() {
    use super::board::layout::CardAction as A;
    let label = |body: &silo_proto::studio::NodeBody, refused: bool| {
        let (a, label, _) = footer_button(body, refused);
        (a, label)
    };
    let draft = fixtures::gen_body(GenKind::Image, "x");
    assert_eq!(label(&draft, false), (A::Run, "Run"));
    assert_eq!(label(&draft, true), (A::Retry, "Retry"), "a run the view saw refused");
    let mut running = draft.clone();
    if let silo_proto::studio::NodeBody::Gen { status, .. } = &mut running {
        *status = silo_proto::studio::GenStatus::Running;
    }
    assert_eq!(label(&running, true), (A::Stop, "Stop"));
    assert_eq!(failure_text(&running, Some("no")), None, "nothing while it runs");
    // no_provider: Connect provider, and the face says which kind is missing.
    let none = failed(GenKind::Video, Some("no provider for video (set one up)"), Some("no_provider"));
    assert_eq!(label(&none, false), (A::Connect, "Connect provider"));
    assert_eq!(failure_text(&none, None).as_deref(), Some("No video provider connected"));
    assert_eq!(no_provider_text(GenKind::Image), "No image provider connected");
    assert_eq!(no_provider_text(GenKind::Video), "No video provider connected");
    for makes in [GenKind::Speech, GenKind::Sound, GenKind::Music] {
        assert_eq!(no_provider_text(makes), "No audio provider connected");
    }
    // Any other failure: its message in full on the face, Retry.
    let long = "The model refused the prompt because it names a real person; rewrite it without the name and run it again.";
    let other = failed(GenKind::Image, Some(long), Some("model"));
    assert_eq!(label(&other, false), (A::Retry, "Retry"));
    assert_eq!(failure_text(&other, None).as_deref(), Some(long), "not truncated");
    assert_eq!(failure_text(&failed(GenKind::Image, None, None), None).as_deref(), Some("Failed"));
    assert_eq!(failure_text(&other, Some("offline")).as_deref(), Some("offline"), "the newest refusal first");
    assert_eq!(failure_text(&draft, None), None);
}

#[test]
fn no_provider_opens_the_setup_card_for_the_kind_needed() {
    assert_eq!(setup_kind(Some(&json!({ "kind": "video", "provider": null })), Some(GenKind::Image)), MediaKind::Video, "otto's kind first");
    assert_eq!(setup_kind(Some(&json!({ "kind": "audio" })), None), MediaKind::Audio);
    assert_eq!(setup_kind(Some(&json!({ "provider": "fal" })), Some(GenKind::Music)), MediaKind::Audio, "else the card's");
    assert_eq!(setup_kind(Some(&json!({ "kind": "nonsense" })), Some(GenKind::Video)), MediaKind::Video);
    assert_eq!(setup_kind(None, Some(GenKind::Speech)), MediaKind::Audio);
    assert_eq!(setup_kind(None, None), MediaKind::Image);
}

#[test]
fn a_reveal_waits_for_the_canvas_and_the_views_size() {
    let c = canvas(vec![image("far", 5000.0, 5000.0), note("near", 0.0, 0.0, "n")], Vec::new());
    // Not read yet: kept.
    let mut b = Board::new(1);
    let mut want = None;
    assert!(reveal_when_placed(&mut b, &mut want, "far").is_empty());
    assert_eq!(want.as_deref(), Some("far"));
    // Read, but the view isn't sized: still kept (the first fit would undo it).
    b.set_canvas(&c);
    assert!(reveal_wanted(&mut b, &mut want).is_empty());
    assert_eq!(want.as_deref(), Some("far"));
    // Sized: revealed and selected.
    b.set_view_size(Size::new(800.0, 600.0));
    let fx = reveal_wanted(&mut b, &mut want);
    assert!(fx.contains(&Effect::Redraw), "{fx:?}");
    assert_eq!(want, None);
    assert_eq!(b.selection(), ["far"]);
    let r = b.camera().rect_to_view(b.laid("far").unwrap().rect);
    assert!(r.x >= 0.0 && r.right() <= 800.0 && r.y >= 0.0 && r.bottom() <= 600.0, "in view: {r:?}");
    // A node the canvas doesn't have (yet): kept for the next read.
    assert!(reveal_when_placed(&mut b, &mut want, "later").is_empty());
    assert_eq!(want.as_deref(), Some("later"));
    let mut more = c.clone();
    more.rev = 2;
    more.nodes.push(card("later", GenKind::Image, 9000.0, 0.0));
    b.set_canvas(&more);
    assert!(!reveal_wanted(&mut b, &mut want).is_empty());
    assert_eq!((want, b.selection()), (None, &["later".to_owned()][..]));
    // Nothing wanted: nothing.
    assert!(reveal_wanted(&mut b, &mut None).is_empty());
}

#[test]
fn the_pane_keeps_a_reveal_for_its_canvas() {
    let mut reveal = Some(("ep01".to_owned(), "n_1".to_owned()));
    assert_eq!(super::super::take_reveal(&mut reveal, "ep02"), None, "another canvas's stays");
    assert!(reveal.is_some());
    assert_eq!(super::super::take_reveal(&mut reveal, "ep01").as_deref(), Some("n_1"));
    assert_eq!(super::super::take_reveal(&mut reveal, "ep01"), None, "taken once");
}

#[test]
fn the_minimap_shows_only_in_a_big_enough_board() {
    let r = minimap_frame(Size::new(1200.0, 800.0)).unwrap();
    assert_eq!((r.x, r.y, r.w, r.h), (1200.0 - 16.0 - 180.0, 800.0 - 16.0 - 120.0, 180.0, 120.0), "bottom right");
    assert!(minimap_frame(Size::new(520.0, 320.0)).is_some(), "at the thresholds");
    assert!(minimap_frame(Size::new(519.9, 800.0)).is_none());
    assert!(minimap_frame(Size::new(1200.0, 319.0)).is_none());
    assert!(minimap_frame(Size::default()).is_none());
}

fn of(p: &(String, Instant)) -> (&str, Instant) {
    (p.0.as_str(), p.1)
}

#[test]
fn hover_playback_stops_when_the_pointer_leaves_and_four_at_most_play() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut playing = vec![("a.mp4".to_owned(), at(0)), ("b.mp4".to_owned(), at(1))];
    // Over b: a stops.
    keep_playing(&mut playing, of, Some("b.mp4"), &["a.mp4", "b.mp4"]);
    assert_eq!(playing.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(), ["b.mp4"]);
    // Scrolled out of view while hovered: stops.
    keep_playing(&mut playing, of, Some("b.mp4"), &["a.mp4"]);
    assert!(playing.is_empty());
    // The pointer left every tile: all stop.
    let mut playing = vec![("a.mp4".to_owned(), at(0))];
    keep_playing(&mut playing, of, None, &["a.mp4"]);
    assert!(playing.is_empty());
    // The limit: the newest four.
    let mut playing: Vec<(String, Instant)> = (0..6).map(|i| ("same.mp4".to_owned(), at(i))).collect();
    keep_playing(&mut playing, of, Some("same.mp4"), &["same.mp4"]);
    assert_eq!(playing.iter().map(|p| p.1).collect::<Vec<_>>(), [at(5), at(4), at(3), at(2)]);
}
