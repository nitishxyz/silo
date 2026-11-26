//! A card and what is wired into it as one request (docs/STUDIO.md §6.2 "Compiling"): exactly a call of
//! `generate_image`/`generate_video`/`generate_audio`, or of `generate_media` for a model's mode, resolved the way
//! those tools resolve models (handles, `provider/id`, endpoint ids, modes).
use crate::models::Model;
use crate::schema::{self, Media as FieldMedia};
use crate::tools::{noun, pick};
use crate::{AudioType, Media, MediaKind, MediaProvider};
use otto_proto::{CardFile, CardProblem, CompiledCard, ModelFit};
use otto_tools::studio::{Card, CardInput, Makes, Port, SourceMedia};
use serde_json::{Map, Value, json};
use silo_proto::studio::mention::{self, Target};
use std::sync::Arc;

/// Why a card can't run (`CardProblem`'s codes).
#[derive(Clone, Debug)]
pub struct Problem {
    /// `no_provider`, `input`, `model`, `invalid`.
    pub code: &'static str,
    pub message: String,
    pub kind: Option<MediaKind>,
    pub provider: Option<String>,
}

impl Problem {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), kind: None, provider: None }
    }
    #[must_use]
    pub fn wire(&self) -> CardProblem {
        CardProblem { code: self.code.into(), message: self.message.clone(), kind: self.kind, provider: self.provider.clone() }
    }
}

/// A compiled card.
#[derive(Clone, Debug)]
pub struct Compiled {
    pub tool: &'static str,
    /// The tool's input, without `path`, `background` or `canvas`.
    pub input: Value,
    pub kind: MediaKind,
    /// Its handle; empty when the provider picks (audio on Automatic).
    pub model: String,
    pub model_name: String,
    pub provider: String,
    pub mode: Option<String>,
    pub prompt: String,
    pub files: Vec<CardFile>,
    pub notes: Vec<String>,
    /// Wired cards still queued or running.
    pub waiting: Vec<String>,
    /// The ports its model takes.
    pub ports: Vec<Port>,
}

impl Compiled {
    #[must_use]
    pub fn wire(&self) -> CompiledCard {
        CompiledCard {
            tool: self.tool.into(),
            input: self.input.clone(),
            model: self.model.clone(),
            model_name: self.model_name.clone(),
            provider: self.provider.clone(),
            mode: self.mode.clone(),
            prompt: self.prompt.clone(),
            files: self.files.clone(),
            notes: self.notes.clone(),
        }
    }
}

/// What the card's project sets for compiling: its style line, motion line and `[models]` picks.
#[derive(Clone, Debug, Default)]
pub struct Setup {
    /// `world/style.md`'s `## Style line`: added to image and video cards.
    pub style: Option<String>,
    /// `world/style.md`'s `## Motion line`: added to video cards, after the style line.
    pub motion: Option<String>,
    pub picks: std::collections::BTreeMap<String, String>,
}

impl Setup {
    /// Read from the studio at `root`.
    #[must_use]
    pub fn read(root: &std::path::Path) -> Self {
        Self {
            style: otto_tools::studio::style_line(root),
            motion: otto_tools::studio::motion_line(root),
            picks: otto_tools::studio::model_picks(root),
        }
    }
}

/// `prompt` with the style line and then the motion line (each when set and not empty) after it, a blank line
/// apart. The caller decides which apply: the style line on image and video, the motion line on video only.
pub(crate) fn with_style(prompt: String, style: Option<&str>, motion: Option<&str>) -> String {
    [style, motion].into_iter().flatten().map(str::trim).filter(|s| !s.is_empty()).fold(prompt, |p, line| format!("{p}\n\n{line}"))
}

/// Where a mentioned source went in the request: what its token expands to after its label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placed {
    /// `(image 2)`, `(video 1)`, `(audio 1)`: its 1-based position among that media's files as sent.
    Numbered(&'static str, usize),
    /// `(the first frame)`.
    Frame(&'static str),
    /// The label alone: wired at `text`, or wired at a port the model has no field for.
    Plain,
}

/// What a token expands to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Expansion {
    /// The entity's name, a reference's with the entity's (`Dhritarashtra, Crown off`), the node's label.
    pub label: String,
    /// Where its file went; `None`: not in the request (a note says so).
    pub placed: Option<Placed>,
    /// A named reference's job, said with its image number (`image 2: take only …`).
    pub defines: Option<String>,
}

impl Expansion {
    pub(crate) fn new(label: String, placed: Option<Placed>) -> Self {
        Self { label, placed, defines: None }
    }
}

/// `prompt` with each token that resolves expanded (docs/STUDIO.md §6.2, "Mentions"): `resolve` gives a
/// target's [`Expansion`]; a target it gives nothing for (unknown) stays as written. The same token twice expands
/// the same way twice. A reference numbered among the images says what to take from it when it has a job:
/// `Dhritarashtra, Crown off (image 2: take only his bare head and hair, without the crown)`.
pub(crate) fn expand(prompt: &str, mut resolve: impl FnMut(&Target) -> Option<Expansion>, notes: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut at = 0;
    let mut unwired: Vec<String> = Vec::new();
    for token in mention::tokens(prompt) {
        let Some(Expansion { label, placed, defines }) = resolve(&token.target) else { continue };
        out.push_str(&prompt[at..token.range.start]);
        out.push_str(&label);
        let job = defines.as_deref().map(str::trim).filter(|d| !d.is_empty());
        match placed {
            Some(Placed::Numbered("image", n)) if job.is_some() => out.push_str(&format!(" (image {n}: take only {})", job.unwrap_or_default())),
            Some(Placed::Numbered(what, n)) => out.push_str(&format!(" ({what} {n})")),
            Some(Placed::Frame(which)) => out.push_str(&format!(" (the {which} frame)")),
            Some(Placed::Plain) => {}
            None if !unwired.contains(&label) => unwired.push(label),
            None => {}
        }
        at = token.range.end;
    }
    out.push_str(&prompt[at..]);
    notes.extend(unwired.into_iter().map(|label| format!("{label} is mentioned but not wired.")));
    out
}

/// What a file sent at `port` as its `n`th of its media is called in a prompt.
fn placed_at(port: Port, n: usize) -> Placed {
    match port {
        Port::Reference => Placed::Numbered("image", n),
        Port::Video => Placed::Numbered("video", n),
        Port::Audio => Placed::Numbered("audio", n),
        Port::FirstFrame => Placed::Frame("first"),
        Port::MiddleFrame => Placed::Frame("middle"),
        Port::LastFrame => Placed::Frame("last"),
        Port::Text | Port::Unknown => Placed::Plain,
    }
}

/// Whether `input`'s source is what `target` names; `path`: a named reference's file (`CardMention::path`).
fn names(target: &Target, input: &CardInput, path: Option<&str>) -> bool {
    match target {
        Target::Node(id) => input.from == *id,
        Target::Entity(id) => input.entity.as_deref() == Some(id),
        Target::Ref { .. } => input.entity.is_none() && path.is_some_and(|p| input.path.as_deref() == Some(p)),
    }
}

/// The media kind and audio type a card makes.
pub(crate) fn kind_of(makes: Makes) -> Option<(MediaKind, Option<AudioType>)> {
    match makes {
        Makes::Image => Some((MediaKind::Image, None)),
        Makes::Video => Some((MediaKind::Video, None)),
        Makes::Speech => Some((MediaKind::Audio, Some(AudioType::Speech))),
        Makes::Sound => Some((MediaKind::Audio, Some(AudioType::Sound))),
        Makes::Music => Some((MediaKind::Audio, Some(AudioType::Music))),
        Makes::Unknown => None,
    }
}

/// Where a port's files go in a request.
#[derive(Clone, Debug)]
enum Slot {
    One(String),
    /// A list, at most this many.
    Many(String, Option<u64>),
}

impl Slot {
    fn field(&self) -> &str {
        match self {
            Self::One(f) | Self::Many(f, _) => f,
        }
    }
}

/// A way to run a model: its quick tool, or one of its modes through `generate_media`.
#[derive(Clone, Debug)]
pub(crate) struct Route {
    /// The mode's index and name; `None`: the quick tool.
    mode: Option<(usize, String)>,
    slots: Vec<(Port, Slot)>,
    /// Required media fields: the port feeding each (`None`: no port can).
    required: Vec<(String, Option<Port>)>,
    schema: Value,
}

impl Route {
    pub(crate) fn ports(&self) -> Vec<Port> {
        let mut ports: Vec<Port> = self.slots.iter().map(|(p, _)| *p).collect();
        ports.push(Port::Text);
        ports
    }
    fn slot(&self, port: Port) -> Option<&Slot> {
        self.slots.iter().find(|(p, _)| *p == port).map(|(_, s)| s)
    }
}

/// The card port a media field serves, from its name and media.
pub(crate) fn port_of(name: &str, media: FieldMedia, many: bool, kind: MediaKind) -> Option<Port> {
    let n = name.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    match media {
        FieldMedia::Video => Some(Port::Video),
        FieldMedia::Audio => Some(Port::Audio),
        FieldMedia::File => None,
        FieldMedia::Image if has(&["mask"]) => None,
        FieldMedia::Image if has(&["last", "end_", "tail"]) => Some(Port::LastFrame),
        FieldMedia::Image if has(&["middle", "mid_"]) => Some(Port::MiddleFrame),
        FieldMedia::Image if has(&["first", "start"]) => Some(Port::FirstFrame),
        FieldMedia::Image if many || has(&["reference", "ref_", "style", "subject", "element"]) => Some(Port::Reference),
        FieldMedia::Image if kind == MediaKind::Video => Some(Port::FirstFrame),
        FieldMedia::Image => Some(Port::Reference),
    }
}

fn quick_route(model: &Model) -> Route {
    let inputs = model.entry.as_ref().map(|e| e.inputs.clone()).unwrap_or_default();
    let takes = |what: &str| inputs.iter().any(|i| i == what);
    let mut slots = Vec::new();
    if model.kind == MediaKind::Video && (takes("first frame") || takes("first/last frame")) {
        slots.push((Port::FirstFrame, Slot::One("first_frame".into())));
    }
    if model.kind == MediaKind::Video && takes("first/last frame") {
        slots.push((Port::LastFrame, Slot::One("last_frame".into())));
    }
    if model.kind != MediaKind::Audio && takes("reference images") {
        slots.push((Port::Reference, Slot::Many("references".into(), None)));
    }
    Route { mode: None, slots, required: Vec::new(), schema: pick::quick_schema(model) }
}

fn mode_route(model: &Model, i: usize, schema: Value) -> Route {
    let mut slots: Vec<(Port, Slot)> = Vec::new();
    let mut required = Vec::new();
    // Top-level fields only: nested ones (`elements[].…`) a card can't fill.
    for f in schema::fields(&schema).into_iter().filter(|f| !f.name.contains(['.', '['])) {
        let port = port_of(&f.name, f.media, f.many.is_some(), model.kind);
        let free = port.filter(|p| !slots.iter().any(|(q, _)| q == p));
        if let Some(p) = free {
            slots.push((p, if let Some((_, max)) = f.many { Slot::Many(f.name.clone(), max) } else { Slot::One(f.name.clone()) }));
        }
        if f.required {
            required.push((f.name.clone(), free));
        }
    }
    Route { mode: Some((i, model.modes[i].0.clone())), slots, required, schema }
}

/// A model's routes: its quick tool, the mode `wanted`, or every mode (schemas fetched when not cached).
async fn routes(media: &Media, model: &Model, wanted: Option<&str>, fetch: bool) -> Result<Vec<Route>, Problem> {
    if model.modes.is_empty() {
        return Ok(vec![quick_route(model)]);
    }
    let picked: Vec<usize> = match pick::mode(model, wanted).map_err(|e| Problem::new("model", e.message))? {
        Some(i) => vec![i],
        None => (0..model.modes.len()).collect(),
    };
    let mut out = Vec::new();
    for i in picked {
        let schema = match &model.modes[i].1.schema {
            Some(s) => Some(s.clone()),
            None if fetch => pick::with_schema(media, model, i).await.ok().and_then(|e| e.schema),
            None => None,
        };
        if let Some(schema) = schema {
            out.push(mode_route(model, i, schema));
        }
    }
    if out.is_empty() {
        return Err(Problem::new("model", format!("Can't read {}'s input schema from {}", model.name, model.provider.name())));
    }
    Ok(out)
}

/// The wired media ports and how many files each has.
fn wired(card: &Card) -> Vec<(Port, usize)> {
    let mut out: Vec<(Port, usize)> = Vec::new();
    for input in card.inputs.iter().filter(|i| i.port != Port::Text) {
        match out.iter_mut().find(|(p, _)| *p == input.port) {
            Some((_, n)) => *n += 1,
            None => out.push((input.port, 1)),
        }
    }
    out
}

/// Whether `route` takes `wired`: every wired port has a field with room, every required field is fed.
fn check(route: &Route, wired: &[(Port, usize)]) -> Result<(), String> {
    for (port, n) in wired {
        match route.slot(*port) {
            None => return Err(format!("no {} input", port.name())),
            Some(Slot::Many(_, Some(max))) if (*n as u64) > *max => {
                let plural = if *max == 1 { "" } else { "s" };
                return Err(format!("takes {max} {}{plural}, {n} wired", port.name()));
            }
            _ => {}
        }
    }
    for (field, port) in &route.required {
        match port {
            Some(p) if !wired.iter().any(|(q, _)| q == p) => return Err(format!("needs a {}", p.name())),
            None => return Err(format!("needs {field}")),
            _ => {}
        }
    }
    Ok(())
}

/// The first of `routes` taking `wired`, else the first one's reason.
fn first_fit(routes: Vec<Route>, wired: &[(Port, usize)]) -> Result<Route, String> {
    let mut reason = None;
    for route in routes {
        match check(&route, wired) {
            Ok(()) => return Ok(route),
            Err(why) => {
                reason.get_or_insert(why);
            }
        }
    }
    Err(reason.unwrap_or_else(|| "no way to run it".into()))
}

/// A model picked for a card: it and the route taking what is wired; `None` model: the provider picks (audio).
struct Choice {
    model: Option<Model>,
    provider: Arc<dyn MediaProvider>,
    route: Route,
}

fn makes_audio(provider: &dyn MediaProvider, audio: Option<AudioType>) -> bool {
    audio.is_none_or(|t| provider.audio_types().contains(&t))
}

fn no_provider(kind: MediaKind, audio: Option<AudioType>, provider: Option<&dyn MediaProvider>) -> Problem {
    let what = audio.map_or_else(|| noun(kind).to_owned(), |t| t.noun().to_owned());
    let message = match provider {
        Some(p) => format!("{} isn't connected. Connect it in Otto settings → Providers, then run the card again.", p.name()),
        None => format!("No {what} provider is connected. Connect one in Otto settings → Providers, then run the card again."),
    };
    Problem { code: "no_provider", message, kind: Some(kind), provider: provider.map(|p| p.id().to_owned()) }
}

/// The model `name` names, of `kind`.
async fn named(media: &Media, name: &str, kind: MediaKind) -> Result<Model, Problem> {
    let (model, _) = pick::model(media, name).await.map_err(|e| Problem::new("model", e.message))?;
    if model.kind != kind {
        return Err(Problem::new("model", format!("{} makes {}, not {}", model.name, noun(model.kind), noun(kind))));
    }
    Ok(model)
}

async fn choose(media: &Media, card: &Card, setup: &Setup, kind: MediaKind, audio: Option<AudioType>) -> Result<Choice, Problem> {
    let wired = wired(card);
    if let Some(name) = card.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        let model = named(media, name, kind).await?;
        if media.key(model.provider.as_ref()).is_none() {
            return Err(no_provider(kind, audio, Some(model.provider.as_ref())));
        }
        let (_, endpoint) = pick::model(media, name).await.map_err(|e| Problem::new("model", e.message))?;
        let wanted = card.mode.as_deref().or(endpoint.as_deref());
        let route = first_fit(routes(media, &model, wanted, true).await?, &wired)
            .map_err(|why| Problem::new("model", format!("{} can't make this card: {why}", model.name)))?;
        return Ok(Choice { provider: model.provider.clone(), model: Some(model), route });
    }
    // Automatic: the project's pick, the kind's default, then each connected provider's models (with their live
    // modes: the catalogs are read as a named model's are).
    media.refresh_catalogs(false).await;
    let catalog = media.catalog();
    let mut candidates: Vec<Model> = Vec::new();
    let picks = [Some(card.makes.key()), audio.map(|_| "audio")];
    for pick in picks.into_iter().flatten().filter_map(|k| setup.picks.get(k)) {
        if let Ok(m) = named(media, pick, kind).await {
            candidates.push(m);
        }
    }
    let defaults = media.defaults();
    let chosen = defaults.get(&kind).cloned().unwrap_or_default();
    let mut providers: Vec<Arc<dyn MediaProvider>> =
        media.providers().iter().filter(|p| p.kinds().contains(&kind) && makes_audio(p.as_ref(), audio)).cloned().collect();
    providers.sort_by_key(|p| (Some(p.id()) != chosen.provider.as_deref(), kind == MediaKind::Audio && p.id() != "elevenlabs"));
    let connected: Vec<Arc<dyn MediaProvider>> = providers.iter().filter(|p| media.key(p.as_ref()).is_some()).cloned().collect();
    if connected.is_empty() {
        return Err(no_provider(kind, audio, None));
    }
    // Audio with nothing chosen: the provider picks its model for the type, as `generate_audio` does.
    if kind == MediaKind::Audio && candidates.is_empty() && chosen.model.is_none() {
        let provider = connected[0].clone();
        let route = Route { mode: None, slots: Vec::new(), required: Vec::new(), schema: crate::tools::audio_schema() };
        return Ok(Choice { model: None, provider, route });
    }
    for p in &connected {
        let id =
            if Some(p.id()) == chosen.provider.as_deref() { chosen.model.clone().or_else(|| p.default_model(kind)) } else { p.default_model(kind) };
        let of = |m: &&Model| m.kind == kind && m.provider.id() == p.id();
        if let Some(id) = id
            && let Some(m) = catalog.iter().filter(of).find(|m| m.id == id || m.handle.eq_ignore_ascii_case(&id) || m.named(&id))
        {
            candidates.push(m.clone());
        }
        candidates.extend(catalog.iter().filter(of).filter(|m| m.entry.is_some()).cloned());
    }
    let mut reason = None;
    let mut seen = Vec::new();
    for (i, model) in candidates.into_iter().enumerate() {
        if seen.contains(&model.handle) || media.key(model.provider.as_ref()).is_none() {
            continue;
        }
        seen.push(model.handle.clone());
        // Schemas are fetched for the picks and defaults only; the rest use what is cached.
        let Ok(routes) = routes(media, &model, card.mode.as_deref(), i < 3).await else { continue };
        match first_fit(routes, &wired) {
            Ok(route) => return Ok(Choice { provider: model.provider.clone(), model: Some(model), route }),
            Err(why) => {
                reason.get_or_insert(format!("{}: {why}", model.name));
            }
        }
    }
    let ports: Vec<&str> = wired.iter().map(|(p, _)| p.name()).collect();
    Err(Problem::new(
        "model",
        format!("No connected {} model takes what is wired ({}); {}", noun(kind), ports.join(", "), reason.unwrap_or_default()),
    ))
}

/// The most of a wired document's text a request takes (characters).
pub const DOCUMENT_CAP: usize = 8_000;

/// A wired document's text (root-relative `path` under `root`): at most [`DOCUMENT_CAP`] characters, with a note
/// when cut.
async fn document_text(root: &std::path::Path, path: &str, label: &str, notes: &mut Vec<String>) -> Result<String, Problem> {
    let file = root.join(path.trim_start_matches('/'));
    let bytes = tokio::fs::read(&file).await.map_err(|e| Problem::new("input", format!("Can't read {label}: {e}")))?;
    let text = String::from_utf8_lossy(&bytes);
    if text.chars().count() <= DOCUMENT_CAP {
        return Ok(text.into_owned());
    }
    notes.push(format!("{label} is over {DOCUMENT_CAP} characters: only its first {DOCUMENT_CAP} are used."));
    Ok(text.chars().take(DOCUMENT_CAP).collect())
}

/// `W:H` as a ratio.
fn ratio(aspect: &str) -> Option<f64> {
    let (w, h) = aspect.trim().split_once(':')?;
    let (w, h) = (w.trim().parse::<f64>().ok()?, h.trim().parse::<f64>().ok()?);
    (w > 0.0 && h > 0.0).then_some(w / h)
}

/// The short side of `1080p`, `4k`, `2K`.
fn lines(resolution: &str) -> Option<f64> {
    let r = resolution.trim().to_lowercase();
    if let Some(p) = r.strip_suffix('p') {
        return p.parse().ok();
    }
    match r.as_str() {
        "4k" => Some(2160.0),
        "2k" => Some(1440.0),
        "1k" => Some(1080.0),
        _ => None,
    }
}

/// The value of `options` nearest `want` by `measure`.
fn nearest(options: &[&str], want: f64, measure: impl Fn(&str) -> Option<f64>, log: bool) -> Option<String> {
    let d = |x: f64| if log { (x.ln() - want.ln()).abs() } else { (x - want).abs() };
    options.iter().filter_map(|o| Some((*o, d(measure(o)?)))).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(o, _)| o.to_owned())
}

/// The size fields the card leaves unset, from its effective format: the nearest the schema offers, with a note.
pub(crate) fn sizes(
    format: Option<&otto_tools::studio::Format>,
    schema: &Value,
    input: &mut Map<String, Value>,
    name: &str,
    notes: &mut Vec<String>,
) {
    let Some(format) = format else { return };
    let props = &schema["properties"];
    let mut fill = |key: &str, want: Option<String>, measure: fn(&str) -> Option<f64>, log: bool| {
        let (Some(want), Some(prop)) = (want, props.get(key)) else { return };
        if input.contains_key(key) {
            return;
        }
        let options: Vec<&str> = prop["enum"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let value = if options.is_empty() {
            if prop["type"] == "string" { want.clone() } else { return }
        } else if options.iter().any(|o| o.eq_ignore_ascii_case(&want)) {
            want.clone()
        } else {
            let Some(target) = measure(&want) else { return };
            let Some(found) = nearest(&options, target, measure, log) else { return };
            notes.push(format!("{name} has no {want}; made {found}."));
            found
        };
        input.insert(key.into(), Value::String(value));
    };
    fill("aspect_ratio", Some(format.aspect.clone()), ratio, true);
    fill("resolution", format.resolution.clone(), lines, false);
    if let (Some(fps), Some(prop)) = (format.fps, props.get("fps"))
        && !input.contains_key("fps")
        && (prop["type"] == "integer" || prop["type"] == "number")
    {
        input.insert("fps".into(), json!(fps));
    }
}

/// Card `card` as one request (docs/STUDIO.md §6.2): its prompt with the wired texts, the style line and (video)
/// the motion line, the wired
/// files by port, its options and the size from its format, for the model it names or one Automatic picks.
/// `extra`: fields a `generate_* {canvas}` call adds that are not wires (its own take, `markup`, files outside the
/// canvas); their `references` go first. Wired cards still running are listed in `waiting`, their last take used.
///
/// # Errors
/// What keeps it from running: no provider, a wire's problem, a model that can't take what is wired, no prompt.
pub async fn compile(media: &Media, card: &Card, setup: &Setup, extra: &Map<String, Value>) -> Result<Compiled, Problem> {
    let (kind, audio) = kind_of(card.makes).ok_or_else(|| Problem::new("invalid", "This card makes something this otto doesn't know"))?;
    let at = |mut p: Problem| {
        p.kind = Some(kind);
        p
    };
    // Wires first: a wire's problem is the card's whatever the model.
    let mut waiting = Vec::new();
    let mut texts = Vec::new();
    let mut notes = Vec::new();
    for input in &card.inputs {
        if input.pending {
            waiting.push(input.label.clone());
        } else if let Some(problem) = &input.problem {
            return Err(at(Problem::new("input", problem.clone())));
        }
        if input.media != input.port.accepts() && input.media != SourceMedia::Unknown {
            return Err(at(Problem::new(
                "input",
                format!("{} outputs {}; the {} port takes {}", input.label, input.media.name(), input.port.name(), input.port.accepts().name()),
            )));
        }
        if input.port != Port::Text {
            continue;
        }
        // A note gives its text; a document (a text file) its contents, read here.
        let text = match (&input.text, &input.path) {
            (Some(t), _) => t.clone(),
            (None, Some(path)) if input.media == SourceMedia::Text => document_text(&card.root, path, &input.label, &mut notes).await.map_err(at)?,
            _ => continue,
        };
        if !text.trim().is_empty() {
            texts.push(text.trim().to_owned());
        }
    }
    let choice = choose(media, card, setup, kind, audio).await.map_err(at)?;
    let model_name = choice.model.as_ref().map_or_else(|| choice.provider.name().to_owned(), |m| m.name.clone());
    let options = card.options.as_object().cloned().unwrap_or_default();
    // The request's fields: files by port, then the options the route knows, then sizes.
    let route = &choice.route;
    let props = route.schema["properties"].as_object().cloned().unwrap_or_default();
    let mut input = Map::new();
    let mut files = Vec::new();
    // Each file sent: its input's index in `card.inputs`, and its index within its port.
    let mut sent: Vec<(usize, u32)> = Vec::new();
    for (at, input_file) in card.inputs.iter().enumerate().filter(|(_, i)| i.port != Port::Text) {
        let Some(path) = &input_file.path else { continue };
        let Some(slot) = route.slot(input_file.port) else { continue };
        let index = files.iter().filter(|f: &&CardFile| f.port == input_file.port.id()).count() as u32 + 1;
        match slot {
            Slot::One(field) if index == 1 => {
                input.insert(field.clone(), Value::String(path.clone()));
            }
            Slot::One(_) => continue,
            Slot::Many(field, _) => {
                input.entry(field.clone()).or_insert_with(|| json!([])).as_array_mut().into_iter().for_each(|a| a.push(Value::String(path.clone())));
            }
        }
        files.push(CardFile {
            port: input_file.port.id().into(),
            index,
            path: path.clone(),
            label: input_file.label.clone(),
            field: slot.field().into(),
        });
        sent.push((at, index));
    }
    // The call's own files go first in a list field: they shift the wired ones' numbers.
    let shift = |port: Port| match route.slot(port) {
        Some(Slot::Many(field, _)) => extra.get(field).and_then(Value::as_array).map_or(0, Vec::len),
        _ => 0,
    };
    for (key, value) in extra {
        match (input.get_mut(key), value) {
            (Some(Value::Array(have)), Value::Array(first)) => {
                let mut joined = first.clone();
                joined.append(have);
                *have = joined;
            }
            (Some(_), _) => {}
            (None, v) => {
                input.insert(key.clone(), v.clone());
            }
        }
    }
    // The prompt: the card's own with its mentions expanded (the wires as sent), then the wired texts.
    let own = expand(
        card.prompt.trim(),
        |target| {
            let mention = card.mentions.iter().find(|m| m.token == target.token());
            let path = mention.and_then(|m| m.path.as_deref());
            let mut wired = card.inputs.iter().enumerate().filter(|(_, i)| names(target, i, path)).peekable();
            let Some((_, first)) = wired.peek().copied() else {
                return mention.map(|m| Expansion::new(m.label.clone(), None));
            };
            // A reference reads as the entity's name and its own, not its file's.
            let label = match (target, mention) {
                (Target::Ref { .. }, Some(m)) => m.label.clone(),
                _ => first.label.clone(),
            };
            let file = wired.find_map(|(at, i)| sent.iter().find(|(s, _)| *s == at).map(|(_, n)| (i.port, *n as usize)));
            let placed = file.map_or(Placed::Plain, |(port, n)| placed_at(port, shift(port) + n));
            Some(Expansion { label, placed: Some(placed), defines: mention.and_then(|m| m.defines.clone()) })
        },
        &mut notes,
    );
    let own = own.trim();
    let mut prompt = if card.makes == Makes::Speech {
        if own.is_empty() {
            texts.join("\n\n")
        } else {
            if !texts.is_empty() {
                notes.push("The wired text isn't read: the card has its own line.".into());
            }
            own.to_owned()
        }
    } else {
        std::iter::once(own.to_owned()).filter(|o| !o.is_empty()).chain(texts).collect::<Vec<_>>().join("\n\n")
    };
    if matches!(kind, MediaKind::Image | MediaKind::Video) && options.get("style") != Some(&Value::Bool(false)) && !prompt.is_empty() {
        prompt = with_style(prompt, setup.style.as_deref(), setup.motion.as_deref().filter(|_| kind == MediaKind::Video));
    }
    if prompt.trim().is_empty() {
        return Err(at(Problem::new("invalid", "Write the card's prompt (or wire a text into it) first")));
    }
    for (key, value) in options.into_iter().filter(|(k, v)| k != "style" && !v.is_null()) {
        if input.contains_key(&key) {
            notes.push(format!("Option {key} left out: the wires fill it."));
        } else if props.contains_key(&key) {
            input.insert(key, value);
        } else {
            notes.push(format!("{model_name} has no option {key}; left out."));
        }
    }
    if matches!(kind, MediaKind::Image | MediaKind::Video) {
        sizes(card.format.as_ref(), &route.schema, &mut input, &model_name, &mut notes);
    }
    let (tool, request, mode) = match (&route.mode, &choice.model) {
        (Some((_, mode)), Some(model)) => {
            let field = ["prompt", "text"].into_iter().find(|f| props.contains_key(*f));
            if let Some(field) = field {
                input.insert(field.into(), Value::String(prompt.clone()));
            } else {
                notes.push(format!("{model_name} {mode} takes no prompt; it isn't sent."));
            }
            let mut call = json!({"model": model.handle, "input": input});
            if !mode.is_empty() {
                call["mode"] = json!(mode);
            }
            ("generate_media", call, Some(mode.clone()).filter(|m| !m.is_empty()))
        }
        _ => {
            let tool = match kind {
                MediaKind::Image => "generate_image",
                MediaKind::Video => "generate_video",
                MediaKind::Audio => "generate_audio",
            };
            if kind == MediaKind::Audio {
                input.insert("type".into(), json!(crate::tools::audio_type_name(audio.unwrap_or(AudioType::Speech))));
                input.insert("text".into(), Value::String(prompt.clone()));
            } else {
                input.insert("prompt".into(), Value::String(prompt.clone()));
            }
            input.insert("provider".into(), json!(choice.provider.id()));
            if let Some(model) = &choice.model {
                input.insert("model".into(), json!(model.id));
            }
            (tool, Value::Object(input), None)
        }
    };
    Ok(Compiled {
        tool,
        input: request,
        kind,
        model: choice.model.as_ref().map(|m| m.handle.clone()).unwrap_or_default(),
        model_name,
        provider: choice.provider.id().into(),
        mode,
        prompt,
        files,
        notes,
        waiting,
        ports: route.ports(),
    })
}

/// Whether model `name` could make `card` as wired, and with which mode; not connected is a reason, not a misfit.
pub async fn fit(media: &Media, card: &Card, name: &str) -> ModelFit {
    let mut out = ModelFit { model: name.into(), mode: None, fits: false, reason: None };
    let Some((kind, _)) = kind_of(card.makes) else { return out };
    let model = match named(media, name, kind).await {
        Ok(m) => m,
        Err(p) => {
            out.reason = Some(p.message);
            return out;
        }
    };
    let endpoint = pick::model(media, name).await.ok().and_then(|(_, e)| e);
    // The card's mode only binds the card's own model.
    let wanted = endpoint.as_deref().or(card.mode.as_deref().filter(|_| card.model.as_deref() == Some(name)));
    match routes(media, &model, wanted, true).await.map_err(|p| p.message).and_then(|r| first_fit(r, &wired(card))) {
        Ok(route) => {
            out.fits = true;
            out.mode = route.mode.map(|(_, m)| m).filter(|m| !m.is_empty());
            if media.key(model.provider.as_ref()).is_none() {
                out.reason = Some("not connected".into());
            }
        }
        Err(why) => out.reason = Some(why),
    }
    out
}
