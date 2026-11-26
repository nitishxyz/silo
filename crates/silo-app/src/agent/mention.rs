//! `@` mentions (`docs/AGENT_BLOCK.md` §4g2, `docs/MEDIA.md` §3.0.1, `docs/STUDIO.md` §6.3): the composer
//! popup's query and choices (agents, a studio's entities, canvases and a canvas's nodes from `studio.mentions`,
//! media models, then project paths from `files.complete`), chips sent as `InputPart::Mention`s (skills' `/name`
//! chips too, `agent/skills.rs`), and chips drawn inline over text (composer, bubbles).

use std::{ops::Range, time::Duration};

use gpui::{
    AnyElement, App, Bounds, HighlightStyle, Hsla, Pixels, Point, ScrollHandle, StyledText, TransformationMatrix, Window, canvas, div, fill, point,
    prelude::*, px, rgb,
};
use otto_proto::{FileMatch, InputPart, Item, MediaKind, MediaStatus, MentionKind, StudioMention};

use crate::theme;

/// Agents the popup lists at most (files: [`FILES`]).
pub const AGENTS: usize = 8;
pub const MODELS: usize = 6;
/// Entities and canvases the popup lists at most for a typed query, of the [`STUDIO_ASK`] otto sends.
pub const STUDIO: usize = 6;
/// A bare `@` in a studio: the first entities and canvases it lists (before agents).
pub const BARE_ENTITIES: usize = 4;
pub const BARE_CANVASES: usize = 4;
/// A canvas's nodes (`@ep01-pilot#…`) the popup lists at most (it scrolls).
pub const NODES: usize = 40;
pub const STUDIO_ASK: u32 = 50;
pub const FILES: u32 = 50;
/// Typing pauses this long before the popup asks otto for paths.
pub const DEBOUNCE: Duration = Duration::from_millis(60);

/// `@target` at `range` of a text (`/name` for a skill).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    pub range: Range<usize>,
    pub kind: MentionKind,
    /// An agent name, a project path (folders end in `/`), a media model handle, an entity id
    /// (`characters/mara`), a canvas (`ep01-pilot`, `ep01-pilot#n_3f2a9c`), or a skill name.
    pub target: String,
    /// otto found no such agent or path (drawn struck through).
    pub missing: bool,
    /// What it reads and shows when its host knows better than [`label`] and [`icon`] (the Studio's card prompts:
    /// from the canvas and the world).
    pub face: Option<Face>,
}

/// A chip's badge as its host gives it ([`Chip::face`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Face {
    pub label: String,
    pub icon: &'static str,
    /// Drawn quieter, its pill dashed (a mention with no wire into its card).
    pub quiet: bool,
}

impl Chip {
    pub fn new(at: usize, kind: MentionKind, target: String) -> Self {
        Self { range: at..at + 1 + target.len(), kind, target, missing: false, face: None }
    }

    /// Its icon; a model's is its provider's logo, by the media status last loaded ([`ModelProviders`]).
    pub fn icon(&self, cx: &App) -> &'static str {
        if let Some(face) = &self.face {
            return face.icon;
        }
        match self.kind {
            MentionKind::Model => {
                super::media_setup::logo(cx.try_global::<ModelProviders>().and_then(|p| p.0.get(&self.target)).map_or("", String::as_str))
            }
            MentionKind::Entity if self.target.contains('#') => {
                ref_icon(cx.try_global::<StudioNames>().and_then(|n| n.details.get(&self.target)).map_or("", String::as_str))
            }
            kind => icon(kind, &self.target),
        }
    }

    /// A folder: typing right after it opens it again as a query (drilling in).
    pub fn folder(&self) -> bool {
        self.kind == MentionKind::File && self.target.ends_with('/')
    }

    /// A whole canvas: `#` typed right after it opens it again as a query for its nodes (drilling in).
    pub fn canvas(&self) -> bool {
        self.kind == MentionKind::Canvas && !self.target.contains('#')
    }

    /// A whole entity (not one of its references): `#` typed right after it opens it again as a query for its
    /// named references (drilling in, as a canvas's nodes).
    pub fn entity(&self) -> bool {
        self.kind == MentionKind::Entity && !self.target.contains('#')
    }

    /// Drilled into by typing right after it (gets no trailing space when picked): a folder, a whole canvas or a
    /// whole entity.
    pub fn drills(&self) -> bool {
        self.folder() || self.canvas() || self.entity()
    }

    /// `typed` right after it makes it a query again: a folder takes anything but whitespace, a canvas or an
    /// entity `#`.
    pub fn opens_on(&self, typed: &str) -> bool {
        (self.folder() && typed.starts_with(|c: char| !c.is_whitespace())) || ((self.canvas() || self.entity()) && typed.starts_with('#'))
    }

    /// Its text's first character: `/` for a skill, else `@`.
    pub fn sigil(&self) -> char {
        sigil(self.kind)
    }

    /// What its badge reads: [`label`].
    pub fn label(&self, cx: &App) -> String {
        match &self.face {
            Some(face) => face.label.clone(),
            None => label(self.kind, &self.target, cx.try_global::<StudioNames>()),
        }
    }

    fn quiet(&self) -> bool {
        self.face.as_ref().is_some_and(|f| f.quiet)
    }

    /// Its text is `@target` (`/name`) in `text`.
    fn fits(&self, text: &str) -> bool {
        text.get(self.range.clone()).is_some_and(|t| t.strip_prefix(self.sigil()) == Some(self.target.as_str()))
    }
}

/// A mention's first character in the text: `/` for a skill, else `@`.
pub fn sigil(kind: MentionKind) -> char {
    if kind == MentionKind::Skill { '/' } else { '@' }
}

pub fn icon(kind: MentionKind, target: &str) -> &'static str {
    match kind {
        MentionKind::Agent => "bot",
        MentionKind::File if target.ends_with('/') => "folder",
        MentionKind::File => "file-text",
        MentionKind::Model => "image",
        MentionKind::Entity => "earth",
        MentionKind::Canvas => crate::studio_block::ICON,
        MentionKind::Skill => "sparkles",
    }
}

/// A named reference's icon by its popup row's detail (`Look · Mara Venn`, `Video · …`, `Audio · …`): an
/// image's unless it says otherwise.
pub fn ref_icon(detail: &str) -> &'static str {
    match detail.split(" · ").next().unwrap_or_default() {
        "Video" => "video",
        "Audio" => "audio-lines",
        _ => "image",
    }
}

/// What a reference's chip reads: its entity's name, then its own (`Dhritarashtra · Crown off`).
pub fn ref_label(entity: &str, name: &str) -> String {
    format!("{} · {}", entity.trim(), name.trim())
}

/// A popup row's icon: a node's by its kind (its detail's first word: `Video card · S2.3`), a named reference's
/// by its media ([`ref_icon`]), else [`icon`].
pub fn row_icon(kind: MentionKind, target: &str, detail: &str) -> &'static str {
    if kind == MentionKind::Entity && target.contains('#') {
        return ref_icon(detail);
    }
    if kind != MentionKind::Canvas || !target.contains('#') {
        return icon(kind, target);
    }
    let card = detail.split(" · ").next().unwrap_or_default();
    match card.split(' ').collect::<Vec<_>>().as_slice() {
        ["Scene" | "Shot" | "Frame"] => "frame",
        ["Note"] => "sticky-note",
        ["Document" | "File"] => "file-text",
        ["Video", ..] => "video",
        ["Audio" | "Speech" | "Music" | "Sound", ..] => "audio-lines",
        ["Image", ..] => "image",
        _ => icon(kind, target),
    }
}

/// A chip's badge text: an entity's name or a canvas's or node's title when known (a node's otherwise: its
/// canvas's), else the target's last `/` or `#` segment; a named reference its entity's name and its own
/// ([`ref_label`]; each, unknown, its id's last part); a skill `/name`; other kinds their target.
pub fn label(kind: MentionKind, target: &str, names: Option<&StudioNames>) -> String {
    if let (MentionKind::Entity, Some((entity, id))) = (kind, target.split_once('#')) {
        let known = |t: &str| names.and_then(|n| n.entities.get(t)).map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_owned);
        let entity_name = known(entity).unwrap_or_else(|| entity.trim_end_matches('/').rsplit('/').next().unwrap_or(entity).to_owned());
        return ref_label(&entity_name, &known(target).unwrap_or_else(|| id.to_owned()));
    }
    let map = match kind {
        MentionKind::Entity => names.map(|n| &n.entities),
        MentionKind::Canvas => names.map(|n| &n.canvases),
        MentionKind::Skill => return format!("/{target}"),
        _ => return target.to_owned(),
    };
    let canvas = || (kind == MentionKind::Canvas).then(|| target.split_once('#').map(|(c, _)| c)).flatten();
    let known = map.and_then(|m| m.get(target).or_else(|| m.get(canvas()?)));
    known.filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| {
        let last = target.trim_end_matches('/').rsplit(['/', '#']).next().unwrap_or(target);
        if last.is_empty() { target.to_owned() } else { last.to_owned() }
    })
}

/// The `studio.mentions` query that finds `target`'s name: an entity by its id's last part, a canvas by its name,
/// a node among its canvas's nodes (`canvas#`), a named reference among its entity's (`characters/mara#`, whose
/// rows name the entity too).
#[must_use]
pub fn name_query(kind: MentionKind, target: &str) -> String {
    match (kind, target.split_once('#')) {
        (MentionKind::Canvas | MentionKind::Entity, Some((head, _))) => format!("{head}#"),
        _ => target.trim_end_matches('/').rsplit('/').next().unwrap_or(target).to_owned(),
    }
}

/// Names of the entities (by id), their named references (by `entity#ref_id`), canvases (by name) and canvas
/// nodes (by `canvas#node_id`) `studio.mentions` has returned, for chips' labels.
#[derive(Default)]
pub struct StudioNames {
    pub entities: std::collections::HashMap<String, String>,
    /// A named reference's row detail (`Look · Mara Venn`), by target: its chip's icon ([`ref_icon`]).
    pub details: std::collections::HashMap<String, String>,
    /// Canvases' titles and nodes' names.
    pub canvases: std::collections::HashMap<String, String>,
    /// `<cwd>\n<canvas>` whose title a placed result's link has asked for.
    pub asked: std::collections::HashSet<String>,
}
impl gpui::Global for StudioNames {}

impl StudioNames {
    pub fn add(&mut self, found: &[StudioMention]) {
        for m in found {
            let map = match m.kind {
                MentionKind::Entity => &mut self.entities,
                MentionKind::Canvas => &mut self.canvases,
                _ => continue,
            };
            map.insert(m.target.clone(), m.name.clone());
            // A reference's row names its entity too (`Look · Mara Venn`): the entity's chip may need it.
            if let (MentionKind::Entity, Some((entity, _))) = (m.kind, m.target.split_once('#')) {
                self.details.insert(m.target.clone(), m.detail.clone());
                if let Some((_, name)) = m.detail.split_once(" · ").filter(|(_, n)| !n.trim().is_empty()) {
                    self.entities.entry(entity.to_owned()).or_insert_with(|| name.trim().to_owned());
                }
            }
        }
    }
}

/// Each media model handle's provider id from the last `media.status` (chips draw its logo).
#[derive(Default)]
pub struct ModelProviders(pub std::collections::HashMap<String, String>);
impl gpui::Global for ModelProviders {}

impl ModelProviders {
    pub fn of(status: &MediaStatus) -> Self {
        Self(status.providers.iter().flat_map(|p| p.models.iter().map(|m| (m.handle.clone(), p.id.clone()))).collect())
    }
}

/// The popup's state: the `@query` it shows and the paths otto found (`None` until its first reply).
#[derive(Default)]
pub struct Popup {
    /// The `@query`'s range in the composer (to the word's end) and the query before the caret.
    pub at: Option<(Range<usize>, String)>,
    /// Esc closed it at this `@`; it stays closed until that `@` goes.
    pub dismissed: Option<usize>,
    pub files: Option<Vec<FileMatch>>,
    /// The studio's entities and canvases (a canvas's nodes for `canvas#…`) for the last query otto answered
    /// (none outside a studio).
    pub studio: Vec<StudioMention>,
    /// The latest `files.complete` (and `studio.mentions`) asked: older replies are dropped.
    pub asked: u64,
    pub scroll: ScrollHandle,
}

/// The mention the caret is typing: an `@` at a word start with no space up to the caret, outside
/// chips. Its range runs to the word's end; the query is what is typed before the caret.
pub fn query<'a>(text: &'a str, caret: usize, chips: &[Chip]) -> Option<(Range<usize>, &'a str)> {
    let before = text.get(..caret)?;
    let at = before.rfind(|c: char| c == '@' || c.is_whitespace())?;
    if !before[at..].starts_with('@') || chips.iter().any(|c| c.range.start <= at && at < c.range.end) {
        return None;
    }
    if before[..at].chars().next_back().is_some_and(|c| !c.is_whitespace() && !"([{\"'`".contains(c)) {
        return None;
    }
    let rest = &text[caret..];
    let mut end = caret + rest.find(char::is_whitespace).unwrap_or(rest.len());
    if let Some(next) = chips.iter().map(|c| c.range.start).filter(|s| *s >= caret).min() {
        end = end.min(next);
    }
    Some((at..end, &before[at + 1..]))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Agent {
        name: String,
        about: String,
    },
    Model {
        handle: String,
        name: String,
        /// Its provider's id (`higgsfield`, its logo) and name (`Higgsfield`).
        provider_id: String,
        provider: String,
        kind: MediaKind,
        /// The provider has a key (an unconnected one is still pickable).
        connected: bool,
    },
    /// A world entity, a canvas or a canvas's node (`studio.mentions`).
    Studio {
        kind: MentionKind,
        target: String,
        name: String,
        /// `Character`, `Canvas · Ep 1`, `Video card · Pass 3A`.
        detail: String,
    },
    File {
        path: String,
        dir: bool,
    },
    /// otto hasn't answered with paths yet: a quiet row after the agents, never picked.
    Loading,
}

impl Choice {
    pub fn mention(&self) -> Option<(MentionKind, String)> {
        match self {
            Self::Agent { name, .. } => Some((MentionKind::Agent, name.clone())),
            Self::Model { handle, .. } => Some((MentionKind::Model, handle.clone())),
            Self::Studio { kind, target, .. } => Some((*kind, target.clone())),
            Self::File { path, .. } => Some((MentionKind::File, path.clone())),
            Self::Loading => None,
        }
    }
}

/// Agents matching `query` (fuzzy, best first; all in order when empty), at most [`AGENTS`].
pub fn agent_matches<'a>(agents: &'a [(String, String)], query: &str) -> Vec<&'a (String, String)> {
    if query.contains('/') {
        return Vec::new();
    }
    let mut hits: Vec<(i32, usize, &(String, String))> =
        agents.iter().enumerate().filter_map(|(i, a)| crate::commands::fuzzy(query, &a.0).map(|(score, _)| (score, i, a))).collect();
    hits.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
    hits.into_iter().take(AGENTS).map(|(_, _, a)| a).collect()
}

/// Media models matching `query` (fuzzy on handle, name and provider): connected providers' first, then best
/// first; at most [`MODELS`]; none for an empty query or a path.
pub fn model_matches(media: Option<&MediaStatus>, query: &str) -> Vec<Choice> {
    if query.is_empty() || query.contains('/') {
        return Vec::new();
    }
    let models = media.into_iter().flat_map(|s| &s.providers).flat_map(|p| p.models.iter().filter(|m| !m.handle.is_empty()).map(move |m| (p, m)));
    let mut hits: Vec<(i32, usize, Choice)> = models
        .enumerate()
        .filter_map(|(i, (p, m))| {
            let score = [&m.handle, &m.name, &p.name].into_iter().filter_map(|t| crate::commands::fuzzy(query, t)).map(|(s, _)| s).max()?;
            let (handle, name, provider_id, provider) = (m.handle.clone(), m.name.clone(), p.id.clone(), p.name.clone());
            Some((score, i, Choice::Model { handle, name, provider_id, provider, kind: m.kind, connected: p.connected }))
        })
        .collect();
    let connected = |c: &Choice| matches!(c, Choice::Model { connected: true, .. });
    hits.sort_by_key(|(score, i, c)| (!connected(c), std::cmp::Reverse(*score), *i));
    hits.into_iter().take(MODELS).map(|(.., c)| c).collect()
}

/// A `canvas#text` query (or `entity#text`, `characters/mara#crown`: an entity's named references): the canvas or
/// entity and the text after `#`.
pub fn node_query(query: &str) -> Option<(&str, &str)> {
    query.split_once('#').filter(|(canvas, _)| !canvas.is_empty())
}

/// A drill-in query names an entity (`characters/mara#`), not a canvas: its head has a `/`.
pub fn looks_query(query: &str) -> bool {
    node_query(query).is_some_and(|(head, _)| head.contains('/'))
}

/// `rows` with each named reference moved right under its entity's row, when that is listed (else it stays).
fn under_entities(rows: Vec<Choice>) -> Vec<Choice> {
    let entity_of = |c: &Choice| match c {
        Choice::Studio { kind: MentionKind::Entity, target, .. } => target.split_once('#').map(|(e, _)| e.to_owned()),
        _ => None,
    };
    let target_of = |c: &Choice| match c {
        Choice::Studio { target, .. } => Some(target.clone()),
        _ => None,
    };
    let (refs, mut out): (Vec<Choice>, Vec<Choice>) = rows.into_iter().partition(|c| entity_of(c).is_some());
    for r in refs {
        let owner = entity_of(&r);
        match out.iter().rposition(|c| target_of(c) == owner || (entity_of(c).is_some() && entity_of(c) == owner)) {
            Some(at) => out.insert(at + 1, r),
            None => out.push(r),
        }
    }
    out
}

fn studio_row(m: &StudioMention) -> Choice {
    let name = if m.name.trim().is_empty() { label(m.kind, &m.target, None) } else { m.name.clone() };
    Choice::Studio { kind: m.kind, target: m.target.clone(), name, detail: m.detail.clone() }
}

/// A studio's entities and canvases matching `query` (fuzzy on name and target, best first), at most [`STUDIO`].
/// An empty query lists the first [`BARE_ENTITIES`] entities, then the first [`BARE_CANVASES`] canvases, in otto's
/// order; a `canvas#text` query that canvas's nodes matching `text` (fuzzy on their name, all in otto's order when
/// empty), at most [`NODES`]. `found` may answer an earlier query (it is filtered again until otto replies).
pub fn studio_matches(found: &[StudioMention], query: &str) -> Vec<Choice> {
    let node = |m: &&StudioMention| m.kind == MentionKind::Canvas && m.target.contains('#');
    let look = |m: &&StudioMention| m.kind == MentionKind::Entity && m.target.contains('#');
    if let Some((canvas, text)) = node_query(query) {
        let prefix = format!("{canvas}#");
        let mut hits: Vec<(i32, usize, &StudioMention)> = found
            .iter()
            .enumerate()
            .filter(|(_, m)| (node(m) || look(m)) && m.target.starts_with(&prefix))
            .filter_map(|(i, m)| Some((crate::commands::fuzzy(text, &m.name)?.0, i, m)))
            .collect();
        if !text.is_empty() {
            hits.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
        }
        return hits.into_iter().take(NODES).map(|(.., m)| studio_row(m)).collect();
    }
    let listed = found.iter().filter(|m| matches!(m.kind, MentionKind::Entity | MentionKind::Canvas) && !node(m));
    if query.is_empty() {
        let entities = listed.clone().filter(|m| m.kind == MentionKind::Entity).take(BARE_ENTITIES);
        let canvases = listed.filter(|m| m.kind == MentionKind::Canvas).take(BARE_CANVASES);
        return entities.chain(canvases).map(studio_row).collect();
    }
    let mut hits: Vec<(i32, usize, &StudioMention)> = listed
        .enumerate()
        .filter_map(|(i, m)| {
            Some((crate::commands::fuzzy(query, &m.name).into_iter().chain(crate::commands::fuzzy(query, &m.target)).map(|(s, _)| s).max()?, i, m))
        })
        .collect();
    hits.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
    // A named reference found by its name (`crown`) reads whole (`Dhritarashtra · Crown off`), under its entity.
    let row = |m: &StudioMention| match (studio_row(m), m.target.split_once('#')) {
        (Choice::Studio { kind: MentionKind::Entity, target, name, detail }, Some((entity, _))) => {
            let owner = detail.split_once(" · ").map_or_else(|| label(MentionKind::Entity, entity, None), |(_, o)| o.to_owned());
            let kind = detail.split(" · ").next().unwrap_or_default().to_owned();
            Choice::Studio { kind: MentionKind::Entity, target, name: ref_label(&owner, &name), detail: kind }
        }
        (row, _) => row,
    };
    under_entities(hits.into_iter().take(STUDIO).map(|(.., m)| row(m)).collect())
}

/// The popup's rows: matching agents, studio entities and canvases, media models, then otto's paths (already
/// ranked, at most [`FILES`]), or [`Choice::Loading`] while there are none yet. A bare `@` lists the studio's
/// entities and canvases first (none outside a studio); a `canvas#text` query only that canvas's nodes (loading
/// until otto answers: `files` is set with them).
pub fn choices(
    agents: &[(String, String)],
    studio: &[StudioMention],
    media: Option<&MediaStatus>,
    query: &str,
    files: Option<&[FileMatch]>,
) -> Vec<Choice> {
    if node_query(query).is_some() {
        let mut rows = studio_matches(studio, query);
        if files.is_none() && rows.is_empty() {
            rows.push(Choice::Loading);
        }
        return rows;
    }
    let agents = agent_matches(agents, query).into_iter().map(|(name, about)| Choice::Agent { name: name.clone(), about: about.clone() });
    let mut rows: Vec<Choice> = Vec::new();
    if query.is_empty() {
        rows.extend(studio_matches(studio, query));
        rows.extend(agents);
    } else {
        rows.extend(agents);
        rows.extend(studio_matches(studio, query));
    }
    rows.extend(model_matches(media, query));
    match files {
        Some(files) => rows.extend(files.iter().take(FILES as usize).map(|f| Choice::File { path: f.path.clone(), dir: f.dir })),
        None => rows.push(Choice::Loading),
    }
    rows
}

impl Choice {
    /// The heading of its group of rows ([`headings`]).
    pub fn group(&self) -> &'static str {
        match self {
            Self::Agent { .. } => "Agents",
            Self::Studio { kind: MentionKind::Entity, .. } => "World",
            Self::Studio { target, .. } if target.contains('#') => "On the canvas",
            Self::Studio { .. } => "Canvases",
            Self::Model { .. } => "Models",
            Self::File { .. } | Self::Loading => "Files",
        }
    }
}

/// Each row's heading, drawn above it when it starts a group: only when the rows hold a studio's and more than one
/// group (elsewhere the popup is as it was).
pub fn headings(choices: &[Choice]) -> Vec<Option<&'static str>> {
    let studio = choices.iter().any(|c| matches!(c, Choice::Studio { .. }));
    let groups = choices.iter().map(Choice::group).collect::<std::collections::BTreeSet<_>>().len();
    choices.iter().enumerate().map(|(i, c)| (studio && groups > 1 && (i == 0 || choices[i - 1].group() != c.group())).then(|| c.group())).collect()
}

/// Row `index`'s place among the popup's children (headings come before their rows), to scroll it into view.
pub fn child(choices: &[Choice], index: usize) -> usize {
    index + headings(choices).iter().take(index + 1).filter(|h| h.is_some()).count()
}

/// Rows ↑/↓ and ⏎ reach (the loading row is last and isn't one).
pub fn pickable(choices: &[Choice]) -> usize {
    choices.iter().filter(|c| **c != Choice::Loading).count()
}

/// The popup's padding and corner (the composer's popovers'), for a host drawing [`list`] in its own card.
pub const PAD: f32 = super::rows::menu::PAD;
pub const RADIUS: f32 = super::rows::menu::RADIUS;

/// The popup's list (the composer's `@` popup, a Studio card's prompt): `choices` under their [`headings`],
/// `selected` filled, at most nine rows tall (it scrolls; `scroll` keeps the selection in view), `loading` on the
/// [`Choice::Loading`] row, "No matches" for none. Hovering a row selects it (`hover`); a click picks it (`pick`).
/// Ids start with `id`.
#[allow(clippy::too_many_arguments)]
pub fn list<T: 'static>(
    id: &'static str,
    choices: Vec<Choice>,
    selected: usize,
    scroll: &ScrollHandle,
    loading: &'static str,
    cx: &mut gpui::Context<T>,
    hover: fn(&mut T, usize, &mut gpui::Context<T>),
    pick: fn(&mut T, usize, &mut Window, &mut gpui::Context<T>),
) -> AnyElement {
    use super::rows::{self, menu};
    use gpui::ElementId;
    if choices.is_empty() {
        return menu::empty(ElementId::Name(format!("{id}-empty").into()), "No matches").into_any_element();
    }
    let row_id = |i: usize| ElementId::from((id, i));
    let clickable = |row: gpui::Stateful<gpui::Div>, i: usize, selected: bool, cx: &mut gpui::Context<T>| {
        rows::clickable(row, selected)
            .on_mouse_move(cx.listener(move |this, _, _, cx| hover(this, i, cx)))
            .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| pick(this, i, window, cx)))
    };
    let headings = headings(&choices);
    let mut out = div().id(id).min_h_0().flex().flex_col().max_h(px(menu::max_h(9.0))).overflow_y_scroll().track_scroll(scroll);
    for (i, choice) in choices.into_iter().enumerate() {
        let on = i == selected;
        if let Some(heading) = headings[i] {
            out = out.child(menu::section(heading, None));
        }
        let (glyph, title, about) = match choice {
            Choice::Agent { name, about } => ("bot", name, (!about.is_empty()).then_some(about)),
            Choice::File { path, dir } => (if dir { "folder" } else { "file-text" }, path, None),
            Choice::Model { name, provider_id, provider, kind, connected, .. } => {
                let kind_name = match kind {
                    MediaKind::Image => "Image",
                    MediaKind::Video => "Video",
                    MediaKind::Audio => "Audio",
                };
                let note = (!connected)
                    .then(|| div().flex_none().text_size(px(11.5)).text_color(rgb(theme::muted())).child("Not connected").into_any_element());
                let title = div().text_size(px(12.5)).child(name);
                let about = Some(format!("{provider} · {kind_name}").into());
                let row = menu::row(row_id(i), on, Some(menu::lead(super::media_setup::logo(&provider_id), on)), title, about, note);
                out = out.child(clickable(row, i, on, cx));
                continue;
            }
            Choice::Studio { kind, target, name, detail } => {
                let title = div().text_size(px(12.5)).child(name);
                let lead = menu::lead(row_icon(kind, &target, &detail), on);
                let row = menu::row(row_id(i), on, Some(lead), title, (!detail.is_empty()).then(|| detail.into()), None);
                out = out.child(clickable(row, i, on, cx));
                continue;
            }
            Choice::Loading => {
                out = out.child(menu::empty(ElementId::NamedInteger(format!("{id}-loading").into(), i as u64), loading));
                continue;
            }
        };
        let title = div().font_family(super::view::mono()).text_size(px(12.5)).child(title);
        let row = menu::row(row_id(i), on, Some(menu::lead(glyph, on)), title, about.map(Into::into), None);
        out = out.child(clickable(row, i, on, cx));
    }
    out.into_any_element()
}

/// A sent user message as input again (a retry): its shown text with its mentions as mention parts,
/// from the item's stored `mentions` (attachment notes and files are not re-sent).
pub fn retry_input(item: &Item) -> Vec<InputPart> {
    let (text, _, chips) = super::transcript::user_message(item);
    input(&text, &chips)
}

/// `chips` of `text` that fall within `text[from..to]`, rebased to start there (a trimmed draft).
pub fn within(text: &str, chips: &[Chip], from: usize, to: usize) -> Vec<Chip> {
    chips
        .iter()
        .filter(|c| c.range.start >= from && c.range.end <= to && c.fits(text))
        .map(|c| Chip { range: c.range.start - from..c.range.end - from, ..c.clone() })
        .collect()
}

/// A draft as input parts: the text between chips, each chip a mention.
pub fn input(text: &str, chips: &[Chip]) -> Vec<InputPart> {
    let mut chips: Vec<&Chip> = chips.iter().filter(|c| c.fits(text)).collect();
    chips.sort_by_key(|c| c.range.start);
    let (mut parts, mut at) = (Vec::new(), 0);
    for c in chips {
        if c.range.start < at {
            continue;
        }
        if c.range.start > at {
            parts.push(InputPart::Text { text: text[at..c.range.start].to_owned() });
        }
        parts.push(InputPart::Mention { kind: c.kind, target: c.target.clone() });
        at = c.range.end;
    }
    if at < text.len() {
        parts.push(InputPart::Text { text: text[at..].to_owned() });
    }
    parts
}

/// A user item's mentions as chips of the text the transcript shows: `shown` is where each text part
/// that is shown starts in it (`None` for parts it hides), `lead` the whitespace trimmed off its start.
pub fn item_chips(item: &Item, shown: &[Option<usize>], lead: usize, len: usize) -> Vec<Chip> {
    let mut raw = 0;
    let mut starts = Vec::new();
    for (part, at) in item.parts.iter().filter_map(|p| if let otto_proto::Part::Text { text, .. } = p { Some(text.len()) } else { None }).zip(shown) {
        starts.push((raw..raw + part, *at));
        raw += part;
    }
    item.mentions
        .iter()
        .filter_map(|m| {
            let [start, end] = m.range;
            let (span, at) = starts.iter().find(|(span, _)| span.start <= start && end <= span.end)?;
            let at = (*at)? + start - span.start;
            let at = at.checked_sub(lead)?;
            (at + end - start <= len).then(|| Chip {
                range: at..at + end - start,
                kind: m.kind,
                target: m.target.clone(),
                missing: m.missing,
                face: None,
            })
        })
        .collect()
}

/// Paint chips behind text laid out by `pos` (byte → top-left of its glyph, window coordinates):
/// a rounded fill under each and its icon where the (hidden) `@` is.
/// A chip's colour: its icon and text, and (faint) its pill.
pub fn tint() -> u32 {
    theme::accent()
}

/// The weight a chip's (hidden) text is laid out in: wider than the label painted over it, so the icon's tile
/// and its gap fit in the same space.
pub const LAYOUT_WEIGHT: gpui::FontWeight = gpui::FontWeight::SEMIBOLD;
const GAP: f32 = 4.0;

/// Each chip as a badge over its hidden text: a pill in [`tint`], the icon on a tile, a gap, then its target in
/// the UI font (smaller, then cut, when it is longer than a badge may be); struck through and faded when missing; faded with a dashed pill when
/// quiet ([`Face::quiet`]). A chip broken across lines is drawn on the line holding most of it ([`widest`]).
pub fn paint(chips: &[Chip], pos: impl Fn(usize) -> Option<Point<Pixels>>, line_height: Pixels, window: &mut Window, cx: &mut App) {
    let full: Hsla = rgb(self::tint()).into();
    let size = window.text_style().font_size.to_pixels(window.rem_size());
    for c in chips {
        let Some((a, b)) = span(c.range.clone(), &pos) else { continue };
        let quiet = c.quiet();
        let tint = if quiet { Hsla { a: 0.6, ..full } } else { full };
        // In step with the text's size, so the room [`laid_out`] left for it (in ems) fits.
        let tile = (line_height - px(7.0)).min(px(15.0)).max(px(10.0)).min(size * 1.1);
        let top = a.y + (line_height - tile) / 2.0;
        let path = format!("icons/{}.svg", c.icon(cx));
        let glyph = tile * 0.68;
        let icon = |x: Pixels, window: &mut Window, cx: &mut App| {
            let at = point(x + (tile - glyph) / 2.0, top + (tile - glyph) / 2.0);
            let _ = window.paint_svg(Bounds::new(at, gpui::size(glyph, glyph)), path.clone().into(), None, TransformationMatrix::unit(), tint, cx);
        };
        // Broken across lines (a narrow pane, a long name): the badge goes on the line holding most of it, its
        // label cut to fit; with no room anywhere, only the icon.
        let (a, b) = if (b.y - a.y).abs() >= px(1.0) {
            match widest(c.range.clone(), a, &pos).filter(|(a, b)| b.x - a.x >= tile + px(GAP) + size * 2.0) {
                Some(part) => part,
                None => {
                    icon(a.x, window, cx);
                    continue;
                }
            }
        } else {
            (a, b)
        };
        let pill = Bounds::from_corners(point(a.x - px(1.0), a.y + px(1.5)), point(b.x + px(1.0), a.y + line_height - px(1.5)));
        if quiet {
            let mut outline = fill(pill, Hsla { a: 0.0, ..full }).corner_radii(px(5.0)).border_widths(px(1.0)).border_color(Hsla { a: 0.5, ..full });
            outline.border_style = gpui::BorderStyle::Dashed;
            window.paint_quad(outline);
        } else {
            window.paint_quad(fill(pill, Hsla { a: 0.14, ..tint }).corner_radii(px(5.0)));
        }
        // The badge's fixed parts shrink with small text (a card's prompt zoomed out), as its room does.
        let small = (size / px(13.0)).min(1.0);
        let x = a.x + px(2.0) * small;
        window.paint_quad(
            fill(Bounds::new(point(x, top), gpui::size(tile, tile)), Hsla { a: if quiet { 0.1 } else { 0.22 }, ..full }).corner_radii(px(3.5)),
        );
        icon(x, window, cx);
        let start = x + tile + px(GAP) * small;
        let room = b.x - px(3.0) * small - start;
        let color = if c.missing { Hsla { a: 0.45, ..tint } } else { tint };
        let font = gpui::Font { family: theme::ui_font(), ..window.text_style().font() };
        let label = c.label(cx);
        let shape = |text: &str, size: Pixels, window: &Window| {
            let run = gpui::TextRun { len: text.len(), font: font.clone(), color, background_color: None, underline: None, strikethrough: None };
            window.text_system().shape_line(text.to_owned().into(), size, &[run], None)
        };
        // The room was laid out for this label ([`laid_out`], by glyph advances): a pixel over is not too long.
        let room = room + px(1.5);
        let (mut label_size, mut line) = (size * LABEL_SCALE, shape(&label, size * LABEL_SCALE, window));
        while line.width > room && label_size > size * 0.72 {
            label_size *= 0.94;
            line = shape(&label, label_size, window);
        }
        // A name longer than its target's text (entities, canvases): cut with an ellipsis.
        let mut cut = label.len();
        while line.width > room && cut > 0 {
            cut = label[..cut].char_indices().next_back().map_or(0, |(i, _)| i);
            line = shape(&format!("{}…", label[..cut].trim_end()), label_size, window);
        }
        let _ = line.paint(point(start, a.y), line_height, gpui::TextAlign::Left, None, window, cx);
        if c.missing {
            let y = a.y + line_height / 2.0;
            window.paint_quad(fill(Bounds::from_corners(point(start, y), point(start + line.width, y + px(1.0))), color));
        }
    }
}

/// Where a chip's text starts and ends (`pos` as for [`paint`]). Laid-out text puts an index on a wrap at the end
/// of the line before, so a chip the wrap put wholly on the next line would read as broken; it starts at that
/// line's left edge (the text's, where index 0 is) instead.
fn span(range: Range<usize>, pos: &impl Fn(usize) -> Option<Point<Pixels>>) -> Option<(Point<Pixels>, Point<Pixels>)> {
    let (a, b) = (pos(range.start)?, pos(range.end)?);
    // The `@` is one byte: its successor shows which line the chip's first glyph is on.
    let a = match pos(range.start + 1) {
        Some(next) if range.len() > 1 && next.y > a.y + px(1.0) => point(pos(0)?.x, next.y),
        _ => a,
    };
    Some((a, b))
}

/// The widest part a broken chip has on one line: its ends there (`a`: where [`span`] starts it). A part that
/// ends at a wrap is measured to its last index on that line, so it is short by that glyph.
fn widest(range: Range<usize>, a: Point<Pixels>, pos: &impl Fn(usize) -> Option<Point<Pixels>>) -> Option<(Point<Pixels>, Point<Pixels>)> {
    let mut parts: Vec<(Point<Pixels>, Point<Pixels>)> = vec![(a, a)];
    for i in range.start + 1..=range.end {
        let Some(p) = pos(i) else { continue };
        match parts.iter_mut().find(|(from, _)| (from.y - p.y).abs() < px(1.0)) {
            Some((from, to)) => {
                from.x = from.x.min(p.x);
                to.x = to.x.max(p.x);
            }
            // A later line starts at the text's left edge (the index on the wrap read as the line before's end).
            None => parts.push((point(pos(0).map_or(p.x, |left| left.x.min(p.x)), p.y), p)),
        }
    }
    // The first of equals: the line the chip starts on.
    parts.into_iter().reduce(|best, part| if part.1.x - part.0.x > best.1.x - best.0.x { part } else { best })
}

/// A chip's text in a bubble: laid out wider and clear, [`paint`] draws the badge in its place. Faded out, not a
/// clear colour: gpui blends a highlight's colour over the text's, so a transparent one changes nothing.
fn hidden() -> HighlightStyle {
    HighlightStyle { fade_out: Some(1.0), font_weight: Some(LAYOUT_WEIGHT), ..Default::default() }
}

/// What a badge takes beside its label, in ems of the text's size: the icon's tile, the gaps and the pill's ends.
const BADGE_EM: f32 = 1.85;
/// A badge is laid out this wide at most; a longer name is cut with an ellipsis.
const MAX_EM: f32 = 16.0;
/// The label is drawn this much smaller than the text around it.
const LABEL_SCALE: f32 = 0.94;
/// What a chip's hidden text is made of: ASCII that gpui's wrapper keeps inside a word
/// (`LineWrapper::is_word_char`), so a chip never wraps inside, from narrow to wide.
const STAND_INS: &[u8] = b".':ilIjtr1scaenoHNUDOGMWm_@%";

/// Glyphs and their widths in ems, narrowest first.
type Widths = std::rc::Rc<Vec<(u8, f32)>>;

thread_local! {
    /// [`STAND_INS`]' widths per UI font.
    static GLYPHS: std::cell::RefCell<std::collections::HashMap<gpui::SharedString, Widths>> = std::cell::RefCell::default();
}

const MEASURE: f32 = 100.0;

fn ui(weight: gpui::FontWeight) -> gpui::Font {
    gpui::Font { weight, ..gpui::font(theme::ui_font()) }
}

fn glyphs(cx: &App) -> Widths {
    let family = theme::ui_font();
    if let Some(found) = GLYPHS.with(|g| g.borrow().get(&family).cloned()) {
        return found;
    }
    let text = cx.text_system();
    let id = text.resolve_font(&ui(LAYOUT_WEIGHT));
    let mut widths: Vec<(u8, f32)> = STAND_INS.iter().map(|b| (*b, f32::from(text.layout_width(id, px(MEASURE), *b as char)) / MEASURE)).collect();
    widths.retain(|(_, w)| *w > 0.0);
    widths.sort_by(|a, b| a.1.total_cmp(&b.1));
    let widths = std::rc::Rc::new(widths);
    GLYPHS.with(|g| g.borrow_mut().insert(family, widths.clone()));
    widths
}

/// `label`'s width in ems as [`paint`] draws it.
fn label_em(label: &str, cx: &App) -> f32 {
    let text = cx.text_system();
    let id = text.resolve_font(&ui(gpui::FontWeight::NORMAL));
    label.chars().map(|c| f32::from(text.layout_width(id, px(MEASURE), c)) / MEASURE).sum::<f32>() * LABEL_SCALE
}

/// `len` bytes of `glyphs` (ems, narrowest first) about `want` ems wide: wide ones, one that fits the rest, then
/// narrow ones (one kind beside its own kind, so kerning hardly moves the sum). Within what `len` glyphs can be.
#[must_use]
pub fn stand_in(len: usize, want: f32, glyphs: &[(u8, f32)]) -> Vec<u8> {
    let (Some(&(narrow, n)), Some(&(wide, w))) = (glyphs.first(), glyphs.last()) else { return vec![b'_'; len] };
    #[allow(clippy::cast_precision_loss)]
    let count = len as f32;
    if len == 0 || want <= count * n || w <= n {
        return vec![narrow; len];
    }
    if want >= count * w {
        return vec![wide; len];
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let (wides, rest) = {
        let wides = (((want - count * n) / (w - n)).floor() as usize).min(len - 1);
        (wides, want - wides as f32 * w - (len - 1 - wides) as f32 * n)
    };
    let fit = glyphs.iter().min_by(|a, b| (a.1 - rest).abs().total_cmp(&(b.1 - rest).abs())).map_or(narrow, |g| g.0);
    let mut out = vec![wide; wides];
    out.push(fit);
    out.resize(len, narrow);
    out
}

/// `text` as laid out under its chips: the same number of bytes (offsets hold), but each chip's hidden text is
/// glyphs as wide as its badge ([`stand_in`]): the icon, then what it reads. So a badge is as wide as its label,
/// whatever its target's length (`@characters/sanjaya` reads "Sanjaya"; `@soul-2` needs room for its icon), and
/// never wraps inside.
pub fn laid_out(text: &str, chips: &[Chip], cx: &App) -> String {
    if chips.is_empty() {
        return text.to_owned();
    }
    let glyphs = glyphs(cx);
    let mut bytes = text.as_bytes().to_vec();
    for c in chips {
        // Whole characters only: the result stays UTF-8.
        if !(text.is_char_boundary(c.range.start) && text.is_char_boundary(c.range.end)) {
            continue;
        }
        let Some(hidden) = bytes.get_mut(c.range.clone()) else { continue };
        let want = (BADGE_EM + label_em(&c.label(cx), cx)).min(MAX_EM);
        hidden.copy_from_slice(&stand_in(hidden.len(), want, &glyphs));
    }
    String::from_utf8(bytes).unwrap_or_else(|_| text.to_owned())
}

/// `text` with its chips inline (a sent message's bubble), `selected` (a range, its colour) highlighted; and
/// its layout.
pub fn inline(text: &str, chips: &[Chip], selected: Option<(Range<usize>, Hsla)>, cx: &App) -> (AnyElement, gpui::TextLayout) {
    let mut chips: Vec<Chip> = chips.iter().filter(|c| c.fits(text)).cloned().collect();
    chips.sort_by_key(|c| c.range.start);
    chips.dedup_by(|b, a| b.range.start < a.range.end);
    let mut highlights = Vec::new();
    for c in &chips {
        highlights.push((c.range.clone(), hidden()));
    }
    if let Some((range, bg)) = selected {
        highlights = super::select::overlay(highlights, range, bg);
    }
    let styled = StyledText::new(laid_out(text, &chips, cx)).with_highlights(highlights);
    let layout = styled.layout().clone();
    let back = {
        let layout = layout.clone();
        canvas(|_, _, _| {}, move |_, _, window, cx| paint(&chips, |i| layout.position_for_index(i), layout.line_height(), window, cx))
            .absolute()
            .top_0()
            .left_0()
            .size_full()
    };
    (div().relative().child(back).child(styled).into_any_element(), layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(at: usize, kind: MentionKind, target: &str) -> Chip {
        Chip::new(at, kind, target.into())
    }

    #[test]
    fn a_chip_s_text_is_hidden_under_its_badge() {
        let ink = gpui::TextStyle { color: gpui::black(), ..Default::default() };
        assert_eq!(ink.clone().highlight(hidden()).color.a, 0.0);
        assert_eq!(ink.clone().highlight(hidden()).font_weight, LAYOUT_WEIGHT);
        // What it was: a clear colour blends to the text's own.
        let clear = HighlightStyle { color: Some(gpui::transparent_black()), ..Default::default() };
        assert_eq!(ink.highlight(clear).color.a, 1.0);
    }

    #[test]
    fn a_sent_chip_s_name_is_asked_for_by_what_finds_it() {
        assert_eq!(name_query(MentionKind::Entity, "characters/sanjaya"), "sanjaya");
        assert_eq!(name_query(MentionKind::Canvas, "ep01-pilot"), "ep01-pilot");
        assert_eq!(name_query(MentionKind::Canvas, "ep01-pilot#n_3f2a9c"), "ep01-pilot#");
    }

    #[test]
    fn a_chip_s_hidden_text_is_as_wide_as_its_badge() {
        let glyphs = [(b'i', 0.25), (b'a', 0.5), (b'o', 0.6), (b'W', 1.0)];
        let width = |s: &[u8]| s.iter().map(|b| glyphs.iter().find(|g| g.0 == *b).unwrap().1).sum::<f32>();
        // A long target reading a short name (`@characters/sanjaya`, "Sanjaya"): narrow glyphs, a few wide.
        let long = stand_in(19, 6.0, &glyphs);
        assert_eq!(String::from_utf8(long.clone()).unwrap(), format!("Wo{}", "i".repeat(17)));
        assert!((width(&long) - 6.0).abs() < 0.25, "{}", width(&long));
        // A short target that needs room for its icon (`@soul-2`): wide glyphs.
        let short = stand_in(7, 5.3, &glyphs);
        assert!((width(&short) - 5.3).abs() < 0.25 && short.len() == 7, "{short:?}");
        // No narrower or wider than its glyphs can be; nothing to break a line at either way.
        assert_eq!(stand_in(4, 0.1, &glyphs), b"iiii");
        assert_eq!(stand_in(2, 9.0, &glyphs), b"WW");
        assert_eq!(stand_in(0, 3.0, &glyphs), b"");
        assert_eq!(stand_in(3, 2.0, &[]), b"___");
        assert!(STAND_INS.iter().all(|b| b.is_ascii_alphanumeric() || b"-_.'$%@#^~,=:;".contains(b)), "word characters to gpui's wrapper");
    }

    #[test]
    fn a_chip_the_wrap_moved_to_the_next_line_is_whole_there() {
        // Laid out from x 100, 8 px a glyph, wrapped at 10: index 10 reads as the end of the first line.
        let pos =
            |i: usize| Some(if i <= 10 { point(px(100.0 + 8.0 * i as f32), px(0.0)) } else { point(px(100.0 + 8.0 * (i - 10) as f32), px(20.0)) });
        let at = |x: f32, y: f32| point(px(x), px(y));
        assert_eq!(span(10..20, &pos), Some((at(100.0, 20.0), at(180.0, 20.0))), "starts at the second line's left edge");
        assert_eq!(span(2..8, &pos), Some((at(116.0, 0.0), at(164.0, 0.0))));
        assert_eq!(span(8..14, &pos), Some((at(164.0, 0.0), at(132.0, 20.0))), "one really broken keeps both ends");
        // Broken: two characters on the first line, four on the second, where the badge goes.
        assert_eq!(widest(8..14, at(164.0, 0.0), &pos), Some((at(100.0, 20.0), at(132.0, 20.0))));
        assert_eq!(widest(2..8, at(116.0, 0.0), &pos), Some((at(116.0, 0.0), at(164.0, 0.0))), "whole: itself");
    }

    #[test]
    fn the_query_is_an_at_word_before_the_caret() {
        assert_eq!(query("@", 1, &[]), Some((0..1, "")));
        assert_eq!(query("ask @bu", 7, &[]), Some((4..7, "bu")));
        assert_eq!(query("ask @src/ma", 11, &[]), Some((4..11, "src/ma")));
        // The range runs to the word's end; the query stops at the caret.
        assert_eq!(query("ask @build now", 7, &[]), Some((4..10, "bu")));
        assert_eq!(query("(@a", 3, &[]), Some((1..3, "a")));
        // Not at a word start, past a space, or before any `@`.
        assert_eq!(query("me@host", 7, &[]), None);
        assert_eq!(query("@a b", 4, &[]), None);
        assert_eq!(query("plain", 5, &[]), None);
        assert_eq!(query("", 0, &[]), None);
        // Inside a chip there is nothing to complete.
        let chips = [chip(4, MentionKind::Agent, "build")];
        assert_eq!(query("ask @build", 10, &chips), None);
        assert_eq!(query("ask @build", 7, &chips), None);
        // A new `@` right before a chip stops at it.
        let chips = [chip(5, MentionKind::Agent, "build")];
        assert_eq!(query("ask @@build", 5, &chips), Some((4..5, "")));
    }

    #[test]
    fn agents_rank_by_name_and_files_follow() {
        let agents: Vec<(String, String)> =
            ["build", "reviewer", "docs-writer", "a1", "a2", "a3", "a4", "a5", "a6"].iter().map(|n| ((*n).to_owned(), String::new())).collect();
        let names = |q: &str| agent_matches(&agents, q).into_iter().map(|a| a.0.as_str()).collect::<Vec<_>>();
        assert_eq!(names("").len(), AGENTS);
        assert_eq!(names("")[0], "build");
        assert_eq!(names("rev"), ["reviewer"]);
        assert_eq!(names("dw")[0], "docs-writer");
        assert!(names("src/").is_empty());
        let files = vec![FileMatch { path: "src/".into(), dir: true }, FileMatch { path: "src/main.rs".into(), dir: false }];
        let all = choices(&agents, &[], None, "b", Some(&files));
        assert_eq!(all[0], Choice::Agent { name: "build".into(), about: String::new() });
        assert_eq!(all.last().unwrap().mention(), Some((MentionKind::File, "src/main.rs".to_owned())));
        assert_eq!(choices(&agents, &[], None, "src/", Some(&files)).len(), 2);
        assert_eq!(pickable(&all), all.len());
        // otto answered with nothing: no rows (the popup says "No matches").
        assert!(choices(&agents, &[], None, "zzz", Some(&[])).is_empty());
    }

    #[test]
    fn before_the_first_reply_agents_show_with_a_loading_row() {
        let agents = vec![("build".to_owned(), "Builds".to_owned())];
        let rows = choices(&agents, &[], None, "", None);
        assert_eq!(rows, [Choice::Agent { name: "build".into(), about: "Builds".into() }, Choice::Loading]);
        assert_eq!(pickable(&rows), 1);
        assert_eq!(Choice::Loading.mention(), None);
        // No agent matches: still the loading row, never an empty popup.
        let rows = choices(&agents, &[], None, "src/", None);
        assert_eq!((rows.as_slice(), pickable(&rows)), ([Choice::Loading].as_slice(), 0));
    }

    #[test]
    fn a_retry_sends_the_stored_mentions_again() {
        let note = "[attachment att_1: a.txt, text/plain, 1 B]";
        let said = "ask @build about @src/ and @gone.rs";
        let mut item = Item::new("u", otto_llm::Role::User, vec![otto_proto::Part::text(note), otto_proto::Part::text(said)]);
        let at = |s: usize, e: usize| [note.len() + s, note.len() + e];
        let m = |kind, target: &str, range, missing| otto_proto::Mention { kind, target: target.into(), range, missing };
        item.mentions = vec![
            m(MentionKind::Agent, "build", at(4, 10), false),
            m(MentionKind::File, "src/", at(17, 22), false),
            m(MentionKind::File, "gone.rs", at(27, 35), true),
        ];
        let mention = |kind, target: &str| InputPart::Mention { kind, target: target.into() };
        let text = |t: &str| InputPart::Text { text: t.into() };
        assert_eq!(
            retry_input(&item),
            [
                text("ask "),
                mention(MentionKind::Agent, "build"),
                text(" about "),
                mention(MentionKind::File, "src/"),
                text(" and "),
                mention(MentionKind::File, "gone.rs"),
            ]
        );
        // Without mentions it is the plain text.
        assert_eq!(retry_input(&Item::user_text("v", " hi @you ")), [text("hi @you")]);
    }

    #[test]
    fn drafts_become_text_and_mentions() {
        let text = "ask @build about @src/main.rs";
        let chips = [chip(4, MentionKind::Agent, "build"), chip(17, MentionKind::File, "src/main.rs")];
        assert_eq!(
            input(text, &chips),
            [
                InputPart::Text { text: "ask ".into() },
                InputPart::Mention { kind: MentionKind::Agent, target: "build".into() },
                InputPart::Text { text: " about ".into() },
                InputPart::Mention { kind: MentionKind::File, target: "src/main.rs".into() },
            ]
        );
        // A plain `@word` stays text; a chip whose text changed is dropped.
        assert_eq!(input("hi @you", &[]), [InputPart::Text { text: "hi @you".into() }]);
        assert_eq!(input("ask @bxild", &[chip(4, MentionKind::Agent, "build")]), [InputPart::Text { text: "ask @bxild".into() }]);
        assert!(input("", &[]).is_empty());
        // Trimmed drafts keep their chips, rebased.
        let draft = "  @docs/ x ";
        let kept = within(draft, &[chip(2, MentionKind::File, "docs/")], 2, 10);
        assert_eq!(kept[0].range, 0..6);
        assert!(kept[0].folder() && icon(kept[0].kind, &kept[0].target) == "folder");
        assert!(within(draft, &[chip(2, MentionKind::File, "docs/")], 3, 10).is_empty());
    }

    #[test]
    fn item_mentions_map_onto_the_shown_text() {
        let mut item = Item::new(
            "u",
            otto_llm::Role::User,
            vec![otto_proto::Part::text("[attachment att_1: a.txt, text/plain, 1 B]"), otto_proto::Part::text(" ask @build")],
        );
        let note = "[attachment att_1: a.txt, text/plain, 1 B]".len();
        item.mentions.push(otto_proto::Mention { kind: MentionKind::Agent, target: "build".into(), range: [note + 5, note + 11], missing: true });
        // The note is hidden, the text shown from 0 and trimmed by one space.
        let chips = item_chips(&item, &[None, Some(0)], 1, "ask @build".len());
        assert_eq!(chips, [Chip { range: 4..10, kind: MentionKind::Agent, target: "build".into(), missing: true, face: None }]);
        // A mention in a hidden part, or past the shown text, is dropped.
        assert!(item_chips(&item, &[None, None], 0, 20).is_empty());
        assert!(item_chips(&item, &[None, Some(0)], 1, 5).is_empty());
    }

    fn media() -> MediaStatus {
        let model = |id: &str, name: &str, handle: &str, kind| otto_proto::MediaModel {
            id: id.into(),
            kind,
            name: name.into(),
            handle: handle.into(),
            default: false,
        };
        let provider = |id: &str, name: &str, connected, models| otto_proto::MediaProviderInfo {
            id: id.into(),
            name: name.into(),
            kinds: vec![MediaKind::Image, MediaKind::Video],
            connected,
            source: None,
            models,
        };
        let higgsfield =
            (0..8).map(|i| model(&format!("kling/{i}"), &format!("Kling {i}.0 Pro"), &format!("kling-{i}.0-pro"), MediaKind::Video)).collect();
        MediaStatus {
            providers: vec![
                provider(
                    "openai",
                    "OpenAI",
                    true,
                    vec![model("gpt-image-2.6-flare", "GPT Image 2.6 Flare", "gpt-image-2.6-flare", MediaKind::Image)],
                ),
                provider("higgsfield", "Higgsfield", false, higgsfield),
            ],
            defaults: Default::default(),
        }
    }

    #[test]
    fn models_come_between_agents_and_files_for_a_typed_query() {
        let agents = vec![("build".to_owned(), String::new()), ("klingon".to_owned(), String::new())];
        let files = vec![FileMatch { path: "kling.md".into(), dir: false }];
        let status = media();
        let rows = choices(&agents, &[], Some(&status), "kli", Some(&files));
        assert_eq!(rows[0], Choice::Agent { name: "klingon".into(), about: String::new() });
        let models: Vec<&Choice> = rows.iter().filter(|c| matches!(c, Choice::Model { .. })).collect();
        assert_eq!(models.len(), MODELS, "at most six");
        assert_eq!(
            *models[0],
            Choice::Model {
                handle: "kling-0.0-pro".into(),
                name: "Kling 0.0 Pro".into(),
                provider_id: "higgsfield".into(),
                provider: "Higgsfield".into(),
                kind: MediaKind::Video,
                connected: false
            }
        );
        assert_eq!(rows.last().unwrap().mention(), Some((MentionKind::File, "kling.md".to_owned())));
        assert_eq!(rows.len(), 1 + MODELS + 1);
        assert_eq!(models[0].mention(), Some((MentionKind::Model, "kling-0.0-pro".to_owned())));
        // By provider name, and by handle.
        let first = |q: &str| model_matches(Some(&status), q).first().and_then(Choice::mention).map(|m| m.1);
        assert_eq!(first("openai").as_deref(), Some("gpt-image-2.6-flare"));
        assert_eq!(first("gpt-image-2.6").as_deref(), Some("gpt-image-2.6-flare"));
        // None for an empty query, a path, or before the status loads.
        assert!(model_matches(Some(&status), "").is_empty());
        assert!(model_matches(Some(&status), "src/k").is_empty());
        assert!(model_matches(None, "kli").is_empty());
        assert_eq!(choices(&agents, &[], Some(&status), "", None).iter().filter(|c| matches!(c, Choice::Model { .. })).count(), 0);
        // Their providers, for the chips' logos.
        let providers = ModelProviders::of(&status);
        assert_eq!(super::super::media_setup::logo(&providers.0["gpt-image-2.6-flare"]), "provider-openai");
        assert_eq!(super::super::media_setup::logo(&providers.0["kling-3.0-pro"]), "provider-higgsfield");
    }

    #[test]
    fn connected_providers_models_come_first() {
        let model = |name: &str, handle: &str| otto_proto::MediaModel {
            id: handle.into(),
            kind: MediaKind::Image,
            name: name.into(),
            handle: handle.into(),
            default: false,
        };
        let provider = |id: &str, connected, models| otto_proto::MediaProviderInfo {
            id: id.into(),
            name: id.into(),
            kinds: vec![MediaKind::Image],
            connected,
            source: None,
            models,
        };
        // fal holds the shorter handles (its built-in model) but has no key; Higgsfield's comes first anyway.
        let status = MediaStatus {
            providers: vec![
                provider("higgsfield", true, vec![model("Seedream 5.0 Pro", "higgsfield-seedream-5.0-pro")]),
                provider("fal", false, vec![model("Seedream 5.5", "seedream-5.5"), model("Seedream 5.0 Pro", "seedream-5.0-pro")]),
            ],
            defaults: Default::default(),
        };
        let handles: Vec<String> = model_matches(Some(&status), "seedream").iter().filter_map(|c| c.mention()).map(|m| m.1).collect();
        assert_eq!(handles, ["higgsfield-seedream-5.0-pro", "seedream-5.5", "seedream-5.0-pro"]);
    }

    #[test]
    fn model_chips_send_and_retry_as_model_mentions() {
        let text = "draw with @gpt-image-2.6-flare";
        let parts = input(text, &[chip(10, MentionKind::Model, "gpt-image-2.6-flare")]);
        let mention = InputPart::Mention { kind: MentionKind::Model, target: "gpt-image-2.6-flare".into() };
        assert_eq!(parts, [InputPart::Text { text: "draw with ".into() }, mention.clone()]);
        let mut item = Item::user_text("u", text);
        item.mentions = vec![otto_proto::Mention { kind: MentionKind::Model, target: "gpt-image-2.6-flare".into(), range: [10, 30], missing: false }];
        assert_eq!(retry_input(&item), parts);
        let chips = item_chips(&item, &[Some(0)], 0, text.len());
        assert_eq!(chips, [Chip { range: 10..30, kind: MentionKind::Model, target: "gpt-image-2.6-flare".into(), missing: false, face: None }]);
    }

    fn found(kind: MentionKind, target: &str, name: &str, detail: &str) -> StudioMention {
        StudioMention { kind, target: target.into(), name: name.into(), detail: detail.into() }
    }

    fn studio() -> Vec<StudioMention> {
        vec![
            found(MentionKind::Canvas, "ep01-pilot", "Pilot", "Canvas · Ep 1"),
            found(MentionKind::Entity, "characters/mara", "Mara Venn", "Character"),
            found(MentionKind::Entity, "places/harbor", "Harbor", "Place"),
            found(MentionKind::Canvas, "moodboard", "", "Canvas"),
        ]
    }

    #[test]
    fn studio_entities_and_canvases_match_by_name_or_target() {
        let all = studio();
        // A bare `@`: entities, then canvases, in otto's order.
        let bare: Vec<String> = studio_matches(&all, "").iter().filter_map(Choice::mention).map(|m| m.1).collect();
        assert_eq!(bare, ["characters/mara", "places/harbor", "ep01-pilot", "moodboard"]);
        let mara = studio_matches(&all, "mara");
        assert_eq!(
            mara[0],
            Choice::Studio { kind: MentionKind::Entity, target: "characters/mara".into(), name: "Mara Venn".into(), detail: "Character".into() }
        );
        // By target alone (the name doesn't have it).
        assert_eq!(studio_matches(&all, "ep01").first().and_then(Choice::mention), Some((MentionKind::Canvas, "ep01-pilot".to_owned())));
        assert_eq!(studio_matches(&all, "places").first().and_then(Choice::mention), Some((MentionKind::Entity, "places/harbor".to_owned())));
        // No name: the target's last segment shows.
        let Some(Choice::Studio { name, .. }) = studio_matches(&all, "mood").into_iter().next() else { panic!() };
        assert_eq!(name, "moodboard");
        // A name matching wholly ranks before one matching scattered.
        let ranked = studio_matches(
            &[found(MentionKind::Entity, "props/hat", "Harbor Hat", "Prop"), found(MentionKind::Entity, "places/harbor", "Harbor", "Place")],
            "harbor",
        );
        assert_eq!(ranked.iter().filter_map(Choice::mention).map(|m| m.1).collect::<Vec<_>>(), ["places/harbor", "props/hat"]);
        // The better of name and target counts: `hat` is the hat's target's word, the harbor's name has it scattered.
        let ranked = studio_matches(
            &[found(MentionKind::Entity, "places/harbor", "Harbor at night", "Place"), found(MentionKind::Entity, "props/hat", "Cap", "Prop")],
            "hat",
        );
        assert_eq!(ranked.first().and_then(Choice::mention).map(|m| m.1).as_deref(), Some("props/hat"));
        // Other kinds otto might send aren't listed.
        assert!(studio_matches(&[found(MentionKind::File, "mara.md", "mara", "")], "mara").is_empty());
        // At most six.
        let many: Vec<StudioMention> =
            (0..9).map(|i| found(MentionKind::Entity, &format!("characters/m{i}"), &format!("Mara {i}"), "Character")).collect();
        let rows = studio_matches(&many, "mara");
        assert_eq!(rows.len(), STUDIO);
        assert_eq!(rows[0].mention().unwrap().1, "characters/m0", "ties keep otto's order");
        assert_eq!(rows[0].mention().unwrap().0, MentionKind::Entity);
    }

    #[test]
    fn studio_rows_come_after_agents_and_before_models_and_files() {
        let agents = vec![("marathon".to_owned(), String::new())];
        let status = media();
        let mut studio = studio();
        studio.push(found(MentionKind::Canvas, "kling-tests", "Kling tests", "Canvas"));
        let files = vec![FileMatch { path: "kling.md".into(), dir: false }];
        let kinds = |rows: &[Choice]| -> Vec<&'static str> {
            rows.iter()
                .map(|c| match c {
                    Choice::Agent { .. } => "agent",
                    Choice::Studio { .. } => "studio",
                    Choice::Model { .. } => "model",
                    Choice::File { .. } => "file",
                    Choice::Loading => "loading",
                })
                .collect()
        };
        let rows = choices(&agents, &studio, Some(&status), "kli", Some(&files));
        let k = kinds(&rows);
        assert_eq!(k.first(), Some(&"studio"));
        assert!(
            k.windows(2)
                .all(|w| ["studio", "model", "file"].iter().position(|x| *x == w[0]) <= ["studio", "model", "file"].iter().position(|x| *x == w[1])),
            "{k:?}"
        );
        assert_eq!(k.last(), Some(&"file"));
        let rows = choices(&agents, &studio, None, "mara", None);
        assert_eq!(kinds(&rows), ["agent", "studio", "loading"]);
        assert_eq!(pickable(&rows), 2);
    }

    #[test]
    fn studio_chips_read_their_names() {
        let mut names = StudioNames::default();
        names.add(&studio());
        names.add(&[found(MentionKind::Agent, "build", "Build", ""), found(MentionKind::Entity, "characters/teo", "  ", "Character")]);
        assert_eq!(names.entities.len(), 3, "agents aren't kept");
        assert_eq!(label(MentionKind::Entity, "characters/mara", Some(&names)), "Mara Venn");
        assert_eq!(label(MentionKind::Canvas, "ep01-pilot", Some(&names)), "Pilot");
        assert_eq!(label(MentionKind::Canvas, "ep01-pilot#n_3f2a9c", Some(&names)), "Pilot", "a node: its canvas's title");
        // Once otto has listed the canvas's nodes: the node's own title.
        names.add(&[found(MentionKind::Canvas, "ep01-pilot#n_3f2a9c", "S2.3 key", "Image card · S2.3")]);
        assert_eq!(label(MentionKind::Canvas, "ep01-pilot#n_3f2a9c", Some(&names)), "S2.3 key");
        assert_eq!(label(MentionKind::Canvas, "ep01-pilot#n_other", Some(&names)), "Pilot");
        // A skill reads `/name`.
        assert_eq!(label(MentionKind::Skill, "action-choreography", Some(&names)), "/action-choreography");
        // Unknown, or a blank name: the target's last segment.
        assert_eq!(label(MentionKind::Entity, "characters/teo", Some(&names)), "teo");
        assert_eq!(label(MentionKind::Entity, "places/old-mill", None), "old-mill");
        assert_eq!(label(MentionKind::Canvas, "ep02#n_9", None), "n_9");
        assert_eq!(label(MentionKind::Canvas, "ep02", Some(&names)), "ep02");
        // Other kinds: their target.
        assert_eq!(label(MentionKind::File, "src/main.rs", Some(&names)), "src/main.rs");
        assert_eq!(label(MentionKind::Agent, "build", Some(&names)), "build");
        assert_eq!(label(MentionKind::Model, "fal/kling", Some(&names)), "fal/kling");
        // A later answer renames.
        names.add(&[found(MentionKind::Canvas, "ep01-pilot", "The Pilot", "Canvas · Ep 1")]);
        assert_eq!(label(MentionKind::Canvas, "ep01-pilot", Some(&names)), "The Pilot");
        assert_eq!((icon(MentionKind::Entity, "x"), icon(MentionKind::Canvas, "x")), ("earth", crate::studio_block::ICON));
    }

    #[test]
    fn studio_chips_send_and_retry_as_their_mentions() {
        let text = "redo @characters/mara in @ep01-pilot#n_3f warmer";
        let chips = [chip(5, MentionKind::Entity, "characters/mara"), chip(25, MentionKind::Canvas, "ep01-pilot#n_3f")];
        let entity = InputPart::Mention { kind: MentionKind::Entity, target: "characters/mara".into() };
        let canvas = InputPart::Mention { kind: MentionKind::Canvas, target: "ep01-pilot#n_3f".into() };
        let t = |s: &str| InputPart::Text { text: s.into() };
        let parts = input(text, &chips);
        assert_eq!(parts, [t("redo "), entity, t(" in "), canvas, t(" warmer")]);
        let row = Choice::Studio { kind: MentionKind::Canvas, target: "ep01-pilot#n_3f".into(), name: "Pilot".into(), detail: "Canvas".into() };
        assert_eq!(row.mention(), Some((MentionKind::Canvas, "ep01-pilot#n_3f".to_owned())));
        let mut item = Item::user_text("u", text);
        let m = |kind, target: &str, range| otto_proto::Mention { kind, target: target.into(), range, missing: false };
        item.mentions = vec![m(MentionKind::Entity, "characters/mara", [5, 21]), m(MentionKind::Canvas, "ep01-pilot#n_3f", [25, 41])];
        assert_eq!(retry_input(&item), parts);
        let shown = item_chips(&item, &[Some(0)], 0, text.len());
        assert_eq!(
            shown,
            [
                Chip { range: 5..21, kind: MentionKind::Entity, target: "characters/mara".into(), missing: false, face: None },
                Chip { range: 25..41, kind: MentionKind::Canvas, target: "ep01-pilot#n_3f".into(), missing: false, face: None },
            ]
        );
    }

    #[test]
    fn a_bare_at_in_a_studio_lists_its_world_and_canvases_first() {
        let agents = vec![("build".to_owned(), String::new())];
        let files = vec![FileMatch { path: "src/".into(), dir: true }];
        let mut many: Vec<StudioMention> =
            (0..7).map(|i| found(MentionKind::Entity, &format!("characters/c{i}"), &format!("C{i}"), "Character")).collect();
        many.extend((0..6).map(|i| found(MentionKind::Canvas, &format!("ep0{i}"), &format!("Ep {i}"), "Canvas")));
        let rows = choices(&agents, &many, None, "", Some(&files));
        let kinds: Vec<&str> = rows.iter().map(Choice::group).collect();
        let mut expected = vec!["World"; BARE_ENTITIES];
        expected.extend(vec!["Canvases"; BARE_CANVASES]);
        expected.extend(["Agents", "Files"]);
        assert_eq!(kinds, expected);
        assert_eq!(rows[0].mention(), Some((MentionKind::Entity, "characters/c0".to_owned())));
        assert_eq!(rows[BARE_ENTITIES].mention(), Some((MentionKind::Canvas, "ep00".to_owned())));
        // Headings above each group, and rows' places among the children shift past them.
        let heads = headings(&rows);
        assert_eq!(heads[0], Some("World"));
        assert_eq!(heads[1], None);
        assert_eq!(heads[BARE_ENTITIES], Some("Canvases"));
        assert_eq!(heads.iter().flatten().copied().collect::<Vec<_>>(), ["World", "Canvases", "Agents", "Files"]);
        assert_eq!((child(&rows, 0), child(&rows, 1), child(&rows, BARE_ENTITIES)), (1, 2, BARE_ENTITIES + 2));
        // Before otto answers: agents and the loading row, no headings.
        let rows = choices(&agents, &[], None, "", None);
        assert_eq!(rows, [Choice::Agent { name: "build".into(), about: String::new() }, Choice::Loading]);
        assert!(headings(&rows).iter().all(Option::is_none));
        assert_eq!(child(&rows, 1), 1);
        // Outside a studio, as before: agents then paths, no headings.
        let rows = choices(&agents, &[], None, "", Some(&files));
        assert_eq!(rows.iter().map(Choice::group).collect::<Vec<_>>(), ["Agents", "Files"]);
        assert!(headings(&rows).iter().all(Option::is_none));
        // A typed query keeps agents first.
        let rows = choices(&[("c-agent".to_owned(), String::new())], &many, None, "c", Some(&[]));
        assert_eq!(rows[0].group(), "Agents");
    }

    fn nodes() -> Vec<StudioMention> {
        vec![
            found(MentionKind::Canvas, "ep01#n_scene", "Pass 3A · The quay fight", "Scene"),
            found(MentionKind::Canvas, "ep01#n_key", "S2.3 key", "Image card · S2.3"),
            found(MentionKind::Canvas, "ep01#n_move", "Mara vaults the rail", "Video card · S2.3"),
            found(MentionKind::Canvas, "ep01#n_note", "Keep it cold.", "Note"),
            found(MentionKind::Canvas, "ep01#n_doc", "script.md", "Document"),
            found(MentionKind::Canvas, "ep02#n_x", "Elsewhere", "Note"),
        ]
    }

    #[test]
    fn a_canvas_query_lists_its_nodes_by_title() {
        assert_eq!(node_query("ep01#ke"), Some(("ep01", "ke")));
        assert_eq!(node_query("ep01#"), Some(("ep01", "")));
        assert_eq!((node_query("#x"), node_query("ep01")), (None, None));
        let all = nodes();
        let names = |q: &str| {
            studio_matches(&all, q).into_iter().map(|c| if let Choice::Studio { name, .. } = c { name } else { panic!() }).collect::<Vec<_>>()
        };
        // Only that canvas's, in otto's order for nothing typed, filtered by what follows `#`.
        assert_eq!(names("ep01#"), ["Pass 3A · The quay fight", "S2.3 key", "Mara vaults the rail", "Keep it cold.", "script.md"]);
        assert_eq!(names("ep01#key")[0], "S2.3 key");
        assert_eq!(names("ep01#quay"), ["Pass 3A · The quay fight"]);
        assert_eq!(names("ep02#"), ["Elsewhere"]);
        assert!(names("ep01#zzz").is_empty());
        // Picking one mentions the node.
        assert_eq!(studio_matches(&all, "ep01#key")[0].mention(), Some((MentionKind::Canvas, "ep01#n_key".to_owned())));
        // Nothing else is offered for a node query: no agents, models or paths; loading until otto answers.
        let agents = vec![("ep01".to_owned(), String::new())];
        let rows = choices(&agents, &all, None, "ep01#s", None);
        assert!(rows.iter().all(|c| matches!(c, Choice::Studio { .. })), "{rows:?}");
        assert_eq!(choices(&agents, &[], None, "ep01#", None), [Choice::Loading]);
        assert!(choices(&agents, &[], None, "ep01#", Some(&[])).is_empty());
        // Nodes never show for other queries; a node list has no headings.
        assert!(studio_matches(&all, "").is_empty() && studio_matches(&all, "key").is_empty());
        assert!(headings(&studio_matches(&all, "ep01#")).iter().all(Option::is_none));
        // Each row's icon by its kind.
        let icons: Vec<&str> = all.iter().map(|m| row_icon(m.kind, &m.target, &m.detail)).collect();
        assert_eq!(icons, ["frame", "image", "video", "sticky-note", "file-text", "sticky-note"]);
        assert_eq!(row_icon(MentionKind::Canvas, "ep01", "Canvas"), crate::studio_block::ICON);
        for icon in icons {
            assert!(crate::assets::has_icon(icon), "{icon}");
        }
    }

    fn looks() -> Vec<StudioMention> {
        vec![
            found(MentionKind::Entity, "characters/dhritarashtra#crown-off", "Crown off", "Look · Dhritarashtra"),
            found(MentionKind::Entity, "characters/dhritarashtra#orbit", "Orbit", "Video · Dhritarashtra"),
            found(MentionKind::Entity, "characters/dhritarashtra#voice", "Voice", "Audio · Dhritarashtra"),
        ]
    }

    #[test]
    fn hash_after_an_entity_lists_its_references() {
        // An entity's chip drills in on `#` (no space when picked), a reference's doesn't.
        let entity = chip(4, MentionKind::Entity, "characters/dhritarashtra");
        let look = chip(4, MentionKind::Entity, "characters/dhritarashtra#crown-off");
        assert!(entity.drills() && entity.opens_on("#c") && !entity.opens_on("x") && !entity.opens_on(" "));
        assert!(!look.drills() && !look.opens_on("#"));
        // The query is a drill-in for references (not a canvas's nodes): its head is an entity id.
        assert!(looks_query("characters/dhritarashtra#") && looks_query("characters/dhritarashtra#cr"));
        assert!(!looks_query("ep01#") && !looks_query("dhritarashtra"));
        assert_eq!(name_query(MentionKind::Entity, "characters/dhritarashtra#crown-off"), "characters/dhritarashtra#");
        // otto's answer: that entity's references, by name, in order; filtered by what follows `#`.
        let mut all = looks();
        all.extend(nodes());
        let rows = choices(&[], &all, None, "characters/dhritarashtra#", None);
        let names: Vec<String> = rows.iter().map(|c| if let Choice::Studio { name, .. } = c { name.clone() } else { panic!() }).collect();
        assert_eq!(names, ["Crown off", "Orbit", "Voice"]);
        assert_eq!(rows[0].mention(), Some((MentionKind::Entity, "characters/dhritarashtra#crown-off".to_owned())));
        assert_eq!(studio_matches(&all, "characters/dhritarashtra#orb").len(), 1);
        assert_eq!(choices(&[], &[], None, "characters/dhritarashtra#", None), [Choice::Loading], "until otto answers");
        // Each row's icon by its media.
        let icons: Vec<&str> = all.iter().take(3).map(|m| row_icon(m.kind, &m.target, &m.detail)).collect();
        assert_eq!(icons, ["image", "video", "audio-lines"]);
    }

    #[test]
    fn a_reference_found_by_name_sits_under_its_entity() {
        let mut all = vec![
            found(MentionKind::Entity, "characters/dhritarashtra", "Dhritarashtra", "Character"),
            found(MentionKind::Entity, "props/crown", "Crown", "Prop"),
        ];
        all.extend(looks());
        let rows = studio_matches(&all, "crown");
        let got: Vec<(String, String)> =
            rows.iter().map(|c| if let Choice::Studio { target, name, .. } = c { (target.clone(), name.clone()) } else { panic!() }).collect();
        assert_eq!(
            got,
            [
                ("props/crown".to_owned(), "Crown".to_owned()),
                ("characters/dhritarashtra#crown-off".to_owned(), "Dhritarashtra · Crown off".to_owned())
            ]
        );
        // Its entity listed too: right under it.
        let rows = studio_matches(&all, "d");
        let targets: Vec<String> = rows.iter().filter_map(Choice::mention).map(|m| m.1).collect();
        let at = targets.iter().position(|t| t == "characters/dhritarashtra").unwrap();
        assert!(targets[at + 1].starts_with("characters/dhritarashtra#"), "{targets:?}");
    }

    #[test]
    fn a_reference_chip_reads_the_entity_then_the_reference() {
        let target = "characters/dhritarashtra#crown-off";
        // Before otto names them: the ids' last parts.
        assert_eq!(label(MentionKind::Entity, target, None), "dhritarashtra · crown-off");
        assert_eq!(ref_icon(""), "image");
        // Its entity's references listed (`characters/dhritarashtra#`): both names, and the icon of its media.
        let mut names = StudioNames::default();
        names.add(&looks());
        assert_eq!(label(MentionKind::Entity, target, Some(&names)), "Dhritarashtra · Crown off");
        assert_eq!(label(MentionKind::Entity, "characters/dhritarashtra", Some(&names)), "Dhritarashtra", "the entity named by its rows");
        assert_eq!(ref_icon(&names.details["characters/dhritarashtra#orbit"]), "video");
        assert_eq!(ref_icon(&names.details["characters/dhritarashtra#voice"]), "audio-lines");
        // An entity's own name, once known, wins over its rows'.
        let mut names = StudioNames::default();
        names.add(&[found(MentionKind::Entity, "characters/dhritarashtra", "King Dhritarashtra", "Character")]);
        names.add(&looks());
        assert_eq!(label(MentionKind::Entity, target, Some(&names)), "King Dhritarashtra · Crown off");
        // Sent and retried as the reference's mention.
        let text = "him @characters/dhritarashtra#crown-off";
        let parts = input(text, &[chip(4, MentionKind::Entity, target)]);
        assert_eq!(parts[1], InputPart::Mention { kind: MentionKind::Entity, target: target.into() });
    }

    #[test]
    fn canvas_and_skill_chips() {
        let canvas = chip(4, MentionKind::Canvas, "ep01");
        let node = chip(4, MentionKind::Canvas, "ep01#n_1");
        assert!(canvas.drills() && canvas.opens_on("#") && !canvas.opens_on("x") && !canvas.opens_on(" "));
        assert!(!node.drills() && !node.opens_on("#"));
        let folder = chip(0, MentionKind::File, "src/");
        assert!(folder.drills() && folder.opens_on("m") && !folder.opens_on(" "));
        // A skill is `/name`: its range, its text, sent and shown as a mention.
        let skill = chip(0, MentionKind::Skill, "action-choreography");
        assert_eq!((skill.range.clone(), skill.sigil(), skill.drills()), (0..20, '/', false));
        assert!(crate::assets::has_icon(icon(MentionKind::Skill, "x")));
        let text = "/action-choreography the quay fight";
        let parts = input(text, std::slice::from_ref(&skill));
        let mention = InputPart::Mention { kind: MentionKind::Skill, target: "action-choreography".into() };
        assert_eq!(parts, [mention, InputPart::Text { text: " the quay fight".into() }]);
        // `@name` isn't its text.
        assert_eq!(input("@action-choreography", std::slice::from_ref(&skill)), [InputPart::Text { text: "@action-choreography".into() }]);
        let mut item = Item::user_text("u", text);
        item.mentions = vec![otto_proto::Mention { kind: MentionKind::Skill, target: "action-choreography".into(), range: [0, 20], missing: false }];
        assert_eq!(retry_input(&item), parts);
        assert_eq!(item_chips(&item, &[Some(0)], 0, text.len()), [skill]);
    }
}
