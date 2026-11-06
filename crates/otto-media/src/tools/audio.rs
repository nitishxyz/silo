//! `generate_audio` (docs/MEDIA.md §6.2).
use super::{Launch, background, choice, choose, launch, model, run, seconds, text};
use crate::{AudioRequest, AudioType, Media, MediaKind};
use otto_tools::{Tool, ToolCx, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

const TYPES: &[&str] = &["speech", "sound", "music"];

pub(crate) struct GenerateAudio {
    media: Arc<Media>,
    spec: ToolSpec,
}

impl GenerateAudio {
    pub(crate) fn new(media: Arc<Media>) -> Self {
        let spec = super::spec(
            "generate_audio",
            "Make speech, a sound effect or music with the user's audio provider (ElevenLabs makes all three, OpenAI speech). The MP3 is kept with the session: a [generated att_…] line with its length (copy_attachment saves it); you don't hear it. type speech: text is what to say, voice a voice id or name (list_media_models with kind audio lists them; else the provider's default voice); sound and music: text describes it, duration in seconds; path: also save into the project (replaces what is there). Picking a model or voice: if the user named one, use it (list_media_models first when the id isn't obvious); if the choice matters (a particular voice, style or cost) and they didn't say, list_media_models, then pick and say why in one line, or ask_user with 2–4 options when they differ a lot; otherwise leave provider, model and voice out for the defaults. For anything beyond this (a model's own modes and options, video or audio inputs, video edit/extend, motion transfer): media_model_schema, then generate_media. With no provider connected, the user is asked to connect one. Music runs in the background by default (background: true for the others): the call returns at once; tell the user it's generating and keep talking, don't poll; the result arrives as a message.",
            schema(),
        );
        Self { media, spec }
    }
}

/// Its input schema (also `generate_media`'s for built-in models, §3.2).
pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": {"type": "string", "enum": TYPES},
            "text": {"type": "string", "description": "What to say, or the sound or music prompt"},
            "voice": {"type": "string", "description": "Speech: a voice id or name (list_media_models lists voices)"},
            "duration": {"type": "number", "description": "Sound and music: seconds"},
            "path": {"type": "string", "description": "Also save into the project here, relative to the working directory"},
            "provider": {"type": "string", "description": "elevenlabs or openai (speech only); else the default"},
            "model": {"type": "string", "description": "A model handle or id (list_media_models); else the default for the type"},
            "background": {"type": "boolean", "description": "Run as a background job (default: true for music, else false): returns at once, the audio arrives as a message"}
        },
        "required": ["type", "text"],
        "additionalProperties": false
    })
}

#[async_trait::async_trait]
impl Tool for GenerateAudio {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        let kind = match choice(&input, "type", TYPES)? {
            Some("speech") => AudioType::Speech,
            Some("sound") => AudioType::Sound,
            Some("music") => AudioType::Music,
            _ => return Err(ToolError::invalid("type must be one of speech, sound, music")),
        };
        let words = text(&input, "text").ok_or_else(|| ToolError::invalid("text must be a non-empty string"))?.to_owned();
        let duration = seconds(&input, "duration")?;
        let background = background(&input, kind == AudioType::Music)?;
        let (provider, key) =
            choose(&self.media, &cx, MediaKind::Audio, Some(kind), super::requested(&self.media, &input, MediaKind::Audio).as_deref()).await?;
        let request = AudioRequest {
            kind,
            text: words.clone(),
            model: model(&self.media, provider.as_ref(), MediaKind::Audio, &input),
            voice: text(&input, "voice").map(str::to_owned),
            duration,
        };
        let what = Launch {
            kind: MediaKind::Audio,
            label: format!("generate_audio: {words}"),
            provider: provider.clone(),
            model: request.model.clone().unwrap_or_default(),
            name: None,
            extra: json!({}),
        };
        let (media, path) = (self.media.clone(), text(&input, "path").map(str::to_owned));
        launch(&cx, background, what, move |cx| async move {
            let maker = provider.clone();
            let mut output = run(&media, &cx, provider.as_ref(), |call| async move { maker.generate_audio(&key, request, &call).await }).await?;
            if kind == AudioType::Speech && duration.is_some() {
                output.notes.push("Speech takes its length from the text; duration was not used.".into());
            }
            super::result::result(&cx, MediaKind::Audio, provider.as_ref(), &words, output, path.as_deref()).await
        })
        .await
    }
}

/// `speech`, `sound`, `music`: the `type` naming `kind`.
pub(crate) fn type_name(kind: AudioType) -> &'static str {
    match kind {
        AudioType::Speech => "speech",
        AudioType::Sound => "sound",
        AudioType::Music => "music",
    }
}
