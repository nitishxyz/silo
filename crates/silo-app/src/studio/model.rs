//! The Studio pane's view models, apart from drawing: tab order and labels, the format chip, the canvas in
//! reading order, the world's switcher and grid, the cards' header bits, the requests sent to the agent.

use silo_proto::studio::{
    Canvas, CanvasInfo, EntityInfo, Format, FrameRole, GenStatus, NodeBody, Project, RefInfo, Section, SectionKind, StudioMedia, Take,
};

/// The aspect used when nothing says one (16:9).
pub const DEFAULT_ASPECT: f32 = 16.0 / 9.0;

/// The open tabs (`open`, canvas names) in tab order: episodes by number, then the others in project order
/// (`Project::canvases` lists them so); names the project doesn't list are left out.
#[must_use]
pub fn tab_order<'a>(project: &'a Project, open: &[String]) -> Vec<&'a CanvasInfo> {
    let mut tabs: Vec<(usize, &CanvasInfo)> = project.canvases.iter().enumerate().filter(|(_, c)| open.contains(&c.name)).collect();
    tabs.sort_by_key(|(at, c)| (c.episode.is_none(), c.episode.unwrap_or(0), *at));
    tabs.into_iter().map(|(_, c)| c).collect()
}

/// A tab's label: `Ep 2 Undertow` for an episode, else the title (the folder's name when it has none).
#[must_use]
pub fn tab_label(info: &CanvasInfo) -> String {
    let title = if info.title.trim().is_empty() { info.name.as_str() } else { info.title.trim() };
    match info.episode {
        Some(n) => format!("Ep {n} {title}"),
        None => title.to_owned(),
    }
}

/// The canvases not open, for `+` → Open canvas, in project order.
#[must_use]
pub fn closed<'a>(project: &'a Project, open: &[String]) -> Vec<&'a CanvasInfo> {
    project.canvases.iter().filter(|c| !open.contains(&c.name)).collect()
}

/// The next free episode number (after the highest).
#[must_use]
pub fn next_episode(project: &Project) -> u32 {
    project.canvases.iter().filter_map(|c| c.episode).max().map_or(1, |n| n + 1)
}

/// The effective format of a canvas (its override when it names a preset, else the project's): its name, it,
/// and whether it is the project default (inherited).
#[must_use]
pub fn canvas_format(project: &Project, canvas: Option<&CanvasInfo>) -> Option<(String, Format, bool)> {
    let own = canvas.and_then(|c| c.format.as_deref()).and_then(|n| project.formats.get(n).map(|f| (n.to_owned(), f.clone(), false)));
    own.or_else(|| project.formats.get(&project.format).map(|f| (project.format.clone(), f.clone(), true)))
}

/// The format chip's text: the effective format's aspect (`9:16`), its preset's name when the aspect doesn't
/// parse; `None` without a project.
#[must_use]
pub fn format_chip(project: &Project, canvas: Option<&CanvasInfo>) -> Option<String> {
    let (name, format, _) = canvas_format(project, canvas)?;
    Some(if format.aspect_ratio().is_some() { format.aspect.trim().to_owned() } else { name })
}

/// A preset's menu label: `portrait · 9:16 · 1080p`.
#[must_use]
pub fn preset_label(name: &str, format: &Format) -> String {
    let mut parts = vec![name.to_owned(), format.aspect.trim().to_owned()];
    parts.extend(format.resolution.clone());
    parts.join(" · ")
}

/// `w / h` of a format, [`DEFAULT_ASPECT`] when it doesn't parse.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn ratio(format: Option<&Format>) -> f32 {
    format.and_then(Format::aspect_ratio).map_or(DEFAULT_ASPECT, |(w, h)| (w / h) as f32)
}

// ---------------------------------------------------------------- reading order

/// A titled group of the read-only flow: a frame (scene, its shots, a plain group) or the unsorted rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// The frame's id; `None`: nodes in no frame.
    pub frame: Option<String>,
    pub title: String,
    /// 0 for a top frame, 1 for a shot in a scene, …
    pub depth: usize,
    /// Its own nodes (not its sub-frames), in reading order.
    pub nodes: Vec<String>,
}

/// The title "Unsorted" nodes go under.
pub const UNSORTED: &str = "Unsorted";

/// A title that numbers itself (`6 · The Dice`, `II. Build`, `3) Hair`): its number and the rest. The number
/// is digits or a Roman numeral set off by punctuation or a separator, so `I am here` is no number.
#[must_use]
pub fn self_numbered(title: &str) -> Option<(&str, &str)> {
    let (word, rest) = title.trim().split_once(char::is_whitespace)?;
    let number = word.trim_end_matches(['.', ')', ':']);
    let numeral = !number.is_empty()
        && number.len() <= 6
        && (number.chars().all(|c| c.is_ascii_digit()) || number.chars().all(|c| matches!(c, 'I' | 'V' | 'X' | 'L' | 'C')));
    let rest = rest.trim_start();
    let after = rest.strip_prefix(['\u{b7}', '-', '\u{2013}', '\u{2014}', ':']).map(str::trim_start);
    let rest = match after {
        Some(after) => after,
        None if number.len() < word.len() => rest,
        None => return None,
    };
    (numeral && !rest.is_empty()).then_some((number, rest))
}

/// A frame's label: `Scene 1 · Harbor at dawn`, `Shot 3`, a group's title. A scene or shot whose title numbers
/// itself keeps that number (`6 · The Dice` → `Shot 6 · The Dice`, not `Shot 1 · 6 · The Dice`).
#[must_use]
pub fn frame_label(title: &str, role: FrameRole, order: Option<u32>) -> String {
    let lead = match role {
        FrameRole::Scene => Some("Scene"),
        FrameRole::Shot => Some("Shot"),
        FrameRole::Group | FrameRole::Unknown => None,
    };
    let title = title.trim();
    if let (Some(lead), Some((number, rest))) = (lead, self_numbered(title)) {
        return format!("{lead} {number} \u{b7} {rest}");
    }
    match (lead, order) {
        (Some(lead), Some(n)) if title.is_empty() => format!("{lead} {n}"),
        (Some(lead), Some(n)) => format!("{lead} {n} \u{b7} {title}"),
        _ if title.is_empty() => lead.unwrap_or("Frame").to_owned(),
        _ => title.to_owned(),
    }
}

/// `canvas` as groups in reading order: frames by `order` (then top to bottom, left to right), each followed by
/// its sub-frames (a scene's shots), nodes inside by position; nodes in no frame (or a missing one) last, as
/// [`UNSORTED`]. Empty groups are kept for frames (a shot not made yet), dropped for the unsorted.
#[must_use]
pub fn reading_order(canvas: &Canvas) -> Vec<Group> {
    let frame_ids: std::collections::HashSet<&str> =
        canvas.nodes.iter().filter(|n| matches!(n.body, NodeBody::Frame { .. })).map(|n| n.id.as_str()).collect();
    fn parent_of<'n>(n: &'n silo_proto::studio::Node, frames: &std::collections::HashSet<&str>) -> Option<&'n str> {
        n.parent.as_deref().filter(|p| frames.contains(p) && *p != n.id)
    }
    let pos = |n: &silo_proto::studio::Node| (n.y, n.x);
    let by_pos = |a: &&silo_proto::studio::Node, b: &&silo_proto::studio::Node| pos(a).partial_cmp(&pos(b)).unwrap_or(std::cmp::Ordering::Equal);
    let by_order = |a: &&silo_proto::studio::Node, b: &&silo_proto::studio::Node| {
        let order = |n: &silo_proto::studio::Node| match &n.body {
            NodeBody::Frame { order, .. } => order.unwrap_or(u32::MAX),
            _ => u32::MAX,
        };
        order(a).cmp(&order(b)).then_with(|| by_pos(a, b))
    };
    let frames_in = |parent: Option<&str>| {
        let mut frames: Vec<&silo_proto::studio::Node> =
            canvas.nodes.iter().filter(|n| matches!(n.body, NodeBody::Frame { .. }) && parent_of(n, &frame_ids) == parent).collect();
        frames.sort_by(by_order);
        frames
    };
    let nodes_in = |frame: Option<&str>| {
        let mut nodes: Vec<&silo_proto::studio::Node> =
            canvas.nodes.iter().filter(|n| !matches!(n.body, NodeBody::Frame { .. }) && parent_of(n, &frame_ids) == frame).collect();
        nodes.sort_by(by_pos);
        nodes.into_iter().map(|n| n.id.clone()).collect::<Vec<_>>()
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Depth-first; `seen` ends a cycle of parents. Frames whose parents form a cycle are under no top frame:
    // each cycle is then read from its first frame (by order, then position), as a top one.
    let mut stack: Vec<(&silo_proto::studio::Node, usize)> = frames_in(None).into_iter().rev().map(|f| (f, 0)).collect();
    loop {
        while let Some((frame, depth)) = stack.pop() {
            if !seen.insert(frame.id.as_str()) {
                continue;
            }
            let NodeBody::Frame { title, role, order, .. } = &frame.body else { continue };
            out.push(Group { frame: Some(frame.id.clone()), title: frame_label(title, *role, *order), depth, nodes: nodes_in(Some(&frame.id)) });
            stack.extend(frames_in(Some(&frame.id)).into_iter().rev().map(|f| (f, depth + 1)));
        }
        let left = canvas.nodes.iter().filter(|n| matches!(n.body, NodeBody::Frame { .. }) && !seen.contains(n.id.as_str()));
        match left.min_by(by_order) {
            Some(first) => stack.push((first, 0)),
            None => break,
        }
    }
    let loose = nodes_in(None);
    if !loose.is_empty() {
        out.push(Group { frame: None, title: UNSORTED.to_owned(), depth: 0, nodes: loose });
    }
    out
}

/// Every node of `canvas` with something to view (not frames), in reading order: what `[`/`]` walk.
#[must_use]
pub fn walk(canvas: &Canvas) -> Vec<String> {
    reading_order(canvas).into_iter().flat_map(|g| g.nodes).collect()
}

/// The node `by` steps from `at` in `order`; `None` past either end (it stays where it is) or when `at` isn't
/// in it.
#[must_use]
pub fn step<'a>(order: &'a [String], at: &str, by: isize) -> Option<&'a String> {
    let i = order.iter().position(|id| id == at)?;
    let to = i.checked_add_signed(by)?;
    order.get(to).filter(|_| to != i)
}

// ---------------------------------------------------------------- cards

/// A card's take stepper (`2/3`): the picked take (`pick`, else the last) of `takes`; `None` with fewer than two.
#[must_use]
pub fn take_stepper(takes: &[Take], pick: Option<u32>) -> Option<String> {
    let n = takes.len();
    (n > 1).then(|| format!("{}/{n}", pick.map_or(n, |p| (p as usize).min(n - 1) + 1)))
}

/// The take index after stepping `by` from the picked one.
#[must_use]
pub fn stepped_pick(takes: usize, pick: Option<u32>, by: isize) -> Option<u32> {
    let last = takes.checked_sub(1)?;
    let now = pick.map_or(last, |p| (p as usize).min(last));
    let to = now.checked_add_signed(by).filter(|t| *t < takes)?;
    u32::try_from(to).ok()
}

/// What a card's header says after its title: **Out of date**, else its model (`Auto` when automatic).
#[must_use]
pub fn card_subtitle(model: Option<&str>, take_model: Option<&str>, out_of_date: bool) -> String {
    if out_of_date {
        return "Out of date".to_owned();
    }
    take_model.or(model).map_or_else(|| "Auto".to_owned(), short_model)
}

/// A model id without its provider (`fal/kling-3.0` → `kling-3.0`).
#[must_use]
pub fn short_model(model: &str) -> String {
    model.rsplit_once('/').map_or(model, |(_, m)| m).to_owned()
}

/// Whether a card's status shows the running face.
#[must_use]
pub fn running(status: GenStatus) -> bool {
    matches!(status, GenStatus::Queued | GenStatus::Running)
}

/// The waiting face's words: the provider's progress, else `Queued` / `Generating`.
#[must_use]
pub fn progress_text(status: GenStatus, progress: Option<&str>) -> String {
    match progress.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => p.to_owned(),
        None if status == GenStatus::Queued => "Queued".to_owned(),
        None => "Generating".to_owned(),
    }
}

/// A duration in seconds as `0:08`.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn seconds(secs: f64) -> String {
    crate::agent::media::clock((secs.max(0.0) * 1000.0) as u64)
}

// ---------------------------------------------------------------- world

/// A switcher entry: Overview, Style, then entity sections with their counts (`Characters·4`).
#[must_use]
pub fn section_label(section: &Section) -> String {
    match section.kind {
        SectionKind::Entities => format!("{}\u{b7}{}", section.title, section.entities.len()),
        _ => section.title.clone(),
    }
}

/// The switcher's sections in order: overview and style first (as the server lists them), then the rest.
#[must_use]
pub fn switcher(sections: &[Section]) -> Vec<&Section> {
    let rank = |s: &Section| match s.kind {
        SectionKind::Overview => 0,
        SectionKind::Style => 1,
        _ => 2,
    };
    let mut out: Vec<(usize, &Section)> = sections.iter().enumerate().collect();
    out.sort_by_key(|(at, s)| (rank(s), *at));
    out.into_iter().map(|(_, s)| s).collect()
}

/// The section shown: `wanted` when it exists, else the first.
#[must_use]
pub fn shown_section<'a>(sections: &'a [Section], wanted: Option<&str>) -> Option<&'a Section> {
    wanted.and_then(|w| sections.iter().find(|s| s.id == w)).or_else(|| switcher(sections).first().copied())
}

/// One of a section's entities, by its title: `character` for Characters, the title lowercased otherwise.
#[must_use]
pub fn singular(title: &str) -> String {
    let lower = title.trim().to_lowercase();
    if let Some(stem) = lower.strip_suffix("ies") {
        return format!("{stem}y");
    }
    lower.strip_suffix('s').filter(|s| !s.is_empty() && !s.ends_with('s')).map_or(lower.clone(), str::to_owned)
}

/// Columns of the entity grid in `width` points (cards at least `min` wide, `gap` apart); at least one.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn grid_columns(width: f32, min: f32, gap: f32) -> usize {
    (((width + gap) / (min + gap)).floor() as usize).max(1)
}

/// What a frontmatter value shows as: strings bare, lists joined, the rest as JSON.
#[must_use]
pub fn field_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items.iter().map(field_text).collect::<Vec<_>>().join(", "),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Frontmatter keys the detail shows elsewhere (not as fields).
pub const HIDDEN_FIELDS: &[&str] = &["name", "summary", "sheet", "refs", "links", "tags", "approved"];

/// What an entity is, by its section (the built-in `characters`, `places`, `props`; any other is custom): what
/// its sheet holds and which actions its page offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityKind {
    Character,
    Place,
    Prop,
    Other,
}

impl EntityKind {
    /// The kind of section `section` (its id, `characters`).
    #[must_use]
    pub fn of_section(section: &str) -> Self {
        match section {
            "characters" => Self::Character,
            "places" => Self::Place,
            "props" => Self::Prop,
            _ => Self::Other,
        }
    }

    /// The kind of entity `id` (`characters/mara`).
    #[must_use]
    pub fn of_entity(id: &str) -> Self {
        Self::of_section(id.split_once('/').map_or(id, |(s, _)| s))
    }

    /// The page's actions, in order.
    #[must_use]
    pub fn asks(self) -> &'static [EntityAsk] {
        match self {
            Self::Character => &[EntityAsk::Sheet, EntityAsk::VideoSheet, EntityAsk::Voice],
            Self::Prop => &[EntityAsk::Sheet, EntityAsk::VideoSheet],
            Self::Place => &[EntityAsk::Sheet, EntityAsk::Angles],
            Self::Other => &[EntityAsk::Sheet],
        }
    }
}

/// An entity action's request to the agent (it does the generating; its skills hold the method).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityAsk {
    /// The sheet: several views of the design in one image (a place's: its establishing image).
    Sheet,
    /// A short video orbiting it once through 360°, added to its refs.
    VideoSheet,
    /// A character's voice.
    Voice,
    /// A place's reverse angle and the variants its scenes need.
    Angles,
    /// One more named reference (a look, an angle, a detail), the user saying which: the Add reference tile's.
    Look,
}

impl EntityAsk {
    /// Its button's label; `has_sheet`: the entity has one already.
    #[must_use]
    pub fn label(self, has_sheet: bool) -> &'static str {
        match self {
            Self::Sheet if has_sheet => "Regenerate sheet",
            Self::Sheet => "Generate sheet",
            Self::VideoSheet => "Generate video sheet",
            Self::Voice => "Cast voice",
            Self::Angles => "Generate angles",
            Self::Look => "Ask the agent\u{2026}",
        }
    }

    /// A stable id for its button.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Sheet => "sheet",
            Self::VideoSheet => "video-sheet",
            Self::Voice => "voice",
            Self::Angles => "angles",
            Self::Look => "look",
        }
    }

    /// The project's model pick this job uses (`studio.toml` `[models]`): a sheet or angles the Image pick, a
    /// video sheet the Video pick; a voice none (cast from the voices offered).
    #[must_use]
    pub fn model_kind(self) -> Option<&'static str> {
        match self {
            Self::Sheet | Self::Angles | Self::Look => Some("image"),
            Self::VideoSheet => Some("video"),
            Self::Voice => None,
        }
    }

    /// The prepared message naming the entity (`@world:characters/mara` is its mention) of `kind`; `one` names a
    /// custom section's entity (`faction`, [`singular`] of its title); `model`: the project's pick for this job
    /// (named at the end, `… Use soul-2.`; `None`: Automatic, the agent chooses).
    #[must_use]
    pub fn message(self, kind: EntityKind, one: &str, name: &str, id: &str, model: Option<&str>) -> String {
        let text = self.request(kind, one, name, id);
        match model.map(str::trim).filter(|m| !m.is_empty()).filter(|_| self.model_kind().is_some()) {
            Some(m) => format!("{text} Use {m}."),
            None => text,
        }
    }

    fn request(self, kind: EntityKind, one: &str, name: &str, id: &str) -> String {
        let it = format!("{name} (world entity {id})");
        match (self, kind) {
            (Self::Sheet, EntityKind::Character) => {
                format!("Generate a character sheet for {it}: three views in one image, and set it as its sheet.")
            }
            (Self::Sheet, EntityKind::Prop) => format!("Generate a prop sheet for {it}: two views in one image, and set it as its sheet."),
            (Self::Sheet, EntityKind::Place) => format!("Generate the establishing image for {it} and set it as its sheet."),
            (Self::Sheet, EntityKind::Other) => {
                let one = one.trim();
                let one = if one.is_empty() { "reference" } else { one };
                format!("Generate a {one} sheet for {it} and set it as its sheet.")
            }
            (Self::VideoSheet, EntityKind::Character) => {
                format!(
                    "Generate a 360° video sheet for {it} from its sheet, in which it says one line (its voice reference), and add it to its refs."
                )
            }
            (Self::VideoSheet, _) => format!("Generate a 360° video sheet for {it} from its sheet and add it to its refs."),
            (Self::Voice, _) => format!("Cast a voice for {it}: offer two or three with a sample line, then set the one I pick."),
            (Self::Angles, _) => format!(
                "Generate angles of {it}: the reverse angle and the variants its scenes need, consistent with its sheet, and add them to its refs."
            ),
            (Self::Look, _) => format!(
                "Add a look for {it}: ask me which look I want (an outfit, an angle, a detail alone), then generate it consistent with its \
                 sheet and add it to its refs, named, with what it defines."
            ),
        }
    }
}

/// What a ref's file is, by its extension: drawn as an image, a video's poster or an audio player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefMedia {
    Image,
    Video,
    Audio,
}

impl RefMedia {
    #[must_use]
    pub fn of(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path);
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "mp4" | "mov" | "m4v" | "webm" => Self::Video,
            "mp3" | "wav" | "m4a" | "aac" | "aiff" => Self::Audio,
            _ => Self::Image,
        }
    }

    /// A named reference's: the server's kind (`RefInfo::media`), by its file when that says nothing.
    #[must_use]
    pub fn of_look(look: &RefInfo) -> Self {
        match look.media {
            StudioMedia::Video => Self::Video,
            StudioMedia::Audio => Self::Audio,
            StudioMedia::Image => Self::Image,
            StudioMedia::Text | StudioMedia::Unknown => Self::of(&look.path),
        }
    }

    /// Its icon (a chip's, a popup row's).
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio-lines",
        }
    }

    /// What a popup row calls a reference of this kind (`Look · Mara Venn`, as otto's `studio.mentions` does).
    #[must_use]
    pub fn kind_label(self) -> &'static str {
        match self {
            Self::Image => "Look",
            Self::Video => "Video",
            Self::Audio => "Audio",
        }
    }
}

/// Entity `info`'s named references in file order: its `looks`; from an older server listing only `refs`, those
/// paths named by their files (ids as the server makes them, `silo_proto::studio::ref_ids`).
#[must_use]
pub fn entity_looks(info: &EntityInfo) -> Vec<RefInfo> {
    if !info.looks.is_empty() || info.refs.is_empty() {
        return info.looks.clone();
    }
    let pairs: Vec<(Option<&str>, &str)> = info.refs.iter().map(|p| (None, p.as_str())).collect();
    let ids = silo_proto::studio::ref_ids(&pairs);
    info.refs
        .iter()
        .zip(ids)
        .map(|(path, id)| RefInfo {
            id,
            path: path.clone(),
            name: silo_proto::studio::file_stem(path).to_owned(),
            defines: None,
            media: StudioMedia::of_path(path),
        })
        .collect()
}

/// What a reference tile says under its face: its name, then (when set, a second muted line) what it defines.
#[must_use]
pub fn ref_caption(look: &RefInfo) -> (String, Option<String>) {
    let name = if look.name.trim().is_empty() { silo_proto::studio::file_stem(&look.path).to_owned() } else { look.name.trim().to_owned() };
    (name, look.defines.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_owned))
}

/// A reference tile's tooltip: what it is for (`Takes only: …`), then what a click does (audio plays on its tile).
#[must_use]
pub fn ref_tip(look: &RefInfo) -> String {
    let hint = if RefMedia::of_look(look) == RefMedia::Audio { "Right-click for options" } else { "Click to view, right-click for options" };
    match ref_caption(look).1 {
        Some(d) => format!("Takes only: {d}\n{hint}"),
        None => format!("Not said what it defines: right-click \u{2192} What it defines\u{2026}\n{hint}"),
    }
}

/// The sheet's label under it on the entity page: it is the default look (what `@<entity>` wires).
pub const SHEET_LABEL: &str = "Sheet \u{b7} default look";

/// A row of the sheet's or a ref's right-click menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAction {
    /// A reference's name, and what it defines (a one-line ask each).
    Rename,
    Describe,
    SetSheet,
    /// One place earlier or later in the list.
    MoveEarlier,
    MoveLater,
    /// This Mac's host only, as the next.
    RevealInFinder,
    OpenInApp,
    /// A remote host's: the file in a Files block.
    OpenInFiles,
    /// Its `@entity#ref` token, for a card prompt or the agent.
    CopyMention,
    Remove,
}

impl FileAction {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Rename => "Rename\u{2026}",
            Self::Describe => "What it defines\u{2026}",
            Self::SetSheet => "Set as sheet",
            Self::MoveEarlier => "Move earlier",
            Self::MoveLater => "Move later",
            Self::RevealInFinder => "Reveal in Finder",
            Self::OpenInApp => "Open in default app",
            Self::OpenInFiles => "Open in Files",
            Self::CopyMention => "Copy mention",
            Self::Remove => "Remove",
        }
    }
}

/// Where the file can be fixed by hand: on this Mac in Finder and its default app, else in a Files block.
fn by_hand(local: bool) -> &'static [FileAction] {
    if local { &[FileAction::RevealInFinder, FileAction::OpenInApp] } else { &[FileAction::OpenInFiles] }
}

/// The sheet's right-click menu; `local`: the block's host is this Mac.
#[must_use]
pub fn sheet_menu(local: bool) -> Vec<FileAction> {
    by_hand(local).to_vec()
}

/// A reference's right-click menu, `at` its place among `count`: Rename…, What it defines…, Set as sheet (an
/// image only: a video or audio can't be the sheet), Move earlier / later (where it can go), the file by hand,
/// Copy mention, Remove (the file stays).
#[must_use]
pub fn ref_menu(media: RefMedia, at: usize, count: usize, local: bool) -> Vec<FileAction> {
    let mut out = vec![FileAction::Rename, FileAction::Describe];
    if media == RefMedia::Image {
        out.push(FileAction::SetSheet);
    }
    if at > 0 {
        out.push(FileAction::MoveEarlier);
    }
    if at + 1 < count {
        out.push(FileAction::MoveLater);
    }
    out.extend_from_slice(by_hand(local));
    out.push(FileAction::CopyMention);
    out.push(FileAction::Remove);
    out
}

/// Where a reference at `at` goes for `action` (Move earlier / later): `None` past either end or for another row.
#[must_use]
pub fn moved_to(action: FileAction, at: usize, count: usize) -> Option<usize> {
    match action {
        FileAction::MoveEarlier => at.checked_sub(1),
        FileAction::MoveLater => Some(at + 1).filter(|to| *to < count),
        _ => None,
    }
}

// ---------------------------------------------------------------- links

/// Entity `id` of `sections`.
#[must_use]
pub fn find_entity<'a>(sections: &'a [Section], id: &str) -> Option<&'a EntityInfo> {
    sections.iter().flat_map(|s| &s.entities).find(|e| e.id == id)
}

/// An entity's name as shown: its name, its id's last part when it has none.
#[must_use]
pub fn entity_name(e: &EntityInfo) -> String {
    if e.name.trim().is_empty() { e.id.rsplit('/').next().unwrap_or(&e.id).to_owned() } else { e.name.trim().to_owned() }
}

/// The "Goes with" add menu of entity `id` (linked to `linked`): the world's other entities by section, each
/// section `(id, title, entities)`. A character's page lists props first, a prop's characters first, the rest in the
/// world's order; itself and those it goes with already are left out, and so are sections left empty.
#[must_use]
pub fn link_menu<'a>(sections: &'a [Section], id: &str, linked: &[String]) -> Vec<(&'a str, &'a str, Vec<&'a EntityInfo>)> {
    let first = match EntityKind::of_entity(id) {
        EntityKind::Character => Some("props"),
        EntityKind::Prop => Some("characters"),
        EntityKind::Place | EntityKind::Other => None,
    };
    let mut order: Vec<(usize, &Section)> = sections.iter().filter(|s| s.kind == SectionKind::Entities).enumerate().collect();
    order.sort_by_key(|(at, s)| (Some(s.id.as_str()) != first, *at));
    order
        .into_iter()
        .map(|(_, s)| (s.id.as_str(), s.title.as_str(), s.entities.iter().filter(|e| e.id != id && !linked.contains(&e.id)).collect::<Vec<_>>()))
        .filter(|(.., es)| !es.is_empty())
        .collect()
}

/// Whose prop `prop` is: the one character it goes with, when it goes with exactly one (`None` for any other
/// entity, or a prop shared by several characters or by none).
#[must_use]
pub fn owner<'a>(sections: &'a [Section], prop: &EntityInfo) -> Option<&'a EntityInfo> {
    if EntityKind::of_entity(&prop.id) != EntityKind::Prop {
        return None;
    }
    let mut characters = prop.links.iter().filter(|l| EntityKind::of_entity(l) == EntityKind::Character);
    let (one, none) = (characters.next()?, characters.next());
    none.is_none().then(|| find_entity(sections, one)).flatten()
}

/// A possessive (`Bhima's`), what a prop's card says of its owner.
#[must_use]
pub fn possessive(name: &str) -> String {
    format!("{}\u{2019}s", name.trim())
}

/// The props that sit under a character's row in the Production view: by character id, those of [`owner`] it is
/// (in the world's order).
#[must_use]
pub fn props_by_owner(sections: &[Section]) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for e in sections.iter().flat_map(|s| &s.entities) {
        if let Some(o) = owner(sections, e) {
            out.entry(o.id.clone()).or_default().push(e.id.clone());
        }
    }
    out
}

/// How many references an entity has beyond its sheet, as its Production row says it: `3 looks`; `None` for none.
#[must_use]
pub fn looks_count(info: &EntityInfo) -> Option<String> {
    let n = entity_looks(info).iter().filter(|l| info.sheet.as_deref() != Some(l.path.as_str())).count();
    match n {
        0 => None,
        1 => Some("1 look".to_owned()),
        n => Some(format!("{n} looks")),
    }
}

/// Whether `canvas` shows world entities: its cards' Out of date depends on their sheets, so a change under
/// `world/` reads it again.
#[must_use]
pub fn shows_entities(canvas: &Canvas) -> bool {
    canvas.nodes.iter().any(|n| matches!(n.body, NodeBody::Entity { .. }))
}

/// "Create with agent"'s message for a section titled `section`.
#[must_use]
pub fn create_with_agent(section: &str, what: &str) -> String {
    format!("Create a new {} for this world: {}", singular(section), what.trim())
}

/// The first message of Start with the agent.
pub const ONBOARDING: &str = "Let's set up this studio.";

/// A node's mention target (`ep01-pilot#n_3f2a9c`).
#[must_use]
pub fn node_target(canvas: &str, node: &str) -> String {
    format!("{canvas}#{node}")
}

/// The presets `studio.init` writes (docs/STUDIO.md §2), for Setup: name, aspect, what it is for.
pub const INIT_PRESETS: &[(&str, &str, &str)] = &[
    ("landscape", "16:9", "Films, YouTube"),
    ("vertical", "9:16", "Reels, shorts, stories"),
    ("square", "1:1", "Posts"),
    ("portrait", "4:5", "Feed posts"),
];

/// `W:H` as `w / h`, [`DEFAULT_ASPECT`] when it doesn't parse.
#[must_use]
pub fn aspect_of(aspect: &str) -> f32 {
    ratio(Some(&Format { aspect: aspect.to_owned(), resolution: None, fps: None }))
}

/// A box `w`×`h` at `ratio` fitting inside `max` × `max` (an aspect tile's face).
#[must_use]
pub fn fit(ratio: f32, max: f32) -> (f32, f32) {
    if ratio >= 1.0 { (max, max / ratio.max(0.01)) } else { (max * ratio, max) }
}
