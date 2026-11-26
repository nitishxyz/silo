//! What the prompt editors' `@` popup offers and the chips a prompt draws.

use otto_proto::MentionKind;
use silo_proto::studio::{Canvas, EdgeRole, GenKind, NodeBody, StudioMedia};

use super::{chips, choices, unwired_tip, wired};
use crate::agent::mention::{Choice, Face, headings};
use crate::studio::board::tests::fixtures::{canvas, card, edge, entity, frame, image, in_frame, node, note, video};

/// Card `c` (the one being edited) in frame `f` with `hero` (an image) and a titled video card `v`; a note, an
/// empty note, a document, a video file and Mara's Entity node loose.
fn fixture() -> Canvas {
    let mut v = card("v", GenKind::Video, 400.0, 0.0);
    if let NodeBody::Gen { title, .. } = &mut v.body {
        *title = Some("Harbor at dawn".into());
    }
    let doc = node(
        "doc",
        0.0,
        900.0,
        320.0,
        400.0,
        NodeBody::Asset { path: "docs/script.md".into(), media: StudioMedia::Text, width: None, height: None, seconds: None, prompt: None },
    );
    canvas(
        vec![
            frame("f", 0.0, 0.0, 1200.0, 800.0),
            in_frame(card("c", GenKind::Image, 40.0, 80.0), "f"),
            in_frame(image("hero", 400.0, 80.0), "f"),
            v,
            note("n", -400.0, 0.0, "# Beat one\nThe storm"),
            note("empty", -400.0, 300.0, "  "),
            doc,
            video("clip", -400.0, 600.0),
            entity("m", -800.0, 0.0, "characters/mara"),
        ],
        Vec::new(),
    )
}

fn sections() -> Vec<silo_proto::studio::Section> {
    crate::studio::board::tests::fixtures::sections()
}

/// Rows as `(target, name, detail)`.
fn rows(c: &Canvas, card: Option<&str>, query: &str) -> Vec<(String, String, String)> {
    choices(c, card, &sections(), query)
        .into_iter()
        .map(|r| match r {
            Choice::Studio { target, name, detail, .. } => (target, name, detail),
            other => panic!("not a studio row: {other:?}"),
        })
        .collect()
}

fn targets(c: &Canvas, card: Option<&str>, query: &str) -> Vec<String> {
    rows(c, card, query).into_iter().map(|r| r.0).collect()
}

#[test]
fn a_bare_at_lists_the_world_then_the_canvas_s_nodes_by_kind() {
    let c = fixture();
    let all = rows(&c, Some("c"), "");
    let row = |t: &str, n: &str, d: &str| (t.to_owned(), n.to_owned(), d.to_owned());
    assert_eq!(
        all,
        [
            row("characters/mara", "Mara Venn", "Character"),
            row("characters/teo", "Teo", "Character"),
            // Cards, notes, documents, other files; frames, the card itself, empty notes and Entity nodes not.
            row("#v", "Harbor at dawn", "Video card"),
            row("#n", "Beat one", "Note"),
            row("#doc", "script.md", "Document"),
            row("#hero", "hero.png", "Image \u{b7} f"),
            row("#clip", "clip.mp4", "Video"),
        ]
    );
    // Under headings, as the composer's popup.
    let list = choices(&c, Some("c"), &sections(), "");
    let heads: Vec<_> = headings(&list).into_iter().flatten().collect();
    assert_eq!(heads, ["World", "On the canvas"]);
    assert!(list.iter().all(|r| matches!(r, Choice::Studio { kind: MentionKind::Entity | MentionKind::Canvas, .. })));
}

#[test]
fn the_canvas_prompt_offers_every_card() {
    let c = fixture();
    let t = targets(&c, None, "");
    assert!(t.contains(&"#c".to_owned()) && t.contains(&"#v".to_owned()));
    // A card's prompt never offers the card.
    assert!(!targets(&c, Some("v"), "").contains(&"#v".to_owned()));
}

#[test]
fn a_typed_query_filters_each_group_best_first() {
    let c = fixture();
    assert_eq!(targets(&c, Some("c"), "mara"), ["characters/mara"]);
    assert_eq!(targets(&c, Some("c"), "teo"), ["characters/teo"]);
    // By the id too (`characters/…`), and nodes by their name.
    assert_eq!(targets(&c, Some("c"), "characters/t"), ["characters/teo"]);
    assert_eq!(targets(&c, Some("c"), "harbor"), ["#v"]);
    assert_eq!(targets(&c, Some("c"), "script"), ["#doc"]);
    // `#…`: nodes only, by name or id.
    assert_eq!(targets(&c, Some("c"), "#"), ["#v", "#n", "#doc", "#hero", "#clip"]);
    assert_eq!(targets(&c, Some("c"), "#cli"), ["#clip"]);
    assert!(targets(&c, Some("c"), "zzz").is_empty());
}

/// Chips as `(text, label, icon, quiet)`.
fn shown(text: &str, c: &Canvas, card: Option<&str>, settled: Option<&str>) -> Vec<(String, String, &'static str, bool)> {
    chips(text, c, card, &sections(), settled)
        .into_iter()
        .map(|chip| {
            let Face { label, icon, quiet } = chip.face.clone().expect("a studio chip has a face");
            assert_eq!(text[chip.range.clone()].strip_prefix('@'), Some(chip.target.as_str()), "the chip's text is its token");
            (text[chip.range].to_owned(), label, icon, quiet)
        })
        .collect()
}

#[test]
fn tokens_that_resolve_are_chips_reading_their_names() {
    let c = fixture();
    let text = "@characters/mara walks to @#hero, past @#v and @#n. @#doc (@#clip)";
    let got = shown(text, &c, Some("c"), None);
    let s = |t: &str, l: &str, i: &'static str| (t.to_owned(), l.to_owned(), i, false);
    assert_eq!(
        got,
        [
            s("@characters/mara", "Mara Venn", "earth"),
            s("@#hero", "hero.png", "image"),
            s("@#v", "Harbor at dawn", "video"),
            s("@#n", "Beat one", "sticky-note"),
            s("@#doc", "script.md", "file-text"),
            s("@#clip", "clip.mp4", "video"),
        ]
    );
    let kinds: Vec<MentionKind> = chips(text, &c, Some("c"), &sections(), None).into_iter().map(|c| c.kind).collect();
    assert_eq!(kinds[..2], [MentionKind::Entity, MentionKind::Canvas]);
}

#[test]
fn tokens_that_do_not_resolve_stay_text() {
    let c = fixture();
    // No such entity or node, a frame, the card itself, an e-mail.
    let text = "@characters/nobody @#gone @#f @#c mara@harbor.com @characters/mara";
    assert_eq!(shown(text, &c, Some("c"), None).into_iter().map(|s| s.0).collect::<Vec<_>>(), ["@characters/mara"]);
    // The card is a fine mention from elsewhere.
    assert_eq!(shown("@#c", &c, Some("v"), None).len(), 1);
}

#[test]
fn a_saved_mention_with_no_wire_into_the_card_is_quiet() {
    let mut c = fixture();
    let text = "@characters/mara and @#hero and @#clip";
    let quiet = |c: &Canvas, settled: Option<&str>| shown(text, c, Some("c"), settled).into_iter().map(|s| s.3).collect::<Vec<_>>();
    // Not saved yet (being typed), or edits still in flight: none is.
    assert_eq!(quiet(&c, None), [false, false, false]);
    // Saved, nothing wired: all are (the video waits for a hand-drawn wire).
    assert_eq!(quiet(&c, Some(text)), [true, true, true]);
    // Wired: Mara through her Entity node, the image by a wire drawn by hand (any port counts).
    c.edges.push(edge("e1", "m", "c", EdgeRole::Reference));
    c.edges.push(edge("e2", "hero", "c", EdgeRole::FirstFrame));
    assert_eq!(quiet(&c, Some(text)), [false, false, true]);
    assert!(wired(&c, "c", &silo_proto::studio::mention::Target::Entity("characters/mara".into()), &[]));
    // Only the saved tokens: one typed since isn't quiet yet.
    assert_eq!(quiet(&c, Some("@#hero")), [false, false, false]);
    assert_eq!(unwired_tip("clip.mp4"), "clip.mp4 is mentioned but not wired into this card");
    // The canvas prompt has no card: never quiet.
    assert!(shown(text, &c, None, Some(text)).iter().all(|s| !s.3));
}

/// The fixture's world with Mara's named references: an image, a video, audio (and Teo's one).
fn with_looks() -> Vec<silo_proto::studio::Section> {
    let look = |id: &str, file: &str, name: &str, media| silo_proto::studio::RefInfo {
        id: id.into(),
        path: format!("world/characters/mara/refs/{file}"),
        name: name.into(),
        defines: None,
        media,
    };
    let mut s = sections();
    s[0].entities[0].looks = vec![
        look("crown-off", "crown-off.png", "Crown off", StudioMedia::Image),
        look("orbit", "orbit.mp4", "Orbit", StudioMedia::Video),
        look("voice", "voice.mp3", "Voice", StudioMedia::Audio),
    ];
    s[0].entities[1].looks = vec![silo_proto::studio::RefInfo {
        id: "coat".into(),
        path: "world/characters/teo/refs/coat.png".into(),
        name: "Coat".into(),
        defines: Some("the coat alone".into()),
        media: StudioMedia::Image,
    }];
    s
}

fn look_rows(query: &str) -> Vec<(String, String, String)> {
    choices(&fixture(), Some("c"), &with_looks(), query)
        .into_iter()
        .map(|r| match r {
            Choice::Studio { target, name, detail, .. } => (target, name, detail),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn hash_after_an_entity_lists_its_references() {
    let row = |t: &str, n: &str, d: &str| (t.to_owned(), n.to_owned(), d.to_owned());
    // All of them, in order, by name with their kind and entity.
    assert_eq!(
        look_rows("characters/mara#"),
        [
            row("characters/mara#crown-off", "Crown off", "Look \u{b7} Mara Venn"),
            row("characters/mara#orbit", "Orbit", "Video \u{b7} Mara Venn"),
            row("characters/mara#voice", "Voice", "Audio \u{b7} Mara Venn"),
        ]
    );
    // Filtered by name or id.
    assert_eq!(look_rows("characters/mara#orb").into_iter().map(|r| r.0).collect::<Vec<_>>(), ["characters/mara#orbit"]);
    assert_eq!(look_rows("characters/mara#crown-").into_iter().map(|r| r.0).collect::<Vec<_>>(), ["characters/mara#crown-off"]);
    assert!(look_rows("characters/mara#zzz").is_empty());
    // An entity the world doesn't have: not a drill-in.
    assert!(look_rows("characters/nobody#").iter().all(|r| !r.0.contains('#')));
    // Picking one makes the reference's token; each row's icon is its media's.
    let picked = choices(&fixture(), Some("c"), &with_looks(), "characters/mara#");
    assert_eq!(picked[1].mention(), Some((MentionKind::Entity, "characters/mara#orbit".to_owned())));
    let icons: Vec<&str> = picked
        .iter()
        .map(|c| match c {
            Choice::Studio { kind, target, detail, .. } => crate::agent::mention::row_icon(*kind, target, detail),
            _ => "",
        })
        .collect();
    assert_eq!(icons, ["image", "video", "audio-lines"]);
}

#[test]
fn a_query_finds_references_by_name_under_their_entity() {
    let row = |t: &str, n: &str, d: &str| (t.to_owned(), n.to_owned(), d.to_owned());
    // `crown`: no entity by that name; Mara's reference, named whole.
    assert_eq!(look_rows("crown"), [row("characters/mara#crown-off", "Mara Venn \u{b7} Crown off", "Look")]);
    // `teo`: Teo; his coat isn't found by his name.
    assert_eq!(look_rows("teo").into_iter().map(|r| r.0).collect::<Vec<_>>(), ["characters/teo"]);
    // `coat`: under Teo when he is listed too, else alone; a scattered match (`c…o…a…t`) isn't one.
    assert_eq!(look_rows("coat").into_iter().map(|r| r.0).collect::<Vec<_>>(), ["characters/teo#coat"]);
    let mut s = with_looks();
    s[0].entities[1].name = "Coat Teo".into();
    let got: Vec<String> = choices(&fixture(), Some("c"), &s, "coat").into_iter().filter_map(|c| c.mention()).map(|m| m.1).collect();
    assert_eq!(got, ["characters/teo", "characters/teo#coat"]);
    // A bare `@` lists no references.
    assert!(look_rows("").iter().all(|r| !r.0.contains('#') || r.0.starts_with('#')));
}

#[test]
fn a_reference_token_is_a_chip_reading_the_entity_then_the_reference() {
    let c = fixture();
    let text = "@characters/mara#crown-off with @characters/mara#orbit and @characters/mara#voice, not @characters/mara#gone";
    let got: Vec<(String, String, &'static str)> = chips(text, &c, Some("c"), &with_looks(), None)
        .into_iter()
        .map(|chip| {
            let face = chip.face.clone().unwrap();
            assert_eq!(chip.kind, MentionKind::Entity);
            (chip.target, face.label, face.icon)
        })
        .collect();
    assert_eq!(
        got,
        [
            ("characters/mara#crown-off".to_owned(), "Mara Venn \u{b7} Crown off".to_owned(), "image"),
            ("characters/mara#orbit".to_owned(), "Mara Venn \u{b7} Orbit".to_owned(), "video"),
            ("characters/mara#voice".to_owned(), "Mara Venn \u{b7} Voice".to_owned(), "audio-lines"),
        ],
        "an unknown reference stays text"
    );
    // Saved with no wire from its file: quiet, with the tooltip of any mention.
    let quiet = chips("@characters/mara#crown-off", &c, Some("c"), &with_looks(), Some("@characters/mara#crown-off"));
    assert!(quiet[0].face.as_ref().unwrap().quiet);
    assert_eq!(unwired_tip("Mara Venn \u{b7} Crown off"), "Mara Venn \u{b7} Crown off is mentioned but not wired into this card");
}

#[test]
fn hash_typed_after_an_entity_chip_reopens_it_as_a_query() {
    let c = fixture();
    // `#` alone after the entity: its token, the `#` text; with the caret after the `#` the chip is left out and
    // the `@…` is the query.
    let text = "see @characters/mara# now";
    let found = chips(text, &c, Some("c"), &with_looks(), None);
    assert_eq!(found.len(), 1);
    let caret = "see @characters/mara#".len();
    let open = super::drilled(text, caret, found.clone());
    assert!(open.is_empty());
    assert_eq!(crate::agent::mention::query(text, caret, &open).map(|q| q.1), Some("characters/mara#"));
    // The caret elsewhere, or a space before the `#`: the chip stays.
    assert_eq!(super::drilled(text, text.len(), found.clone()).len(), 1);
    assert_eq!(super::drilled(text, 4, found).len(), 1);
    let spaced = "@characters/mara #x";
    assert_eq!(super::drilled(spaced, spaced.len(), chips(spaced, &c, Some("c"), &with_looks(), None)).len(), 1);
    // Typing on (`#cr`, no such reference yet): no chip at all, the query goes on.
    let typing = "see @characters/mara#cr";
    assert!(chips(typing, &c, Some("c"), &with_looks(), None).is_empty());
    assert_eq!(crate::agent::mention::query(typing, typing.len(), &[]).map(|q| q.1), Some("characters/mara#cr"));
}
