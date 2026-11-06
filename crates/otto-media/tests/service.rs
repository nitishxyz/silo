//! Keys (docs/MEDIA.md §2.1), status and defaults.
use otto_llm::auth::{Credential, CredentialStore};
use otto_media::{Media, MediaErrorKind, MediaKeySource, MediaKind, providers};
use otto_proto::MediaChoice;
use std::collections::HashMap;
use std::sync::Arc;

fn media(env: &[(&str, &str)]) -> (tempfile::TempDir, CredentialStore, Arc<Media>) {
    let dir = tempfile::tempdir().unwrap();
    let store = CredentialStore::open(dir.path().join("auth")).unwrap();
    let env: HashMap<String, String> = env.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
    let media = Media::with(store.clone(), dir.path().into(), providers::all(), Arc::new(move |name: &str| env.get(name).cloned()));
    (dir, store, media)
}

fn source(media: &Media, id: &str) -> Option<(String, MediaKeySource)> {
    media.key(media.provider(id).unwrap().as_ref())
}

#[test]
fn media_key_then_chat_api_key_then_environment() {
    let (_dir, store, media) = media(&[("OPENAI_API_KEY", "env-key"), ("FAL_KEY", "fal-env")]);
    assert_eq!(source(&media, "openai"), Some(("env-key".into(), MediaKeySource::Env)));
    store
        .set(
            "openai",
            Credential::OAuth { access: "a".into(), refresh: "r".into(), expires_at_ms: u64::MAX, account_id: None, extra: serde_json::Value::Null },
        )
        .unwrap();
    assert_eq!(source(&media, "openai").unwrap().1, MediaKeySource::Env, "a subscription sign-in never counts");
    store.set("openai", Credential::ApiKey { key: "chat-key".into() }).unwrap();
    assert_eq!(source(&media, "openai"), Some(("chat-key".into(), MediaKeySource::Chat)));
    let info = media.set_key("OpenAI", Some(" media-key ".into())).unwrap();
    assert_eq!((info.connected, info.source), (true, Some(MediaKeySource::Media)));
    assert_eq!(source(&media, "openai"), Some(("media-key".into(), MediaKeySource::Media)));
    assert_eq!(store.get("media:openai").unwrap(), Some(Credential::ApiKey { key: "media-key".into() }));
    media.set_key("openai", Some(String::new())).unwrap();
    assert_eq!(source(&media, "openai").unwrap().1, MediaKeySource::Chat, "an empty key removes the media key");
    store.set("fal", Credential::ApiKey { key: "not-a-chat-provider".into() }).unwrap();
    assert_eq!(source(&media, "fal"), Some(("fal-env".into(), MediaKeySource::Env)), "fal has no chat key");
    assert_eq!(source(&media, "xai"), None);
}

#[test]
fn higgsfield_keys_are_id_and_secret() {
    let (_dir, _store, media) = media(&[("HF_API_KEY_ID", "id1"), ("HF_API_KEY_SECRET", "s1")]);
    assert_eq!(source(&media, "higgsfield"), Some(("id1:s1".into(), MediaKeySource::Env)));
    let error = media.set_key("higgsfield", Some("just-one-part".into())).unwrap_err();
    assert_eq!(error.kind, MediaErrorKind::Invalid);
    media.set_key("higgsfield", Some("id2:s2".into())).unwrap();
    assert_eq!(source(&media, "higgsfield"), Some(("id2:s2".into(), MediaKeySource::Media)));
}

#[test]
fn status_lists_every_provider_with_kinds_and_defaults() {
    let (_dir, _store, media) = media(&[("ELEVENLABS_API_KEY", "el")]);
    let status = media.status();
    let ids: Vec<&str> = status.providers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["openai", "google", "xai", "higgsfield", "fal", "elevenlabs"]);
    let higgsfield = &status.providers[3];
    assert_eq!(higgsfield.kinds, [MediaKind::Image, MediaKind::Video]);
    assert!(higgsfield.models.iter().any(|m| m.kind == MediaKind::Video && m.default));
    let elevenlabs = &status.providers[5];
    assert_eq!((elevenlabs.connected, elevenlabs.kinds.as_slice()), (true, &[MediaKind::Audio][..]));
    for provider in &status.providers {
        for kind in &provider.kinds {
            assert_eq!(provider.models.iter().filter(|m| m.kind == *kind && m.default).count(), 1, "{} {kind:?}", provider.id);
        }
    }
    let defaults: Vec<(&str, &str)> =
        status.providers[..5].iter().map(|p| (p.id.as_str(), p.models.iter().find(|m| m.default).unwrap().id.as_str())).collect();
    assert_eq!(
        defaults,
        [
            ("openai", "gpt-image-2.6-flare"),
            ("google", "gemini-nano-banana-2.1"),
            ("xai", "grok-imagine-image-2.0"),
            ("higgsfield", "higgsfield-ai/soul/v2/standard"),
            ("fal", "blackforestlabs/flux-3/text-to-image")
        ]
    );
}

#[test]
fn defaults_are_checked_and_kept() {
    let (dir, _store, media) = media(&[]);
    assert_eq!(media.set_default(MediaKind::Image, Some("nope".into()), None).unwrap_err().kind, MediaErrorKind::Invalid);
    assert_eq!(media.set_default(MediaKind::Video, Some("openai".into()), None).unwrap_err().kind, MediaErrorKind::Invalid);
    assert_eq!(media.set_default(MediaKind::Image, None, Some("m".into())).unwrap_err().kind, MediaErrorKind::Invalid);
    let status = media.set_default(MediaKind::Image, Some("Gemini".into()), Some("gemini-3-pro-image".into())).unwrap();
    let choice = MediaChoice { provider: Some("google".into()), model: Some("gemini-3-pro-image".into()) };
    assert_eq!(status.defaults.get(&MediaKind::Image), Some(&choice));
    assert!(dir.path().join("media.json").exists());
    assert_eq!(media.status().defaults.get(&MediaKind::Image), Some(&choice));
    assert!(media.set_default(MediaKind::Image, None, None).unwrap().defaults.is_empty());
    assert!(media.set_key("midjourney", Some("k".into())).is_err());
}

#[test]
fn handles_are_slugged_names_unique_over_the_catalog() {
    assert_eq!(otto_media::slug("GPT Image 2.6 Flare"), "gpt-image-2.6-flare");
    assert_eq!(otto_media::slug("GPT-4o mini TTS (2025-12-15)"), "gpt-4o-mini-tts-2025-12-15");
    let (_dir, _store, media) = media(&[]);
    let status = media.status();
    let mut seen = std::collections::HashSet::new();
    for p in &status.providers {
        for m in &p.models {
            assert!(m.handle.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '-')), "{}", m.handle);
            assert!(seen.insert(m.handle.clone()), "{} is taken twice", m.handle);
            let (provider, _, id, name, kind) = media.resolve_handle(&m.handle.to_uppercase()).unwrap();
            assert_eq!((provider.as_str(), id.as_str(), name.as_str(), kind), (p.id.as_str(), m.id.as_str(), m.name.as_str(), m.kind));
        }
    }
    let handle =
        |p: &str, name: &str| status.providers.iter().find(|i| i.id == p).unwrap().models.iter().find(|m| m.name == name).unwrap().handle.clone();
    assert_eq!(handle("higgsfield", "Kling 3.0 Pro"), "kling-3.0-pro");
    assert_eq!(handle("fal", "Kling 3.0 Pro"), "fal-kling-3.0-pro", "the first in list order keeps the plain slug");
    assert_eq!(media.resolve_handle("no-such-model"), None);
    // Live catalog entries after the built-in ones never change a built-in handle.
    let mut names: Vec<_> = status.providers.iter().flat_map(|p| p.models.iter().map(|m| (p.id.as_str(), m.name.as_str()))).collect();
    let built_in = otto_media::handles(&names);
    names.extend([("fal", "Kling 3.0 Pro"), ("fal", "FLUX 3 Dev"), ("fal", "FLUX 3 Dev")]);
    let all = otto_media::handles(&names);
    assert_eq!(all[..built_in.len()], built_in[..]);
    assert_eq!(all[built_in.len()..], ["fal-kling-3.0-pro-2", "flux-3-dev", "fal-flux-3-dev"]);
    assert_eq!(all.iter().collect::<std::collections::HashSet<_>>().len(), all.len());
}
