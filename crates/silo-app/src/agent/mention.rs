//! `@` mentions (`docs/AGENT_BLOCK.md` §4g2, `docs/MEDIA.md` §3.0.1): the composer popup's query and
//! choices (agents, media models, then project paths from `files.complete`), chips sent as
//! `InputPart::Mention`s, and chips drawn inline over text (composer, bubbles).

use std::{ops::Range, time::Duration};

use gpui::{
    AnyElement, App, Bounds, HighlightStyle, Hsla, Pixels, Point, ScrollHandle, StyledText, TransformationMatrix, Window, canvas, div, fill, point,
    prelude::*, px, rgb,
};
use otto_proto::{FileMatch, InputPart, Item, MediaKind, MediaStatus, MentionKind};

use crate::theme;

/// Agents the popup lists at most (files: [`FILES`]).
pub const AGENTS: usize = 8;
pub const MODELS: usize = 6;
pub const FILES: u32 = 50;
/// Typing pauses this long before the popup asks otto for paths.
pub const DEBOUNCE: Duration = Duration::from_millis(60);

/// `@target` at `range` of a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    pub range: Range<usize>,
    pub kind: MentionKind,
    /// An agent name, a project path (folders end in `/`), or a media model handle.
    pub target: String,
    /// otto found no such agent or path (drawn struck through).
    pub missing: bool,
}

impl Chip {
    pub fn new(at: usize, kind: MentionKind, target: String) -> Self {
        Self { range: at..at + 1 + target.len(), kind, target, missing: false }
    }

    /// Its icon; a model's is its provider's logo, by the media status last loaded ([`ModelProviders`]).
    pub fn icon(&self, cx: &App) -> &'static str {
        match self.kind {
            MentionKind::Model => {
                super::media_setup::logo(cx.try_global::<ModelProviders>().and_then(|p| p.0.get(&self.target)).map_or("", String::as_str))
            }
            kind => icon(kind, &self.target),
        }
    }

    /// A folder: typing right after it opens it again as a query (drilling in).
    pub fn folder(&self) -> bool {
        self.kind == MentionKind::File && self.target.ends_with('/')
    }

    /// Its text is `@target` in `text`.
    fn fits(&self, text: &str) -> bool {
        text.get(self.range.clone()).is_some_and(|t| t.strip_prefix('@') == Some(self.target.as_str()))
    }
}

pub fn icon(kind: MentionKind, target: &str) -> &'static str {
    match kind {
        MentionKind::Agent => "bot",
        MentionKind::File if target.ends_with('/') => "folder",
        MentionKind::File => "file-text",
        MentionKind::Model => "image",
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
    /// The latest `files.complete` asked: older replies are dropped.
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

/// Media models matching `query` (fuzzy on handle, name and provider, best first), at most [`MODELS`];
/// none for an empty query or a path.
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
    hits.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
    hits.into_iter().take(MODELS).map(|(.., c)| c).collect()
}

/// The popup's rows: matching agents, media models, then otto's paths (already ranked, at most [`FILES`]), or
/// [`Choice::Loading`] while there are none yet.
pub fn choices(agents: &[(String, String)], media: Option<&MediaStatus>, query: &str, files: Option<&[FileMatch]>) -> Vec<Choice> {
    let agents = agent_matches(agents, query).into_iter().map(|(name, about)| Choice::Agent { name: name.clone(), about: about.clone() });
    let mut rows: Vec<Choice> = agents.collect();
    rows.extend(model_matches(media, query));
    match files {
        Some(files) => rows.extend(files.iter().take(FILES as usize).map(|f| Choice::File { path: f.path.clone(), dir: f.dir })),
        None => rows.push(Choice::Loading),
    }
    rows
}

/// Rows ↑/↓ and ⏎ reach (the loading row is last and isn't one).
pub fn pickable(choices: &[Choice]) -> usize {
    choices.iter().filter(|c| **c != Choice::Loading).count()
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
            (at + end - start <= len).then(|| Chip { range: at..at + end - start, kind: m.kind, target: m.target.clone(), missing: m.missing })
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
/// the UI font (smaller when it would overflow); struck through and faded when missing. A chip broken across lines
/// gets only its icon.
pub fn paint(chips: &[Chip], pos: impl Fn(usize) -> Option<Point<Pixels>>, line_height: Pixels, window: &mut Window, cx: &mut App) {
    let tint: Hsla = rgb(self::tint()).into();
    let size = window.text_style().font_size.to_pixels(window.rem_size());
    for c in chips {
        let Some((a, b)) = span(c.range.clone(), &pos) else { continue };
        let tile = (line_height - px(7.0)).min(px(15.0)).max(px(10.0));
        let top = a.y + (line_height - tile) / 2.0;
        let path = format!("icons/{}.svg", c.icon(cx));
        let glyph = tile * 0.68;
        let icon = |x: Pixels, window: &mut Window, cx: &mut App| {
            let at = point(x + (tile - glyph) / 2.0, top + (tile - glyph) / 2.0);
            let _ = window.paint_svg(Bounds::new(at, gpui::size(glyph, glyph)), path.clone().into(), None, TransformationMatrix::unit(), tint, cx);
        };
        if (b.y - a.y).abs() >= px(1.0) {
            icon(a.x, window, cx);
            continue;
        }
        let pill = Bounds::from_corners(point(a.x - px(1.0), a.y + px(1.5)), point(b.x + px(1.0), a.y + line_height - px(1.5)));
        window.paint_quad(fill(pill, Hsla { a: 0.14, ..tint }).corner_radii(px(5.0)));
        let x = a.x + px(2.0);
        window.paint_quad(fill(Bounds::new(point(x, top), gpui::size(tile, tile)), Hsla { a: 0.22, ..tint }).corner_radii(px(3.5)));
        icon(x, window, cx);
        let start = x + tile + px(GAP);
        let room = b.x - px(3.0) - start;
        let color = if c.missing { Hsla { a: 0.45, ..tint } } else { tint };
        let font = gpui::Font { family: theme::ui_font(), ..window.text_style().font() };
        let shape = |size: Pixels, window: &Window| {
            let run = gpui::TextRun { len: c.target.len(), font: font.clone(), color, background_color: None, underline: None, strikethrough: None };
            window.text_system().shape_line(c.target.clone().into(), size, &[run], None)
        };
        let (mut label_size, mut line) = (size * 0.94, shape(size * 0.94, window));
        while line.width > room && label_size > size * 0.72 {
            label_size *= 0.94;
            line = shape(label_size, window);
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

/// A chip's text in a bubble: laid out wider and clear, [`paint`] draws the badge in its place. Faded out, not a
/// clear colour: gpui blends a highlight's colour over the text's, so a transparent one changes nothing.
fn hidden() -> HighlightStyle {
    HighlightStyle { fade_out: Some(1.0), font_weight: Some(LAYOUT_WEIGHT), ..Default::default() }
}

/// `text` with its chips inline (a sent message's bubble), `selected` (a range, its colour) highlighted; and
/// its layout.
pub fn inline(text: &str, chips: &[Chip], selected: Option<(Range<usize>, Hsla)>) -> (AnyElement, gpui::TextLayout) {
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
    let styled = StyledText::new(text.to_owned()).with_highlights(highlights);
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
    fn a_chip_the_wrap_moved_to_the_next_line_is_whole_there() {
        // Laid out from x 100, 8 px a glyph, wrapped at 10: index 10 reads as the end of the first line.
        let pos =
            |i: usize| Some(if i <= 10 { point(px(100.0 + 8.0 * i as f32), px(0.0)) } else { point(px(100.0 + 8.0 * (i - 10) as f32), px(20.0)) });
        let at = |x: f32, y: f32| point(px(x), px(y));
        assert_eq!(span(10..20, &pos), Some((at(100.0, 20.0), at(180.0, 20.0))), "starts at the second line's left edge");
        assert_eq!(span(2..8, &pos), Some((at(116.0, 0.0), at(164.0, 0.0))));
        assert_eq!(span(8..14, &pos), Some((at(164.0, 0.0), at(132.0, 20.0))), "one really broken keeps both ends");
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
        let all = choices(&agents, None, "b", Some(&files));
        assert_eq!(all[0], Choice::Agent { name: "build".into(), about: String::new() });
        assert_eq!(all.last().unwrap().mention(), Some((MentionKind::File, "src/main.rs".to_owned())));
        assert_eq!(choices(&agents, None, "src/", Some(&files)).len(), 2);
        assert_eq!(pickable(&all), all.len());
        // otto answered with nothing: no rows (the popup says "No matches").
        assert!(choices(&agents, None, "zzz", Some(&[])).is_empty());
    }

    #[test]
    fn before_the_first_reply_agents_show_with_a_loading_row() {
        let agents = vec![("build".to_owned(), "Builds".to_owned())];
        let rows = choices(&agents, None, "", None);
        assert_eq!(rows, [Choice::Agent { name: "build".into(), about: "Builds".into() }, Choice::Loading]);
        assert_eq!(pickable(&rows), 1);
        assert_eq!(Choice::Loading.mention(), None);
        // No agent matches: still the loading row, never an empty popup.
        let rows = choices(&agents, None, "src/", None);
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
        assert_eq!(chips, [Chip { range: 4..10, kind: MentionKind::Agent, target: "build".into(), missing: true }]);
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
        let rows = choices(&agents, Some(&status), "kli", Some(&files));
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
        assert_eq!(choices(&agents, Some(&status), "", None).iter().filter(|c| matches!(c, Choice::Model { .. })).count(), 0);
        // Their providers, for the chips' logos.
        let providers = ModelProviders::of(&status);
        assert_eq!(super::super::media_setup::logo(&providers.0["gpt-image-2.6-flare"]), "provider-openai");
        assert_eq!(super::super::media_setup::logo(&providers.0["kling-3.0-pro"]), "provider-higgsfield");
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
        assert_eq!(chips, [Chip { range: 10..30, kind: MentionKind::Model, target: "gpt-image-2.6-flare".into(), missing: false }]);
    }
}
