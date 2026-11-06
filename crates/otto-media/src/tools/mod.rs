//! The media tools: `generate_image` (§3), `generate_video` (§6.1), `generate_audio` (§6.2), `list_media_models`
//! (§3.0), `media_model_schema` and `generate_media` (§3.2), and what they share: the provider and model a call
//! uses, its inputs, its run.
mod audio;
mod generate;
mod list;
mod media;
mod model_schema;
mod pick;
mod result;
mod save;
mod video;

use crate::{AudioType, Call, Media, MediaError, MediaErrorKind, MediaKind, MediaProvider, Output, Reference};
use otto_tools::jobs::{JobKind, Report};
use otto_tools::{Effects, Tool, ToolCx, ToolError, ToolErrorKind, ToolOutput, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn all(media: &Arc<Media>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(generate::GenerateImage::new(media.clone())),
        Arc::new(video::GenerateVideo::new(media.clone())),
        Arc::new(audio::GenerateAudio::new(media.clone())),
        Arc::new(list::ListMediaModels::new(media.clone())),
        Arc::new(model_schema::MediaModelSchema::new(media.clone())),
        Arc::new(media::GenerateMedia::new(media.clone())),
    ]
}

/// A generation tool's spec: network and write (it may save `path`), not exclusive, eager.
pub(crate) fn spec(name: &str, description: &str, input_schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema,
        output_schema: None,
        effects: Effects { network: true, ..Effects::WRITE },
        sensitive: vec![],
        loading: otto_tools::Loading::Eager,
        mode: otto_tools::CallMode::Sync,
        presentations: vec![],
        source: otto_tools::Source::Builtin,
        exclusive: false,
    }
}

/// `key` as one of `allowed`.
pub(crate) fn choice<'a>(input: &'a Value, key: &str, allowed: &[&str]) -> Result<Option<&'a str>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if allowed.contains(&s.as_str()) => Ok(Some(s)),
        Some(_) => Err(ToolError::invalid(format!("{key} must be one of {}", allowed.join(", ")))),
    }
}

/// `key` as a non-empty string.
pub(crate) fn text<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// `key` as a list of strings.
pub(crate) fn strings(input: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(items)) => {
            items.iter().map(|i| i.as_str().map(str::to_owned).ok_or_else(|| ToolError::invalid(format!("{key} must be strings")))).collect()
        }
        Some(_) => Err(ToolError::invalid(format!("{key} must be an array of strings"))),
    }
}

/// `key` as seconds, more than 0.
pub(crate) fn seconds(input: &Value, key: &str) -> Result<Option<f64>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            v.as_f64().filter(|s| *s > 0.0 && s.is_finite()).map(Some).ok_or_else(|| ToolError::invalid(format!("{key} must be a number of seconds")))
        }
    }
}

/// Images (project paths, attachment ids or http(s) links, downloaded) read and prepared for a provider.
pub(crate) async fn images(media: &Media, cx: &ToolCx, names: Vec<String>) -> Result<Vec<Reference>, ToolError> {
    let mut out = Vec::new();
    for name in names {
        if is_link(&name) {
            let file = crate::http::download(&media.call(cx.cancel.clone(), Arc::new(|_| {})), "the link", &name).await.map_err(tool_error)?;
            out.push(save::image(&file.bytes, &name)?);
        } else {
            let cx = cx.clone();
            let one = tokio::task::spawn_blocking(move || save::reference(&cx, &name)).await.map_err(|e| ToolError::failed(e.to_string()))??;
            out.push(one);
        }
    }
    Ok(out)
}

/// An `http(s)://` link.
pub(crate) fn is_link(name: &str) -> bool {
    name.starts_with("https://") || name.starts_with("http://")
}

/// The call's model (a model id, or a handle of this provider's), else the kind's default when it names this provider.
pub(crate) fn model(media: &Media, provider: &dyn MediaProvider, kind: MediaKind, input: &Value) -> Option<String> {
    let handle = |m: &str| media.catalog().into_iter().find(|c| c.provider.id() == provider.id() && c.handle.eq_ignore_ascii_case(m)).map(|c| c.id);
    text(input, "model")
        .map(|m| handle(m).unwrap_or_else(|| m.to_owned()))
        .or_else(|| media.defaults().get(&kind).filter(|c| c.provider.as_deref() == Some(provider.id())).and_then(|c| c.model.clone()))
}

/// The call's provider, else the provider of the model it names by handle.
pub(crate) fn requested(media: &Media, input: &Value, kind: MediaKind) -> Option<String> {
    text(input, "provider").map(str::to_owned).or_else(|| {
        let m = text(input, "model")?;
        media.catalog().into_iter().find(|c| c.kind == kind && c.handle.eq_ignore_ascii_case(m)).map(|c| c.provider.id().to_owned())
    })
}

/// `generation` run for `provider`, with its progress shown and the call's cancel honoured.
pub(crate) async fn run<F>(media: &Media, cx: &ToolCx, provider: &dyn MediaProvider, generation: impl FnOnce(Call) -> F) -> Result<Output, ToolError>
where
    F: std::future::Future<Output = Result<Output, MediaError>>,
{
    cx.progress(Some(format!("Generating with {}…", provider.name())), None);
    let progress_cx = cx.clone();
    let call = media.call(cx.cancel.clone(), Arc::new(move |message| progress_cx.progress(Some(message), None)));
    let generation = generation(call);
    tokio::pin!(generation);
    tokio::select! {
        output = &mut generation => output.map_err(tool_error),
        () = cx.cancel.cancelled() => {
            // A moment to withdraw the provider's request (`poll::wait` sends its cancel).
            let _ = tokio::time::timeout(std::time::Duration::from_secs(6), &mut generation).await;
            Err(tool_error(MediaError::cancelled()))
        }
    }
}

pub(crate) fn tool_error(e: MediaError) -> ToolError {
    let kind = match e.kind {
        MediaErrorKind::Invalid => ToolErrorKind::InvalidInput,
        MediaErrorKind::Cancelled => ToolErrorKind::Cancelled,
        MediaErrorKind::Auth | MediaErrorKind::Failed => ToolErrorKind::Failed,
    };
    ToolError::new(kind, e.message)
}

/// The provider and key a call of `kind` uses: `requested`, else the kind's default when connected, else the
/// first connected provider. None connected (or not the requested one): the user is asked (§3.1).
/// `audio`, when set, narrows the providers to those making that type; audio prefers ElevenLabs (§6.2).
pub(crate) async fn choose(
    media: &Media,
    cx: &ToolCx,
    kind: MediaKind,
    audio: Option<AudioType>,
    requested: Option<&str>,
) -> Result<(Arc<dyn MediaProvider>, String), ToolError> {
    let makes = |p: &Arc<dyn MediaProvider>| p.kinds().contains(&kind) && audio.is_none_or(|t| p.audio_types().contains(&t));
    let mut able: Vec<Arc<dyn MediaProvider>> = media.providers().iter().filter(|p| makes(p)).cloned().collect();
    able.sort_by_key(|p| kind == MediaKind::Audio && p.id() != "elevenlabs");
    let requested = match requested {
        Some(id) => {
            let provider = media.provider(id).filter(makes).ok_or_else(|| {
                let ids: Vec<&str> = able.iter().map(|p| p.id()).collect();
                let what = audio.map_or_else(|| format!("{} provider", noun(kind)), |t| format!("{} provider", t.noun()));
                ToolError::invalid(format!("Unknown {what} {id}; one of {}", ids.join(", ")))
            })?;
            if let Some((key, _)) = media.key(provider.as_ref()) {
                return Ok((provider, key));
            }
            Some(provider)
        }
        None => None,
    };
    if requested.is_none() {
        let default = media.defaults().get(&kind).and_then(|c| c.provider.clone()).and_then(|id| media.provider(&id));
        let connected = default.filter(makes).into_iter().chain(able.iter().cloned());
        for provider in connected {
            if let Some((key, _)) = media.key(provider.as_ref()) {
                return Ok((provider, key));
            }
        }
    }
    let none = || ToolError::failed(format!("No {} provider is connected. Connect one in Otto settings → Providers.", noun(kind)));
    let approver = cx.approver.as_ref().ok_or_else(none)?;
    let mut options: Vec<String> = able.iter().map(|p| p.name().to_owned()).collect();
    options.push("Not now".into());
    let question = format!("{} generation needs a provider. Connect one to continue.", capitalized(noun(kind)));
    let answer = approver.ask(&cx.call_id, &question, &options).await.ok_or_else(none)?;
    let provider = media.provider(&answer).filter(makes).ok_or_else(none)?;
    let (key, _) = media.key(provider.as_ref()).ok_or_else(|| ToolError::failed(format!("No API key for {}", provider.name())))?;
    Ok((provider, key))
}

/// `image`, `video`, `audio`.
pub(crate) fn noun(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
    }
}

pub(crate) fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// `background` as asked, else `default` (§6.5: videos and music in the background).
pub(crate) fn background(input: &Value, default: bool) -> Result<bool, ToolError> {
    match input.get("background") {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_bool().ok_or_else(|| ToolError::invalid("background must be true or false")),
    }
}

/// A generation about to run: what its job is called and says about itself.
pub(crate) struct Launch {
    pub kind: MediaKind,
    /// `generate_video: <prompt>`.
    pub label: String,
    pub provider: Arc<dyn MediaProvider>,
    pub model: String,
    /// As shown (`Seedance 2.5 · reference-to-video`); else the model's known name, else its id.
    pub name: Option<String>,
    /// More metadata: `n`, `aspect_ratio`.
    pub extra: Value,
}

/// `work` (the generation and its result) as a session job when `background`: the call returns at once and the
/// result arrives as a message (§6.5). Else in the call, which the user can detach into a job. Without a job
/// registry (no session) it runs in the call.
pub(crate) async fn launch<F, Fut>(cx: &ToolCx, background: bool, launch: Launch, work: F) -> Result<ToolOutput, ToolError>
where
    F: FnOnce(ToolCx) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
{
    let Some(jobs) = cx.jobs.clone() else { return work(cx.clone()).await };
    let Launch { kind, label, provider, model, name, extra } = launch;
    let name = name.unwrap_or_else(|| provider.models().into_iter().find(|m| m.id == model).map_or_else(|| model.clone(), |m| m.name));
    // Audio picks its model by type when none was asked for.
    let what = format!("{} with {} {name}", noun(kind), provider.name()).trim_end().to_owned();
    let mut metadata = json!({"background": true, "kind": noun(kind), "provider": provider.id()});
    if !model.is_empty() {
        metadata["model"] = json!(model);
    }
    for (key, value) in extra.as_object().into_iter().flatten().filter(|(_, v)| !v.is_null()) {
        metadata[key] = value.clone();
    }
    if !background {
        let detached = move |id: &str| {
            metadata["job_id"] = json!(id);
            metadata["detached"] = json!(true);
            let text = format!(
                "Started media {id} ({what}): the user detached this generation and it runs on in the background. The result arrives as a message when it's done; don't wait or poll for it, or start it again."
            );
            ToolOutput::text(text).with_metadata(metadata)
        };
        return jobs.foreground(&label, JobKind::Media, cx, work, report(kind), detached).await;
    }
    let id = jobs.start(label, JobKind::Media, cx.clone(), work, report(kind)).await?;
    metadata["job_id"] = json!(id);
    let text = format!(
        "Started media {id} ({what}); the result arrives as a message when it's done. Keep talking with the user; don't wait or poll for it."
    );
    Ok(ToolOutput::text(text).with_metadata(metadata))
}

/// A media job's report: `Background media <id> (<kind>) finished.` and the result's text, or how it ended.
fn report(kind: MediaKind) -> Report {
    Box::new(move |id, result| {
        let head = format!("Background media {id} ({})", noun(kind));
        match result {
            Ok(output) => format!("{head} finished.\n{}", output.joined_text()),
            Err(e) if e.kind == ToolErrorKind::Cancelled => format!("{head} was stopped."),
            Err(e) => format!("{head} failed: {}", e.message),
        }
    })
}
