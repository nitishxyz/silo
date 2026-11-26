//! The studio disk helpers shared by card runs, mentions and `studio.mentions` (docs/STUDIO.md §1, §6.2, §6.3).

use otto_tools::studio::{self, Candidate};
use std::path::Path;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A studio: Mara (frontmatter, a sheet, refs from `refs/`), Harbor (listed refs, no sheet), a custom section,
/// a style line, `[models]` picks, an episode canvas and a moodboard.
pub fn studio() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "studio.toml",
        "name = \"Harbor Lights\"\nformat = \"landscape\"\n\n[models]\nvideo = \"kling-3.0-pro\" # the user's\nimage = 'gpt-image-2.6'\n\n[formats.landscape]\naspect = \"16:9\"\n",
    );
    write(
        root,
        "world/characters/mara/mara.md",
        "---\nname: Mara Venn\nsheet: sheet.png\ntags: [lead, crew]\nsummary: Salvage diver, scar across the left brow.\n---\nBackstory.\n",
    );
    write(root, "world/characters/mara/sheet.png", "png");
    write(root, "world/characters/mara/refs/profile.png", "png");
    write(root, "world/characters/mara/refs/face.jpg", "jpg");
    write(root, "world/characters/mara/refs/notes.txt", "not an image");
    write(root, "world/places/harbor/about.md", "---\nname: \"The Harbor\"\nrefs:\n  - refs/dawn.png\n  - ./refs/night.png\n---\n");
    write(root, "world/factions/salvage-guild/salvage-guild.md", "No frontmatter at all.\n");
    write(root, "world/style.md", "# Style\n\nPalette notes.\n\n## Style line\n\nMuted teal and amber,\n35mm film grain.\n\n## Lenses\n\nWide.\n");
    write(
        root,
        "canvases/ep01-pilot/canvas.json",
        r#"{"version":1,"title":"Pilot","episode":1,"nodes":[{"id":"n_1","x":0,"y":0,"w":1,"h":1,"body":{"kind":"note","text":"hi"}}],"edges":[]}"#,
    );
    write(root, "canvases/moodboard/canvas.json", r#"{"version":1,"title":"Moodboard","nodes":[],"edges":[]}"#);
    write(root, "canvases/broken/canvas.json", "not json");
    dir
}

#[test]
fn roots_entities_style_and_picks_are_read_from_disk() {
    let dir = studio();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("canvases/ep01-pilot/assets")).unwrap();
    assert_eq!(studio::root_for(&root.join("canvases/ep01-pilot/assets")), Some(root.clone()), "a subfolder finds its studio");
    let plain = tempfile::tempdir().unwrap();
    assert_eq!(studio::root_for(plain.path()), None);

    let mara = studio::entity(&root, "characters/mara").unwrap();
    assert_eq!((mara.name.as_str(), mara.kind(), mara.file.as_str()), ("Mara Venn", "character".to_owned(), "world/characters/mara/mara.md"));
    assert_eq!(mara.sheet.as_deref(), Some("world/characters/mara/sheet.png"));
    assert_eq!(mara.refs, ["world/characters/mara/refs/face.jpg", "world/characters/mara/refs/profile.png"], "every image in refs/");
    assert_eq!(mara.summary.as_deref(), Some("Salvage diver, scar across the left brow."));
    let harbor = studio::entity(&root, "places/harbor").unwrap();
    assert_eq!((harbor.name.as_str(), harbor.file.as_str(), harbor.sheet.as_deref()), ("The Harbor", "world/places/harbor/about.md", None));
    assert_eq!(harbor.refs, ["world/places/harbor/refs/dawn.png", "world/places/harbor/refs/night.png"]);
    let guild = studio::entity(&root, "factions/salvage-guild").unwrap();
    assert_eq!((guild.name.as_str(), guild.kind()), ("salvage-guild", "faction".to_owned()));
    for bad in ["characters", "characters/nobody", "../characters/mara", "characters/../mara", ""] {
        assert!(studio::entity(&root, bad).is_none(), "{bad}");
    }
    let ids: Vec<String> = studio::entities(&root).into_iter().map(|e| e.id).collect();
    assert_eq!(ids, ["characters/mara", "factions/salvage-guild", "places/harbor"]);

    assert_eq!(studio::style_line(&root).as_deref(), Some("Muted teal and amber, 35mm film grain."));
    assert_eq!(studio::motion_line(&root), None, "no motion line in the file");
    let picks = studio::model_picks(&root);
    assert_eq!((picks["video"].as_str(), picks["image"].as_str()), ("kling-3.0-pro", "gpt-image-2.6"));
    assert_eq!(studio::canvas_file(&root, "ep01-pilot").unwrap()["title"], "Pilot");
    assert!(studio::canvas_file(&root, "../ep01-pilot").is_none() && studio::canvas_file(&root, "broken").is_none());
    assert_eq!(
        studio::canvases(&root),
        [("ep01-pilot".to_owned(), "Pilot".to_owned(), Some(1)), ("moodboard".to_owned(), "Moodboard".to_owned(), None)]
    );
}

#[test]
fn the_style_and_motion_lines_are_read_from_their_own_headings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // No style.md at all: neither.
    assert_eq!((studio::style_line(root), studio::motion_line(root)), (None, None));

    // Both, among other headings, any level and case; each stops at the next heading.
    write(
        root,
        "world/style.md",
        "# Style\n\nNotes.\n\n## Style line\n\nFlat 2D cel shading,\nink outlines.\n\n### MOTION LINE\n\n  Limited animation on twos,\n\nsnappy holds, no camera shake.  \n\n## Palette\n\nTeal.\n",
    );
    assert_eq!(studio::style_line(root).as_deref(), Some("Flat 2D cel shading, ink outlines."));
    assert_eq!(studio::motion_line(root).as_deref(), Some("Limited animation on twos, snappy holds, no camera shake."));

    // The motion line first, last in the file, with no style line.
    write(root, "world/style.md", "# Style\n\n## Lenses\n\nWide.\n\n## motion line\nHand-drawn smears on fast moves.\n");
    assert_eq!(studio::style_line(root), None);
    assert_eq!(studio::motion_line(root).as_deref(), Some("Hand-drawn smears on fast moves."));

    // A heading with nothing under it, or a near-miss heading: none.
    write(root, "world/style.md", "## Motion line\n\n## Motion lines\n\nNot this.\n## Style line\n\nInk.\n");
    assert_eq!(studio::motion_line(root), None);
    assert_eq!(studio::style_line(root).as_deref(), Some("Ink."));
}

fn targets(found: &[Candidate]) -> Vec<&str> {
    found.iter().map(|c| c.target.as_str()).collect()
}

#[test]
fn mentions_match_fuzzily_with_prefixes_first() {
    let dir = studio();
    let root = dir.path();
    let all = studio::mentions(root, "", 50);
    assert_eq!(targets(&all), ["characters/mara", "factions/salvage-guild", "places/harbor", "ep01-pilot", "moodboard"]);
    assert_eq!(all[0], Candidate { entity: true, target: "characters/mara".into(), name: "Mara Venn".into(), detail: "Character".into() });
    let details: Vec<&str> = all.iter().map(|c| c.detail.as_str()).collect();
    assert_eq!(details, ["Character", "Faction", "Place", "Canvas · Ep 1", "Canvas"]);
    assert!(!all[3].entity && all[3].name == "Pilot");

    // Initials and subsequences match; every word needn't appear whole.
    assert_eq!(targets(&studio::mentions(root, "mvn", 50)), ["characters/mara"]);
    assert_eq!(targets(&studio::mentions(root, "M V", 50))[0], "characters/mara");
    // A prefix of the name or the target ranks first.
    assert_eq!(targets(&studio::mentions(root, "ha", 50))[0], "places/harbor");
    assert_eq!(targets(&studio::mentions(root, "ep01", 50)), ["ep01-pilot"]);
    assert_eq!(targets(&studio::mentions(root, "pil", 50))[0], "ep01-pilot", "the title's prefix");
    assert_eq!(targets(&studio::mentions(root, "mood", 50))[0], "moodboard");
    assert!(studio::mentions(root, "zzz", 50).is_empty());
    assert_eq!(studio::mentions(root, "", 2).len(), 2, "at most limit");
    assert_eq!(studio::mentions(root, "a", 1).len(), 1);
}

#[test]
fn an_empty_query_keeps_room_for_canvases() {
    let dir = studio();
    let root = dir.path();
    for i in 0..9 {
        write(root, &format!("world/characters/c{i}/c{i}.md"), "x\n");
    }
    // Twelve entities, two canvases: at most four, two of them the canvases.
    let found = studio::mentions(root, "", 4);
    assert_eq!(targets(&found), ["characters/c0", "characters/c1", "ep01-pilot", "moodboard"]);
    // Fewer canvases than half: entities fill the rest.
    assert_eq!(studio::mentions(root, "", 6).iter().filter(|c| c.entity).count(), 4);
    assert_eq!(studio::mentions(root, "", 50).len(), 14);
}

/// A canvas with a scene frame holding a shot frame, cards (titled, untitled with a prompt, an empty draft),
/// notes (markdown, blank), a document, an image asset, an entity node and an untitled frame.
fn board(root: &Path) {
    let node = |id: &str, (x, y, w, h): (i32, i32, i32, i32), body: serde_json::Value| serde_json::json!({"id": id, "x": x, "y": y, "w": w, "h": h, "body": body});
    let canvas = serde_json::json!({"version": 1, "title": "Quay", "nodes": [
        node("n_note", (10, 10, 50, 50), serde_json::json!({"kind": "note", "text": "\n## Keep it cold.\nMore."})),
        node("n_scene", (0, 0, 1000, 1000), serde_json::json!({"kind": "frame", "role": "scene", "title": "Pass 3A · The quay fight"})),
        node("n_shot", (100, 100, 400, 400), serde_json::json!({"kind": "frame", "role": "shot", "title": "S2.3"})),
        node("n_key", (120, 120, 100, 100), serde_json::json!({"kind": "gen", "makes": "image", "title": "S2.3 key"})),
        node("n_move", (300, 120, 100, 100), serde_json::json!({"kind": "gen", "makes": "video", "prompt": "Mara vaults the rail\nslow motion"})),
        node("n_draft", (300, 300, 100, 100), serde_json::json!({"kind": "gen", "makes": "image"})),
        node("n_blank", (2000, 0, 10, 10), serde_json::json!({"kind": "note", "text": "  \n"})),
        node("n_doc", (2000, 0, 10, 10), serde_json::json!({"kind": "asset", "path": "canvases/quay/script.md"})),
        node("n_plate", (2000, 0, 10, 10), serde_json::json!({"kind": "asset", "path": "plates/harbor.png", "media": "image"})),
        node("n_mara", (2000, 0, 10, 10), serde_json::json!({"kind": "entity", "id": "characters/mara"})),
        node("n_box", (3000, 0, 10, 10), serde_json::json!({"kind": "frame", "title": ""})),
    ], "edges": []});
    write(root, "canvases/quay/canvas.json", &canvas.to_string());
}

#[test]
fn a_canvas_query_lists_its_nodes_by_title() {
    let dir = studio();
    let root = dir.path();
    board(root);
    let all = studio::mentions(root, "quay#", 50);
    let rows: Vec<(&str, &str, &str)> = all.iter().map(|c| (c.target.as_str(), c.name.as_str(), c.detail.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("quay#n_scene", "Pass 3A · The quay fight", "Scene"),
            ("quay#n_shot", "S2.3", "Shot · Pass 3A · The quay fight"),
            ("quay#n_key", "S2.3 key", "Image card · S2.3"),
            ("quay#n_move", "Mara vaults the rail", "Video card · S2.3"),
            ("quay#n_note", "Keep it cold.", "Note · Pass 3A · The quay fight"),
            ("quay#n_doc", "script.md", "Document"),
            ("quay#n_plate", "harbor.png", "Image"),
        ]
    );
    assert!(all.iter().all(|c| !c.entity));
    // Filtered by the text after `#`, a title's prefix first.
    assert_eq!(targets(&studio::mentions(root, "quay#s2", 50))[..2], ["quay#n_shot", "quay#n_key"]);
    assert_eq!(targets(&studio::mentions(root, "quay#quay", 50)), ["quay#n_scene"]);
    assert_eq!(targets(&studio::mentions(root, "quay#cold", 50)), ["quay#n_note"]);
    assert!(studio::mentions(root, "quay#zzz", 50).is_empty());
    assert_eq!(studio::mentions(root, "quay#", 3).len(), 3);
    // Only that canvas, named exactly; none for an unknown or a path.
    assert_eq!(targets(&studio::mentions(root, "ep01-pilot#", 50)), ["ep01-pilot#n_1"]);
    assert!(studio::mentions(root, "qua#", 50).is_empty());
    assert!(studio::mentions(root, "../quay#", 50).is_empty());
    // A long first line is cut.
    let long = "word ".repeat(30);
    let canvas = serde_json::json!({"version": 1, "nodes": [{"id": "n", "x": 0, "y": 0, "w": 1, "h": 1, "body": {"kind": "note", "text": long}}], "edges": []});
    write(root, "canvases/long/canvas.json", &canvas.to_string());
    let name = &studio::mentions(root, "long#", 50)[0].name;
    assert!(name.ends_with('…') && name.chars().count() <= 61, "{name}");
}

/// The king: named references in every form the frontmatter takes, and links.
fn court(root: &Path) {
    write(
        root,
        "world/characters/dhritarashtra/dhritarashtra.md",
        "---\nname: Dhritarashtra\nsheet: sheet.png\nrefs:\n  - refs/face.png\n  - file: refs/crown-off.png   # bare\n    name: Crown off\n    defines: \"his bare head and hair, without the crown\"\n  -\n    file: refs/c2.png\n    name: crown OFF\n  - refs/walk.mp4\n  - ../../../../outside.png\nlinks: [props/kuru-crown, props/nothing, characters/dhritarashtra]\n---\n",
    );
    write(
        root,
        "world/props/kuru-crown/kuru-crown.md",
        "---\nname: Kuru crown\nrefs: [refs/top.png, {file: refs/side.png, name: Side view, defines: the crest from the side}]\n---\n",
    );
    write(root, "world/props/gada/gada.md", "---\nname: Bhima's mace\nlinks:\n  - characters/dhritarashtra\n---\n");
}

#[test]
fn named_references_and_links_are_read_from_disk() {
    let dir = studio();
    let root = dir.path();
    court(root);
    let king = studio::entity(root, "characters/dhritarashtra").unwrap();
    let looks: Vec<(&str, &str, &str, Option<&str>, &str)> =
        king.looks.iter().map(|l| (l.id.as_str(), l.path.as_str(), l.name.as_str(), l.defines.as_deref(), l.kind())).collect();
    assert_eq!(
        looks,
        [
            ("face", "world/characters/dhritarashtra/refs/face.png", "face", None, "look"),
            (
                "crown-off",
                "world/characters/dhritarashtra/refs/crown-off.png",
                "Crown off",
                Some("his bare head and hair, without the crown"),
                "look"
            ),
            ("c2", "world/characters/dhritarashtra/refs/c2.png", "crown OFF", None, "look"),
            ("walk", "world/characters/dhritarashtra/refs/walk.mp4", "walk", None, "video"),
        ],
        "a path leaving the root is dropped"
    );
    assert_eq!(king.refs, king.looks.iter().map(|l| l.path.clone()).collect::<Vec<_>>());
    assert_eq!(king.links, ["props/kuru-crown", "props/gada"], "its own (existing, not itself), then those naming it");
    let crown = studio::entity(root, "props/kuru-crown").unwrap();
    assert_eq!(crown.looks[1].defines.as_deref(), Some("the crest from the side"));
    assert_eq!((crown.looks[1].id.as_str(), crown.links.as_slice()), ("side", ["characters/dhritarashtra".to_owned()].as_slice()));
    let all = studio::entities(root);
    assert_eq!(all.iter().find(|e| e.id == "props/gada").unwrap().links, ["characters/dhritarashtra"]);
    assert_eq!(all.iter().find(|e| e.id == "props/kuru-crown").unwrap().links, ["characters/dhritarashtra"]);
    // Mention targets: the entity, or one of its references.
    assert!(studio::mention_target(root, "characters/dhritarashtra").is_some_and(|(_, r)| r.is_none()));
    let (_, look) = studio::mention_target(root, "characters/dhritarashtra#crown-off").unwrap();
    assert_eq!(look.unwrap().name, "Crown off");
    assert!(studio::mention_target(root, "characters/dhritarashtra#nope").is_none());
    assert!(studio::mention_target(root, "characters/nobody#face").is_none());
    // The same ids as the server's.
    assert_eq!(
        studio::ref_ids(&[(None, "a/face.png"), (Some("Bare head"), "b/face.jpg"), (Some("Flute"), "बाँसुरी.png"), (Some("कृष्ण"), "वेणु.png")]),
        ["face", "face-2", "flute", "ref"],
        "the file's stem, else the name, else ref"
    );
}

#[test]
fn an_entity_query_lists_its_references() {
    let dir = studio();
    let root = dir.path();
    court(root);
    board(root);
    let rows = |q: &str| studio::mentions(root, q, 50).into_iter().map(|c| (c.target, c.name, c.detail, c.entity)).collect::<Vec<_>>();
    let all = rows("characters/dhritarashtra#");
    assert_eq!(
        all.iter().map(|r| (r.0.as_str(), r.1.as_str(), r.2.as_str())).collect::<Vec<_>>(),
        [
            ("characters/dhritarashtra#face", "face", "Look · Dhritarashtra"),
            ("characters/dhritarashtra#crown-off", "Crown off", "Look · Dhritarashtra"),
            ("characters/dhritarashtra#c2", "crown OFF", "Look · Dhritarashtra"),
            ("characters/dhritarashtra#walk", "walk", "Video · Dhritarashtra"),
        ]
    );
    assert!(all.iter().all(|r| r.3), "entity mentions");
    // By folder or name, case aside; filtered by what follows the `#`.
    assert_eq!(rows("dhritarashtra#crown")[0].0, "characters/dhritarashtra#crown-off");
    assert_eq!(rows("Dhritarashtra#wa").len(), 1);
    assert_eq!(rows("kuru-crown#side")[0].0, "props/kuru-crown#side");
    assert_eq!(rows("kuru-crown#view")[0].0, "props/kuru-crown#side", "by its name");
    assert!(rows("dhritarashtra#zzz").is_empty());
    assert_eq!(studio::mentions(root, "characters/dhritarashtra#", 2).len(), 2);
    // A canvas of that name wins over an entity's folder; a path is never a canvas.
    write(
        root,
        "canvases/gada/canvas.json",
        r#"{"version":1,"title":"Gada","nodes":[{"id":"n_g","x":0,"y":0,"w":1,"h":1,"body":{"kind":"note","text":"g"}}],"edges":[]}"#,
    );
    assert_eq!(rows("gada#")[0].0, "gada#n_g");
    assert!(rows("props/gada#").is_empty(), "no references");
    assert_eq!(rows("quay#")[0].0, "quay#n_scene", "canvases as before");
}

#[test]
fn a_plain_query_finds_references_by_name_or_id() {
    let dir = studio();
    let root = dir.path();
    court(root);
    write(
        root,
        "world/props/kuru-crown/kuru-crown.md",
        "---\nname: Kuru crown\nrefs: [refs/top.png, {file: refs/crown-gold.png, name: Gilded}]\n---\n",
    );
    write(root, "canvases/crowning/canvas.json", r#"{"version":1,"title":"Crowning","nodes":[],"edges":[]}"#);
    let found = |q: &str, limit: usize| targets(&studio::mentions(root, q, limit)).into_iter().map(str::to_owned).collect::<Vec<_>>();
    // Each right after its entity's row when that is listed (by id here), else after the entities (by name here).
    assert_eq!(
        found("crown", 50),
        ["crowning", "props/kuru-crown", "props/kuru-crown#crown-gold", "characters/dhritarashtra#crown-off", "characters/dhritarashtra#c2"]
    );
    let row = studio::mentions(root, "crown", 50).into_iter().find(|c| c.target == "characters/dhritarashtra#c2").unwrap();
    assert_eq!(
        row,
        Candidate { entity: true, target: "characters/dhritarashtra#c2".into(), name: "crown OFF".into(), detail: "Look · Dhritarashtra".into() }
    );
    assert_eq!(found("crown", 3), ["crowning", "props/kuru-crown", "props/kuru-crown#crown-gold"], "at most limit");
    // Case aside; no entity listed: they come first.
    assert_eq!(found("CROWN OFF", 50), ["characters/dhritarashtra#crown-off", "characters/dhritarashtra#c2"]);
    assert_eq!(found("gilded", 50), ["props/kuru-crown#crown-gold"]);
    assert_eq!(found("walk", 50), ["characters/dhritarashtra#walk"], "a video too");
    // A substring, not a subsequence: no reference for `cwn`.
    let fuzzy = found("cwn", 50);
    assert!(!fuzzy.is_empty() && fuzzy.iter().all(|t| !t.contains('#')), "{fuzzy:?}");
}
