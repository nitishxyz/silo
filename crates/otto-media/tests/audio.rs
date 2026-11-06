//! Audio providers against a fake server: ElevenLabs speech, sound effects, music and voices; OpenAI speech.
mod fake;

use fake::{json, mp3, serve};
use otto_media::providers::{ElevenLabs, OpenAi};
use otto_media::{AudioRequest, AudioType, Call, MediaErrorKind, MediaProvider};
use serde_json::json;
use std::sync::Arc;

fn call() -> Call {
    Call { http: reqwest::Client::new(), cancel: Default::default(), progress: Arc::new(|_| {}) }
}

fn request(kind: AudioType, voice: Option<&str>, duration: Option<f64>) -> AudioRequest {
    AudioRequest { kind, text: "Welcome aboard".into(), model: None, voice: voice.map(str::to_owned), duration }
}

/// Answers voices and every generation with an MP3.
async fn elevenlabs() -> (ElevenLabs, std::sync::Arc<std::sync::Mutex<Vec<fake::Got>>>) {
    let (base, got) = serve(|got, _| {
        if got.path.starts_with("/v2/voices") {
            json(200, json!({"voices": [
                {"voice_id": "JBFqnCBsd6RMkjVDRZzb", "name": "George - Warm Storyteller", "description": "Warm British storyteller", "category": "premade"},
                {"voice_id": "XrExE9yKIg1WjnnlVkGX", "name": "Matilda", "labels": {"accent": "american", "gender": "female"}}
            ], "has_more": false}))
        } else {
            fake::bytes("audio/mpeg", mp3())
        }
    })
    .await;
    (ElevenLabs::new(base), got)
}

#[tokio::test]
async fn elevenlabs_speech_with_the_default_voice_needs_no_lookup() {
    let (el, got) = elevenlabs().await;
    let out = el.generate_audio("el-key", request(AudioType::Speech, None, None), &call()).await.unwrap();
    assert_eq!((out.model.as_str(), out.media[0].mime.as_str()), ("eleven_v4", "audio/mpeg"));
    assert_eq!(out.notes, ["Voice George (JBFqnCBsd6RMkjVDRZzb)."]);
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].method.as_str(), got[0].path.as_str()), ("POST", "/v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb?output_format=mp3_44100_128"));
    assert_eq!(got[0].header("xi-api-key"), Some("el-key"));
    assert_eq!(got[0].json(), json!({"text": "Welcome aboard", "model_id": "eleven_v4"}));
}

#[tokio::test]
async fn elevenlabs_resolves_voice_names_through_a_cached_list() {
    let (el, got) = elevenlabs().await;
    for asked in ["matilda", "George", "XrExE9yKIg1WjnnlVkGX"] {
        el.generate_audio("el-key", request(AudioType::Speech, Some(asked), None), &call()).await.unwrap();
    }
    let error = el.generate_audio("el-key", request(AudioType::Speech, Some("Nobody Here"), None), &call()).await.unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::Invalid);
    assert_eq!(error.message, "No ElevenLabs voice Nobody Here; list_media_models with kind audio lists them");
    let paths: Vec<String> = got.lock().unwrap().iter().map(|g| g.path.clone()).collect();
    assert_eq!(
        paths,
        [
            "/v2/voices?page_size=100",
            "/v1/text-to-speech/XrExE9yKIg1WjnnlVkGX?output_format=mp3_44100_128",
            "/v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb?output_format=mp3_44100_128",
            "/v1/text-to-speech/XrExE9yKIg1WjnnlVkGX?output_format=mp3_44100_128"
        ],
        "the voice list is fetched once"
    );
    let voices = el.voices(&reqwest::Client::new(), Some("el-key")).await;
    assert_eq!(voices.iter().map(|v| (v.name.as_str(), v.default)).collect::<Vec<_>>(), [("George - Warm Storyteller", true), ("Matilda", false)]);
    assert_eq!(voices[1].about, "american, female");
}

#[tokio::test]
async fn elevenlabs_sound_effects_and_music() {
    let (el, got) = elevenlabs().await;
    let out = el.generate_audio("k", request(AudioType::Sound, Some("George"), Some(45.0)), &call()).await.unwrap();
    assert_eq!(out.model, "eleven_text_to_sound_v3");
    assert_eq!(out.notes, ["Sound effects are 0.5–30 s: made 30 s.", "Voices are for speech; sound effects has none."]);
    let mut music = request(AudioType::Music, None, Some(12.5));
    music.model = Some("eleven_v3".into());
    let out = el.generate_audio("k", music, &call()).await.unwrap();
    assert_eq!((out.model.as_str(), out.notes.as_slice()), ("music_v2_5", &["eleven_v3 doesn't make music: used music_v2_5.".to_owned()][..]));
    let got = got.lock().unwrap();
    assert_eq!(got[0].path, "/v1/sound-generation?output_format=mp3_44100_128");
    assert_eq!(got[0].json(), json!({"text": "Welcome aboard", "model_id": "eleven_text_to_sound_v3", "duration_seconds": 30.0}));
    assert_eq!(got[1].path, "/v1/music?output_format=mp3_44100_128");
    assert_eq!(got[1].json(), json!({"prompt": "Welcome aboard", "model_id": "music_v2_5", "music_length_ms": 12500}));
}

#[tokio::test]
async fn openai_speech_and_what_it_refuses() {
    let (base, got) = serve(|_, _| fake::bytes("audio/mpeg", mp3())).await;
    let openai = OpenAi::new(&base);
    let out = openai.generate_audio("sk-k", request(AudioType::Speech, Some("Cedar"), None), &call()).await.unwrap();
    assert_eq!((out.model.as_str(), out.media[0].mime.as_str()), ("gpt-4o-mini-tts", "audio/mpeg"));
    let sent = got.lock().unwrap()[0].clone();
    assert_eq!((sent.path.as_str(), sent.header("authorization")), ("/v1/audio/speech", Some("Bearer sk-k")));
    assert_eq!(sent.json(), json!({"model": "gpt-4o-mini-tts", "input": "Welcome aboard", "voice": "cedar", "response_format": "mp3"}));
    let error = openai.generate_audio("sk-k", request(AudioType::Music, None, None), &call()).await.unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::Invalid);
    let voices = openai.voices(&reqwest::Client::new(), None).await;
    assert_eq!((voices.len(), voices[0].id.as_str(), voices[0].default), (13, "marin", true));
}
