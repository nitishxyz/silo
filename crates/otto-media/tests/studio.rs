//! Studio cards (docs/STUDIO.md §6.2): compiling a card, running it against an in-memory Studio, `canvas_run`,
//! `generate_* {canvas}` and the session-less path, with fake providers.

mod studio_fake;

use otto_media::studio::{self as card_api, Setup};
use otto_tools::jobs::Jobs;
use otto_tools::studio::Studio;
use otto_tools::{Mailbox, OutputPart};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use studio_fake::*;

fn text_of(out: &otto_tools::ToolOutput) -> String {
    out.content.iter().filter_map(|p| if let OutputPart::Text { text } = p { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n")
}

// Compiling.

#[tokio::test]
async fn the_prompt_is_the_cards_then_wired_texts_then_the_style_line() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_note", note("Fog rolls in."));
    f.studio.put("ep", "n_img", card("image", "A harbor at dawn"));
    f.studio.wire("ep", "n_note", "n_img", "text");
    let c = f.compile("n_img").await.unwrap();
    assert_eq!(c.prompt, format!("A harbor at dawn\n\nFog rolls in.\n\n{STYLE}"));
    assert_eq!((c.tool, c.input["prompt"].as_str()), ("generate_image", Some(c.prompt.as_str())));

    // style: false leaves the line out.
    f.studio.edit("b", "ep", vec![json!({"op": "update_node", "id": "n_img", "body": {"options": {"style": false}}})]).await.unwrap();
    let c = f.compile("n_img").await.unwrap();
    assert_eq!(c.prompt, "A harbor at dawn\n\nFog rolls in.");
    assert!(c.input.get("style").is_none(), "style is not an option sent: {}", c.input);

    // A video card gets it too, then the motion line; audio never.
    f.studio.put("ep", "n_vid", card("video", "Waves"));
    let c = f.compile("n_vid").await.unwrap();
    assert_eq!(c.prompt, format!("Waves\n\n{STYLE}\n\n{MOTION}"));
    assert_eq!(c.input["prompt"].as_str(), Some(c.prompt.as_str()));
    f.studio.put("ep", "n_sfx", card("sound", "Gulls over a harbor"));
    let c = f.compile("n_sfx").await.unwrap();
    assert_eq!((c.tool, c.prompt.as_str(), c.input["type"].as_str()), ("generate_audio", "Gulls over a harbor", Some("sound")));

    // A speech card with no prompt reads the wired text; with its own line, the line.
    f.studio.put("ep", "n_line", note("We dive at dawn."));
    f.studio.put("ep", "n_say", card("speech", ""));
    f.studio.wire("ep", "n_line", "n_say", "text");
    let c = f.compile("n_say").await.unwrap();
    assert_eq!(
        (c.prompt.as_str(), c.input["text"].as_str(), c.input["type"].as_str()),
        ("We dive at dawn.", Some("We dive at dawn."), Some("speech"))
    );
    assert_eq!(c.input["provider"], "voc");
    assert!(c.input.get("model").is_none(), "audio on Automatic leaves the model to the provider: {}", c.input);
    f.studio.edit("b", "ep", vec![json!({"op": "update_node", "id": "n_say", "body": {"prompt": "Hold on."}})]).await.unwrap();
    let c = f.compile("n_say").await.unwrap();
    assert_eq!(c.prompt, "Hold on.");
    assert!(c.notes.iter().any(|n| n.contains("wired text isn't read")), "{:?}", c.notes);

    // No prompt and nothing wired: invalid.
    f.studio.put("ep", "n_empty", card("image", "  "));
    let p = f.compile("n_empty").await.unwrap_err();
    assert_eq!(p.code, "invalid", "{}", p.message);
}

#[tokio::test]
async fn the_motion_line_goes_on_video_cards_only_after_the_style_line() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_note", note("Fog rolls in."));
    f.studio.put("ep", "n_img", card("image", "A harbor at dawn"));
    f.studio.put("ep", "n_vid", card("video", "Waves break"));
    f.studio.wire("ep", "n_note", "n_vid", "text");
    assert_eq!((Setup::read(&f.root).style.as_deref(), Setup::read(&f.root).motion.as_deref()), (Some(STYLE), Some(MOTION)));

    // Video: the prompt, the wired text, the style line, then the motion line. Image: the style line only.
    assert_eq!(f.compile("n_vid").await.unwrap().prompt, format!("Waves break\n\nFog rolls in.\n\n{STYLE}\n\n{MOTION}"));
    assert_eq!(f.compile("n_img").await.unwrap().prompt, format!("A harbor at dawn\n\n{STYLE}"));

    // style: false turns both off.
    f.studio.edit("b", "ep", vec![json!({"op": "update_node", "id": "n_vid", "body": {"options": {"style": false}}})]).await.unwrap();
    assert_eq!(f.compile("n_vid").await.unwrap().prompt, "Waves break\n\nFog rolls in.");

    // A motion line without a style line still goes on video.
    let card_ = f.card("n_img").await;
    let setup = Setup { motion: Some(MOTION.into()), ..Setup::default() };
    assert_eq!(card_api::compile(&f.media, &card_, &setup, &Map::new()).await.unwrap().prompt, "A harbor at dawn", "never on image");
    f.studio.put("ep", "n_vid2", card("video", "Gulls"));
    let card_ = f.card("n_vid2").await;
    assert_eq!(card_api::compile(&f.media, &card_, &setup, &Map::new()).await.unwrap().prompt, format!("Gulls\n\n{MOTION}"));

    // No motion line in style.md: video gets the style line alone, as before.
    write(&f.root, "world/style.md", format!("# Style\n\n## Style line\n\n{STYLE}\n\n## Palette\n\nTeal.\n").as_bytes());
    assert_eq!(Setup::read(&f.root).motion, None);
    assert_eq!(f.compile("n_vid2").await.unwrap().prompt, format!("Gulls\n\n{STYLE}"));
    assert_eq!(f.compile("n_img").await.unwrap().prompt, format!("A harbor at dawn\n\n{STYLE}"));
}

#[tokio::test]
async fn a_wired_document_gives_its_contents_capped() {
    let f = fx(ALL, false);
    write(&f.root, "canvases/ep/script.md", b"# The Knot\n1. The river at dusk");
    f.studio.put("ep", "n_doc", asset("canvases/ep/script.md", "text"));
    f.studio.put("ep", "n_img", card("image", "Beat 1"));
    f.studio.wire("ep", "n_doc", "n_img", "text");
    let c = f.compile("n_img").await.unwrap();
    assert_eq!(c.prompt, format!("Beat 1\n\n# The Knot\n1. The river at dusk\n\n{STYLE}"));
    assert!(c.files.is_empty() && c.notes.is_empty(), "context, not a file: {:?} {:?}", c.files, c.notes);

    // A speech card with no line reads it, as a note's.
    f.studio.put("ep", "n_say", card("speech", ""));
    f.studio.wire("ep", "n_doc", "n_say", "text");
    assert_eq!(f.compile("n_say").await.unwrap().prompt, "# The Knot\n1. The river at dusk");

    // Over the cap: the first 8,000 characters, with a note.
    let long = "é".repeat(card_api::DOCUMENT_CAP + 50);
    write(&f.root, "canvases/ep/script.md", long.as_bytes());
    let c = f.compile("n_say").await.unwrap();
    assert_eq!(c.prompt.chars().count(), card_api::DOCUMENT_CAP);
    assert!(c.notes.iter().any(|n| n.contains("script.md is over 8000 characters")), "{:?}", c.notes);

    // Gone: an input problem.
    std::fs::remove_file(f.root.join("canvases/ep/script.md")).unwrap();
    let p = f.compile("n_say").await.unwrap_err();
    assert!(p.code == "input" && p.message.contains("script.md"), "{}", p.message);
}

#[tokio::test]
async fn wires_become_files_by_port_in_the_order_they_were_made() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_mara", json!({"kind": "entity", "id": "characters/mara"}));
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.put("ep", "n_dock", asset("plates/dock.png", "image"));
    f.studio.put("ep", "n_img", card("image", "Mara on the dock"));
    f.studio.wire("ep", "n_plate", "n_img", "reference");
    f.studio.wire("ep", "n_mara", "n_img", "reference");
    let c = f.compile("n_img").await.unwrap();
    assert_eq!(c.input["references"], json!(["plates/harbor.png", "world/characters/mara/sheet.png"]), "the first wire is image 1");
    let files: Vec<(String, u32, String, String)> = c.files.iter().map(|f| (f.port.clone(), f.index, f.path.clone(), f.label.clone())).collect();
    assert_eq!(
        files,
        [
            ("reference".into(), 1, "plates/harbor.png".into(), "harbor.png".into()),
            ("reference".into(), 2, "world/characters/mara/sheet.png".into(), "Mara Venn".into())
        ]
    );
    assert_eq!((c.input["provider"].as_str(), c.input["model"].as_str(), c.model.as_str()), (Some("pix"), Some("pix-1"), "pix-1"));

    // Frames through the quick tool: frames first whatever order they were wired in.
    f.studio.put("ep", "n_vid", card("video", "Mara walks"));
    f.studio.wire("ep", "n_mara", "n_vid", "reference");
    f.studio.wire("ep", "n_dock", "n_vid", "last_frame");
    f.studio.wire("ep", "n_plate", "n_vid", "first_frame");
    let c = f.compile("n_vid").await.unwrap();
    assert_eq!(c.tool, "generate_video");
    assert_eq!((c.input["first_frame"].as_str(), c.input["last_frame"].as_str()), (Some("plates/harbor.png"), Some("plates/dock.png")));
    assert_eq!(c.input["references"], json!(["world/characters/mara/sheet.png"]));
    let ports: Vec<&str> = c.files.iter().map(|f| f.port.as_str()).collect();
    assert_eq!(ports, ["first_frame", "last_frame", "reference"]);
    assert_eq!(c.model, "vid-1");

    // A middle frame needs a mode with a field for it: Automatic picks the aggregator's image-to-video.
    f.studio.put("ep", "n_mid", card("video", "Through the fog"));
    f.studio.wire("ep", "n_plate", "n_mid", "first_frame");
    f.studio.wire("ep", "n_dock", "n_mid", "middle_frame");
    let c = f.compile("n_mid").await.unwrap();
    assert_eq!((c.tool, c.mode.as_deref(), c.input["model"].as_str()), ("generate_media", Some("image-to-video"), Some("agg-motion")));
    assert_eq!(c.input["input"]["start_image_url"], "plates/harbor.png");
    assert_eq!(c.input["input"]["middle_image_url"], "plates/dock.png");
    assert_eq!(c.input["input"]["prompt"].as_str(), Some(c.prompt.as_str()));
    let fields: Vec<&str> = c.files.iter().map(|f| f.field.as_str()).collect();
    assert_eq!(fields, ["start_image_url", "middle_image_url"]);

    // Video and audio wired: reference-to-video, its fields by media.
    f.studio.put("ep", "n_wave", asset("clips/wave.mp4", "video"));
    f.studio.put("ep", "n_gulls", asset("sounds/gulls.mp3", "audio"));
    f.studio.put("ep", "n_mix", card("video", "The harbor wakes"));
    f.studio.wire("ep", "n_gulls", "n_mix", "audio");
    f.studio.wire("ep", "n_wave", "n_mix", "video");
    f.studio.wire("ep", "n_mara", "n_mix", "reference");
    let c = f.compile("n_mix").await.unwrap();
    assert_eq!(c.mode.as_deref(), Some("reference-to-video"));
    let input = &c.input["input"];
    assert_eq!((input["video_url"].as_str(), input["audio_url"].as_str()), (Some("clips/wave.mp4"), Some("sounds/gulls.mp3")));
    assert_eq!(input["reference_image_urls"], json!(["world/characters/mara/sheet.png"]));
    let ports: Vec<String> = c.ports.iter().map(|p| p.id().to_owned()).collect();
    assert!(ports.contains(&"video".into()) && ports.contains(&"audio".into()) && ports.contains(&"text".into()), "{ports:?}");
}

#[tokio::test]
async fn automatic_takes_the_projects_pick_then_the_default_then_a_model_that_fits() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_img", card("image", "A lighthouse"));
    assert_eq!(f.compile("n_img").await.unwrap().model, "pix-1", "the provider's default");
    f.media.set_default(otto_media::MediaKind::Image, Some("pix".into()), Some("pix-solo".into())).unwrap();
    assert_eq!(f.compile("n_img").await.unwrap().model, "pix-solo", "the kind's chosen default");
    // The project's pick wins over the default.
    f.media.set_default(otto_media::MediaKind::Image, None, None).unwrap();
    write(&f.root, "studio.toml", b"name = \"Show\"\n\n[models]\nimage = \"pix-solo\"\nvideo = \"vid-lite\"\n");
    assert_eq!(f.compile("n_img").await.unwrap().model, "pix-solo");

    // A pick that can't take what is wired is passed over for a connected model that can.
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.wire("ep", "n_plate", "n_img", "reference");
    let c = f.compile("n_img").await.unwrap();
    assert_eq!(c.model, "pix-1");
    f.studio.put("ep", "n_vid", card("video", "Waves"));
    assert_eq!(f.compile("n_vid").await.unwrap().model, "vid-lite");
    f.studio.wire("ep", "n_plate", "n_vid", "reference");
    assert_eq!(f.compile("n_vid").await.unwrap().model, "vid-1");
}

#[tokio::test]
async fn a_chosen_model_that_cant_take_the_wires_is_a_model_problem() {
    let f = fx(ALL, false);
    for i in 0..5 {
        f.studio.put("ep", &format!("n_r{i}"), asset("plates/harbor.png", "image"));
    }
    f.studio.put("ep", "n_vid", json!({"kind": "gen", "makes": "video", "prompt": "Crowd", "model": "vid-lite"}));
    f.studio.wire("ep", "n_r0", "n_vid", "reference");
    let p = f.compile("n_vid").await.unwrap_err();
    assert_eq!(p.code, "model");
    assert_eq!(p.message, "Vid Lite can't make this card: no reference input");
    f.studio.put("ep", "n_many", json!({"kind": "gen", "makes": "video", "prompt": "Crowd", "model": "agg-motion", "mode": "reference-to-video"}));
    for i in 0..5 {
        f.studio.wire("ep", &format!("n_r{i}"), "n_many", "reference");
    }
    let p = f.compile("n_many").await.unwrap_err();
    assert_eq!((p.code, p.message.as_str()), ("model", "Agg Motion can't make this card: takes 3 references, 5 wired"));
    // An image model on a video card, an unknown one.
    f.studio.put("ep", "n_wrong", json!({"kind": "gen", "makes": "video", "prompt": "x", "model": "pix-1"}));
    let p = f.compile("n_wrong").await.unwrap_err();
    assert_eq!(p.code, "model");
    assert!(p.message.contains("Pix 1 makes image"), "{}", p.message);
    f.studio.put("ep", "n_nope", json!({"kind": "gen", "makes": "image", "prompt": "x", "model": "nope-9"}));
    assert_eq!(f.compile("n_nope").await.unwrap_err().code, "model");
}

#[tokio::test]
async fn sizes_come_from_the_format_only_where_unset_and_unknown_options_are_dropped() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_vid", card("video", "Waves"));
    let c = f.compile("n_vid").await.unwrap();
    assert_eq!((c.input["aspect_ratio"].as_str(), c.input["resolution"].as_str()), (Some("16:9"), Some("1080p")));
    assert!(c.notes.is_empty(), "{:?}", c.notes);
    // A 4:5 canvas: the nearest offered aspect, with a note; the card's own value wins.
    f.studio.formats.lock().unwrap().insert("ep".into(), format("4:5", Some("4k"), Some(24)));
    let c = f.compile("n_vid").await.unwrap();
    assert_eq!((c.input["aspect_ratio"].as_str(), c.input["resolution"].as_str()), (Some("1:1"), Some("1080p")));
    assert!(c.notes.contains(&"Vid 1 has no 4:5; made 1:1.".to_owned()), "{:?}", c.notes);
    assert!(c.notes.contains(&"Vid 1 has no 4k; made 1080p.".to_owned()), "{:?}", c.notes);
    f.studio
        .edit(
            "b",
            "ep",
            vec![json!({"op": "update_node", "id": "n_vid", "body": {"options": {"aspect_ratio": "9:16", "duration": 5, "voice": "x"}}})],
        )
        .await
        .unwrap();
    let c = f.compile("n_vid").await.unwrap();
    assert_eq!((c.input["aspect_ratio"].as_str(), c.input["duration"].as_u64()), (Some("9:16"), Some(5)));
    assert!(c.input.get("voice").is_none());
    assert!(c.notes.contains(&"Vid 1 has no option voice; left out.".to_owned()), "{:?}", c.notes);
    assert!(!c.notes.iter().any(|n| n.contains("4:5")), "the card's aspect wins: {:?}", c.notes);
    // An image card: 4:5 → 3:4 (no resolution field to fill).
    f.studio.put("ep", "n_img", card("image", "A still"));
    let c = f.compile("n_img").await.unwrap();
    assert_eq!(c.input["aspect_ratio"], "3:4");
    assert!(c.input.get("resolution").is_none());
    // A mode with fps: filled from the format.
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.put("ep", "n_mid", card("video", "Fog"));
    f.studio.wire("ep", "n_plate", "n_mid", "first_frame");
    f.studio.wire("ep", "n_plate", "n_mid", "middle_frame");
    let c = f.compile("n_mid").await.unwrap();
    assert_eq!((c.input["input"]["fps"].as_u64(), c.input["input"]["aspect_ratio"].as_str()), (Some(24), Some("9:16")));
}

#[tokio::test]
async fn wire_problems_and_missing_providers() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_key", json!({"kind": "gen", "makes": "image", "title": "Keyframe 3", "prompt": "x"}));
    f.studio.put("ep", "n_vid", card("video", "From the keyframe"));
    f.studio.wire("ep", "n_key", "n_vid", "first_frame");
    let p = f.compile("n_vid").await.unwrap_err();
    assert_eq!((p.code, p.message.as_str()), ("input", "Run Keyframe 3 first"));
    // The wrong media on a port.
    f.studio.put("ep", "n_wave", asset("clips/wave.mp4", "video"));
    f.studio.put("ep", "n_img", card("image", "x"));
    f.studio.wire("ep", "n_wave", "n_img", "reference");
    let p = f.compile("n_img").await.unwrap_err();
    assert_eq!(p.code, "input");
    assert_eq!(p.message, "wave.mp4 outputs a video; the reference port takes an image");

    // Nothing connected: no_provider with its kind; a chosen model's provider named.
    let g = fx(&["pix"], false);
    g.studio.put("ep", "n_vid", card("video", "Waves"));
    let p = g.compile("n_vid").await.unwrap_err();
    assert_eq!((p.code, p.kind, p.provider.as_deref()), ("no_provider", Some(otto_media::MediaKind::Video), None));
    assert!(p.message.starts_with("No video provider is connected"), "{}", p.message);
    g.studio.put("ep", "n_chosen", json!({"kind": "gen", "makes": "video", "prompt": "x", "model": "vid-1"}));
    let p = g.compile("n_chosen").await.unwrap_err();
    assert_eq!((p.code, p.provider.as_deref()), ("no_provider", Some("vid")));
    assert_eq!(p.wire().kind, Some(otto_media::MediaKind::Video));
}

#[tokio::test]
async fn fit_says_per_model_whether_it_can_make_the_card() {
    let f = fx(&["pix", "vid"], false);
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.put("ep", "n_vid", card("video", "Waves"));
    f.studio.wire("ep", "n_plate", "n_vid", "reference");
    let card = f.card("n_vid").await;
    let fit = |name: &'static str| {
        let (media, card) = (f.media.clone(), card.clone());
        async move { card_api::fit(&media, &card, name).await }
    };
    let ok = fit("vid-1").await;
    assert!(ok.fits && ok.reason.is_none() && ok.mode.is_none(), "{ok:?}");
    let lite = fit("vid-lite").await;
    assert_eq!((lite.fits, lite.reason.as_deref()), (false, Some("no reference input")));
    let agg = fit("agg-motion").await;
    assert_eq!((agg.fits, agg.mode.as_deref(), agg.reason.as_deref()), (true, Some("reference-to-video"), Some("not connected")));
    assert!(agg.fits, "not connected is a reason, not a misfit");
    let image = fit("pix-1").await;
    assert!(!image.fits && image.reason.as_deref().is_some_and(|r| r.contains("makes image")), "{image:?}");
    let unknown = fit("nope-9").await;
    assert!(!unknown.fits && unknown.reason.is_some());
}

// Running.

#[tokio::test]
async fn a_run_goes_queued_running_done_with_a_picked_take() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_note", note("Fog"));
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.put(
        "ep",
        "n_img",
        json!({"kind": "gen", "makes": "image", "prompt": "A harbor", "options": {"n": 2},
        "takes": [{"path": "canvases/ep/assets/old.png", "media": "image", "at": 1}], "status": "done"}),
    );
    f.studio.wire("ep", "n_plate", "n_img", "reference");
    f.studio.wire("ep", "n_note", "n_img", "text");
    let expected = f.compile("n_img").await.unwrap();
    let preview = card_api::preview_card(&f.media, &f.root, "ep", "n_img", &[]).await.unwrap();
    card_api::start_card(&f.media, &f.root, "ep", "n_img").await.unwrap();
    let body = f.studio.written("n_img", "done", 1).await;
    assert_eq!(f.studio.statuses("n_img"), ["queued", "running", "done"]);
    let running = f.studio.updates("n_img").into_iter().find(|b| b["status"] == "running").unwrap();
    assert_eq!(running["tool"], "generate_image");
    assert_eq!(running["input"], expected.input, "the record is the compiled request");
    assert_eq!(preview.compiled.as_ref().unwrap().input, expected.input, "the preview is what a run sends");
    let takes = body["takes"].as_array().unwrap();
    assert_eq!(takes.len(), 3, "the old take and one per file: {body}");
    assert_eq!(body["pick"], 1, "the first new take");
    let card = f.card("n_img").await;
    assert_eq!(body["inputs_used"], json!(card.keys));
    assert!(card.keys[0].starts_with("plates/harbor.png@") && card.keys[1].starts_with("note:"), "{:?}", card.keys);
    let paths: Vec<&str> = takes[1..].iter().map(|t| t["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["canvases/ep/assets/a-harbor-1.png", "canvases/ep/assets/a-harbor-2.png"]);
    for (t, p) in takes[1..].iter().zip(&paths) {
        assert!(f.root.join(p).is_file(), "{p}");
        assert_eq!((t["media"].as_str(), t["model"].as_str(), t["width"].as_u64()), (Some("image"), Some("pix-1"), Some(2)));
        assert!(t["prompt"].as_str().unwrap().starts_with("A harbor"));
    }
    assert_eq!(f.log.last()["n"], 2);
    assert!(body.get("error").is_none() && body.get("progress").is_none() && body.get("job").is_none(), "{body}");
    // Session-less: nothing is kept as an attachment.
    assert!(!f.dir.path().join("attachments").exists());

    // Again: new names, nothing overwritten.
    let before = std::fs::read(f.root.join(paths[0])).unwrap();
    card_api::start_card(&f.media, &f.root, "ep", "n_img").await.unwrap();
    let body = f.studio.written("n_img", "done", 2).await;
    let takes = body["takes"].as_array().unwrap();
    assert_eq!((takes.len(), body["pick"].as_u64()), (5, Some(3)));
    let mut all: Vec<&str> = takes.iter().map(|t| t["path"].as_str().unwrap()).collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 5, "unique names: {all:?}");
    assert_eq!(std::fs::read(f.root.join(paths[0])).unwrap(), before);
}

#[tokio::test]
async fn names_never_collide_with_files_already_there() {
    let f = fx(ALL, false);
    write(&f.root, "canvases/ep/assets/lighthouse.png", b"mine");
    write(&f.root, "canvases/ep/assets/lighthouse-2.jpg", b"mine too");
    f.studio.put("ep", "n_img", card("image", "Lighthouse"));
    card_api::start_card(&f.media, &f.root, "ep", "n_img").await.unwrap();
    let body = f.studio.until("ep", "n_img", "done").await;
    assert_eq!(body["takes"][0]["path"], "canvases/ep/assets/lighthouse-3.png");
    assert_eq!(std::fs::read(f.root.join("canvases/ep/assets/lighthouse.png")).unwrap(), b"mine");
}

#[tokio::test]
async fn progress_reaches_the_card_at_most_once_a_second() {
    let f = fx(ALL, false);
    *f.vid.1.lock().unwrap() = Behaviour::Slow(600);
    f.studio.put("ep", "n_vid", card("video", "Waves"));
    card_api::start_card(&f.media, &f.root, "ep", "n_vid").await.unwrap();
    f.studio.until("ep", "n_vid", "done").await;
    let progress = f.studio.updates("n_vid").iter().filter(|b| b["progress"].is_string()).count();
    assert_eq!(progress, 1, "21 messages in 0.6 s: one write, {:?}", f.studio.updates("n_vid"));
    let take = &f.studio.body("ep", "n_vid")["takes"][0];
    assert_eq!((take["media"].as_str(), take["seconds"].as_f64(), take["model"].as_str()), (Some("video"), Some(8.0), Some("vid-1")));
}

#[tokio::test]
async fn failures_and_stops_are_written_on_the_card() {
    let f = fx(ALL, false);
    *f.vid.1.lock().unwrap() = Behaviour::Fail("content policy: no".into());
    f.studio.put("ep", "n_vid", card("video", "Waves"));
    card_api::start_card(&f.media, &f.root, "ep", "n_vid").await.unwrap();
    let body = f.studio.until("ep", "n_vid", "failed").await;
    assert!(body["error"].as_str().unwrap().contains("content policy: no"), "{body}");
    assert_eq!(body["error_code"], "failed");
    assert!(body.get("takes").is_none());

    // A compile problem found when it runs carries its code.
    f.studio.put("ep", "n_key", json!({"kind": "gen", "makes": "image", "title": "Keyframe 3", "prompt": "x"}));
    f.studio.put("ep", "n_from", card("video", "From it"));
    f.studio.wire("ep", "n_key", "n_from", "first_frame");
    card_api::start_card(&f.media, &f.root, "ep", "n_from").await.unwrap();
    let body = f.studio.until("ep", "n_from", "failed").await;
    assert_eq!((body["error"].as_str(), body["error_code"].as_str()), (Some("Run Keyframe 3 first"), Some("input")));

    // Stopped while the provider works; a second run is refused while one runs.
    *f.vid.1.lock().unwrap() = Behaviour::Hang;
    card_api::start_card(&f.media, &f.root, "ep", "n_vid").await.unwrap();
    f.studio.until("ep", "n_vid", "running").await;
    assert!(f.dir.path().join("state/studio-runs.json").is_file());
    let runs: Vec<String> = serde_json::from_slice(&std::fs::read(f.dir.path().join("state/studio-runs.json")).unwrap()).unwrap();
    assert_eq!(runs, [format!("{}|ep|n_vid", f.root.display())]);
    card_api::stop_card(&f.media, &f.root, "ep", "n_vid").await.unwrap();
    let body = f.studio.until("ep", "n_vid", "stopped").await;
    assert!(body.get("error").is_none() && body.get("error_code").is_none(), "{body}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let runs: Vec<String> = serde_json::from_slice(&std::fs::read(f.dir.path().join("state/studio-runs.json")).unwrap()).unwrap();
    assert!(runs.is_empty(), "{runs:?}");

    // A card left running by no run here is marked stopped by stop_card.
    f.studio.put("ep", "n_stale", json!({"kind": "gen", "makes": "image", "prompt": "x", "status": "running"}));
    card_api::stop_card(&f.media, &f.root, "ep", "n_stale").await.unwrap();
    assert_eq!(f.studio.body("ep", "n_stale")["status"], "stopped");
}

#[tokio::test]
async fn a_run_waits_for_a_wired_card_still_running() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_key", json!({"kind": "gen", "makes": "image", "title": "Key", "prompt": "x", "status": "running"}));
    f.studio.put("ep", "n_vid", card("video", "From the key"));
    f.studio.wire("ep", "n_key", "n_vid", "first_frame");
    let preview = card_api::preview_card(&f.media, &f.root, "ep", "n_vid", &[]).await.unwrap();
    assert_eq!(preview.waiting, ["Key"]);
    card_api::start_card(&f.media, &f.root, "ep", "n_vid").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(f.studio.body("ep", "n_vid")["status"], "queued");
    write(&f.root, "canvases/ep/assets/key.png", &fake::png());
    f.studio
        .edit("b", "ep", vec![json!({"op": "update_node", "id": "n_key", "body": {"status": "done", "takes": [{"path": "canvases/ep/assets/key.png", "media": "image", "at": 2}]}})])
        .await
        .unwrap();
    let body = f.studio.until("ep", "n_vid", "done").await;
    assert_eq!(body["inputs_used"], json!(["canvases/ep/assets/key.png"]));
    assert_eq!(f.log.last()["first_frame"], true);
}

#[tokio::test]
async fn run_card_without_a_provider_fails_with_no_provider() {
    let f = fx(&["pix"], false);
    f.studio.put("ep", "n_vid", card("video", "Waves"));
    let p = card_api::start_card(&f.media, &f.root, "ep", "n_vid").await.unwrap_err();
    assert_eq!((p.code, p.kind), ("no_provider", Some(otto_media::MediaKind::Video)));
    let body = f.studio.body("ep", "n_vid");
    assert_eq!((body["status"].as_str(), body["error_code"].as_str()), (Some("failed"), Some("no_provider")));
    // Other problems are left for the run (and fail the card then).
    f.studio.put("ep", "n_empty", card("image", ""));
    card_api::start_card(&f.media, &f.root, "ep", "n_empty").await.unwrap();
    assert_eq!(f.studio.until("ep", "n_empty", "failed").await["error_code"], "invalid");
    // No Studio block, no Silo.
    f.studio.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(card_api::start_card(&f.media, &f.root, "ep", "n_vid").await.unwrap_err().code, "not_found");
    let g = fx(ALL, true);
    assert_eq!(card_api::start_card(&g.media, &g.root, "ep", "n_vid").await.unwrap_err().code, "no_silo");
}

#[tokio::test]
async fn preview_reports_ports_fits_and_problems() {
    let f = fx(&["pix", "vid"], false);
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.put("ep", "n_vid", card("video", "Waves"));
    f.studio.wire("ep", "n_plate", "n_vid", "first_frame");
    let p = card_api::preview_card(&f.media, &f.root, "ep", "n_vid", &["vid-1".into(), "agg-motion".into()]).await.unwrap();
    let compiled = p.compiled.unwrap();
    assert_eq!((compiled.tool.as_str(), compiled.model.as_str(), compiled.provider.as_str()), ("generate_video", "vid-1", "vid"));
    assert_eq!(compiled.files[0].field, "first_frame");
    assert_eq!(p.ports, ["first_frame", "last_frame", "reference", "text"]);
    assert_eq!(p.fits.iter().map(|f| (f.model.as_str(), f.fits)).collect::<Vec<_>>(), [("vid-1", true), ("agg-motion", true)]);
    assert!(p.error.is_none());
    f.studio.put("ep", "n_empty", card("image", ""));
    let p = card_api::preview_card(&f.media, &f.root, "ep", "n_empty", &[]).await.unwrap();
    assert_eq!(p.error.unwrap().code, "invalid");
    assert!(p.compiled.is_none());
    assert_eq!(card_api::preview_card(&f.media, &f.root, "ep", "n_missing", &[]).await.unwrap_err().code, "not_found");
}

#[tokio::test]
async fn recover_marks_cards_a_previous_process_left_running_stopped() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_a", json!({"kind": "gen", "makes": "image", "prompt": "x", "status": "running"}));
    f.studio.put(
        "ep",
        "n_b",
        json!({"kind": "gen", "makes": "image", "prompt": "x", "status": "done", "takes": [{"path": "p.png", "media": "image", "at": 1}]}),
    );
    let runs = f.dir.path().join("state/studio-runs.json");
    std::fs::create_dir_all(runs.parent().unwrap()).unwrap();
    let list = json!([format!("{}|ep|n_a", f.root.display()), format!("{}|ep|n_b", f.root.display())]);
    std::fs::write(&runs, list.to_string()).unwrap();
    // Silo isn't answering at first: the list is kept and tried again.
    f.studio.down.store(3, std::sync::atomic::Ordering::SeqCst);
    let media = f.media.clone();
    let recovering = tokio::spawn(async move { card_api::recover_every(&media, Duration::from_millis(20)).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    let kept: Vec<String> = serde_json::from_slice(&std::fs::read(&runs).unwrap()).unwrap();
    assert_eq!(kept.len(), 2, "kept while Silo is down: {kept:?}");
    tokio::time::timeout(Duration::from_secs(5), recovering).await.unwrap().unwrap();
    assert_eq!(f.studio.body("ep", "n_a")["status"], "stopped");
    assert_eq!(f.studio.body("ep", "n_b")["status"], "done", "a finished card stays");
    let left: Vec<String> = serde_json::from_slice(&std::fs::read(&runs).unwrap()).unwrap();
    assert!(left.is_empty(), "{left:?}");
}

// canvas_run.

#[derive(Default)]
struct Mail(Mutex<Vec<String>>);
impl Mailbox for Mail {
    fn deliver(&self, text: String) {
        self.0.lock().unwrap().push(text);
    }
}

async fn mails(mail: &Mail, n: usize) -> Vec<String> {
    for _ in 0..1000 {
        if mail.0.lock().unwrap().len() >= n {
            return mail.0.lock().unwrap().clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{} messages, wanted {n}", mail.0.lock().unwrap().len())
}

#[tokio::test]
async fn canvas_run_runs_cards_as_jobs_in_dependency_order() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_key", json!({"kind": "gen", "makes": "image", "title": "Keyframe 1", "prompt": "Mara at the rail"}));
    f.studio.put("ep", "n_vid", card("video", "She turns"));
    f.studio.wire("ep", "n_key", "n_vid", "first_frame");
    let (mail, jobs): (Arc<Mail>, Arc<Jobs>) = (Arc::default(), Arc::default());
    let mut cx = f.cx();
    cx.mailbox = Some(mail.clone());
    cx.jobs = Some(jobs.clone());
    let out = f.tool("canvas_run", json!({"canvas": "ep", "nodes": ["n_vid", "n_key"]}), cx).await.unwrap();
    let text = text_of(&out);
    assert_eq!(text.matches("Started media").count(), 2, "{text}");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("Keyframe 1 (n_key)") && lines[1].starts_with("Video n_vid (n_vid)"), "{text}");
    // Both queued in the first edit.
    let first = f.studio.ops.lock().unwrap()[0..2].to_vec();
    assert!(first.iter().all(|(_, op)| op["body"]["status"] == "queued"), "{first:?}");
    let got = mails(&mail, 2).await;
    for m in &got {
        assert!(m.contains("[generated att_") && m.contains("Placed on canvas ep as "), "{m}");
    }
    assert!(got[0].contains("as n_key") && got[1].contains("as n_vid"), "the keyframe first: {got:?}");
    let vid = f.studio.body("ep", "n_vid");
    assert_eq!(vid["status"], "done");
    let key_take = f.studio.body("ep", "n_key")["takes"][0]["path"].clone();
    assert_eq!(vid["inputs_used"], json!([key_take]));
    assert_eq!(f.log.all().iter().map(|r| r["tool"].as_str().unwrap()).collect::<Vec<_>>(), ["image", "video"]);
    // The card's job while it ran.
    let jobbed = f.studio.updates("n_vid").iter().any(|b| b["job"].as_str().is_some_and(|j| !j.is_empty()));
    assert!(jobbed, "{:?}", f.studio.updates("n_vid"));
    assert!(vid.get("job").is_none(), "cleared when done");
}

#[tokio::test]
async fn canvas_run_fails_a_card_wired_from_a_draft_outside_the_set() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_key", json!({"kind": "gen", "makes": "image", "title": "Keyframe 3", "prompt": "x"}));
    f.studio.put("ep", "n_vid", card("video", "From it"));
    f.studio.wire("ep", "n_key", "n_vid", "first_frame");
    let mut cx = f.cx();
    cx.jobs = Some(Arc::default());
    cx.mailbox = Some(Arc::new(Mail::default()));
    let out = f.tool("canvas_run", json!({"canvas": "ep", "nodes": ["n_vid"]}), cx).await.unwrap();
    assert_eq!(text_of(&out), "Video n_vid (n_vid) not run: Run Keyframe 3 first");
    let body = f.studio.body("ep", "n_vid");
    assert_eq!((body["status"].as_str(), body["error_code"].as_str()), (Some("failed"), Some("input")));
    assert!(f.log.all().is_empty());
    let empty = f.tool("canvas_run", json!({"canvas": "ep", "nodes": []}), f.cx()).await.unwrap_err();
    assert_eq!(empty.kind, otto_tools::ToolErrorKind::InvalidInput);
}

// generate_* {canvas}.

#[tokio::test]
async fn a_generation_aimed_at_a_canvas_makes_a_wired_card_with_its_take() {
    let f = fx(ALL, false);
    f.studio.put("ep", "f_1", json!({"kind": "frame", "title": "Scene 1", "role": "scene"}));
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    let input = json!({
        "prompt": "Mara on the dock", "quality": "high",
        "references": ["world/characters/mara/sheet.png", "plates/harbor.png", "plates/dock.png", "world/characters/mara/refs/face.png"],
        "canvas": {"name": "ep", "frame": "f_1", "title": "Hero shot"}
    });
    let out = f.tool("generate_image", input, f.cx()).await.unwrap();
    let text = text_of(&out);
    let node = text.rsplit("Placed on canvas ep as ").next().unwrap().lines().next().unwrap().to_owned();
    assert!(text.contains("[generated att_"), "a session keeps its attachment: {text}");
    let card = f.studio.node("ep", &node);
    assert_eq!(card.parent.as_deref(), Some("f_1"));
    let body = serde_json::to_value(&card.body).unwrap();
    assert_eq!(
        (body["title"].as_str(), body["prompt"].as_str(), body["status"].as_str()),
        (Some("Hero shot"), Some("Mara on the dock"), Some("done"))
    );
    assert_eq!(body["options"], json!({"quality": "high"}));
    assert_eq!(body["takes"].as_array().unwrap().len(), 1);
    assert!(f.root.join(body["takes"][0]["path"].as_str().unwrap()).is_file());
    // Wires: the plate's node, an Entity node for the sheet, an Asset node for the ref; dock.png is no node's.
    let nodes = f.studio.nodes("ep");
    let entity =
        nodes.iter().find(|n| matches!(&n.body, silo_proto::studio::NodeBody::Entity { id } if id == "characters/mara")).expect("an Entity node");
    let face = nodes
        .iter()
        .find(|n| matches!(&n.body, silo_proto::studio::NodeBody::Asset { path, .. } if path.ends_with("face.png")))
        .expect("an Asset for the ref");
    assert_eq!((entity.parent.as_deref(), face.parent.as_deref()), (Some("f_1"), Some("f_1")));
    let wires: Vec<(String, String)> = f
        .studio
        .edges("ep")
        .iter()
        .filter(|e| e.to == node)
        .map(|e| (e.from.clone(), serde_json::to_value(e.role).unwrap().as_str().unwrap().to_owned()))
        .collect();
    let froms: Vec<&str> = wires.iter().map(|(f, _)| f.as_str()).collect();
    assert_eq!(froms.len(), 3, "{wires:?}");
    assert!(froms.contains(&"n_plate") && froms.contains(&entity.id.as_str()) && froms.contains(&face.id.as_str()));
    assert!(wires.iter().all(|(_, r)| r == "reference"));
    // The request: the unwired file first, then the wires; the style line at the end.
    assert_eq!(f.log.last()["references"], 4);
    let running = f.studio.updates(&node).into_iter().find(|b| b["status"] == "running").unwrap();
    assert_eq!(running["input"]["references"][0], "plates/dock.png");
    assert!(running["input"]["prompt"].as_str().unwrap().ends_with(STYLE));
    assert_eq!(running["input"]["aspect_ratio"], "16:9", "sized from the canvas");
}

#[tokio::test]
async fn a_generation_can_add_takes_to_an_existing_card() {
    let f = fx(ALL, false);
    write(&f.root, "canvases/ep/assets/old.png", &fake::png());
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.put(
        "ep",
        "n_img",
        json!({"kind": "gen", "makes": "image", "prompt": "Old", "status": "done",
        "takes": [{"path": "canvases/ep/assets/old.png", "media": "image", "at": 1}]}),
    );
    f.studio.wire("ep", "n_plate", "n_img", "reference");
    let input = json!({"prompt": "Make it night", "n": 2, "references": ["canvases/ep/assets/old.png"], "canvas": {"name": "ep", "node": "n_img", "style": false}});
    let out = f.tool("generate_image", input, f.cx()).await.unwrap();
    assert!(text_of(&out).contains("Placed on canvas ep as n_img"));
    let body = f.studio.body("ep", "n_img");
    assert_eq!((body["takes"].as_array().unwrap().len(), body["pick"].as_u64()), (3, Some(1)), "n results, n takes");
    assert_eq!((body["prompt"].as_str(), body["options"]["style"].as_bool()), (Some("Make it night"), Some(false)));
    assert!(f.studio.edges("ep").iter().all(|e| e.to != "n_img"), "its own take is a file, its old wires go");
    let req = f.log.last();
    assert_eq!((req["prompt"].as_str(), req["references"].as_u64(), req["n"].as_u64()), (Some("Make it night"), Some(1), Some(2)));
}

#[tokio::test]
async fn video_and_audio_generations_land_on_the_canvas_too() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    let out = f
        .tool(
            "generate_video",
            json!({"prompt": "Tide", "first_frame": "plates/harbor.png", "provider": "vid", "background": false, "canvas": {"name": "ep"}}),
            f.cx(),
        )
        .await
        .unwrap();
    let node = text_of(&out).rsplit("as ").next().unwrap().lines().next().unwrap().to_owned();
    let body = f.studio.body("ep", &node);
    assert_eq!((body["makes"].as_str(), body["model"].as_str(), body["status"].as_str()), (Some("video"), Some("vid/vid-1"), Some("done")));
    assert_eq!(f.studio.edges("ep").iter().filter(|e| e.to == node).count(), 1);
    assert_eq!(f.log.last()["prompt"], format!("Tide\n\n{STYLE}\n\n{MOTION}"), "the style line, then the motion line");
    let out = f
        .tool("generate_audio", json!({"type": "music", "text": "Sea shanty", "canvas": {"name": "ep"}, "background": false}), f.cx())
        .await
        .unwrap();
    let node = text_of(&out).rsplit("as ").next().unwrap().lines().next().unwrap().to_owned();
    let body = f.studio.body("ep", &node);
    assert_eq!((body["makes"].as_str(), body["takes"][0]["media"].as_str()), (Some("music"), Some("audio")));
    assert_eq!(f.log.last()["text"], "Sea shanty", "never a style line on audio");
}

#[tokio::test]
async fn without_silo_a_canvas_target_saves_into_its_assets() {
    let f = fx(ALL, true);
    let out = f.tool("generate_image", json!({"prompt": "A buoy", "canvas": {"name": "ep"}}), f.cx()).await.unwrap();
    let text = text_of(&out);
    assert!(text.contains("The canvas wasn't updated"), "{text}");
    assert!(f.root.join("canvases/ep/assets/a-buoy.png").is_file(), "{text}");
    let req = f.log.last();
    assert_eq!(req["prompt"], format!("A buoy\n\n{STYLE}"), "the style line, as a card would");
    assert_eq!(req["aspect_ratio"], "16:9", "the project's format");
    // style: false, a canvas override, an explicit aspect wins.
    write(&f.root, "canvases/tall/canvas.json", br#"{"version":1,"title":"Tall","format":"portrait","nodes":[],"edges":[]}"#);
    f.tool("generate_image", json!({"prompt": "A mast", "canvas": {"name": "tall", "style": false}}), f.cx()).await.unwrap();
    let req = f.log.last();
    assert_eq!((req["prompt"].as_str(), req["aspect_ratio"].as_str()), (Some("A mast"), Some("3:4")));
    f.tool("generate_image", json!({"prompt": "A mast", "aspect_ratio": "1:1", "canvas": {"name": "tall"}}), f.cx()).await.unwrap();
    assert_eq!(f.log.last()["aspect_ratio"], "1:1");
    // Video: the style line, then the motion line; style: false neither.
    let video = |style: bool| json!({"prompt": "Tide", "provider": "vid", "background": false, "canvas": {"name": "ep", "style": style}});
    f.tool("generate_video", video(true), f.cx()).await.unwrap();
    assert_eq!(f.log.last()["prompt"], format!("Tide\n\n{STYLE}\n\n{MOTION}"));
    f.tool("generate_video", video(false), f.cx()).await.unwrap();
    assert_eq!(f.log.last()["prompt"], "Tide");
    // Outside a studio: refused.
    let mut cx = f.cx();
    cx.cwd = f.dir.path().to_path_buf();
    let e = f.tool("generate_image", json!({"prompt": "x", "canvas": {"name": "ep"}}), cx).await.unwrap_err();
    assert!(e.message.contains("isn't in a studio"), "{}", e.message);
}

#[tokio::test]
async fn compile_takes_extra_files_before_the_wires() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.put("ep", "n_img", card("image", "x"));
    f.studio.wire("ep", "n_plate", "n_img", "reference");
    let card = f.card("n_img").await;
    let mut extra = Map::new();
    extra.insert("references".into(), json!(["https://example.com/a.png"]));
    let c = card_api::compile(&f.media, &card, &Setup::default(), &extra).await.unwrap();
    assert_eq!(c.input["references"], json!(["https://example.com/a.png", "plates/harbor.png"]));
    assert_eq!(c.prompt, "x", "no style line without one");
    let _: Value = c.input;
}

// Mentions (docs/STUDIO.md §6.2): tokens in a card's own prompt expand to what the model gets.

#[tokio::test]
async fn mentions_expand_by_the_port_they_are_wired_at() {
    let f = fx(ALL, false);
    write(&f.root, "world/places/harbor/harbor.md", b"---\nname: The Harbor\n---\n");
    f.studio.put("ep", "n_mara", json!({"kind": "entity", "id": "characters/mara"}));
    f.studio.put("ep", "n_plate", asset("plates/harbor.png", "image"));
    f.studio.put("ep", "n_dock", asset("plates/dock.png", "image"));
    f.studio.put("ep", "n_note", note("Dawn light\nlow and cold"));
    let own = "@characters/mara (again: @characters/mara) on @#n_dock, lit as @#n_note. Behind her @places/harbor; mail me@x.com, @#n_nope.";
    f.studio.put("ep", "n_img", card("image", own));
    f.studio.wire("ep", "n_plate", "n_img", "reference");
    f.studio.wire("ep", "n_mara", "n_img", "reference");
    f.studio.wire("ep", "n_dock", "n_img", "reference");
    f.studio.wire("ep", "n_note", "n_img", "text");
    let c = f.compile("n_img").await.unwrap();
    let expanded =
        "Mara Venn (image 2) (again: Mara Venn (image 2)) on dock.png (image 3), lit as Dawn light. Behind her The Harbor; mail me@x.com, @#n_nope.";
    assert_eq!(c.prompt, format!("{expanded}\n\nDawn light\nlow and cold\n\n{STYLE}"));
    assert_eq!(c.input["prompt"].as_str(), Some(c.prompt.as_str()));
    assert!(c.notes.contains(&"The Harbor is mentioned but not wired.".to_owned()), "{:?}", c.notes);
    assert_eq!(c.notes.iter().filter(|n| n.contains("mentioned")).count(), 1, "{:?}", c.notes);
    assert_eq!(f.studio.body("ep", "n_img")["prompt"], own, "the card keeps its tokens");

    // Files a generate_* {canvas} call adds go first: the wired references count after them.
    let resolved = f.card("n_img").await;
    let mut extra = Map::new();
    extra.insert("references".into(), json!(["https://example.com/a.png", "canvases/ep/assets/own.png"]));
    let c = card_api::compile(&f.media, &resolved, &Setup::default(), &extra).await.unwrap();
    assert!(c.prompt.starts_with("Mara Venn (image 4) (again: Mara Venn (image 4)) on dock.png (image 5)"), "{}", c.prompt);
    assert_eq!(c.input["references"][3], "world/characters/mara/sheet.png");

    // Frames through the quick video tool.
    f.studio.put("ep", "n_vid", card("video", "From @#n_plate to @#n_dock, @characters/mara walks."));
    f.studio.wire("ep", "n_mara", "n_vid", "reference");
    f.studio.wire("ep", "n_dock", "n_vid", "last_frame");
    f.studio.wire("ep", "n_plate", "n_vid", "first_frame");
    let c = f.compile("n_vid").await.unwrap();
    assert!(c.prompt.starts_with("From harbor.png (the first frame) to dock.png (the last frame), Mara Venn (image 1) walks."), "{}", c.prompt);

    // A middle frame (a mode's field).
    f.studio.put("ep", "n_mid", card("video", "Through @#n_dock"));
    f.studio.wire("ep", "n_plate", "n_mid", "first_frame");
    f.studio.wire("ep", "n_dock", "n_mid", "middle_frame");
    let c = f.compile("n_mid").await.unwrap();
    assert!(c.prompt.starts_with("Through dock.png (the middle frame)"), "{}", c.prompt);

    // Video and audio by number; a second video the mode's one field drops is the bare label.
    write(&f.root, "clips/surf.mp4", &fake::mp4());
    f.studio.put("ep", "n_wave", asset("clips/wave.mp4", "video"));
    f.studio.put("ep", "n_surf", asset("clips/surf.mp4", "video"));
    f.studio.put("ep", "n_gulls", asset("sounds/gulls.mp3", "audio"));
    f.studio.put("ep", "n_mix", card("video", "Move like @#n_wave, not @#n_surf, to @#n_gulls with @characters/mara"));
    f.studio.wire("ep", "n_wave", "n_mix", "video");
    f.studio.wire("ep", "n_surf", "n_mix", "video");
    f.studio.wire("ep", "n_gulls", "n_mix", "audio");
    f.studio.wire("ep", "n_mara", "n_mix", "reference");
    let c = f.compile("n_mix").await.unwrap();
    assert_eq!(c.mode.as_deref(), Some("reference-to-video"));
    assert!(c.prompt.starts_with("Move like wave.mp4 (video 1), not surf.mp4, to gulls.mp3 (audio 1) with Mara Venn (image 1)"), "{}", c.prompt);
    assert!(!c.notes.iter().any(|n| n.contains("mentioned")), "surf.mp4 is wired: {:?}", c.notes);
}

#[tokio::test]
async fn a_mentioned_text_is_its_label_and_the_take_records_the_expanded_prompt() {
    let f = fx(ALL, false);
    f.studio.put("ep", "n_note", note("Lines\nMara: Hold the rope."));
    f.studio.put("ep", "n_say", card("speech", "As @#n_note says, softly"));
    f.studio.wire("ep", "n_note", "n_say", "text");
    let c = f.compile("n_say").await.unwrap();
    assert_eq!(c.prompt, "As Lines says, softly");

    f.studio.put("ep", "n_mara", json!({"kind": "entity", "id": "characters/mara"}));
    f.studio.put("ep", "n_img", card("image", "@characters/mara at the rail"));
    f.studio.wire("ep", "n_mara", "n_img", "reference");
    let preview = card_api::preview_card(&f.media, &f.root, "ep", "n_img", &[]).await.unwrap();
    assert!(preview.compiled.unwrap().prompt.starts_with("Mara Venn (image 1) at the rail"));
    card_api::start_card(&f.media, &f.root, "ep", "n_img").await.unwrap();
    let body = f.studio.written("n_img", "done", 1).await;
    assert!(body["takes"][0]["prompt"].as_str().unwrap().starts_with("Mara Venn (image 1) at the rail"), "{body}");
    assert_eq!(body["prompt"], "@characters/mara at the rail");
    // Its file is named from the name, not the token.
    let path = body["takes"][0]["path"].as_str().unwrap();
    assert!(path.contains("/assets/mara-venn-at-the-rail"), "{path}");
}

#[tokio::test]
async fn without_silo_mentions_expand_from_the_files_on_disk() {
    let f = fx(ALL, true);
    write(&f.root, "canvases/ep/canvas.json", br#"{"version":1,"title":"Ep","nodes":[{"id":"n_dock","x":0,"y":0,"w":1,"h":1,"body":{"kind":"asset","path":"plates/dock.png","media":"image"}}],"edges":[]}"#);
    let input = json!({
        "prompt": "@characters/mara on @#n_dock by @places/nowhere",
        "references": ["plates/dock.png", "world/characters/mara/sheet.png"],
        "canvas": {"name": "ep", "style": false},
    });
    f.tool("generate_image", input, f.cx()).await.unwrap();
    assert_eq!(f.log.last()["prompt"], "Mara Venn (image 2) on dock.png (image 1) by @places/nowhere");
    let input = json!({"prompt": "@characters/mara alone", "canvas": {"name": "ep", "style": false}});
    let out = f.tool("generate_image", input, f.cx()).await.unwrap();
    assert_eq!(f.log.last()["prompt"], "Mara Venn alone");
    assert!(text_of(&out).contains("Mara Venn is mentioned but not wired."), "{}", text_of(&out));
}

/// Mara with named references: a coat with its job, the face as a plain path, a walk (a video).
fn dressed(f: &Fx) {
    write(
        &f.root,
        "world/characters/mara/mara.md",
        b"---\nname: Mara Venn\nsheet: sheet.png\nrefs:\n  - refs/face.png\n  - file: refs/coat.png\n    name: Coat\n    defines: the oilskin coat and its toggles\n  - refs/walk.mp4\n---\n",
    );
    write(&f.root, "world/characters/mara/refs/coat.png", &fake::png());
    write(&f.root, "world/characters/mara/refs/walk.mp4", &fake::mp4());
}

#[tokio::test]
async fn a_reference_mention_says_its_names_number_and_job() {
    let f = fx(ALL, false);
    dressed(&f);
    f.studio.put("ep", "n_mara", json!({"kind": "entity", "id": "characters/mara"}));
    f.studio.put("ep", "n_coat", asset("world/characters/mara/refs/coat.png", "image"));
    f.studio.put("ep", "n_face", asset("world/characters/mara/refs/face.png", "image"));
    f.studio.put("ep", "n_walk", asset("world/characters/mara/refs/walk.mp4", "video"));
    let own = "@characters/mara in @characters/mara#coat, her @characters/mara#face; @characters/mara#walk, @characters/mara#nope.";
    f.studio.put("ep", "n_img", card("image", own));
    f.studio.wire("ep", "n_mara", "n_img", "reference");
    f.studio.wire("ep", "n_coat", "n_img", "reference");
    f.studio.wire("ep", "n_face", "n_img", "reference");
    let c = f.compile("n_img").await.unwrap();
    assert!(
        c.prompt.starts_with(
            "Mara Venn (image 1) in Mara Venn, Coat (image 2: take only the oilskin coat and its toggles), her Mara Venn, face (image 3); Mara Venn, walk, @characters/mara#nope."
        ),
        "{}",
        c.prompt
    );
    assert_eq!(c.input["references"][1], "world/characters/mara/refs/coat.png");
    assert_eq!(c.files[1].label, "coat.png", "the file keeps its own label");
    assert_eq!(c.notes.iter().filter(|n| n.contains("mentioned")).collect::<Vec<_>>(), ["Mara Venn, walk is mentioned but not wired."]);

    // The reference alone, the entity unwired: the entity's note, the reference numbered first.
    f.studio.put("ep", "n_two", card("image", "@characters/mara#coat on @characters/mara"));
    f.studio.wire("ep", "n_coat", "n_two", "reference");
    let c = f.compile("n_two").await.unwrap();
    assert!(c.prompt.starts_with("Mara Venn, Coat (image 1: take only the oilskin coat and its toggles) on Mara Venn"), "{}", c.prompt);
    assert!(c.notes.contains(&"Mara Venn is mentioned but not wired.".to_owned()), "{:?}", c.notes);

    // At a frame or a video port: the existing wording with the names; no job outside the images.
    f.studio.put("ep", "n_vid", card("video", "From @characters/mara#coat"));
    f.studio.wire("ep", "n_coat", "n_vid", "first_frame");
    let c = f.compile("n_vid").await.unwrap();
    assert!(c.prompt.starts_with("From Mara Venn, Coat (the first frame)"), "{}", c.prompt);
    f.studio.put("ep", "n_mix", card("video", "Like @characters/mara#walk with @characters/mara#coat"));
    f.studio.wire("ep", "n_walk", "n_mix", "video");
    f.studio.wire("ep", "n_coat", "n_mix", "reference");
    let c = f.compile("n_mix").await.unwrap();
    assert!(c.prompt.starts_with("Like Mara Venn, walk (video 1) with Mara Venn, Coat (image 1: take only the oilskin coat"), "{}", c.prompt);

    // Its take's file is named from the names.
    card_api::start_card(&f.media, &f.root, "ep", "n_two").await.unwrap();
    let body = f.studio.written("n_two", "done", 1).await;
    let path = body["takes"][0]["path"].as_str().unwrap();
    assert!(path.contains("/assets/mara-venn-coat-on-mara-venn"), "{path}");
}

#[tokio::test]
async fn without_silo_a_reference_mention_expands_from_the_files_on_disk() {
    let f = fx(ALL, true);
    dressed(&f);
    let input = json!({
        "prompt": "@characters/mara#coat over @characters/mara#face, @characters/mara",
        "references": ["world/characters/mara/sheet.png", "world/characters/mara/refs/coat.png"],
        "canvas": {"name": "ep", "style": false},
    });
    let out = f.tool("generate_image", input, f.cx()).await.unwrap();
    assert_eq!(
        f.log.last()["prompt"],
        "Mara Venn, Coat (image 2: take only the oilskin coat and its toggles) over Mara Venn, face, Mara Venn (image 1)"
    );
    assert!(text_of(&out).contains("Mara Venn, face is mentioned but not wired."), "{}", text_of(&out));
}
