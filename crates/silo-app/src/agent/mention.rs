//! `@` mentions (`docs/AGENT_BLOCK.md` §4c): the composer popup's query and
//! choices (agents, then project paths from `files.complete`), chips sent as
//! `InputPart::Mention`s, and chips drawn inline over text (composer, bubbles).

use std::{ops::Range, time::Duration};

use gpui::{
    AnyElement, App, Bounds, HighlightStyle, Hsla, Pixels, Point, ScrollHandle, StrikethroughStyle, StyledText, TransformationMatrix, Window, canvas,
    div, fill, point, prelude::*, px, rgb, size,
};
use otto_proto::{FileMatch, InputPart, Item, MentionKind};

use super::view::ink;
use crate::theme;

/// Agents the popup lists at most (files: [`FILES`]).
pub const AGENTS: usize = 8;
pub const FILES: u32 = 50;
/// Typing pauses this long before the popup asks otto for paths.
pub const DEBOUNCE: Duration = Duration::from_millis(60);

/// `@target` at `range` of a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    pub range: Range<usize>,
    pub kind: MentionKind,
    /// An agent name, or a project path (folders end in `/`).
    pub target: String,
    /// otto found no such agent or path (drawn struck through).
    pub missing: bool,
}

impl Chip {
    pub fn new(at: usize, kind: MentionKind, target: String) -> Self {
        Self { range: at..at + 1 + target.len(), kind, target, missing: false }
    }

    pub fn icon(&self) -> &'static str {
        icon(self.kind, &self.target)
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

/// The popup's rows: matching agents, then otto's paths (already ranked, at most [`FILES`]), or
/// [`Choice::Loading`] while there are none yet.
pub fn choices(agents: &[(String, String)], query: &str, files: Option<&[FileMatch]>) -> Vec<Choice> {
    let agents = agent_matches(agents, query).into_iter().map(|(name, about)| Choice::Agent { name: name.clone(), about: about.clone() });
    let mut rows: Vec<Choice> = agents.collect();
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
pub fn paint(chips: &[Chip], pos: impl Fn(usize) -> Option<Point<Pixels>>, line_height: Pixels, window: &mut Window, cx: &App) {
    let back: Hsla = rgb(ink(0.08)).into();
    let tint: Hsla = rgb(theme::muted()).into();
    for c in chips {
        let (Some(a), Some(b), Some(glyph)) = (pos(c.range.start), pos(c.range.end), pos(c.range.start + 1)) else { continue };
        if (b.y - a.y).abs() < px(1.0) {
            let bounds = Bounds::from_corners(point(a.x - px(3.0), a.y + px(1.0)), point(b.x + px(3.0), a.y + line_height - px(1.0)));
            window.paint_quad(fill(bounds, back).corner_radii(px(6.0)));
        }
        let slot = if (glyph.y - a.y).abs() < px(1.0) { glyph.x - a.x } else { px(12.0) };
        let side = px(12.0).min(line_height - px(4.0));
        let origin = point(a.x + (slot - side) / 2.0, a.y + (line_height - side) / 2.0);
        let path = format!("icons/{}.svg", c.icon());
        let _ = window.paint_svg(Bounds::new(origin, size(side, side)), path.into(), None, TransformationMatrix::unit(), tint, cx);
    }
}

/// `text` with its chips inline (a sent message's bubble), `selected` (a range, its colour) highlighted; and
/// its layout.
pub fn inline(text: &str, chips: &[Chip], selected: Option<(Range<usize>, Hsla)>) -> (AnyElement, gpui::TextLayout) {
    let mut chips: Vec<Chip> = chips.iter().filter(|c| c.fits(text)).cloned().collect();
    chips.sort_by_key(|c| c.range.start);
    chips.dedup_by(|b, a| b.range.start < a.range.end);
    let mut highlights = Vec::new();
    for c in &chips {
        highlights.push((c.range.start..c.range.start + 1, HighlightStyle { fade_out: Some(1.0), ..Default::default() }));
        let strike = c.missing.then_some(StrikethroughStyle { thickness: px(1.0), color: None });
        highlights.push((
            c.range.start + 1..c.range.end,
            HighlightStyle { strikethrough: strike, fade_out: c.missing.then_some(0.4), ..Default::default() },
        ));
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
        let all = choices(&agents, "b", Some(&files));
        assert_eq!(all[0], Choice::Agent { name: "build".into(), about: String::new() });
        assert_eq!(all.last().unwrap().mention(), Some((MentionKind::File, "src/main.rs".to_owned())));
        assert_eq!(choices(&agents, "src/", Some(&files)).len(), 2);
        assert_eq!(pickable(&all), all.len());
        // otto answered with nothing: no rows (the popup says "No matches").
        assert!(choices(&agents, "zzz", Some(&[])).is_empty());
    }

    #[test]
    fn before_the_first_reply_agents_show_with_a_loading_row() {
        let agents = vec![("build".to_owned(), "Builds".to_owned())];
        let rows = choices(&agents, "", None);
        assert_eq!(rows, [Choice::Agent { name: "build".into(), about: "Builds".into() }, Choice::Loading]);
        assert_eq!(pickable(&rows), 1);
        assert_eq!(Choice::Loading.mention(), None);
        // No agent matches: still the loading row, never an empty popup.
        let rows = choices(&agents, "src/", None);
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
        assert!(kept[0].folder() && kept[0].icon() == "folder");
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
}
