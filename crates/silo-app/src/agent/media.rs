//! Generated media in the transcript (`docs/MEDIA.md` §5.1, §6.4): a `generate_*` call's images, videos or
//! audio as a row of their own after its group, from the result's `[generated …]` lines (they survive
//! reloads; metadata does not), their bytes fetched once per `session/attachment` (`attachment.get`,
//! chunked): images decoded off the main thread, videos and audio written to a cache file for AVFoundation
//! (a video's poster frame read from it off the main thread).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use gpui::{AnyElement, ClickEvent, Context, Div, ObjectFit, Window, div, img, prelude::*, px, rgb};
use otto_client::{ClientError, Connection, ToolState};
use otto_llm::{Item, Part, ResultPart};
use otto_proto::{MediaKind, MediaStatus};
use serde_json::Value;

use super::{
    AgentPane,
    transcript::{self, Row, Status},
    view::ink,
};
use crate::{
    assets::icon,
    theme::{self, Continuous as _},
    tooltip::Tooltip,
};

/// The tools whose results show here.
pub const TOOL: &str = "generate_image";
pub const VIDEO_TOOL: &str = "generate_video";
pub const AUDIO_TOOL: &str = "generate_audio";
/// Any model with its own inputs (§3.2): its kind is its model's.
pub const ANY_TOOL: &str = "generate_media";

/// What tool `name` makes, when it is one of the `generate_<kind>` tools.
pub(super) fn tool_kind(name: &str) -> Option<MediaKind> {
    [TOOL, VIDEO_TOOL, AUDIO_TOOL].contains(&name).then(|| super::media_setup::kind_of(name)).flatten()
}

/// What a call of tool `name` makes: its tool's kind; `generate_media`'s its model's (by the media status), else
/// its result's (`text`), else what its model and mode say.
pub(super) fn call_kind(name: &str, input: &Value, text: &str, status: Option<&MediaStatus>) -> Option<MediaKind> {
    if name != ANY_TOOL {
        return tool_kind(name);
    }
    if let Some((kind, _)) = any_model(input, status) {
        return Some(kind);
    }
    let word = |w: &str| match w.trim_end_matches('s') {
        "image" => Some(MediaKind::Image),
        "video" => Some(MediaKind::Video),
        "audio" | "audio file" => Some(MediaKind::Audio),
        _ => None,
    };
    // `Started media job-… (video with …`, `Generated 2 images with …`, a `[generated …]` line.
    let started = text.strip_prefix("Started media ").and_then(|t| t.split_once(" (")).and_then(|(_, t)| word(t.split(' ').next()?));
    let made = || text.strip_prefix("Generated ").and_then(|t| t.split_once(" with ")).and_then(|(t, _)| word(t.split_once(' ')?.1));
    let hint = || {
        let said = format!("{} {}", input["model"].as_str().unwrap_or(""), input["mode"].as_str().unwrap_or("")).to_lowercase();
        [
            ("video", MediaKind::Video),
            ("audio", MediaKind::Audio),
            ("speech", MediaKind::Audio),
            ("music", MediaKind::Audio),
            ("image", MediaKind::Image),
        ]
        .into_iter()
        .find_map(|(w, k)| said.contains(w).then_some(k))
    };
    Some(started.or_else(made).or_else(|| parse(text).first().map(Media::kind)).or_else(hint).unwrap_or(MediaKind::Video))
}

/// `generate_media`'s model in the media status (by handle or id, `provider/…` too): its kind and name.
fn any_model(input: &Value, status: Option<&MediaStatus>) -> Option<(MediaKind, String)> {
    let model = input["model"].as_str()?.trim();
    let models = || status.into_iter().flat_map(|s| &s.providers).flat_map(|p| p.models.iter().map(move |m| (p, m)));
    let named = |name: &str| models().find(|(_, m)| m.handle.eq_ignore_ascii_case(name) || m.id.eq_ignore_ascii_case(name));
    let bare = model.split_once(['/', ':']).filter(|(p, _)| models().any(|(i, _)| i.id.eq_ignore_ascii_case(p))).map(|(_, rest)| rest);
    named(model).or_else(|| bare.and_then(named)).map(|(_, m)| (m.kind, m.name.clone()))
}

/// `generate_media`'s subject: `Seedance 2.5 · reference` (its model's name, else as called; its mode, short).
pub(super) fn any_subject(input: &Value, status: Option<&MediaStatus>) -> String {
    let model = any_model(input, status).map(|(_, n)| n).or_else(|| input["model"].as_str().map(|m| m.trim().to_owned())).unwrap_or_default();
    let mode = input["mode"].as_str().map(str::trim).unwrap_or("");
    let mode = ["-to-video", "-to-image", "-to-audio", "-to-speech"].iter().find_map(|s| mode.strip_suffix(s)).unwrap_or(mode);
    [model.as_str(), mode].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
}

/// The call's own fields (`generate_media`'s are under `input`).
pub(super) fn fields<'a>(name: &str, input: &'a Value) -> &'a Value {
    if name == ANY_TOOL { &input["input"] } else { input }
}

/// The job a backgrounded or detached call runs on as, from its result: `Started media job-… (…`.
#[must_use]
pub fn job_id(text: &str) -> Option<&str> {
    text.strip_prefix("Started media ")?.split([' ', ':', ';']).next().filter(|id| !id.is_empty())
}

/// One generated file, as its result line names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Media {
    pub id: String,
    pub name: String,
    pub mime: String,
    /// As otto wrote it (`1.2 MB`).
    pub size: String,
    /// Pixels, for kinds that have them.
    pub dims: Option<(u32, u32)>,
    /// Playing time in ms, for kinds that have one.
    pub ms: Option<u64>,
}

impl Media {
    /// `1024×1024 · PNG · 1.2 MB`, `1280×720 · MP4 · 0:08 · 4.2 MB`.
    #[must_use]
    pub fn caption(&self) -> String {
        let kind = self.mime.rsplit('/').next().unwrap_or("").trim_end_matches("+xml").to_uppercase();
        let dims = self.dims.map(|(w, h)| format!("{w}×{h}"));
        dims.into_iter().chain([kind]).chain(self.ms.map(clock)).chain([self.size.clone()]).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
    }

    /// From the mime type.
    #[must_use]
    pub fn kind(&self) -> MediaKind {
        match self.mime.split('/').next() {
            Some("video") => MediaKind::Video,
            Some("audio") => MediaKind::Audio,
            _ => MediaKind::Image,
        }
    }
}

/// A playing time: `0:08`, `12:05`, `1:02:03` (whole seconds, rounded down).
#[must_use]
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// What a call shows: tiles held for it while it runs, then its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tiles {
    /// `n` placeholders at the requested aspect ratio (`w:h`).
    Pending {
        n: usize,
        ratio: (u32, u32),
    },
    Ready(Vec<Media>),
}

/// A [`Row::Media`]: one call's images, video or audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shelf {
    pub key: String,
    pub call: String,
    pub kind: MediaKind,
    /// The prompt (audio: its text).
    pub prompt: String,
    /// `OpenAI gpt-image-2.5-flare`, from the result's first line.
    pub credit: Option<String>,
    /// The provider's latest word while it runs (`Generating with Higgsfield… in progress`).
    pub progress: Option<String>,
    /// The background job it waits for (§6.5): its progress and time come from `job.list`.
    pub job: Option<String>,
    pub tiles: Tiles,
}

/// `[generated <id>: <name>, <mime>, <size>[, <w>×<h>][, <seconds> s] — …]`.
#[must_use]
pub fn parse_line(line: &str) -> Option<Media> {
    let rest = line.trim().strip_prefix("[generated ")?;
    let (id, rest) = rest.split_once(": ")?;
    let rest = rest.split_once(" — ").map_or_else(|| rest.trim_end_matches(']'), |(fields, _)| fields);
    let dims = |f: &str| {
        let (w, h) = f.split_once('×')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    };
    let secs = |f: &str| f.strip_suffix(" s")?.trim().parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 0.0);
    // From the right: the name may hold `, `.
    let (rest, ms) = match rest.rsplit_once(", ").and_then(|(head, last)| Some((head, secs(last)?))) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((head, s)) => (head, Some((s * 1000.0).round() as u64)),
        None => (rest, None),
    };
    let (fields, dims) = match rest.rsplit_once(", ").and_then(|(head, last)| Some((head, dims(last)?))) {
        Some((head, d)) => (head, Some(d)),
        None => (rest, None),
    };
    let (head, size) = fields.rsplit_once(", ")?;
    let (name, mime) = head.rsplit_once(", ")?;
    if !mime.contains('/') || id.is_empty() {
        return None;
    }
    Some(Media { id: id.to_owned(), name: name.to_owned(), mime: mime.to_owned(), size: size.to_owned(), dims, ms })
}

/// Every generated file a result names, in order.
#[must_use]
pub fn parse(text: &str) -> Vec<Media> {
    text.lines().filter_map(parse_line).collect()
}

/// `Generated 2 images with OpenAI gpt-image-2.5-flare.` → `OpenAI gpt-image-2.5-flare`.
#[must_use]
pub fn credit(text: &str) -> Option<String> {
    let first = text.lines().next()?.strip_prefix("Generated ")?;
    let (_, by) = first.split_once(" with ")?;
    let by = by.trim().strip_suffix('.').unwrap_or(by).trim();
    (!by.is_empty()).then(|| by.to_owned())
}

/// A progress message without its trailing seconds (the tile has its own clock):
/// `Generating with Higgsfield… in progress, 40 s` → `Generating with Higgsfield… in progress`.
#[must_use]
pub fn progress(message: &str) -> String {
    let m = message.trim();
    let Some(t) = m.strip_suffix(" s") else { return m.to_owned() };
    let head = t.trim_end_matches(|c: char| c.is_ascii_digit());
    if head.len() == t.len() { m.to_owned() } else { head.trim_end_matches([',', ' ']).to_owned() }
}

/// The requested aspect ratio (`16:9`), else `default`.
fn ratio(input: &Value, default: (u32, u32)) -> (u32, u32) {
    input["aspect_ratio"]
        .as_str()
        .and_then(|r| r.split_once(':'))
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
        .filter(|(w, h)| *w > 0 && *h > 0)
        .unwrap_or(default)
}

/// Each `generate_*` call's text result, live (`tool.output`) before stored.
fn results<'a>(items: &'a [Item], tools: &'a HashMap<String, ToolState>) -> HashMap<&'a str, String> {
    let mut map: HashMap<&str, String> = HashMap::new();
    for part in items.iter().flat_map(|i| &i.parts) {
        if let Part::ToolResult { call_id, content, .. } = part {
            let text = content.iter().filter_map(|c| if let ResultPart::Text { text } = c { Some(text.as_str()) } else { None });
            map.insert(call_id, text.collect::<Vec<_>>().join("\n"));
        }
    }
    for (id, t) in tools.iter().filter(|(_, t)| tool_kind(&t.name).is_some() || t.name == ANY_TOOL) {
        if let Some(parts) = t.output.as_ref().and_then(|o| o["content"].as_array()) {
            map.insert(id, parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"));
        }
    }
    map
}

/// After each group, a [`Row::Media`] per `generate_*` call in it: placeholders while it runs (or its background
/// job does), its files once done (a job's from its report: `jobs`), none when it failed, stopped or made nothing.
/// `generate_media`'s kind comes from its model in `status` (§3.2).
pub(super) fn place(
    rows: &mut Vec<Row>,
    items: &[Item],
    tools: &HashMap<String, ToolState>,
    jobs: &HashMap<String, (transcript::JobEnd, String)>,
    status: Option<&MediaStatus>,
) {
    let results = results(items, tools);
    let result = |id: &str| {
        let text = results.get(id).map_or("", String::as_str);
        job_id(text).and_then(|id| jobs.get(id)).map_or(text, |(_, result)| result.as_str())
    };
    let kind = |id: &str, name: &str, input: &Value| call_kind(name, input, result(id), status);
    let mut calls: HashMap<&str, (MediaKind, &Value)> =
        items.iter().flat_map(Item::tool_calls).filter_map(|(id, name, input)| Some((id, (kind(id, name, input)?, fields(name, input))))).collect();
    for (id, t) in tools {
        if let Some(k) = kind(id, &t.name, &t.input) {
            calls.entry(id).or_insert((k, fields(&t.name, &t.input)));
        }
    }
    if calls.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(rows.len() + calls.len());
    for row in rows.drain(..) {
        let shelves: Vec<Row> = match &row {
            Row::Group(g) => g
                .lines
                .iter()
                .filter_map(|l| {
                    let &(kind, input) = calls.get(l.key.as_str())?;
                    let text = result(&l.key);
                    let tiles = match (l.status, kind) {
                        (Status::Running | Status::Pending, MediaKind::Image) => Tiles::Pending {
                            n: input["n"].as_u64().map_or(1, |n| usize::try_from(n.clamp(1, 4)).unwrap_or(1)),
                            ratio: ratio(input, (1, 1)),
                        },
                        (Status::Running | Status::Pending, _) => Tiles::Pending { n: 1, ratio: ratio(input, (16, 9)) },
                        (Status::Done, _) => Some(parse(text)).filter(|m| !m.is_empty()).map(Tiles::Ready)?,
                        _ => return None,
                    };
                    let prompt = input[if kind == MediaKind::Audio && input["text"].is_string() { "text" } else { "prompt" }]
                        .as_str()
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    let progress = tools
                        .get(&l.key)
                        .filter(|_| matches!(tiles, Tiles::Pending { .. }))
                        .and_then(|t| t.messages.last())
                        .map(|m| progress(m))
                        .filter(|m| !m.is_empty());
                    let (key, call, credit) = (format!("{}#media", l.key), l.key.clone(), credit(text));
                    let job = l.job.as_ref().map(|j| j.id.clone());
                    Some(Row::Media(Shelf { key, call, kind, prompt, credit, progress, job, tiles }))
                })
                .collect(),
            _ => Vec::new(),
        };
        out.push(row);
        out.extend(shelves);
    }
    *rows = out;
}

/// A fetched image: what the viewer draws, and a smaller copy for the tiles (an animated GIF's is itself: gpui
/// plays it).
pub(super) struct Loaded {
    pub image: Arc<gpui::Image>,
    pub preview: Arc<gpui::Image>,
    pub dims: (u32, u32),
    /// An SVG's document, for the viewer's sharp rasters.
    pub svg: Option<Arc<super::svg::Svg>>,
    /// Decoded at (tiles fade it in).
    pub at: Instant,
}

#[derive(Clone)]
pub(super) enum Entry {
    /// Tiles shimmer meanwhile.
    Loading,
    Ready(Arc<Loaded>),
    /// A video or audio file, written for AVFoundation.
    Clip(Arc<Clip>),
    /// Not fetched or not readable: a quiet tile, never fetched again.
    Failed,
}

/// A video or audio file in the cache, and what AVFoundation read from it.
pub(super) struct Clip {
    pub file: PathBuf,
    /// A video's first frame, and its size in pixels.
    pub poster: Option<(Arc<gpui::Image>, (u32, u32))>,
    pub ms: Option<u64>,
    pub at: Instant,
}

/// `<cache>/media/<session>/<id>.<ext>`: where a video or audio file goes for AVFoundation.
#[must_use]
pub fn cache_file(root: &Path, session: &str, id: &str, name: &str) -> PathBuf {
    let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect::<String>();
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map_or_else(|| "bin".to_owned(), safe);
    root.join("media").join(safe(session)).join(format!("{}.{ext}", safe(id)))
}

/// Off the main thread: `bytes` written to `file` (once; a partial write never takes its name), then read.
fn store(file: PathBuf, bytes: Option<Vec<u8>>, video: bool) -> Option<Clip> {
    if let Some(bytes) = bytes {
        std::fs::create_dir_all(file.parent()?).ok()?;
        let part = file.with_extension("part");
        std::fs::write(&part, bytes).ok()?;
        std::fs::rename(&part, &file).ok()?;
    }
    let probe = super::av::probe(&file, video, PREVIEW);
    let poster = probe.poster.map(|(png, dims)| (Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png)), dims));
    Some(Clip { file, poster, ms: probe.ms, at: Instant::now() })
}

/// Fetched media by `session/attachment`, for the pane's life.
#[derive(Default)]
pub(super) struct Cache(HashMap<String, Entry>);

impl Cache {
    pub(super) fn get(&self, key: &str) -> Option<&Entry> {
        self.0.get(key)
    }
}

/// A tile preview's longest edge in pixels (320 pt at 2×).
const PREVIEW: u32 = 640;

/// All of attachment `id`'s bytes, a chunk (`ATTACHMENT_READ`) per call.
pub(super) async fn fetch(c: Connection, session: String, id: String) -> Result<Vec<u8>, ClientError> {
    let mut bytes = Vec::new();
    loop {
        let offset = bytes.len() as u64;
        let method = otto_proto::Method::AttachmentGet { session_id: session.clone(), attachment_id: id.clone(), offset, length: None };
        let part: otto_proto::AttachmentData = c.call_typed(method).await?;
        let chunk = data_encoding::BASE64.decode(part.data_base64.as_bytes()).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if chunk.is_empty() && offset < part.size {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 >= part.size {
            return Ok(bytes);
        }
    }
}

/// Off the main thread: the image's size and a preview for the tiles (an SVG's at the tile's pixels).
fn decode(bytes: Vec<u8>, mime: &str) -> Option<Loaded> {
    use gpui::ImageFormat as G;
    if mime == "image/svg+xml" || super::svg::sniff(&bytes) {
        let svg = super::svg::Svg::parse(&bytes)?;
        let (w, h) = svg.size();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let dims = ((w.round() as u32).max(1), (h.round() as u32).max(1));
        let preview = Arc::new(gpui::Image::from_bytes(G::Png, svg.png(PREVIEW as f32)?));
        let image = Arc::new(gpui::Image::from_bytes(G::Svg, bytes));
        return Some(Loaded { image, preview, dims, svg: Some(Arc::new(svg)), at: Instant::now() });
    }
    let format = image::guess_format(&bytes).ok()?;
    let shown = match format {
        image::ImageFormat::Png => G::Png,
        image::ImageFormat::Jpeg => G::Jpeg,
        image::ImageFormat::WebP => G::Webp,
        image::ImageFormat::Gif => G::Gif,
        image::ImageFormat::Bmp => G::Bmp,
        image::ImageFormat::Tiff => G::Tiff,
        _ => return None,
    };
    let decoded = image::load_from_memory_with_format(&bytes, format).ok()?;
    let dims = (decoded.width(), decoded.height());
    let image = Arc::new(gpui::Image::from_bytes(shown, bytes));
    let preview = if dims.0.max(dims.1) <= PREVIEW || shown == G::Gif {
        image.clone()
    } else {
        let mut png = std::io::Cursor::new(Vec::new());
        decoded.thumbnail(PREVIEW, PREVIEW).write_to(&mut png, image::ImageFormat::Png).ok()?;
        Arc::new(gpui::Image::from_bytes(G::Png, png.into_inner()))
    };
    Some(Loaded { image, preview, dims, svg: None, at: Instant::now() })
}

impl AgentPane {
    /// File `m` of the shown session, fetched on first ask (one fetch per file): an image decoded, a video or
    /// audio file written to the cache (or found there) and read.
    pub(super) fn media(&mut self, m: &Media, cx: &mut Context<Self>) -> Entry {
        let Some(session) = self.session_id.clone() else { return Entry::Failed };
        let key = format!("{session}/{}", m.id);
        if let Some(entry) = self.media.0.get(&key) {
            return entry.clone();
        }
        self.media.0.insert(key.clone(), Entry::Loading);
        let (kind, mime) = (m.kind(), m.mime.clone());
        let file = (kind != MediaKind::Image).then(|| cache_file(&crate::cache::root(), &session, &m.id, &m.name));
        let cached = file.as_ref().is_some_and(|f| f.exists());
        let failed = key.clone();
        let settle = move |bytes: Option<Vec<u8>>, cx: &mut Context<Self>| {
            let video = kind == MediaKind::Video;
            let done = cx.background_executor().spawn(async move {
                match file {
                    Some(file) => store(file, bytes, video).map(|c| Entry::Clip(Arc::new(c))),
                    None => bytes.and_then(|b| decode(b, &mime)).map(|l| Entry::Ready(Arc::new(l))),
                }
            });
            cx.spawn(async move |this, cx| {
                let entry = done.await.unwrap_or(Entry::Failed);
                let _ = this.update(cx, |this, cx| {
                    this.media.0.insert(key, entry);
                    cx.notify();
                });
            })
            .detach();
        };
        if cached {
            settle(None, cx);
            return Entry::Loading;
        }
        let id = m.id.clone();
        self.run(
            cx,
            move |c| fetch(c, session, id),
            move |this, fetched: Result<Vec<u8>, String>, cx| match fetched {
                Ok(bytes) => settle(Some(bytes), cx),
                Err(_) => _ = this.media.0.insert(failed, Entry::Failed),
            },
        );
        Entry::Loading
    }
}

/// Tiles' corner radius, the gap between them, and their height bounds (under `MIN_H` they wrap).
pub(super) const RADIUS: f32 = 10.0;
const GAP: f32 = 8.0;
const MAX_H: f32 = 320.0;
const MIN_H: f32 = 120.0;
/// A tile's pulse while it waits, and its image fading in.
pub(super) const PULSE: f32 = 1.6;
const FADE_IN: std::time::Duration = std::time::Duration::from_millis(180);

/// Tile sizes for aspect ratios `ratios` (w/h) in a column `avail` wide: one row of one height filling it, at most
/// [`MAX_H`] tall and never wider than the column.
#[must_use]
pub fn tile_sizes(ratios: &[f32], avail: f32) -> Vec<(f32, f32)> {
    #[allow(clippy::cast_precision_loss)]
    let gaps = GAP * ratios.len().saturating_sub(1) as f32;
    let sum: f32 = ratios.iter().sum::<f32>().max(0.01);
    let h = ((avail - gaps) / sum).clamp(MIN_H, MAX_H);
    ratios.iter().map(|r| ((h * r).min(avail), (h * r).min(avail) / r)).collect()
}

/// A tile's frame: rounded, a faint border, its fill pulsing while `pulse`.
pub(super) fn frame(w: f32, h: f32, pulse: Option<f32>) -> Div {
    // Eased in and out over a span wide enough for a dozen-odd 8-bit steps: smooth, not stepped.
    let fill = pulse.map_or(0.05, |p| 0.04 + 0.05 * (0.5 - 0.5 * (p * std::f32::consts::TAU).cos()));
    div()
        .relative()
        .flex_none()
        .w(px(w))
        .h(px(h))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .overflow_hidden()
        .continuous(RADIUS)
        .border_1()
        .border_color(rgb(ink(0.1)))
        .bg(rgb(ink(fill)))
}

/// The kind's glyph on placeholders.
pub(super) fn glyph(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio-lines",
    }
}

impl AgentPane {
    /// A [`Row::Media`]: placeholders while the call runs (the first with its time and the provider's word),
    /// then the files, each a pulse while its bytes come, `… unavailable` if they don't; hover names it, a click
    /// opens the viewer. Audio is a player row (`audio.rs`).
    pub(super) fn render_media(&mut self, shelf: &Shelf, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // A background job's word, from `job.list`.
        let listed = shelf.job.as_ref().and_then(|j| self.shell_jobs.listed.get(j)).and_then(|j| j.progress.as_deref()).map(progress);
        let shelf = &Shelf { progress: listed.or_else(|| shelf.progress.clone()), ..shelf.clone() };
        let width = self.width.get();
        let avail = if width > 0.0 { (width - 40.0).clamp(MIN_H, super::view::COLUMN) } else { super::view::COLUMN };
        let reduce = cx.reduce_motion();
        let pulse = (!reduce).then(|| now.duration_since(self.epoch).as_secs_f32() % PULSE / PULSE);
        let muted = theme::muted();
        // Waiting tiles pulse every frame (not only when the clock or `job.list` repaints).
        if !reduce && matches!(shelf.tiles, Tiles::Pending { .. }) {
            window.request_animation_frame();
        }
        let tiles: Vec<AnyElement> = match &shelf.tiles {
            Tiles::Pending { .. } if shelf.kind == MediaKind::Audio => vec![self.render_audio_pending(shelf, avail, pulse)],
            Tiles::Pending { n, ratio } => {
                #[allow(clippy::cast_precision_loss)]
                let r = ratio.0 as f32 / ratio.1 as f32;
                let elapsed = self.pending_clock(shelf);
                tile_sizes(&vec![r; *n], avail)
                    .into_iter()
                    .enumerate()
                    .map(|(i, (w, h))| {
                        let note =
                            |t: String, size: f32| div().text_size(px(size)).text_color(rgb(muted)).font_features(super::view::tabular()).child(t);
                        frame(w, h, pulse)
                            .child(icon(glyph(shelf.kind), 18.0, muted))
                            .children(elapsed.clone().filter(|_| i == 0).map(|t| note(t, 12.0)))
                            .children(
                                shelf
                                    .progress
                                    .clone()
                                    .filter(|_| i == 0)
                                    .map(|t| note(t, 11.5).max_w(px(w - 24.0)).overflow_hidden().whitespace_nowrap().text_ellipsis()),
                            )
                            .into_any_element()
                    })
                    .collect()
            }
            Tiles::Ready(media) if shelf.kind == MediaKind::Audio => media.iter().map(|m| self.render_audio(m, avail, pulse, window, cx)).collect(),
            Tiles::Ready(media) => {
                let entries: Vec<Entry> = media.iter().map(|m| self.media(m, cx)).collect();
                #[allow(clippy::cast_precision_loss)]
                let ratios: Vec<f32> = media
                    .iter()
                    .zip(&entries)
                    .map(|(m, e)| {
                        let dims = m.dims.or(match e {
                            Entry::Ready(l) => Some(l.dims),
                            Entry::Clip(c) => c.poster.as_ref().map(|p| p.1),
                            _ => None,
                        });
                        let default = if m.kind() == MediaKind::Video { 16.0 / 9.0 } else { 1.0 };
                        dims.filter(|(w, h)| *w > 0 && *h > 0).map_or(default, |(w, h)| w as f32 / h as f32)
                    })
                    .collect();
                if !reduce && entries.iter().any(|e| matches!(e, Entry::Loading)) {
                    window.request_animation_frame();
                }
                let sizes = tile_sizes(&ratios, avail);
                media.iter().zip(entries).zip(sizes).map(|((m, e), (w, h))| self.media_tile(m, e, w, h, pulse, now, cx)).collect()
            }
        };
        div().flex().flex_wrap().gap(px(GAP)).children(tiles).into_any_element()
    }

    /// How long a pending shelf has waited: its call, or its background job since it started.
    pub(super) fn pending_clock(&self, shelf: &Shelf) -> Option<String> {
        let started = shelf.job.as_ref().and_then(|j| self.shell_jobs.listed.get(j)).map(|j| Self::pane_ms(j.started_at));
        let started = started.or_else(|| shelf.job.as_ref().and_then(|_| self.times.get(&shelf.call)?.start));
        let elapsed = match started {
            Some(at) => Some(self.now().saturating_sub(at)),
            None => self.times.elapsed(&shelf.call, self.now()),
        };
        elapsed.map(transcript::dur)
    }

    #[allow(clippy::too_many_arguments)]
    fn media_tile(&self, m: &Media, entry: Entry, w: f32, h: f32, pulse: Option<f32>, now: Instant, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let kind = m.kind();
        // An id keeps an animated GIF's frame between renders (gpui plays it).
        let picture = |image: Arc<gpui::Image>, at: Instant, cx: &mut Context<Self>| {
            let t = super::view::eased(at, FADE_IN, now, cx).unwrap_or(1.0);
            img(image)
                .id(gpui::ElementId::Name(format!("media-img-{}", m.id).into()))
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded(px(RADIUS - 1.0))
                .object_fit(ObjectFit::Cover)
                .opacity(t)
        };
        let tile = match entry {
            Entry::Loading => frame(w, h, pulse),
            Entry::Failed => frame(w, h, None).child(icon(glyph(kind), 16.0, muted)).child(
                div().text_size(px(12.0)).text_color(rgb(muted)).child(if kind == MediaKind::Video {
                    "Video unavailable"
                } else {
                    "Image unavailable"
                }),
            ),
            Entry::Ready(l) => frame(w, h, None).child(picture(l.preview.clone(), l.at, cx)),
            Entry::Clip(c) => {
                let ms = m.ms.or(c.ms);
                frame(w, h, None).children(c.poster.as_ref().map(|(p, _)| picture(p.clone(), c.at, cx))).child(play_badge()).children(ms.map(|ms| {
                    div()
                        .absolute()
                        .right(px(8.0))
                        .bottom(px(8.0))
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(6.0))
                        .bg(gpui::hsla(0.0, 0.0, 0.0, 0.55))
                        .text_size(px(11.5))
                        .text_color(gpui::white())
                        .font_features(super::view::tabular())
                        .child(clock(ms))
                }))
            }
        };
        let id = m.id.clone();
        tile.id(gpui::ElementId::Name(format!("media-{}", m.id).into()))
            .cursor_pointer()
            .hover(|s| s.border_color(rgb(ink(0.22))))
            .tooltip(Tooltip::text(format!("{} · {}", m.name, m.size)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.open_viewer(&id, window, cx)))
            .into_any_element()
    }
}

/// A round dark badge with a play glyph, centred on a video's poster.
fn play_badge() -> Div {
    div().absolute().size_full().flex().items_center().justify_center().child(
        div()
            .size(px(44.0))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.5))
            .child(gpui::svg().path("icons/play.svg").size(px(18.0)).ml(px(2.0)).text_color(gpui::white())),
    )
}

#[cfg(test)]
mod tests {
    use otto_client::ToolStatus;
    use otto_llm::{CallKind, Role};
    use otto_proto::TurnStatus;
    use serde_json::json;

    use super::*;
    use crate::agent::transcript::{Input, Kind, Times, build, indicator_is_group};

    const LINE: &str = "[generated att_3f2a9c1b: sunset-over-dunes.png, image/png, 1.2 MB, 1024×1024 — copy_attachment can save it into the project]";

    #[test]
    fn result_lines_parse() {
        let m = parse_line(LINE).unwrap();
        assert_eq!(
            (m.id.as_str(), m.name.as_str(), m.mime.as_str(), m.size.as_str()),
            ("att_3f2a9c1b", "sunset-over-dunes.png", "image/png", "1.2 MB")
        );
        assert_eq!(m.dims, Some((1024, 1024)));
        assert_eq!(m.caption(), "1024×1024 · PNG · 1.2 MB");
        let audio = parse_line("[generated att_1: a, b.mp3, audio/mpeg, 300 KB — copy_attachment can save it]").unwrap();
        assert_eq!((audio.name.as_str(), audio.dims, audio.caption()), ("a, b.mp3", None, "MPEG · 300 KB".to_owned()));
        assert!(parse_line("Generated 1 image with OpenAI gpt-image-2.5-flare.").is_none());
        assert!(parse_line("[generated att_1: broken]").is_none());
        let text =
            format!("Generated 2 images with OpenAI gpt-image-2.5-flare.\n{LINE}\n{}\nSaved to assets/sunset.png", LINE.replace("3f2a9c1b", "77"));
        assert_eq!(parse(&text).len(), 2);
        assert_eq!(credit(&text).as_deref(), Some("OpenAI gpt-image-2.5-flare"));
    }

    #[test]
    fn tiles_fill_one_row_within_bounds() {
        assert_eq!(tile_sizes(&[1.0, 1.0], 720.0), vec![(320.0, 320.0); 2]);
        assert_eq!(tile_sizes(&[16.0 / 9.0], 400.0), vec![(400.0, 225.0)]);
        // Too narrow for one row: the smallest tiles, wrapping.
        assert_eq!(tile_sizes(&[1.0; 4], 400.0), vec![(120.0, 120.0); 4]);
    }

    fn call(input: Value) -> Part {
        Part::ToolCall { call_id: "c1".into(), name: TOOL.into(), input, kind: CallKind::Function, opaque: None }
    }

    fn rows(items: &[Item], tools: &HashMap<String, ToolState>, turn: TurnStatus) -> Vec<Row> {
        let times = Times::default();
        let input = Input {
            items,
            tools,
            turn: Some(turn),
            error: None,
            retrying: None,
            compactions: &[],
            cwd: None,
            home: None,
            times: &times,
            now: 0,
            older: false,
            turns: &[],
            media: None,
        };
        build(&input)
    }

    #[test]
    fn images_follow_their_group_after_a_reload() {
        let result = format!("Generated 2 images with OpenAI gpt-image-2.5-flare.\n{LINE}\n{}", LINE.replace("3f2a9c1b", "77"));
        let items = [
            Item::user_text("u1", "draw dunes"),
            Item::new("a1", Role::Assistant, vec![call(json!({"prompt": "Sunset over dunes", "n": 2}))]),
            Item::new("t1", Role::Tool, vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text(result)], is_error: false }]),
            Item::new("a2", Role::Assistant, vec![Part::Text { text: "Here they are.".into(), citations: Vec::new() }]),
        ];
        let rows = rows(&items, &HashMap::new(), TurnStatus::Done);
        let [Row::User { .. }, Row::Group(g), Row::Media(shelf), Row::Text { .. }] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((g.lines[0].kind, g.lines[0].title.as_str()), (Kind::Media, "Generated images Sunset over dunes"));
        assert_eq!(g.summary, "Generated 2 images", "the fold counts them");
        assert_eq!(
            (shelf.key.as_str(), shelf.prompt.as_str(), shelf.credit.as_deref()),
            ("c1#media", "Sunset over dunes", Some("OpenAI gpt-image-2.5-flare"))
        );
        let Tiles::Ready(media) = &shelf.tiles else { panic!("{shelf:?}") };
        assert_eq!(media.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["att_3f2a9c1b", "att_77"]);
    }

    #[test]
    fn a_running_call_holds_tiles_and_its_group_stays_the_indicator() {
        let items =
            [Item::user_text("u1", "draw"), Item::new("a1", Role::Assistant, vec![call(json!({"prompt": "p", "n": 3, "aspect_ratio": "16:9"}))])];
        let live = ToolState { name: TOOL.into(), status: ToolStatus::Running, ..ToolState::default() };
        let rows = rows(&items, &HashMap::from([("c1".to_owned(), live)]), TurnStatus::Running);
        let [Row::User { .. }, Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert!(g.live);
        assert_eq!(shelf.tiles, Tiles::Pending { n: 3, ratio: (16, 9) });
        assert!(indicator_is_group(&rows), "the live group's header stays the indicator");
    }

    #[test]
    fn a_failed_call_says_why_and_shows_no_images() {
        let items = [
            Item::new("a1", Role::Assistant, vec![call(json!({"prompt": "p"}))]),
            Item::new(
                "t1",
                Role::Tool,
                vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text("content policy")], is_error: true }],
            ),
        ];
        let rows = rows(&items, &HashMap::new(), TurnStatus::Done);
        let [Row::Group(g)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!(g.lines[0].title, "Couldn't generate the image: content policy");
    }

    const VIDEO: &str = "[generated att_v1: fox-running.mp4, video/mp4, 4.2 MB, 1280×720, 8 s — copy_attachment can save it into the project]";
    const AUDIO: &str = "[generated att_a1: hello, world.mp3, audio/mpeg, 210 KB, 12.4 s — copy_attachment can save it into the project]";

    #[test]
    fn durations_and_kinds_parse() {
        let v = parse_line(VIDEO).unwrap();
        assert_eq!((v.name.as_str(), v.dims, v.ms, v.kind()), ("fox-running.mp4", Some((1280, 720)), Some(8000), MediaKind::Video));
        assert_eq!(v.caption(), "1280×720 · MP4 · 0:08 · 4.2 MB");
        let a = parse_line(AUDIO).unwrap();
        assert_eq!((a.name.as_str(), a.size.as_str(), a.dims, a.ms, a.kind()), ("hello, world.mp3", "210 KB", None, Some(12_400), MediaKind::Audio));
        // Either part may be missing.
        let v = parse_line("[generated att_v2: a.mp4, video/mp4, 1 MB, 5 s]").unwrap();
        assert_eq!((v.dims, v.ms, v.size.as_str()), (None, Some(5000), "1 MB"));
        let v = parse_line("[generated att_v3: a.mp4, video/mp4, 1 MB, 640×360 — …]").unwrap();
        assert_eq!((v.dims, v.ms), (Some((640, 360)), None));
        assert_eq!(parse_line(LINE).unwrap().kind(), MediaKind::Image);
    }

    #[test]
    fn clocks_and_progress_read_short() {
        assert_eq!((clock(0), clock(8_999), clock(65_000), clock(3_725_000)), ("0:00".into(), "0:08".into(), "1:05".into(), "1:02:05".into()));
        assert_eq!(progress("Generating with Higgsfield… 40 s"), "Generating with Higgsfield…");
        assert_eq!(progress("Generating with Higgsfield… in progress, 12 s"), "Generating with Higgsfield… in progress");
        assert_eq!(progress("Generating with OpenAI…"), "Generating with OpenAI…");
        assert_eq!(progress("Queued: 3 ahead"), "Queued: 3 ahead");
    }

    #[test]
    fn cache_files_sit_under_the_session() {
        let f = cache_file(Path::new("/c"), "ses_1", "att_9", "fox running.MP4");
        assert_eq!(f, Path::new("/c/media/ses_1/att_9.MP4"));
        assert_eq!(cache_file(Path::new("/c"), "../x", "a/b", "noext"), Path::new("/c/media/___x/a_b.bin"));
    }

    fn named(name: &str, input: Value) -> Part {
        Part::ToolCall { call_id: "c1".into(), name: name.into(), input, kind: CallKind::Function, opaque: None }
    }

    #[test]
    fn a_video_follows_its_group_and_waits_at_its_ratio() {
        let result = format!("Generated 1 video with Higgsfield Kling 3.0 Pro.\n{VIDEO}");
        let items = [
            Item::new("a1", Role::Assistant, vec![named(VIDEO_TOOL, json!({"prompt": "A fox running"}))]),
            Item::new("t1", Role::Tool, vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text(result)], is_error: false }]),
        ];
        let rows = self::rows(&items, &HashMap::new(), TurnStatus::Done);
        let [Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((g.lines[0].kind, g.lines[0].title.as_str()), (Kind::Video, "Generated video A fox running"));
        assert_eq!(g.summary, "Generated 1 video");
        assert_eq!((shelf.kind, shelf.credit.as_deref()), (MediaKind::Video, Some("Higgsfield Kling 3.0 Pro")));
        let Tiles::Ready(media) = &shelf.tiles else { panic!("{shelf:?}") };
        assert_eq!(media[0].ms, Some(8000));
        // Running: one placeholder, 16:9 unless asked otherwise, with the provider's word.
        let items = [Item::new("a1", Role::Assistant, vec![named(VIDEO_TOOL, json!({"prompt": "p", "aspect_ratio": "9:16"}))])];
        let live = ToolState {
            name: VIDEO_TOOL.into(),
            status: ToolStatus::Running,
            messages: vec!["Generating with Higgsfield… queued, 4 s".into(), "Generating with Higgsfield… 40 s".into()],
            ..ToolState::default()
        };
        let rows = self::rows(&items, &HashMap::from([("c1".to_owned(), live)]), TurnStatus::Running);
        let [Row::Group(_), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((&shelf.tiles, shelf.progress.as_deref()), (&Tiles::Pending { n: 1, ratio: (9, 16) }, Some("Generating with Higgsfield…")));
        let items = [Item::new("a1", Role::Assistant, vec![named(VIDEO_TOOL, json!({"prompt": "p"}))])];
        let live = ToolState { name: VIDEO_TOOL.into(), status: ToolStatus::Running, ..ToolState::default() };
        let rows = self::rows(&items, &HashMap::from([("c1".to_owned(), live)]), TurnStatus::Running);
        assert!(matches!(&rows[..], [Row::Group(_), Row::Media(Shelf { tiles: Tiles::Pending { n: 1, ratio: (16, 9) }, .. })]), "{rows:?}");
    }

    #[test]
    fn audio_follows_its_group_titled_by_its_type() {
        let items = [
            Item::user_text("u1", "say hello"),
            Item::new("a1", Role::Assistant, vec![named(AUDIO_TOOL, json!({"type": "music", "text": "Calm piano, rain"}))]),
            Item::new("t1", Role::Tool, vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text(AUDIO)], is_error: false }]),
            Item::new("a2", Role::Assistant, vec![Part::Text { text: "Done.".into(), citations: Vec::new() }]),
        ];
        let rows = self::rows(&items, &HashMap::new(), TurnStatus::Done);
        let [Row::User { .. }, Row::Group(g), Row::Media(shelf), Row::Text { .. }] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((g.lines[0].kind, g.lines[0].title.as_str()), (Kind::Audio, "Generated music Calm piano, rain"));
        assert_eq!(g.summary, "Generated 1 audio clip");
        assert_eq!((shelf.kind, shelf.prompt.as_str()), (MediaKind::Audio, "Calm piano, rain"));
        let speech = [Item::new("a1", Role::Assistant, vec![named(AUDIO_TOOL, json!({"type": "speech", "text": "Hello"}))])];
        let live = ToolState { name: AUDIO_TOOL.into(), status: ToolStatus::Running, ..ToolState::default() };
        let rows = self::rows(&speech, &HashMap::from([("c1".to_owned(), live)]), TurnStatus::Running);
        let [Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!(g.lines[0].title, "Generating speech Hello");
        assert_eq!(shelf.tiles, Tiles::Pending { n: 1, ratio: (16, 9) });
        let failed = [
            Item::new("a1", Role::Assistant, vec![named(AUDIO_TOOL, json!({"type": "sound", "text": "door"}))]),
            Item::new(
                "t1",
                Role::Tool,
                vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text("rate limited")], is_error: true }],
            ),
        ];
        let rows = self::rows(&failed, &HashMap::new(), TurnStatus::Done);
        let [Row::Group(g)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!(g.lines[0].title, "Couldn't generate the audio: rate limited");
    }

    #[test]
    fn svg_and_gif_lines_parse_and_decode() {
        let svg = parse_line("[generated att_s1: logo.svg, image/svg+xml, 12 KB, 1024×1024 — copy_attachment can save it into the project]").unwrap();
        assert_eq!((svg.dims, svg.kind(), svg.caption()), (Some((1024, 1024)), MediaKind::Image, "1024×1024 · SVG · 12 KB".to_owned()));
        let bare = parse_line("[generated att_s2: logo.svg, image/svg+xml, 3 KB — …]").unwrap();
        assert_eq!((bare.name.as_str(), bare.dims, bare.ms), ("logo.svg", None, None));
        let gif = parse_line("[generated att_g1: dancing-cat.gif, image/gif, 2.1 MB, 480×480 — …]").unwrap();
        assert_eq!((gif.dims, gif.kind(), gif.caption()), (Some((480, 480)), MediaKind::Image, "480×480 · GIF · 2.1 MB".to_owned()));
        // An SVG without dimensions takes its own size; the image crate never sees it.
        let doc = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 32"><circle cx="16" cy="16" r="16" fill="#0a0"/></svg>"##;
        let l = decode(doc.to_vec(), "image/svg+xml").unwrap();
        assert_eq!((l.dims, l.svg.is_some(), l.image.format()), ((64, 32), true, gpui::ImageFormat::Svg));
        assert_eq!(l.preview.format(), gpui::ImageFormat::Png);
        assert_eq!(l.image.bytes(), doc, "saved as made");
        // A GIF's preview is itself, so the tile plays it.
        let mut gif = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut gif);
            let frame = |v: u8| image::Frame::new(image::RgbaImage::from_pixel(900, 700, image::Rgba([v, 0, 0, 255])));
            enc.encode_frames([frame(0), frame(255)]).unwrap();
        }
        let l = decode(gif, "image/gif").unwrap();
        assert_eq!((l.dims, l.image.format(), l.svg.is_none()), ((900, 700), gpui::ImageFormat::Gif, true));
        assert!(Arc::ptr_eq(&l.image, &l.preview));
    }

    const STARTED: &str = "Started media job-a1 (video with xAI Grok Imagine Video 1.5); the result arrives as a message when it's done. Keep talking with the user; don't wait or poll for it.";

    fn said(id: &str, text: &str) -> Item {
        Item::new(id, Role::Assistant, vec![Part::Text { text: text.into(), citations: Vec::new() }])
    }

    #[test]
    fn a_background_video_waits_in_its_place_until_its_report() {
        let started =
            Item::new("t1", Role::Tool, vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text(STARTED)], is_error: false }]);
        let mut items = vec![
            Item::user_text("u1", "make a fox video"),
            Item::new("a1", Role::Assistant, vec![named(VIDEO_TOOL, json!({"prompt": "A fox running", "aspect_ratio": "9:16"}))]),
            started,
            said("a2", "It's rendering; meanwhile?"),
        ];
        let rows = self::rows(&items, &HashMap::new(), TurnStatus::Done);
        let [Row::User { .. }, Row::Group(g), Row::Media(shelf), Row::Text { .. }] = &rows[..] else { panic!("{rows:?}") };
        let line = &g.lines[0];
        assert_eq!((line.status, line.title.as_str(), line.desc.as_deref()), (Status::Running, "Generating video A fox running", Some("background")));
        let job = line.job.as_ref().unwrap();
        assert_eq!((job.id.as_str(), job.media, job.command.as_str()), ("job-a1", Some(MediaKind::Video), "A fox running"));
        assert_eq!((&shelf.tiles, shelf.job.as_deref(), shelf.key.as_str()), (&Tiles::Pending { n: 1, ratio: (9, 16) }, Some("job-a1"), "c1#media"));
        assert_eq!(crate::agent::jobs::running(&rows).len(), 1, "in the jobs bar");
        // The report (a quiet system item, no row of its own) fills the same place; its turn's reply follows.
        let report = format!("Background media job-a1 (video) finished.\nGenerated 1 video with xAI grok-imagine-video-1.5.\n{VIDEO}");
        items.push(Item::new("s1", Role::System, vec![Part::System { text: report }]));
        items.push(said("a3", "Here it is."));
        let rows = self::rows(&items, &HashMap::new(), TurnStatus::Done);
        let [Row::User { .. }, Row::Group(g), Row::Media(shelf), Row::Text { .. }, Row::Text { .. }] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((g.lines[0].status, g.lines[0].title.as_str(), g.lines[0].job.is_none()), (Status::Done, "Generated video A fox running", true));
        assert_eq!((shelf.key.as_str(), shelf.job.as_deref(), shelf.credit.as_deref()), ("c1#media", None, Some("xAI grok-imagine-video-1.5")));
        let Tiles::Ready(media) = &shelf.tiles else { panic!("{shelf:?}") };
        assert_eq!(media[0].id, "att_v1");
        assert!(crate::agent::jobs::running(&rows).is_empty());
        // Failed or stopped: said on the line, no tiles.
        for (end, status, title) in [
            ("failed: xAI: rate limited", Status::Error, "Couldn't generate the video: xAI: rate limited"),
            ("was stopped.", Status::Stopped, "Generated video A fox running"),
        ] {
            items[4] = Item::new("s1", Role::System, vec![Part::System { text: format!("Background media job-a1 (video) {end}") }]);
            let rows = self::rows(&items, &HashMap::new(), TurnStatus::Done);
            let [Row::User { .. }, Row::Group(g), Row::Text { .. }, Row::Text { .. }] = &rows[..] else { panic!("{rows:?}") };
            assert_eq!((g.lines[0].status, g.lines[0].title.as_str()), (status, title));
        }
    }

    #[test]
    fn live_a_backgrounded_call_keeps_its_tile_and_a_running_one_can_detach() {
        let items = [Item::new("a1", Role::Assistant, vec![named(VIDEO_TOOL, json!({"prompt": "p"}))])];
        let output = json!({"content": [{"type": "text", "text": STARTED}], "metadata": {"job_id": "job-a1", "background": true, "kind": "video"}});
        let live = ToolState { name: VIDEO_TOOL.into(), status: ToolStatus::Done, output: Some(output), ..ToolState::default() };
        let rows = self::rows(&items, &HashMap::from([("c1".to_owned(), live)]), TurnStatus::Running);
        let [Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((g.lines[0].status, g.lines[0].detach.as_deref()), (Status::Running, None));
        assert_eq!((&shelf.tiles, shelf.job.as_deref()), (&Tiles::Pending { n: 1, ratio: (16, 9) }, Some("job-a1")));
        // In the foreground, seen running: detachable.
        let live = ToolState { name: VIDEO_TOOL.into(), status: ToolStatus::Running, ..ToolState::default() };
        let rows = self::rows(&items, &HashMap::from([("c1".to_owned(), live)]), TurnStatus::Running);
        let [Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((g.lines[0].detach.as_deref(), shelf.job.as_deref()), (Some("c1"), None));
    }

    fn status() -> MediaStatus {
        let model = |id: &str, kind, name: &str, handle: &str| otto_proto::MediaModel {
            id: id.into(),
            kind,
            name: name.into(),
            handle: handle.into(),
            default: false,
        };
        let higgsfield = otto_proto::MediaProviderInfo {
            id: "higgsfield".into(),
            name: "Higgsfield".into(),
            kinds: vec![MediaKind::Image, MediaKind::Video],
            connected: true,
            source: None,
            models: vec![
                model("bytedance/seedance-2.5/text-to-video", MediaKind::Video, "Seedance 2.5", "seedance-2.5"),
                model("higgsfield-ai/soul/v2", MediaKind::Image, "Soul 2", "soul-2"),
            ],
        };
        MediaStatus { providers: vec![higgsfield], defaults: Default::default() }
    }

    fn rows_with(items: &[Item], tools: &HashMap<String, ToolState>, turn: TurnStatus, media: Option<&MediaStatus>) -> Vec<Row> {
        let times = Times::default();
        let input = Input {
            items,
            tools,
            turn: Some(turn),
            error: None,
            retrying: None,
            compactions: &[],
            cwd: None,
            home: None,
            times: &times,
            now: 0,
            older: false,
            turns: &[],
            media,
        };
        build(&input)
    }

    #[test]
    fn generate_media_shows_like_the_others_named_by_its_model_and_mode() {
        let status = status();
        let call = json!({"model": "seedance-2.5", "mode": "reference-to-video", "input": {"prompt": "The fox runs", "aspect_ratio": "9:16"}});
        let items = [Item::user_text("u1", "animate it"), Item::new("a1", Role::Assistant, vec![named(ANY_TOOL, call)])];
        let live = ToolState { name: ANY_TOOL.into(), status: ToolStatus::Running, ..ToolState::default() };
        let rows = rows_with(&items, &HashMap::from([("c1".to_owned(), live)]), TurnStatus::Running, Some(&status));
        let [Row::User { .. }, Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((g.lines[0].kind, g.lines[0].title.as_str()), (Kind::Video, "Generating video · Seedance 2.5 · reference"));
        assert_eq!((shelf.kind, &shelf.tiles, shelf.prompt.as_str()), (MediaKind::Video, &Tiles::Pending { n: 1, ratio: (9, 16) }, "The fox runs"));
        assert_eq!(g.lines[0].detach.as_deref(), Some("c1"), "detachable like the others");
        // Backgrounded, then its report: in the same place.
        let started = STARTED.replace("xAI Grok Imagine Video 1.5", "Higgsfield Seedance 2.5 · reference-to-video");
        let mut items = items.to_vec();
        items.push(Item::new(
            "t1",
            Role::Tool,
            vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text(started)], is_error: false }],
        ));
        let rows = rows_with(&items, &HashMap::new(), TurnStatus::Done, Some(&status));
        let [Row::User { .. }, Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        let job = g.lines[0].job.as_ref().unwrap();
        assert_eq!((job.media, job.command.as_str(), shelf.job.as_deref()), (Some(MediaKind::Video), "Seedance 2.5 · reference", Some("job-a1")));
        let report = format!(
            "Background media job-a1 (video) finished.\nGenerated 1 video with Higgsfield bytedance/seedance-2.5/reference-to-video.\n{VIDEO}"
        );
        items.push(Item::new("s1", Role::System, vec![Part::System { text: report }]));
        let rows = rows_with(&items, &HashMap::new(), TurnStatus::Done, Some(&status));
        let [Row::User { .. }, Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!(g.lines[0].title, "Generated video · Seedance 2.5 · reference");
        let Tiles::Ready(media) = &shelf.tiles else { panic!("{shelf:?}") };
        assert_eq!(media[0].id, "att_v1");
    }

    #[test]
    fn generate_media_without_the_catalog_takes_its_kind_from_the_result() {
        // An image model the app doesn't know: images by the result, the model as called.
        let call = json!({"model": "fal/fal-ai/flux/dev", "input": {"prompt": "Dunes"}});
        let result = format!("Generated 2 images with fal fal-ai/flux/dev.\n{LINE}\n{}", LINE.replace("3f2a9c1b", "77"));
        let items = [
            Item::new("a1", Role::Assistant, vec![named(ANY_TOOL, call)]),
            Item::new("t1", Role::Tool, vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text(result)], is_error: false }]),
        ];
        let rows = rows_with(&items, &HashMap::new(), TurnStatus::Done, None);
        let [Row::Group(g), Row::Media(shelf)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!((g.lines[0].kind, g.lines[0].title.as_str()), (Kind::Media, "Generated images · fal/fal-ai/flux/dev"));
        assert_eq!(shelf.kind, MediaKind::Image);
        // By handle in the catalog, an image model's placeholder is square; a failure says so.
        let status = status();
        let call = json!({"model": "higgsfield/soul-2", "input": {"prompt": "A portrait"}});
        let failed = [
            Item::new("a1", Role::Assistant, vec![named(ANY_TOOL, call)]),
            Item::new(
                "t1",
                Role::Tool,
                vec![Part::ToolResult { call_id: "c1".into(), content: vec![ResultPart::text("Soul 2 input: prompt is required")], is_error: true }],
            ),
        ];
        let rows = rows_with(&failed, &HashMap::new(), TurnStatus::Done, Some(&status));
        let [Row::Group(g)] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!(g.lines[0].title, "Couldn't generate the image: Soul 2 input: prompt is required");
        assert_eq!(call_kind(ANY_TOOL, &json!({"model": "x", "mode": "text-to-speech"}), "", None), Some(MediaKind::Audio));
        assert_eq!(any_subject(&json!({"model": "soul-2", "mode": "image-to-image"}), Some(&status)), "Soul 2 · image");
    }
}
