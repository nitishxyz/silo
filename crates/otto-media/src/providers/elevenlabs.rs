//! ElevenLabs (`api.elevenlabs.io`, `xi-api-key`): speech (`POST /v1/text-to-speech/{voice_id}`), sound effects
//! (`POST /v1/sound-generation`) and music (`POST /v1/music`), MP3 out. Voices from `GET /v2/voices`, cached.
use crate::catalog::{TEXT, model};
use crate::http::{self, send};
use crate::{AudioRequest, AudioType, Call, MediaError, MediaKind, MediaProvider, ModelEntry, Output, Voice};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a voice list is kept.
const VOICES_TTL: Duration = Duration::from_secs(3600);

/// George, the stock voice of ElevenLabs' examples.
const GEORGE: (&str, &str) = ("JBFqnCBsd6RMkjVDRZzb", "George");

const SPEECH: &str = "eleven_v4";
const SOUND: &str = "eleven_text_to_sound_v3";
const MUSIC: &str = "music_v2_5";

pub struct ElevenLabs {
    base: String,
    /// Voice lists by a hash of the key they were fetched with.
    voices: Mutex<HashMap<u64, (Instant, Vec<Voice>)>>,
}

impl Default for ElevenLabs {
    fn default() -> Self {
        Self::new("https://api.elevenlabs.io")
    }
}

impl ElevenLabs {
    /// `base` without `/v1`.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into().trim_end_matches('/').to_owned(), voices: Mutex::default() }
    }

    /// The account's voices (premade, cloned, designed), cached an hour per key.
    async fn library(&self, http: &reqwest::Client, key: &str, call: Option<&Call>) -> Result<Vec<Voice>, MediaError> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let slot = hasher.finish();
        if let Some((_, voices)) = self.voices.lock().ok().and_then(|c| c.get(&slot).filter(|(at, _)| at.elapsed() < VOICES_TTL).cloned()) {
            return Ok(voices);
        }
        let request = http.get(format!("{}/v2/voices?page_size=100", self.base)).header("xi-api-key", key);
        let value = match call {
            Some(call) => send(call, self.name(), key, request).await?,
            None => {
                let response =
                    request.timeout(Duration::from_secs(15)).send().await.map_err(|_| MediaError::failed("ElevenLabs: can't list voices"))?;
                let response = response.error_for_status().map_err(|_| MediaError::failed("ElevenLabs: can't list voices"))?;
                response.json().await.map_err(|_| MediaError::failed("ElevenLabs: unreadable voices"))?
            }
        };
        let voices: Vec<Voice> = value["voices"].as_array().into_iter().flatten().filter_map(voice).collect();
        if let Ok(mut cache) = self.voices.lock() {
            cache.insert(slot, (Instant::now(), voices.clone()));
        }
        Ok(voices)
    }

    /// `asked` (an id, a name, or the name's first part: `George` for `George - Warm Storyteller`) as a voice id.
    async fn voice_id(&self, key: &str, asked: Option<&str>, call: &Call) -> Result<(String, String), MediaError> {
        let Some(asked) = asked.map(str::trim).filter(|a| !a.is_empty()) else {
            return Ok((GEORGE.0.into(), GEORGE.1.into()));
        };
        let voices = self.library(&call.http, key, Some(call)).await?;
        let short = |name: &str| name.split(" - ").next().unwrap_or(name).trim().to_owned();
        let found = voices.iter().find(|v| v.id == asked).or_else(|| {
            voices.iter().find(|v| v.name.eq_ignore_ascii_case(asked)).or_else(|| voices.iter().find(|v| short(&v.name).eq_ignore_ascii_case(asked)))
        });
        match found {
            Some(v) => Ok((v.id.clone(), short(&v.name))),
            // A voice id from the shared library, not in the account's list.
            None if asked.len() >= 16 && asked.chars().all(|c| c.is_ascii_alphanumeric()) => Ok((asked.into(), asked.into())),
            None => Err(MediaError::invalid(format!("No ElevenLabs voice {asked}; list_media_models with kind audio lists them"))),
        }
    }
}

fn voice(v: &Value) -> Option<Voice> {
    let id = v["voice_id"].as_str()?;
    let labels: Vec<&str> = ["accent", "gender", "age", "descriptive", "use_case"].iter().filter_map(|l| v["labels"][*l].as_str()).collect();
    let about = v["description"]
        .as_str()
        .filter(|d| !d.trim().is_empty())
        .map_or_else(|| labels.join(", "), |d| d.lines().next().unwrap_or("").trim().to_owned());
    Some(Voice { default: id == GEORGE.0, ..Voice::new(id, v["name"].as_str().unwrap_or(id), &about) })
}

/// Whether `model` makes `kind` (sound and music models are named so).
fn makes(model: &str, kind: AudioType) -> bool {
    match kind {
        AudioType::Sound => model.starts_with("eleven_text_to_sound"),
        AudioType::Music => model.starts_with("music_"),
        AudioType::Speech => !model.starts_with("eleven_text_to_sound") && !model.starts_with("music_"),
    }
}

#[async_trait::async_trait]
impl MediaProvider for ElevenLabs {
    fn id(&self) -> &'static str {
        "elevenlabs"
    }
    fn name(&self) -> &'static str {
        "ElevenLabs"
    }
    fn kinds(&self) -> &'static [MediaKind] {
        &[MediaKind::Audio]
    }
    fn models(&self) -> Vec<ModelEntry> {
        let audio = MediaKind::Audio;
        vec![
            model(audio, SPEECH, "Eleven v4", "Most emotionally rich, expressive speech", TEXT, "speech").default(),
            model(audio, "eleven_v3", "Eleven v3", "Human-like, expressive speech, 70+ languages, audio tags", TEXT, "speech"),
            model(audio, "eleven_multilingual_v2", "Multilingual v2", "Stable, natural long-form speech", TEXT, "speech"),
            model(audio, "eleven_flash_v2_5", "Flash v2.5", "Fastest and cheapest speech", TEXT, "speech"),
            model(audio, SOUND, "Sound Effects v3", "Sound effects from a description (the default for sound)", TEXT, "sound effects, 0.5–30 s"),
            model(audio, MUSIC, "Eleven Music v2.5", "Songs and instrumentals from a prompt (the default for music)", TEXT, "music, 3 s to 10 min"),
            model(audio, "music_v2", "Eleven Music v2", "Studio-grade songs", TEXT, "music, 3 s to 10 min"),
        ]
    }
    fn env(&self) -> &'static [&'static str] {
        &["ELEVENLABS_API_KEY"]
    }
    fn audio_types(&self) -> &'static [AudioType] {
        &[AudioType::Speech, AudioType::Sound, AudioType::Music]
    }
    async fn voices(&self, http: &reqwest::Client, key: Option<&str>) -> Vec<Voice> {
        let stock = || vec![Voice { default: true, ..Voice::new(GEORGE.0, GEORGE.1, "stock voice: warm, British storyteller") }];
        match key {
            Some(key) => self.library(http, key, None).await.ok().filter(|v| !v.is_empty()).unwrap_or_else(stock),
            None => stock(),
        }
    }
    async fn generate_audio(&self, key: &str, r: AudioRequest, call: &Call) -> Result<Output, MediaError> {
        let mut notes = Vec::new();
        let default = match r.kind {
            AudioType::Speech => SPEECH,
            AudioType::Sound => SOUND,
            AudioType::Music => MUSIC,
        };
        let model = match r.model {
            Some(model) if makes(&model, r.kind) => model,
            Some(model) => {
                notes.push(format!("{model} doesn't make {}: used {default}.", r.kind.noun()));
                default.to_owned()
            }
            None => default.to_owned(),
        };
        let (path, body) = match r.kind {
            AudioType::Speech => {
                let (id, name) = self.voice_id(key, r.voice.as_deref(), call).await?;
                notes.push(format!("Voice {name} ({id})."));
                (format!("text-to-speech/{id}?output_format=mp3_44100_128"), json!({"text": r.text, "model_id": model}))
            }
            AudioType::Sound => {
                let mut body = json!({"text": r.text, "model_id": model});
                if let Some(seconds) = r.duration {
                    let made = seconds.clamp(0.5, 30.0);
                    if (made - seconds).abs() > f64::EPSILON {
                        notes.push(format!("Sound effects are 0.5–30 s: made {made} s."));
                    }
                    body["duration_seconds"] = made.into();
                }
                ("sound-generation?output_format=mp3_44100_128".into(), body)
            }
            AudioType::Music => {
                let mut body = json!({"prompt": r.text, "model_id": model});
                if let Some(seconds) = r.duration {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let ms = (seconds * 1000.0).round().clamp(3000.0, 600_000.0) as u64;
                    if ms as f64 != (seconds * 1000.0).round() {
                        notes.push(format!("Music is 3 s to 10 min: made {} s.", ms / 1000));
                    }
                    body["music_length_ms"] = ms.into();
                }
                ("music?output_format=mp3_44100_128".into(), body)
            }
        };
        if r.kind != AudioType::Speech && r.voice.is_some() {
            notes.push(format!("Voices are for speech; {} has none.", r.kind.noun()));
        }
        let request = call.http.post(format!("{}/v1/{path}", self.base)).header("xi-api-key", key).json(&body);
        let file = http::send_file(call, self.name(), key, request).await?;
        Ok(Output { media: vec![file], model, revised_prompt: None, notes })
    }
}
