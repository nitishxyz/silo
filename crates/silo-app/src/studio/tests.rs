//! The Studio pane's view models (docs/STUDIO.md §4.1, §4.2, §4.3), as plain functions.

use serde_json::json;
use silo_proto::studio::{CanvasInfo, EntityInfo, Format, FrameRole, GenKind, GenStatus, NodeBody, Section, SectionKind, StudioMedia, Take};

use super::{
    board::tests::fixtures::{canvas, card, frame, image, in_frame, node, note, project},
    model::*,
    world::{EntityLayout, contain, entity_layout, entity_relative},
};

fn info(name: &str, title: &str, episode: Option<u32>) -> CanvasInfo {
    CanvasInfo { name: name.into(), title: title.into(), episode, format: None, nodes: 0, updated: 0, rev: 0 }
}

fn names(tabs: &[&CanvasInfo]) -> Vec<String> {
    tabs.iter().map(|c| c.name.clone()).collect()
}

/// Project order as the server lists it: a moodboard first, two episodes out of number order, an extra.
fn listed() -> silo_proto::studio::Project {
    let mut p = project();
    p.canvases = vec![info("moodboard", "Moodboard", None), info("ep02", "Undertow", Some(2)), info("ep01", "Pilot", Some(1)), info("bts", "", None)];
    p
}

#[test]
fn tabs_are_episodes_by_number_then_the_others_in_project_order() {
    let p = listed();
    let open: Vec<String> = ["bts", "ep02", "moodboard", "ep01", "gone"].map(str::to_owned).to_vec();
    let tabs = tab_order(&p, &open);
    assert_eq!(names(&tabs), ["ep01", "ep02", "moodboard", "bts"], "a name the project doesn't list is left out");
    let labels: Vec<String> = tabs.iter().map(|c| tab_label(c)).collect();
    assert_eq!(labels, ["Ep 1 Pilot", "Ep 2 Undertow", "Moodboard", "bts"], "no title: the folder's name");
    assert_eq!(tab_label(&info("ep03", "  ", Some(3))), "Ep 3 ep03");
    assert!(tab_order(&p, &[]).is_empty());
}

#[test]
fn closed_canvases_and_the_next_episode() {
    let p = listed();
    assert_eq!(names(&closed(&p, &["ep01".to_owned(), "bts".to_owned()])), ["moodboard", "ep02"], "in project order");
    assert!(closed(&p, &p.canvases.iter().map(|c| c.name.clone()).collect::<Vec<_>>()).is_empty());
    assert_eq!(next_episode(&p), 3);
    let mut gap = p.clone();
    gap.canvases.push(info("ep07", "Seven", Some(7)));
    assert_eq!(next_episode(&gap), 8, "after the highest");
    assert_eq!(next_episode(&project()), 1, "none yet");
}

#[test]
fn the_format_chip_reads_the_effective_format() {
    let mut p = project();
    p.formats.insert("scope".into(), Format { aspect: " 2.39:1 ".into(), resolution: None, fps: None });
    p.formats.insert("odd".into(), Format { aspect: "wide".into(), resolution: Some("4k".into()), fps: None });
    let plain = info("ep01", "Pilot", None);
    let (name, f, inherited) = canvas_format(&p, Some(&plain)).unwrap();
    assert_eq!((name.as_str(), f.aspect.as_str(), inherited), ("landscape", "16:9", true));
    assert_eq!(canvas_format(&p, None).map(|f| f.0), Some("landscape".into()), "no canvas: the project's");
    let own = CanvasInfo { format: Some("vertical".into()), ..plain.clone() };
    assert_eq!(canvas_format(&p, Some(&own)).map(|f| (f.0, f.2)), Some(("vertical".into(), false)));
    // A name no preset has: the project's.
    let unknown = CanvasInfo { format: Some("nope".into()), ..plain.clone() };
    assert_eq!(canvas_format(&p, Some(&unknown)).map(|f| (f.0, f.2)), Some(("landscape".into(), true)));
    assert_eq!(format_chip(&p, Some(&own)).as_deref(), Some("9:16"));
    assert_eq!(format_chip(&p, Some(&CanvasInfo { format: Some("scope".into()), ..plain.clone() })).as_deref(), Some("2.39:1"), "trimmed");
    assert_eq!(format_chip(&p, Some(&CanvasInfo { format: Some("odd".into()), ..plain.clone() })).as_deref(), Some("odd"), "unparsed: its name");
    let mut none = project();
    none.format = "missing".into();
    assert_eq!(format_chip(&none, Some(&plain)), None);
    assert_eq!(preset_label("portrait", &Format { aspect: "4:5".into(), resolution: Some("1080p".into()), fps: None }), "portrait · 4:5 · 1080p");
    assert_eq!(preset_label("scope", &p.formats["scope"]), "scope · 2.39:1");
    assert!((ratio(p.formats.get("vertical")) - 9.0 / 16.0).abs() < 1e-6);
    assert!((ratio(p.formats.get("odd")) - DEFAULT_ASPECT).abs() < 1e-6);
    assert!((ratio(None) - DEFAULT_ASPECT).abs() < 1e-6);
    assert!((aspect_of("1:1") - 1.0).abs() < 1e-6);
    assert_eq!(fit(2.0, 100.0), (100.0, 50.0));
    assert_eq!(fit(0.5, 100.0), (50.0, 100.0));
}

#[test]
fn frames_are_labelled_by_role_and_order() {
    assert_eq!(frame_label("Harbor at dawn", FrameRole::Scene, Some(1)), "Scene 1 · Harbor at dawn");
    assert_eq!(frame_label("  ", FrameRole::Shot, Some(3)), "Shot 3");
    assert_eq!(frame_label("Refs", FrameRole::Group, Some(2)), "Refs", "a group shows no order");
    assert_eq!(frame_label("", FrameRole::Group, None), "Frame");
    assert_eq!(frame_label("", FrameRole::Scene, None), "Scene");
    assert_eq!(frame_label("Night", FrameRole::Shot, None), "Night");
    // A title that numbers itself keeps its own number, once.
    assert_eq!(frame_label("1 \u{b7} The River", FrameRole::Shot, Some(1)), "Shot 1 \u{b7} The River");
    assert_eq!(
        frame_label("6 \u{b7} The Dice", FrameRole::Shot, Some(1)),
        "Shot 6 \u{b7} The Dice",
        "the title's number, not the order in its scene"
    );
    assert_eq!(frame_label("II. Build (0:22\u{2013}0:30)", FrameRole::Scene, Some(2)), "Scene II \u{b7} Build (0:22\u{2013}0:30)");
    assert_eq!(frame_label("I \u{b7} Origin & Theme", FrameRole::Scene, Some(1)), "Scene I \u{b7} Origin & Theme");
    assert_eq!(frame_label("3) Hair", FrameRole::Shot, None), "Shot 3 \u{b7} Hair");
    assert_eq!(frame_label("7 - The Hair", FrameRole::Shot, Some(2)), "Shot 7 \u{b7} The Hair");
    // Not numbers: a word that only looks like a numeral, a bare number as the whole title, a group.
    assert_eq!(frame_label("I am here", FrameRole::Shot, Some(4)), "Shot 4 \u{b7} I am here");
    assert_eq!(frame_label("C-section", FrameRole::Shot, Some(2)), "Shot 2 \u{b7} C-section");
    assert_eq!(frame_label("2049", FrameRole::Scene, Some(1)), "Scene 1 \u{b7} 2049");
    assert_eq!(frame_label("1 \u{b7} Refs", FrameRole::Group, Some(2)), "1 \u{b7} Refs");
}

fn framed(id: &str, x: f64, y: f64, role: FrameRole, order: Option<u32>) -> silo_proto::studio::Node {
    node(id, x, y, 1000.0, 800.0, NodeBody::Frame { title: String::new(), role, order, format: None })
}

#[test]
fn reading_order_is_frames_by_order_scenes_with_their_shots_then_unsorted() {
    let c = canvas(
        vec![
            note("loose2", 0.0, 5000.0, "b"),
            framed("s2", 0.0, 0.0, FrameRole::Scene, Some(2)),
            framed("s1", 5000.0, 5000.0, FrameRole::Scene, Some(1)),
            in_frame(framed("shot2", 5600.0, 5100.0, FrameRole::Shot, Some(2)), "s1"),
            in_frame(framed("shot1", 5100.0, 5100.0, FrameRole::Shot, Some(1)), "s1"),
            // Unordered frames: by position, top to bottom, then left to right.
            frame("g_low", 0.0, 9000.0, 100.0, 100.0),
            frame("g_high", 900.0, 8000.0, 100.0, 100.0),
            in_frame(image("k2", 5300.0, 5200.0), "shot1"),
            in_frame(image("k1", 5100.0, 5200.0), "shot1"),
            in_frame(note("beat", 10.0, 10.0, "x"), "s2"),
            in_frame(card("orphan", GenKind::Image, 1.0, 1.0), "no-such-frame"),
            note("loose1", 0.0, 4000.0, "a"),
        ],
        Vec::new(),
    );
    let groups = reading_order(&c);
    let shape: Vec<(Option<&str>, &str, usize, Vec<&str>)> =
        groups.iter().map(|g| (g.frame.as_deref(), g.title.as_str(), g.depth, g.nodes.iter().map(String::as_str).collect())).collect();
    assert_eq!(
        shape,
        [
            (Some("s1"), "Scene 1", 0, vec![]),
            (Some("shot1"), "Shot 1", 1, vec!["k1", "k2"]),
            (Some("shot2"), "Shot 2", 1, vec![]),
            (Some("s2"), "Scene 2", 0, vec!["beat"]),
            (Some("g_high"), "g_high", 0, vec![]),
            (Some("g_low"), "g_low", 0, vec![]),
            (None, UNSORTED, 0, vec!["orphan", "loose1", "loose2"]),
        ]
    );
    assert_eq!(walk(&c), ["k1", "k2", "beat", "orphan", "loose1", "loose2"]);
    // Nothing loose: no Unsorted group.
    let only = canvas(vec![framed("s", 0.0, 0.0, FrameRole::Scene, Some(1))], Vec::new());
    assert_eq!(reading_order(&only).len(), 1);
    assert!(walk(&canvas(Vec::new(), Vec::new())).is_empty());
}

#[test]
fn a_cycle_of_parents_still_reads_every_node_once() {
    let c = canvas(
        vec![
            in_frame(framed("a", 0.0, 0.0, FrameRole::Scene, Some(1)), "b"),
            in_frame(framed("b", 0.0, 1000.0, FrameRole::Shot, Some(1)), "a"),
            in_frame(framed("me", 0.0, 3000.0, FrameRole::Group, None), "me"),
            in_frame(image("in_a", 10.0, 10.0), "a"),
            in_frame(image("in_b", 10.0, 1010.0), "b"),
            in_frame(image("in_me", 10.0, 3010.0), "me"),
        ],
        Vec::new(),
    );
    let groups = reading_order(&c);
    let frames: Vec<(Option<&str>, usize)> = groups.iter().map(|g| (g.frame.as_deref(), g.depth)).collect();
    // Its own parent is no parent: a top frame. The two-frame cycle is read from its first frame.
    assert_eq!(frames, [(Some("me"), 0), (Some("a"), 0), (Some("b"), 1)]);
    let mut all = walk(&c);
    all.sort();
    assert_eq!(all, ["in_a", "in_b", "in_me"]);
}

#[test]
fn stepping_through_the_reading_order_stops_at_the_ends() {
    let order: Vec<String> = ["a", "b", "c"].map(str::to_owned).to_vec();
    assert_eq!(step(&order, "b", 1).map(String::as_str), Some("c"));
    assert_eq!(step(&order, "b", -1).map(String::as_str), Some("a"));
    assert_eq!(step(&order, "a", 2).map(String::as_str), Some("c"));
    assert_eq!(step(&order, "c", 1), None, "the last stays");
    assert_eq!(step(&order, "a", -1), None, "the first stays");
    assert_eq!(step(&order, "zz", 1), None);
    assert_eq!(step(&[], "a", 1), None);
}

fn take(model: Option<&str>) -> Take {
    Take {
        path: "t.png".into(),
        media: StudioMedia::Image,
        width: None,
        height: None,
        seconds: None,
        model: model.map(str::to_owned),
        prompt: None,
        at: 0,
    }
}

#[test]
fn the_take_stepper() {
    let three = vec![take(None), take(None), take(None)];
    assert_eq!(take_stepper(&three, None).as_deref(), Some("3/3"), "the last by default");
    assert_eq!(take_stepper(&three, Some(0)).as_deref(), Some("1/3"));
    assert_eq!(take_stepper(&three, Some(9)).as_deref(), Some("3/3"), "a pick past the end is the last");
    assert_eq!(take_stepper(&three[..1], None), None, "one take: no stepper");
    assert_eq!(take_stepper(&[], None), None);
    assert_eq!(stepped_pick(3, None, -1), Some(1));
    assert_eq!(stepped_pick(3, None, 1), None, "past the last");
    assert_eq!(stepped_pick(3, Some(0), -1), None, "before the first");
    assert_eq!(stepped_pick(3, Some(0), 1), Some(1));
    assert_eq!(stepped_pick(3, Some(7), -1), Some(1));
    assert_eq!(stepped_pick(0, None, 1), None);
}

#[test]
fn card_headers_and_faces() {
    assert_eq!(card_subtitle(Some("fal/kling-3.0"), None, false), "kling-3.0");
    assert_eq!(card_subtitle(Some("fal/kling-3.0"), Some("google/veo-3"), false), "veo-3", "the take's model");
    assert_eq!(card_subtitle(None, None, false), "Auto");
    assert_eq!(card_subtitle(Some("fal/kling-3.0"), Some("x/y"), true), "Out of date", "out of date wins");
    assert_eq!(short_model("fal/fal-ai/flux/dev"), "dev");
    assert_eq!(short_model("kling"), "kling");
    assert!(running(GenStatus::Queued) && running(GenStatus::Running));
    assert!(!running(GenStatus::Done) && !running(GenStatus::Draft) && !running(GenStatus::Failed));
    assert_eq!(progress_text(GenStatus::Running, Some("  40% ")), "40%");
    assert_eq!(progress_text(GenStatus::Running, Some(" ")), "Generating");
    assert_eq!(progress_text(GenStatus::Queued, None), "Queued");
    assert_eq!(progress_text(GenStatus::Running, None), "Generating");
    assert_eq!((seconds(8.4), seconds(65.0), seconds(-3.0), seconds(3725.0)), ("0:08".into(), "1:05".into(), "0:00".into(), "1:02:05".into()));
}

fn section(id: &str, title: &str, kind: SectionKind, n: usize) -> Section {
    let e = |i: usize| EntityInfo {
        id: format!("{id}/e{i}"),
        name: format!("E{i}"),
        file: String::new(),
        summary: None,
        sheet: None,
        refs: Vec::new(),
        looks: Vec::new(),
        links: Vec::new(),
        tags: Vec::new(),
        approval: Default::default(),
        voice: false,
    };
    Section { id: id.into(), title: title.into(), kind, entities: (0..n).map(e).collect(), refs: Vec::new() }
}

#[test]
fn the_world_switcher() {
    let sections = vec![
        section("characters", "Characters", SectionKind::Entities, 4),
        section("style", "Style", SectionKind::Style, 0),
        section("places", "Places", SectionKind::Entities, 0),
        section("overview", "Overview", SectionKind::Overview, 0),
    ];
    let ids: Vec<&str> = switcher(&sections).iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["overview", "style", "characters", "places"]);
    let labels: Vec<String> = switcher(&sections).into_iter().map(section_label).collect();
    assert_eq!(labels, ["Overview", "Style", "Characters·4", "Places·0"]);
    assert_eq!(shown_section(&sections, Some("places")).map(|s| s.id.as_str()), Some("places"));
    assert_eq!(shown_section(&sections, Some("gone")).map(|s| s.id.as_str()), Some("overview"), "else the first");
    assert_eq!(shown_section(&sections, None).map(|s| s.id.as_str()), Some("overview"));
    assert!(shown_section(&[], Some("x")).is_none());
}

#[test]
fn singulars_columns_and_fields() {
    assert_eq!(singular("Characters"), "character");
    assert_eq!(singular(" Places "), "place");
    assert_eq!(singular("Stories"), "story");
    assert_eq!(singular("Glass"), "glass");
    assert_eq!(singular("Lore"), "lore");
    assert_eq!(singular("s"), "s");
    assert_eq!(grid_columns(150.0, 150.0, 12.0), 1);
    assert_eq!(grid_columns(311.0, 150.0, 12.0), 1);
    assert_eq!(grid_columns(312.0, 150.0, 12.0), 2);
    assert_eq!(grid_columns(10.0, 150.0, 12.0), 1, "at least one");
    assert_eq!(field_text(&json!("salvage diver")), "salvage diver");
    assert_eq!(field_text(&json!(["a", 2, true])), "a, 2, true");
    assert_eq!(field_text(&json!(null)), "");
    assert_eq!(field_text(&json!(31)), "31");
    assert_eq!(field_text(&json!({"k": 1})), "{\"k\":1}");
}

#[test]
fn requests_to_the_agent_name_the_entity() {
    assert_eq!(create_with_agent("Characters", "  a tired lighthouse keeper "), "Create a new character for this world: a tired lighthouse keeper");
    assert_eq!(create_with_agent("Lore", "x"), "Create a new lore for this world: x");
    assert_eq!(node_target("ep01-pilot", "n_3f2a9c"), "ep01-pilot#n_3f2a9c");
    assert!(HIDDEN_FIELDS.contains(&"sheet"));
    assert_eq!(INIT_PRESETS.iter().map(|p| p.0).collect::<Vec<_>>(), ["landscape", "vertical", "square", "portrait"]);
}

#[test]
fn an_entity_page_offers_the_actions_of_its_kind() {
    assert_eq!(EntityKind::of_entity("characters/mara"), EntityKind::Character);
    assert_eq!(EntityKind::of_entity("places/harbor"), EntityKind::Place);
    assert_eq!(EntityKind::of_entity("props/bell"), EntityKind::Prop);
    assert_eq!(EntityKind::of_entity("factions/guild"), EntityKind::Other);
    assert_eq!(EntityKind::of_section("characters"), EntityKind::Character);
    assert_eq!(EntityKind::of_entity("characters"), EntityKind::Character, "a bare section id");
    let labels = |kind: EntityKind, sheet: bool| kind.asks().iter().map(|a| a.label(sheet)).collect::<Vec<_>>();
    assert_eq!(labels(EntityKind::Character, false), ["Generate sheet", "Generate video sheet", "Cast voice"]);
    assert_eq!(labels(EntityKind::Prop, false), ["Generate sheet", "Generate video sheet"]);
    assert_eq!(labels(EntityKind::Place, false), ["Generate sheet", "Generate angles"]);
    assert_eq!(labels(EntityKind::Other, false), ["Generate sheet"]);
    assert_eq!(labels(EntityKind::Character, true)[0], "Regenerate sheet", "it has one");
    assert_eq!(labels(EntityKind::Place, true), ["Regenerate sheet", "Generate angles"]);
    // Distinct button ids.
    let keys: Vec<_> = [EntityAsk::Sheet, EntityAsk::VideoSheet, EntityAsk::Voice, EntityAsk::Angles].map(EntityAsk::key).to_vec();
    assert_eq!(keys, ["sheet", "video-sheet", "voice", "angles"]);
}

#[test]
fn requests_to_the_agent_say_what_is_wanted_of_the_entity() {
    let mara = |a: EntityAsk| a.message(EntityKind::Character, "character", "Mara Venn", "characters/mara", None);
    assert_eq!(
        mara(EntityAsk::Sheet),
        "Generate a character sheet for Mara Venn (world entity characters/mara): three views in one image, and set it as its sheet."
    );
    assert_eq!(
        mara(EntityAsk::VideoSheet),
        "Generate a 360° video sheet for Mara Venn (world entity characters/mara) from its sheet, in which it says one line (its voice reference), and add it to its refs."
    );
    assert_eq!(
        mara(EntityAsk::Voice),
        "Cast a voice for Mara Venn (world entity characters/mara): offer two or three with a sample line, then set the one I pick."
    );
    let bell = |a: EntityAsk| a.message(EntityKind::Prop, "prop", "Old Bell", "props/bell", None);
    assert_eq!(
        bell(EntityAsk::Sheet),
        "Generate a prop sheet for Old Bell (world entity props/bell): two views in one image, and set it as its sheet."
    );
    assert_eq!(
        bell(EntityAsk::VideoSheet),
        "Generate a 360° video sheet for Old Bell (world entity props/bell) from its sheet and add it to its refs."
    );
    let harbor = |a: EntityAsk| a.message(EntityKind::Place, "place", "Harbor", "places/harbor", None);
    assert_eq!(harbor(EntityAsk::Sheet), "Generate the establishing image for Harbor (world entity places/harbor) and set it as its sheet.");
    assert!(
        harbor(EntityAsk::Angles)
            .starts_with("Generate angles of Harbor (world entity places/harbor): the reverse angle and the variants its scenes need")
    );
    // A custom section: its own word, never "character".
    let guild = EntityAsk::Sheet.message(EntityKind::Other, "faction", "The Guild", "factions/guild", None);
    assert_eq!(guild, "Generate a faction sheet for The Guild (world entity factions/guild) and set it as its sheet.");
    assert!(EntityAsk::Sheet.message(EntityKind::Other, " ", "X", "misc/x", None).starts_with("Generate a reference sheet for X"));
    for kind in [EntityKind::Character, EntityKind::Place, EntityKind::Prop, EntityKind::Other] {
        for ask in kind.asks() {
            assert!(ask.message(kind, "thing", "N", "s/n", None).contains("N (world entity s/n)"), "{ask:?} of {kind:?} names it");
        }
    }
}

#[test]
fn refs_are_drawn_by_their_file_and_offer_what_fits_it() {
    assert_eq!(RefMedia::of("world/characters/mara/refs/side.png"), RefMedia::Image);
    assert_eq!(RefMedia::of("world/characters/mara/refs/orbit.MP4"), RefMedia::Video);
    for v in ["a.mov", "a.m4v", "a.webm"] {
        assert_eq!(RefMedia::of(v), RefMedia::Video, "{v}");
    }
    for a in ["voice.mp3", "voice.wav", "voice.m4a"] {
        assert_eq!(RefMedia::of(a), RefMedia::Audio, "{a}");
    }
    assert_eq!(RefMedia::of("refs.d/noext"), RefMedia::Image, "a dot in a folder is no extension");
    use FileAction as A;
    // The middle one of three, an image on this Mac: everything.
    assert_eq!(
        ref_menu(RefMedia::Image, 1, 3, true),
        [A::Rename, A::Describe, A::SetSheet, A::MoveEarlier, A::MoveLater, A::RevealInFinder, A::OpenInApp, A::CopyMention, A::Remove]
    );
    // A remote host: a Files block; the first can't go earlier, the last not later, one alone neither.
    assert_eq!(
        ref_menu(RefMedia::Image, 0, 2, false),
        [A::Rename, A::Describe, A::SetSheet, A::MoveLater, A::OpenInFiles, A::CopyMention, A::Remove]
    );
    assert_eq!(ref_menu(RefMedia::Image, 1, 2, false)[3], A::MoveEarlier);
    // A video or audio can't be the sheet.
    assert_eq!(ref_menu(RefMedia::Video, 0, 1, true), [A::Rename, A::Describe, A::RevealInFinder, A::OpenInApp, A::CopyMention, A::Remove]);
    assert_eq!(ref_menu(RefMedia::Audio, 0, 1, false), [A::Rename, A::Describe, A::OpenInFiles, A::CopyMention, A::Remove]);
    assert_eq!(sheet_menu(true), [A::RevealInFinder, A::OpenInApp]);
    assert_eq!(sheet_menu(false), [A::OpenInFiles]);
    assert_eq!(
        [
            A::Rename,
            A::Describe,
            A::SetSheet,
            A::MoveEarlier,
            A::MoveLater,
            A::RevealInFinder,
            A::OpenInApp,
            A::OpenInFiles,
            A::CopyMention,
            A::Remove
        ]
        .map(A::label),
        [
            "Rename\u{2026}",
            "What it defines\u{2026}",
            "Set as sheet",
            "Move earlier",
            "Move later",
            "Reveal in Finder",
            "Open in default app",
            "Open in Files",
            "Copy mention",
            "Remove"
        ]
    );
    // Where a move goes: one place, never past either end.
    assert_eq!((moved_to(A::MoveEarlier, 2, 3), moved_to(A::MoveLater, 1, 3)), (Some(1), Some(2)));
    assert_eq!((moved_to(A::MoveEarlier, 0, 3), moved_to(A::MoveLater, 2, 3), moved_to(A::Rename, 1, 3)), (None, None, None));
}

fn look(id: &str, path: &str, name: &str, defines: Option<&str>, media: StudioMedia) -> silo_proto::studio::RefInfo {
    silo_proto::studio::RefInfo { id: id.into(), path: path.into(), name: name.into(), defines: defines.map(str::to_owned), media }
}

fn entity(id: &str, name: &str) -> EntityInfo {
    EntityInfo {
        id: id.into(),
        name: name.into(),
        file: format!("world/{id}/x.md"),
        summary: None,
        sheet: None,
        refs: Vec::new(),
        looks: Vec::new(),
        links: Vec::new(),
        tags: Vec::new(),
        approval: Default::default(),
        voice: false,
    }
}

#[test]
fn a_reference_tile_reads_its_name_and_what_it_defines() {
    let crown = look("crown-off", "world/characters/d/refs/crown-off.png", "Crown off", Some(" his bare head and hair "), StudioMedia::Image);
    assert_eq!(ref_caption(&crown), ("Crown off".to_owned(), Some("his bare head and hair".to_owned())));
    assert_eq!(ref_tip(&crown), "Takes only: his bare head and hair\nClick to view, right-click for options");
    // No name: its file's; nothing defined: no second line, and the tooltip says where to say it.
    let bare = look("side", "world/characters/d/refs/side.png", " ", Some("  "), StudioMedia::Image);
    assert_eq!(ref_caption(&bare), ("side".to_owned(), None));
    assert!(ref_tip(&bare).starts_with("Not said what it defines"));
    // Audio plays on its tile: no click to view.
    let voice = look("voice", "world/characters/d/refs/voice.mp3", "Voice", Some("his voice"), StudioMedia::Audio);
    assert_eq!(ref_tip(&voice), "Takes only: his voice\nRight-click for options");
    // Its media: the server's, else its file's; and the icon a chip or row draws.
    assert_eq!(RefMedia::of_look(&voice), RefMedia::Audio);
    assert_eq!(RefMedia::of_look(&look("o", "refs/orbit.mp4", "Orbit", None, StudioMedia::Unknown)), RefMedia::Video);
    assert_eq!([RefMedia::Image, RefMedia::Video, RefMedia::Audio].map(RefMedia::icon), ["image", "video", "audio-lines"]);
    assert_eq!([RefMedia::Image, RefMedia::Video, RefMedia::Audio].map(RefMedia::kind_label), ["Look", "Video", "Audio"]);
    for icon in [RefMedia::Image, RefMedia::Video, RefMedia::Audio].map(RefMedia::icon) {
        assert!(crate::assets::has_icon(icon), "{icon}");
    }
    assert!(SHEET_LABEL.contains("default look"));
    // An older server's listing (paths only): named by their files, ids as the server makes them.
    let mut old = entity("characters/d", "D");
    old.refs = vec!["world/characters/d/refs/side.png".into(), "world/characters/d/refs/side.jpg".into()];
    let looks = entity_looks(&old);
    assert_eq!(looks.iter().map(|l| (l.id.as_str(), l.name.as_str())).collect::<Vec<_>>(), [("side", "side"), ("side-2", "side")]);
    old.looks = vec![crown.clone()];
    assert_eq!(entity_looks(&old), [crown], "the server's names when it has them");
    assert!(HIDDEN_FIELDS.contains(&"links"), "links show as Goes with");
}

fn world() -> Vec<Section> {
    let mut bhima = entity("characters/bhima", "Bhima");
    bhima.links = vec!["props/mace".into()];
    let mut mace = entity("props/mace", "Mace");
    mace.links = vec!["characters/bhima".into()];
    let mut bow = entity("props/bow", "Bow");
    bow.links = vec!["characters/arjuna".into(), "characters/karna".into()];
    let mut chariot = entity("props/chariot", "Chariot");
    chariot.links = vec!["places/kurukshetra".into(), "characters/arjuna".into()];
    let sec = |id: &str, title: &str, entities: Vec<EntityInfo>| Section {
        id: id.into(),
        title: title.into(),
        kind: SectionKind::Entities,
        entities,
        refs: Vec::new(),
    };
    vec![
        sec("characters", "Characters", vec![bhima, entity("characters/arjuna", "Arjuna"), entity("characters/karna", "Karna")]),
        sec("places", "Places", vec![entity("places/kurukshetra", "Kurukshetra")]),
        sec("props", "Props", vec![mace, bow, chariot]),
        Section { id: "style".into(), title: "Style".into(), kind: SectionKind::Style, entities: Vec::new(), refs: Vec::new() },
    ]
}

#[test]
fn the_link_menu_lists_the_other_entities_props_first_for_a_character() {
    let w = world();
    let menu = |id: &str, linked: &[&str]| {
        let linked: Vec<String> = linked.iter().map(|s| (*s).to_owned()).collect();
        link_menu(&w, id, &linked)
            .into_iter()
            .map(|(s, _, es)| (s.to_owned(), es.iter().map(|e| e.id.clone()).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
    };
    let ids = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    // A character: props first, then the rest in the world's order; itself and what it goes with left out.
    assert_eq!(
        menu("characters/bhima", &["props/mace"]),
        [
            ("props".to_owned(), ids(&["props/bow", "props/chariot"])),
            ("characters".to_owned(), ids(&["characters/arjuna", "characters/karna"])),
            ("places".to_owned(), ids(&["places/kurukshetra"])),
        ]
    );
    // A prop: characters first.
    assert_eq!(menu("props/mace", &["characters/bhima"]).iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["characters", "places", "props"]);
    assert_eq!(menu("props/mace", &["characters/bhima"])[0].1, ids(&["characters/arjuna", "characters/karna"]));
    // A place: the world's order; a section with nothing left goes.
    assert_eq!(menu("places/kurukshetra", &[]).iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["characters", "props"]);
}

#[test]
fn a_prop_card_says_whose_it_is_when_it_goes_with_one_character() {
    let w = world();
    let of = |id: &str| owner(&w, find_entity(&w, id).unwrap()).map(|o| possessive(&entity_name(o)));
    assert_eq!(of("props/mace").as_deref(), Some("Bhima\u{2019}s"));
    assert_eq!(of("props/bow"), None, "two characters: quiet");
    assert_eq!(of("props/chariot").as_deref(), Some("Arjuna\u{2019}s"), "a place it goes with doesn't count");
    assert_eq!(of("characters/bhima"), None, "only props");
    // A link to a character the world doesn't list: quiet.
    let mut lone = entity("props/ring", "Ring");
    lone.links = vec!["characters/gone".into()];
    assert_eq!(owner(&w, &lone).map(|o| o.id.clone()), None);
    // In the Production view, under their owner.
    let by = props_by_owner(&w);
    assert_eq!(by.get("characters/bhima"), Some(&vec!["props/mace".to_owned()]));
    assert_eq!(by.get("characters/arjuna"), Some(&vec!["props/chariot".to_owned()]));
    assert_eq!(by.len(), 2);
}

#[test]
fn a_production_row_counts_the_references_beyond_the_sheet() {
    let mut d = entity("characters/d", "D");
    assert_eq!(looks_count(&d), None);
    d.sheet = Some("world/characters/d/sheet.png".into());
    d.looks = vec![
        look("sheet", "world/characters/d/sheet.png", "Sheet", None, StudioMedia::Image),
        look("crown-off", "world/characters/d/refs/crown-off.png", "Crown off", None, StudioMedia::Image),
    ];
    assert_eq!(looks_count(&d).as_deref(), Some("1 look"), "the sheet among them isn't counted");
    d.looks.push(look("orbit", "world/characters/d/refs/orbit.mp4", "Orbit", None, StudioMedia::Video));
    d.looks.push(look("voice", "world/characters/d/refs/voice.mp3", "Voice", None, StudioMedia::Audio));
    assert_eq!(looks_count(&d).as_deref(), Some("3 looks"));
}

#[test]
fn the_add_reference_ask_names_the_entity_and_asks_which_look() {
    let ask = EntityAsk::Look.message(EntityKind::Character, "character", "Dhritarashtra", "characters/dhritarashtra", Some("soul-2"));
    assert!(ask.starts_with("Add a look for Dhritarashtra (world entity characters/dhritarashtra): ask me which look I want"), "{ask}");
    assert!(ask.ends_with("Use soul-2."));
    assert_eq!(EntityAsk::Look.key(), "look");
    assert!(!EntityKind::Character.asks().contains(&EntityAsk::Look), "the Add reference tile's, not a page button");
}

#[test]
fn a_world_change_reads_again_the_canvases_showing_entities() {
    let mut c = canvas(vec![image("a", 0.0, 0.0)], Vec::new());
    assert!(!shows_entities(&c));
    c.nodes.push(node("e", 0.0, 0.0, 100.0, 100.0, NodeBody::Entity { id: "characters/mara".into() }));
    assert!(shows_entities(&c));
}

#[test]
fn entity_paths_are_written_relative_to_its_folder() {
    assert_eq!(entity_relative("characters/mara", "world/characters/mara/refs/side.png"), "refs/side.png");
    assert_eq!(entity_relative("characters/mara", "world/characters/mara/sheet.png"), "sheet.png");
    // Outside its folder: root-relative as given.
    assert_eq!(entity_relative("characters/mara", "canvases/ep01/assets/k.png"), "canvases/ep01/assets/k.png");
    assert_eq!(entity_relative("characters/mara", "world/characters/marab/x.png"), "world/characters/marab/x.png");
}

#[test]
fn an_entity_page_is_side_by_side_when_wide_and_its_sheet_is_whole() {
    // The reported block: about 1100 wide.
    let wide = entity_layout(1100.0, 830.0);
    assert!(wide.wide);
    assert_eq!(wide.left, (1100.0 - 32.0 - 20.0) / 2.0, "half the inner width");
    assert_eq!(entity_layout(2400.0, 900.0).left, 640.0, "within the column's widest");
    // A block beside an agent block in a small window (about 700 wide) is still side by side.
    assert!(entity_layout(705.0, 560.0).wide);
    let narrow = entity_layout(500.0, 500.0);
    assert_eq!(narrow, EntityLayout { wide: false, left: 468.0, sheet_max: (468.0, 250.0) }, "stacked, the full width");
    assert!(!entity_layout(591.0, 800.0).wide && entity_layout(592.0, 800.0).wide);
    assert_eq!(entity_layout(10.0, 10.0).sheet_max, (120.0, 200.0), "never a degenerate box");
    // A 16:9 turnaround is contained, not cropped: the box's width bounds it.
    let (w, h) = contain(Some((1920, 1080)), wide.sheet_max);
    assert_eq!(w, wide.left);
    assert!((w / h - 16.0 / 9.0).abs() < 1e-4 && h <= wide.sheet_max.1);
    // A tall one: the height does.
    assert_eq!(contain(Some((600, 1200)), (400.0, 300.0)), (150.0, 300.0));
    // Before it loads (and empty pixels): the placeholder's 4:3.
    assert_eq!(contain(None, (400.0, 400.0)), (400.0, 300.0));
    assert_eq!(contain(Some((0, 10)), (400.0, 400.0)), (400.0, 300.0));
}

// ---------------------------------------------------------------- the media viewer (`viewer.rs`)

mod viewer {
    use silo_proto::studio::{GenKind, NodeBody, StudioMedia, Take};

    use crate::{
        media_viewer::{Item, Kind},
        studio::{
            board::tests::fixtures::{asset, canvas, card, entity, image, note, sections, take, video, with_takes},
            viewer::{Layer, caption_from, document, entity_items, entity_viewer_kept, items, layers},
        },
    };

    fn scratch(path: &str) -> Option<std::path::PathBuf> {
        Some(std::path::PathBuf::from("/scratch").join(path))
    }

    fn ids(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn items_are_the_viewable_nodes_in_reading_order() {
        let made = Take {
            model: Some("fal/kling-3.0".into()),
            prompt: Some("Mara at the bell".into()),
            seconds: Some(5.0),
            ..take("canvases/ep01/takes/k2.mp4", StudioMedia::Video, 1280, 720)
        };
        let c = canvas(
            vec![
                video("clip", 0.0, 400.0),
                image("still", 0.0, 0.0),
                note("beat", 400.0, 0.0, "x"),
                asset("song", StudioMedia::Audio, 800.0, 0.0, 320.0, 80.0),
                asset("script", StudioMedia::Text, 1200.0, 0.0, 320.0, 400.0),
                entity("mara", 0.0, 800.0, "characters/mara"),
                entity("teo", 400.0, 800.0, "characters/teo"),
                card("draft", GenKind::Image, 800.0, 800.0),
                with_takes(card("shot", GenKind::Video, 1200.0, 800.0), vec![take("canvases/ep01/takes/k1.mp4", StudioMedia::Video, 640, 360), made]),
            ],
            Vec::new(),
        );
        let all = items(&c, &sections(), &scratch);
        // Notes, audio, documents, an entity with no sheet and a card with no take have nothing to view.
        assert_eq!(ids(&all), ["still", "clip", "mara", "shot"]);
        let kinds: Vec<Kind> = all.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, [Kind::Image, Kind::Video, Kind::Image, Kind::Video]);
        assert_eq!((all[0].name.as_str(), all[0].mime.as_str(), all[0].title.as_deref()), ("still.png", "image/png", None), "an asset: its file");
        assert_eq!(all[0].scratch, scratch("canvases/ep01/assets/still.png"));
        assert_eq!((all[2].name.as_str(), all[2].title.as_deref()), ("sheet.png", Some("Mara Venn")), "an entity: its sheet, by its name");
        // A card: its picked take (the last without a pick), captioned from it.
        let shot = &all[3];
        assert_eq!((shot.name.as_str(), shot.mime.as_str(), shot.title.as_deref()), ("k2.mp4", "video/mp4", Some("Video shot")));
        assert_eq!(
            (shot.dims, shot.ms, shot.credit.as_deref(), shot.prompt.as_deref()),
            (Some((1280, 720)), Some(5000), Some("fal/kling-3.0"), Some("Mara at the bell"))
        );
        assert_eq!(crate::media_viewer::image::caption(shot, all.len(), 3, None), "1280×720 · MP4 · 0:05 · fal/kling-3.0 · 4 of 4");
        assert!(document(&c, "script") && !document(&c, "still"), "a document goes to the panel");
    }

    #[test]
    fn a_card_shows_its_picked_take_and_a_new_take_is_new_media() {
        let first = take("canvases/ep01/takes/a.png", StudioMedia::Image, 1024, 1024);
        let second = Take { at: 2, ..take("canvases/ep01/takes/b.png", StudioMedia::Image, 512, 512) };
        let mut card = with_takes(card("k", GenKind::Image, 0.0, 0.0), vec![first.clone(), second.clone()]);
        let before = items(&canvas(vec![card.clone()], Vec::new()), &[], &scratch).remove(0);
        if let NodeBody::Gen { pick, .. } = &mut card.body {
            *pick = Some(0);
        }
        let picked = items(&canvas(vec![card], Vec::new()), &[], &scratch).remove(0);
        assert_eq!((before.id.as_str(), before.name.as_str(), before.dims), ("k", "b.png", Some((512, 512))));
        assert_eq!((picked.id.as_str(), picked.name.as_str(), picked.dims), ("k", "a.png", Some((1024, 1024))), "the pick, same item");
        assert_ne!(before.key, picked.key, "other media: the viewer starts over on it");
        // Made again in place: the same file, a new take.
        let mut item = picked.clone();
        caption_from(&mut item, &Take { at: 9, ..first });
        assert_ne!(item.key, picked.key);
        // A take without model or prompt says nothing of them.
        caption_from(&mut item, &Take { model: Some(" ".into()), prompt: Some(String::new()), ..second });
        assert_eq!((item.credit, item.prompt), (None, None));
    }

    #[test]
    fn nothing_draws_over_an_open_viewer() {
        assert_eq!(layers(Some(true)), [Layer::Viewer], "open: the viewer alone, over the whole body");
        assert!(!layers(None).contains(&Layer::Viewer));
        assert!([Layer::Overlays, Layer::Documents, Layer::DocumentPanel].iter().all(|l| layers(None).contains(l)));
        let closing = layers(Some(false));
        assert_eq!(closing.last(), Some(&Layer::Viewer), "fading out: still on top");
        assert!(closing.contains(&Layer::Overlays));
    }

    #[test]
    fn an_entity_page_views_its_sheet_then_its_refs() {
        let info = silo_proto::studio::EntityInfo {
            id: "characters/dhritarashtra".into(),
            name: "Dhritarashtra".into(),
            file: "world/characters/dhritarashtra/dhritarashtra.md".into(),
            summary: None,
            sheet: Some("world/characters/dhritarashtra/sheet.png".into()),
            refs: vec![
                "world/characters/dhritarashtra/refs/court.jpg".into(),
                "world/characters/dhritarashtra/sheet.png".into(),
                "world/characters/dhritarashtra/refs/walk.mp4".into(),
                "world/characters/dhritarashtra/refs/voice.mp3".into(),
                "world/characters/dhritarashtra/refs/court.jpg".into(),
            ],
            looks: Vec::new(),
            links: Vec::new(),
            tags: Vec::new(),
            approval: Default::default(),
            voice: false,
        };
        let all = entity_items(&info, &scratch);
        // The sheet first; a ref that is the sheet, or listed twice, once; audio (played on its tile) not at all.
        assert_eq!(
            ids(&all),
            [
                "world/characters/dhritarashtra/sheet.png",
                "world/characters/dhritarashtra/refs/court.jpg",
                "world/characters/dhritarashtra/refs/walk.mp4"
            ]
        );
        assert_eq!((all[0].name.as_str(), all[0].title.as_deref(), all[0].mime.as_str()), ("sheet.png", Some("Dhritarashtra"), "image/png"));
        assert_eq!((all[1].title.as_deref(), all[1].mime.as_str(), all[1].kind), (None, "image/jpeg", Kind::Image), "a ref: by its file");
        assert_eq!((all[2].kind, all[2].mime.as_str()), (Kind::Video, "video/mp4"));
        assert!(all.iter().all(|i| i.key == i.id && i.scratch == scratch(&i.id)));
        assert_eq!(
            crate::media_viewer::stepped(&all, &all[0].id, 1).map(|i| i.id.as_str()),
            Some(all[1].id.as_str()),
            "] steps from the sheet to the first ref"
        );
        let bare = silo_proto::studio::EntityInfo { sheet: None, refs: Vec::new(), ..info.clone() };
        assert!(entity_items(&bare, &scratch).is_empty(), "nothing to view");
        let refs_only = silo_proto::studio::EntityInfo { sheet: None, ..info };
        assert_eq!(entity_items(&refs_only, &scratch)[0].title, None);
    }

    #[test]
    fn the_world_viewer_closes_once_its_page_is_gone() {
        use silo_proto::studio::StudioView;
        let id = "characters/mara";
        assert!(entity_viewer_kept(true, StudioView::World, Some(id), id));
        assert!(!entity_viewer_kept(true, StudioView::World, Some("characters/teo"), id), "another entity");
        assert!(!entity_viewer_kept(true, StudioView::World, None, id), "back to the section");
        assert!(!entity_viewer_kept(true, StudioView::Canvas, Some(id), id), "a canvas");
        assert!(!entity_viewer_kept(false, StudioView::World, Some(id), id), "Setup or the project's error over it");
    }
}

// ---------------------------------------------------------------- the Production view (`production.rs`)

mod production {
    use serde_json::json;
    use silo_proto::studio::{Approval, AssetState, Cut, EntityInfo, EpisodeState, Production, Step, StepKind, StudioView};

    use crate::{
        media_viewer::Kind,
        studio::{
            model::{EntityAsk, EntityKind},
            production::*,
            store::Load,
            viewer::{cut_items, cut_version, cut_viewer_kept},
            world::entity_model_kinds,
        },
    };

    fn asset(id: &str, sheet: bool, approval: Approval, voice: Option<bool>, video_sheet: Option<bool>) -> AssetState {
        AssetState {
            id: id.into(),
            name: id.rsplit('/').next().unwrap().into(),
            section: id.split('/').next().unwrap().into(),
            sheet: sheet.then(|| format!("world/{id}/sheet.png")),
            approval,
            voice,
            video_sheet,
            used: 0,
        }
    }

    fn info(id: &str, sheet: bool, refs: &[&str], voice: bool) -> EntityInfo {
        EntityInfo {
            id: id.into(),
            name: "N".into(),
            file: format!("world/{id}/n.md"),
            summary: None,
            sheet: sheet.then(|| format!("world/{id}/sheet.png")),
            refs: refs.iter().map(|r| format!("world/{id}/{r}")).collect(),
            looks: Vec::new(),
            links: Vec::new(),
            tags: Vec::new(),
            approval: Approval::None,
            voice,
        }
    }

    /// Marks as `label` (done), `○label` (missing), `!label` (warn).
    fn read(marks: &[Mark]) -> Vec<String> {
        marks
            .iter()
            .map(|m| match m.state {
                MarkState::Done => m.label.clone(),
                MarkState::Missing => format!("\u{25cb}{}", m.label),
                MarkState::Warn => format!("!{}", m.label),
            })
            .collect()
    }

    #[test]
    fn marks_follow_the_kind_and_the_state() {
        let mara = asset("characters/mara", true, Approval::Approved, Some(true), Some(false));
        assert_eq!(read(&marks(&Readiness::of_asset(&mara))), ["Sheet", "Approved", "Voice", "\u{25cb}Video sheet"]);
        let mute = asset("characters/bell", true, Approval::Changed, None, Some(true));
        assert_eq!(read(&marks(&Readiness::of_asset(&mute))), ["Sheet", "!Changed", "Video sheet"], "no voice to cast: no Voice mark");
        let prop = asset("props/bell", false, Approval::None, None, Some(false));
        assert_eq!(read(&marks(&Readiness::of_asset(&prop))), ["\u{25cb}Sheet", "\u{25cb}Approved", "\u{25cb}Video sheet"]);
        let place = asset("places/quay", true, Approval::Unknown, None, None);
        assert_eq!(read(&marks(&Readiness::of_asset(&place))), ["Sheet", "\u{25cb}Approved"], "a place: sheet and approval only");
    }

    #[test]
    fn marks_from_the_world_listing_before_the_production_is_read() {
        let r = Readiness::of_info(&info("characters/mara", true, &["refs/turn.mp4"], false));
        assert_eq!(read(&marks(&r)), ["Sheet", "\u{25cb}Approved", "\u{25cb}Voice", "Video sheet"]);
        let r = Readiness::of_info(&info("props/bell", false, &["refs/a.png"], false));
        assert_eq!(read(&marks(&r)), ["\u{25cb}Sheet", "\u{25cb}Approved", "\u{25cb}Video sheet"]);
        let r = Readiness::of_info(&info("factions/guild", true, &[], true));
        assert_eq!(read(&marks(&r)), ["Sheet", "\u{25cb}Approved"]);
        // Both known: the production's row, with the newer approval of the info (an approval just answered).
        let production = Production { assets: vec![asset("characters/mara", true, Approval::None, None, Some(true))], ..Production::default() };
        let approved = EntityInfo { approval: Approval::Approved, ..info("characters/mara", true, &[], false) };
        let r = readiness(Some(&production), Some(&approved), "characters/mara").unwrap();
        assert_eq!(read(&marks(&r)), ["Sheet", "Approved", "Video sheet"], "the production knows she doesn't speak");
        assert!(readiness(Some(&production), None, "characters/teo").is_none());
    }

    fn episode(shots: u32, keyframes: u32, videos: u32) -> EpisodeState {
        EpisodeState {
            canvas: "ep01-pilot".into(),
            title: "Pilot".into(),
            episode: Some(1),
            script: true,
            shots,
            keyframes,
            videos,
            sound: 0,
            out_of_date: 0,
            failed: 0,
            running: 0,
            cut: None,
        }
    }

    #[test]
    fn an_episode_row_reads_its_stages_and_counts() {
        let e = episode(5, 3, 0);
        assert_eq!(episode_title(&e), "Ep 1 \u{b7} Pilot");
        assert_eq!(read(&stages(&e)), ["Script", "Shots 5", "\u{25cb}Keyframes 3/5", "\u{25cb}Video 0/5", "\u{25cb}Sound", "\u{25cb}Cut"]);
        assert!(issues(&e).is_empty());
        let cut = Cut { path: "canvases/ep01-pilot/out/ep01-pilot-v3.mp4".into(), version: 3, older: Vec::new(), at: 1, stale: false };
        let done = EpisodeState { sound: 4, cut: Some(cut.clone()), out_of_date: 2, failed: 1, running: 3, ..episode(5, 5, 5) };
        assert_eq!(read(&stages(&done)), ["Script", "Shots 5", "Keyframes 5/5", "Video 5/5", "Sound", "Cut v3"]);
        assert_eq!(issues(&done), ["2 out of date", "1 failed", "3 running"]);
        let stale = EpisodeState { cut: Some(Cut { stale: true, ..cut }), ..done };
        assert_eq!(read(&stages(&stale)).last().unwrap(), "!Cut v3 \u{b7} stale");
        let bare = EpisodeState { script: false, ..episode(0, 0, 0) };
        assert_eq!(read(&stages(&bare)), ["\u{25cb}Script", "\u{25cb}Shots", "\u{25cb}Keyframes", "\u{25cb}Video", "\u{25cb}Sound", "\u{25cb}Cut"]);
        let board = EpisodeState { canvas: "moodboard".into(), title: " ".into(), episode: None, sound: 2, ..bare };
        assert!(stages(&board).is_empty(), "a moodboard has no stages");
        assert_eq!(episode_title(&board), "moodboard");
    }

    fn step(kind: StepKind, ask: Option<&str>, entity: Option<&str>, canvas: Option<&str>) -> Step {
        Step { kind, text: "T".into(), ask: ask.map(Into::into), entity: entity.map(Into::into), canvas: canvas.map(Into::into) }
    }

    #[test]
    fn a_step_with_a_message_sends_it_and_one_without_opens_what_to_decide() {
        let sheet = step(StepKind::Sheet, Some("Generate a character sheet for Mara"), Some("characters/mara"), None);
        assert_eq!(step_action(&sheet), StepAction::Send("Generate a character sheet for Mara".into()));
        assert!(step_tip(&sheet).starts_with("Asks the agent: Generate"));
        let approve = step(StepKind::Approve, None, Some("characters/mara"), None);
        assert_eq!(step_action(&approve), StepAction::OpenEntity("characters/mara".into()));
        assert_eq!(
            step_action(&step(StepKind::Cut, Some(" "), None, Some("ep01"))),
            StepAction::OpenCanvas("ep01".into()),
            "a blank ask sends nothing"
        );
        assert_eq!(step_action(&step(StepKind::Unknown, None, None, None)), StepAction::None);
    }

    fn models() -> Vec<otto_proto::MediaModelInfo> {
        let m = |handle: &str, name: &str, connected: bool| {
            serde_json::from_value(json!({"handle": handle, "id": handle, "name": name, "kind": "image", "provider": "fal",
                "provider_name": "fal", "connected": connected}))
            .unwrap()
        };
        vec![m("soul-2", "Soul 2", true), m("gpt-image-2", "GPT Image 2", false), m("flux-pro", "Flux Pro", true)]
    }

    fn labels(rows: &[ModelRow]) -> Vec<(String, bool, bool)> {
        rows.iter().map(|r| (r.label.clone(), r.pick.is_some(), r.checked)).collect()
    }

    #[test]
    fn the_models_menu_is_automatic_then_the_connected_models() {
        let listed = Load::Ready(models());
        let rows = model_menu(Some(&listed), None, "Image");
        assert_eq!(
            labels(&rows),
            [("Automatic".into(), true, true), ("Soul 2 \u{b7} fal".into(), true, false), ("Flux Pro \u{b7} fal".into(), true, false)],
            "unconnected ones are left out"
        );
        assert_eq!(rows[1].pick, Some(Some("soul-2".into())));
        assert_eq!(rows[0].pick, Some(None), "Automatic clears the pick");
        let rows = model_menu(Some(&listed), Some("flux-pro"), "Image");
        assert!(rows.iter().find(|r| r.label.starts_with("Flux")).unwrap().checked && !rows[0].checked);
        // A pick otto doesn't list as connected stays visible, checked.
        let rows = model_menu(Some(&listed), Some("gpt-image-2"), "Image");
        assert_eq!(labels(&rows)[1], ("GPT Image 2 (not connected)".into(), true, true));
        // Notes: loading, failed, none connected.
        assert_eq!(labels(&model_menu(None, None, "Image")).last().unwrap(), &("Loading models\u{2026}".into(), false, false));
        assert_eq!(model_menu(Some(&Load::Failed("no otto".into())), None, "Video").last().unwrap().label, "Models unavailable: no otto");
        let none = Load::Ready(models().into_iter().filter(|m| !m.connected).collect());
        assert_eq!(model_menu(Some(&none), None, "Speech").last().unwrap().label, "No speech provider connected");
        // The chip: the model's name, its short handle before the list comes, or Automatic.
        assert_eq!(model_chip(Some("soul-2"), Some(&listed)), "Soul 2");
        assert_eq!(model_chip(Some("fal/kling-3.0"), None), "kling-3.0");
        assert_eq!(model_chip(None, Some(&listed)), "Automatic");
    }

    #[test]
    fn a_pick_writes_one_kind_and_automatic_removes_it() {
        let p = set_models_params("b1", "image", Some("soul-2".into()));
        assert_eq!(serde_json::to_value(&p).unwrap(), json!({"block_id": "b1", "models": {"image": "soul-2"}}));
        let p = set_models_params("b1", "speech", None);
        assert_eq!(serde_json::to_value(&p).unwrap(), json!({"block_id": "b1", "models": {"speech": null}}));
        assert_eq!(MODEL_KINDS.map(|(k, l, _)| (k, l)), [("image", "Image"), ("video", "Video"), ("speech", "Speech")]);
    }

    #[test]
    fn the_production_view_shows_when_asked_or_with_no_canvas_open() {
        assert!(shows_production(StudioView::Production, Some("ep01")));
        assert!(shows_production(StudioView::Canvas, None));
        assert!(!shows_production(StudioView::Canvas, Some("ep01")));
        assert!(!shows_production(StudioView::World, None) && !shows_production(StudioView::Setup, None));
    }

    #[test]
    fn approval_reads_and_toggles() {
        assert_eq!(approve_button(Approval::None), ("Approve", None, true));
        assert_eq!(approve_button(Approval::Approved), ("Approved \u{2713}", None, false), "a click withdraws");
        assert_eq!(approve_button(Approval::Changed), ("Approve again", Some("Changed since approved"), true));
        assert!(approve_button(Approval::Unknown).2);
    }

    #[test]
    fn the_cut_viewer_steps_from_the_newest_to_the_older_versions() {
        let cut = Cut {
            path: "canvases/ep01/out/ep01-v10.mp4".into(),
            version: 10,
            older: vec!["canvases/ep01/out/ep01-v2.mp4".into(), "canvases/ep01/out/odd.mp4".into()],
            at: 5,
            stale: true,
        };
        let scratch = |p: &str| Some(std::path::PathBuf::from("/s").join(p));
        let items = cut_items(&cut, "Ep 1 \u{b7} Pilot", &scratch);
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["canvases/ep01/out/ep01-v10.mp4", "canvases/ep01/out/ep01-v2.mp4", "canvases/ep01/out/odd.mp4"]);
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_deref().unwrap()).collect();
        assert_eq!(
            titles,
            ["Ep 1 \u{b7} Pilot \u{b7} v10 (stale: takes changed since)", "Ep 1 \u{b7} Pilot \u{b7} v2", "Ep 1 \u{b7} Pilot \u{b7} odd.mp4"]
        );
        assert!(items.iter().all(|i| i.kind == Kind::Video && i.mime == "video/mp4" && i.scratch == scratch(&i.id)));
        assert_eq!(items[0].name, "ep01-v10.mp4");
        assert_eq!(crate::media_viewer::stepped(&items, &items[0].id, 1).map(|i| i.id.as_str()), Some(ids[1]), "] goes to an older cut");
        assert_eq!((cut_version("a/ep-pilot-v3.mp4"), cut_version("a/b.mp4")), (Some(3), None));
        assert!(cut_viewer_kept(true, true) && !cut_viewer_kept(false, true) && !cut_viewer_kept(true, false));
    }

    #[test]
    fn a_generate_button_names_the_projects_pick() {
        let sheet = EntityAsk::Sheet.message(EntityKind::Character, "character", "Mara Venn", "characters/mara", Some("soul-2"));
        assert!(sheet.starts_with("Generate a character sheet for Mara Venn (world entity characters/mara)"));
        assert!(sheet.ends_with("set it as its sheet. Use soul-2."));
        let auto = EntityAsk::Sheet.message(EntityKind::Character, "character", "Mara Venn", "characters/mara", None);
        assert!(auto.ends_with("as its sheet."), "Automatic: the agent chooses");
        assert!(!EntityAsk::Voice.message(EntityKind::Character, "", "M", "characters/m", Some("x")).contains("Use x"), "a voice has no model pick");
        assert_eq!(
            [EntityAsk::Sheet, EntityAsk::Angles, EntityAsk::VideoSheet, EntityAsk::Voice].map(EntityAsk::model_kind),
            [Some("image"), Some("image"), Some("video"), None]
        );
        assert_eq!(entity_model_kinds(EntityKind::Character), ["image", "video"]);
        assert_eq!(entity_model_kinds(EntityKind::Place), ["image"], "sheet and angles share one chip");
        assert_eq!(entity_model_kinds(EntityKind::Other), ["image"]);
    }
}
