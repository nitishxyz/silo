//! A card's settings panel as data, and the exact `update_node` patch each change makes.

use serde_json::json;
use silo_proto::studio::{Canvas, EdgeRole, GenKind, NodeBody, Op};

use super::super::settings::{ChoiceField, card_settings, change_op};
use super::super::{CardSettings, Effect, ModelInfo, Point, Preview, SettingChange, SettingsInput, VoiceInfo};
use super::fixtures::*;

fn fixture() -> Canvas {
    canvas(
        vec![
            card("v", GenKind::Video, 1000.0, 0.0),
            note("n", 0.0, 0.0, "# Beat\nthe bell rings"),
            entity("mara", 0.0, 300.0, "characters/mara"),
            image("i", 0.0, 600.0),
            card("draft", GenKind::Image, 0.0, 900.0),
            entity("teo", 0.0, 1300.0, "characters/teo"),
            card("img", GenKind::Image, 2000.0, 0.0),
            card("speech", GenKind::Speech, 2000.0, 600.0),
            card("sound", GenKind::Sound, 2000.0, 900.0),
            card("music", GenKind::Music, 2000.0, 1200.0),
            frame_with_format("shot", 3000.0, 0.0, 1000.0, 1000.0, "square"),
            in_frame(card("inshot", GenKind::Image, 3100.0, 100.0), "shot"),
        ],
        // Made in this order: text, reference, first frame, reference, reference.
        vec![
            edge("e_t", "n", "v", EdgeRole::Text),
            edge("e_r1", "mara", "v", EdgeRole::Reference),
            edge("e_ff", "i", "v", EdgeRole::FirstFrame),
            edge("e_r2", "draft", "v", EdgeRole::Reference),
            edge("e_r3", "teo", "v", EdgeRole::Reference),
        ],
    )
}

fn models() -> Vec<ModelInfo> {
    let m = |id: &str, label: &str, makes, ports: &[EdgeRole]| ModelInfo {
        id: id.into(),
        label: label.into(),
        makes,
        ports: ports.to_vec(),
        available: true,
        requires: None,
        aspects: Vec::new(),
        durations: Vec::new(),
        resolutions: Vec::new(),
    };
    let mut kling = m("fal/kling", "Kling 3.0", GenKind::Video, &[EdgeRole::FirstFrame, EdgeRole::LastFrame]);
    kling.aspects = vec!["16:9".into(), "9:16".into()];
    kling.durations = vec![5.0, 10.0];
    kling.resolutions = vec!["720p".into(), "1080p".into()];
    let veo = m("google/veo", "Veo", GenKind::Video, &[EdgeRole::FirstFrame, EdgeRole::Reference, EdgeRole::Text]);
    let mut sora = m("openai/sora", "Sora", GenKind::Video, &[EdgeRole::FirstFrame, EdgeRole::Reference]);
    sora.available = false;
    sora.requires = Some("an OpenAI key".into());
    vec![kling, veo, sora, m("fal/flux", "Flux", GenKind::Image, &[EdgeRole::Reference])]
}

fn settings_of(c: &Canvas, id: &str, models: &[ModelInfo], automatic: Option<&str>) -> CardSettings {
    let p = project();
    let s = sections();
    let voices = [VoiceInfo { id: "eleven/mara".into(), label: "Mara".into() }];
    card_settings(c, id, &SettingsInput { project: Some(&p), sections: &s, models, automatic, voices: &voices, preview: None }).unwrap()
}

fn labels(f: &ChoiceField) -> Vec<&str> {
    f.choices.iter().map(|c| c.label.as_str()).collect()
}

fn selected(f: &ChoiceField) -> &str {
    f.selected().map(|c| c.label.as_str()).unwrap_or("<none>")
}

#[test]
fn fields_per_card_kind() {
    let c = fixture();
    let s = |id| settings_of(&c, id, &[], None);
    let img = s("img");
    assert!(img.format.is_some() && img.resolution.is_some() && img.length.is_none() && img.voice.is_none());
    assert_eq!(img.style, Some(true));
    let v = s("v");
    assert!(v.format.is_some() && v.resolution.is_some() && v.length.is_some() && v.voice.is_none());
    assert_eq!(v.style, Some(true));
    let sp = s("speech");
    assert!(sp.voice.is_some() && sp.format.is_none() && sp.length.is_none() && sp.style.is_none());
    for id in ["sound", "music"] {
        let x = s(id);
        assert!(x.length.is_some() && x.voice.is_none() && x.format.is_none() && x.resolution.is_none(), "{id}");
    }
    assert!(card_settings(&c, "n", &SettingsInput::default()).is_none());
    assert_eq!(v.title, "");
    assert_eq!(v.title_placeholder, "Video v");
    assert_eq!(v.makes, GenKind::Video);
}

#[test]
fn inputs_are_numbered_in_the_order_the_model_gets_them() {
    let c = fixture();
    let v = settings_of(&c, "v", &[], None);
    let rows: Vec<(usize, usize, &str, &str)> = v.inputs.iter().map(|r| (r.n, r.in_port, r.edge.as_str(), r.label.as_str())).collect();
    assert_eq!(
        rows,
        [
            (1, 1, "e_ff", "First frame · i.png"),
            (2, 1, "e_r1", "Reference 1 · Mara Venn"),
            (3, 2, "e_r2", "Reference 2 · Image draft"),
            (4, 3, "e_r3", "Reference 3 · Teo"),
            (5, 1, "e_t", "Text 1 · Beat"),
        ]
    );
    let problems: Vec<Option<&str>> = v.inputs.iter().map(|r| r.problem.as_deref()).collect();
    assert_eq!(problems, [None, None, Some("Run Image draft first"), Some("Teo has no sheet yet"), None]);
    assert_eq!(v.inputs[1].from, "mara");
    assert_eq!(v.inputs[1].port, EdgeRole::Reference);
    assert!(v.inputs.iter().all(|r| !r.pending));
}

#[test]
fn a_running_source_is_pending() {
    let mut c = fixture();
    c.nodes[4] = with_status(card("draft", GenKind::Image, 0.0, 900.0), silo_proto::studio::GenStatus::Running);
    let v = settings_of(&c, "v", &[], None);
    assert!(v.inputs[2].pending);
    assert_eq!(v.inputs[2].problem, None);
}

#[test]
fn models_of_the_cards_kind_with_those_that_cant_take_its_inputs_dimmed() {
    let c = fixture();
    let ms = models();
    let v = settings_of(&c, "v", &ms, Some("fal/kling"));
    assert_eq!(labels(&v.model), ["Automatic · kling", "Kling 3.0", "Veo", "Sora"]);
    assert_eq!(selected(&v.model), "Automatic · kling");
    let reasons: Vec<(bool, Option<&str>)> = v.model.choices.iter().map(|c| (c.enabled, c.reason.as_deref())).collect();
    assert_eq!(reasons, [(true, None), (false, Some("Can't take reference")), (true, None), (false, Some("Needs an OpenAI key"))]);
    // Without a catalog: Automatic alone; an unknown model of its own is kept, selected.
    let mut c2 = c.clone();
    if let NodeBody::Gen { model, .. } = &mut c2.nodes[0].body {
        *model = Some("x/custom".into());
    }
    let v = settings_of(&c2, "v", &[], None);
    assert_eq!(labels(&v.model), ["Automatic", "x/custom"]);
    assert_eq!(selected(&v.model), "x/custom");
}

#[test]
fn format_inherited_from_the_effective_format_or_its_own() {
    let mut c = fixture();
    let v = settings_of(&c, "v", &[], None);
    let f = v.format.unwrap();
    assert_eq!(labels(&f), ["Inherited · 16:9 (landscape)", "16:9", "1:1", "9:16", "4:5"]);
    assert_eq!(selected(&f), "Inherited · 16:9 (landscape)");
    assert_eq!(f.choices[0].value, None);
    // A shot frame's format, then the canvas's.
    assert_eq!(selected(&settings_of(&c, "inshot", &[], None).format.unwrap()), "Inherited · 1:1 (square)");
    c.format = Some("vertical".into());
    assert_eq!(selected(&settings_of(&c, "v", &[], None).format.unwrap()), "Inherited · 9:16 (vertical)");
    // Its own, even one no preset has.
    c.nodes[0] = with_options(c.nodes[0].clone(), json!({ "aspect_ratio": "2.39:1", "resolution": "4k", "duration": 7.5 }));
    let v = settings_of(&c, "v", &[], None);
    assert_eq!(selected(v.format.as_ref().unwrap()), "2.39:1");
    assert_eq!(selected(v.resolution.as_ref().unwrap()), "4k");
    assert_eq!(selected(v.length.as_ref().unwrap()), "7.5 s");
    // The model in effect says what it offers.
    let v = settings_of(&c, "v", &models(), Some("fal/kling"));
    assert_eq!(labels(v.format.as_ref().unwrap()), ["Inherited · 9:16 (vertical)", "16:9", "9:16", "2.39:1"]);
    assert_eq!(labels(v.length.as_ref().unwrap()), ["Model default", "5 s", "10 s", "7.5 s"]);
    assert_eq!(labels(v.resolution.as_ref().unwrap()), ["Inherited · 1080p", "720p", "1080p", "4k"]);
}

#[test]
fn voice_and_style() {
    let mut c = fixture();
    let sp = settings_of(&c, "speech", &[], None);
    assert_eq!(labels(sp.voice.as_ref().unwrap()), ["Default voice", "Mara"]);
    assert_eq!(selected(sp.voice.as_ref().unwrap()), "Default voice");
    c.nodes[7] = with_options(c.nodes[7].clone(), json!({ "voice": "eleven/other" }));
    let sp = settings_of(&c, "speech", &[], None);
    assert_eq!(selected(sp.voice.as_ref().unwrap()), "eleven/other");
    c.nodes[6] = with_options(c.nodes[6].clone(), json!({ "style": false }));
    assert_eq!(settings_of(&c, "img", &[], None).style, Some(false));
}

#[test]
fn each_change_makes_its_merge_patch() {
    let patch = |ch: SettingChange| match change_op("v", &ch) {
        Op::UpdateNode { id, x: None, y: None, w: None, h: None, z: None, parent: None, body: Some(b) } if id == "v" => b,
        other => panic!("{other:?}"),
    };
    assert_eq!(patch(SettingChange::Title("  Keyframe 1 ".into())), json!({ "title": "Keyframe 1" }));
    assert_eq!(patch(SettingChange::Title("   ".into())), json!({ "title": null }));
    assert_eq!(patch(SettingChange::Model(Some("google/veo".into()))), json!({ "model": "google/veo", "mode": null }));
    assert_eq!(patch(SettingChange::Model(None)), json!({ "model": null, "mode": null }));
    assert_eq!(patch(SettingChange::Aspect(Some("9:16".into()))), json!({ "options": { "aspect_ratio": "9:16" } }));
    assert_eq!(patch(SettingChange::Aspect(None)), json!({ "options": { "aspect_ratio": null } }));
    assert_eq!(patch(SettingChange::Duration(Some(5.0))), json!({ "options": { "duration": 5.0 } }));
    assert_eq!(patch(SettingChange::Duration(None)), json!({ "options": { "duration": null } }));
    assert_eq!(patch(SettingChange::Resolution(Some("720p".into()))), json!({ "options": { "resolution": "720p" } }));
    assert_eq!(patch(SettingChange::Voice(Some("eleven/mara".into()))), json!({ "options": { "voice": "eleven/mara" } }));
    // "Use the project's style": off is `style: false`; on (the default) unsets it.
    assert_eq!(patch(SettingChange::Style(false)), json!({ "options": { "style": false } }));
    assert_eq!(patch(SettingChange::Style(true)), json!({ "options": { "style": null } }));
    // The model's own options: any top-level key, any JSON value; `null` unsets it.
    assert_eq!(patch(SettingChange::Option("seed".into(), json!(42))), json!({ "options": { "seed": 42 } }));
    assert_eq!(patch(SettingChange::Option("enhance".into(), json!(true))), json!({ "options": { "enhance": true } }));
    assert_eq!(patch(SettingChange::Option("camera".into(), json!("dolly in"))), json!({ "options": { "camera": "dolly in" } }));
    assert_eq!(patch(SettingChange::Option("seed".into(), serde_json::Value::Null)), json!({ "options": { "seed": null } }));
}

#[test]
fn a_model_option_merges_into_the_cards_options_and_null_unsets_it() {
    let mut c = fixture();
    c.nodes[0] = with_options(c.nodes[0].clone(), json!({ "aspect_ratio": "9:16" }));
    let mut b = board(&c);
    let options = |b: &super::super::Board| match &b.canvas().node("v").unwrap().body {
        NodeBody::Gen { options, .. } => options.clone(),
        other => panic!("{other:?}"),
    };
    let (_, ops) = sent(&b.change_setting("v", &SettingChange::Option("seed".into(), json!(7))));
    assert!(matches!(&ops[..], [Op::UpdateNode { id, body: Some(_), .. }] if id == "v"), "{ops:?}");
    assert_eq!(options(&b), json!({ "aspect_ratio": "9:16", "seed": 7 }), "the others kept");
    sent(&b.change_setting("v", &SettingChange::Option("seed".into(), json!(8))));
    assert_eq!(options(&b), json!({ "aspect_ratio": "9:16", "seed": 8 }), "replaced");
    sent(&b.change_setting("v", &SettingChange::Option("seed".into(), serde_json::Value::Null)));
    assert_eq!(options(&b), json!({ "aspect_ratio": "9:16" }), "unset");
    // The panel's own rows read what an option set: an aspect set as an option shows as its format.
    sent(&b.change_setting("v", &SettingChange::Option("aspect_ratio".into(), json!("1:1"))));
    assert_eq!(selected(b.settings("v", &SettingsInput::default()).unwrap().format.as_ref().unwrap()), "1:1");
    // One undo step each: undone, the aspect is back.
    sent(&b.undo());
    assert_eq!(options(&b), json!({ "aspect_ratio": "9:16" }));
}

#[test]
fn changes_through_the_board() {
    let mut c = fixture();
    c.nodes[0] = with_options(c.nodes[0].clone(), json!({ "aspect_ratio": "9:16", "duration": 5 }));
    let mut b = board(&c);
    let input = SettingsInput::default();
    assert_eq!(selected(b.settings("v", &input).unwrap().format.as_ref().unwrap()), "9:16");

    // Back to Inherited: the own aspect is gone, the rest kept.
    sent(&b.change_setting("v", &SettingChange::Aspect(None)));
    let NodeBody::Gen { options, .. } = &b.canvas().node("v").unwrap().body else { panic!() };
    assert_eq!(options, &json!({ "duration": 5 }));
    let s = b.settings("v", &input).unwrap();
    assert_eq!(selected(s.format.as_ref().unwrap()), "Inherited · 16:9 (landscape)");
    // The card's face follows: the project's 16:9.
    close(b.laid("v").unwrap().card.unwrap().face.h, 180.0);

    sent(&b.change_setting("v", &SettingChange::Style(false)));
    assert_eq!(b.settings("v", &input).unwrap().style, Some(false));
    sent(&b.change_setting("v", &SettingChange::Style(true)));
    assert_eq!(b.settings("v", &input).unwrap().style, Some(true));
    sent(&b.change_setting("v", &SettingChange::Title("Shot 1".into())));
    assert_eq!(b.settings("v", &input).unwrap().title, "Shot 1");

    // × on an input unwires it; clicking one reveals and selects its source.
    let (_, ops) = sent(&b.unwire("e_r2"));
    assert_eq!(ops, [Op::RemoveEdge { id: "e_r2".into() }]);
    assert_eq!(b.settings("v", &input).unwrap().inputs.len(), 4);
    let _ = b.set_camera(super::super::Camera { pan: Point::new(-5000.0, -5000.0), zoom: 1.0 });
    let fx = b.reveal("teo");
    assert!(fx.contains(&Effect::Selection));
    assert_eq!(b.selection(), ["teo".to_owned()]);
    assert!(b.visible_rect().contains_rect(b.laid("teo").unwrap().rect));

    // The preview is passed through.
    let p = Preview { prompt: "x".into(), ..Preview::default() };
    let s = b.settings("v", &SettingsInput { preview: Some(&p), ..SettingsInput::default() }).unwrap();
    assert_eq!(s.preview, Some(p));
}
