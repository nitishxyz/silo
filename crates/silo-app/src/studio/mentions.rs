//! `@` mentions in a card's prompt (docs/STUDIO.md §3 "Mentions", §4.2): what the prompt editors' popup offers
//! (the world's entities, then this canvas's nodes a card can mention) and the chips a prompt's tokens draw as,
//! from the board's canvas and the store's world, nothing asked of the server. The popup and the chips are the
//! agent composer's ([`crate::agent::mention`]); tokens and what they resolve to are `silo_proto`'s
//! ([`mention::tokens`], [`Canvas::mention_label`]).

use otto_proto::MentionKind;
use silo_proto::studio::{
    Canvas, NodeBody, Section, StudioMedia,
    mention::{self, Target},
};

use super::{model, tiles};
use crate::agent::mention::{Chip, Choice, Face, ref_label, row_icon};

/// Entities the popup lists at most (the best first for a typed query, the world's order for a bare `@`).
pub const ENTITIES: usize = 8;
/// Named references a typed query finds by name, at most (each under its entity).
pub const LOOKS: usize = 6;
/// Nodes the popup lists at most (it scrolls).
pub const NODES: usize = 40;

/// Entity `e`'s named references as popup rows (`characters/mara#coat`, `Coat`, `Look · Mara Venn`); `whole`: each
/// named with its entity (`Mara Venn · Coat`, detail `Look`), for rows found by a query among other entities'.
fn look_rows(e: &silo_proto::studio::EntityInfo, whole: bool) -> Vec<Choice> {
    let entity = model::entity_name(e);
    model::entity_looks(e)
        .iter()
        .map(|l| {
            let (name, _) = model::ref_caption(l);
            let kind = model::RefMedia::of_look(l).kind_label();
            let target = format!("{}#{}", e.id, l.id);
            if whole {
                Choice::Studio { kind: MentionKind::Entity, target, name: ref_label(&entity, &name), detail: kind.to_owned() }
            } else {
                Choice::Studio { kind: MentionKind::Entity, target, name, detail: format!("{kind} \u{b7} {entity}") }
            }
        })
        .collect()
}

/// A drill-in `entity#text` query of an entity of `sections`: the entity and the text.
fn looks_query<'a>(sections: &'a [Section], query: &'a str) -> Option<(&'a silo_proto::studio::EntityInfo, &'a str)> {
    let (head, text) = query.split_once('#')?;
    Some((model::find_entity(sections, head)?, text))
}

/// The popup's rows for the `@query` in a prompt of `card` (`None`: the canvas prompt, no card yet): the world's
/// entities by name with their kind (`Character`), then the nodes of `canvas` the card can mention (others cards
/// by title, documents and other files by name, notes by first line; not frames, the card itself, empty notes or
/// Entity nodes: the entity is listed above), with their kind and frame (`Image card · Shot 2`). A bare `@`: in
/// that order; a typed query: each group best match first (fuzzy on the name and the id), and an entity's named
/// references whose name matches under it (`crown`: `Dhritarashtra · Crown off`); `#text`: nodes only;
/// `<entity>#text` (typed right after an entity's chip): that entity's references, in order or best first.
#[must_use]
pub fn choices(canvas: &Canvas, card: Option<&str>, sections: &[Section], query: &str) -> Vec<Choice> {
    if let Some((e, text)) = looks_query(sections, query) {
        let rows = look_rows(e, false);
        return if text.is_empty() { rows } else { best_by_name(rows, text, false, NODES) };
    }
    let (query, only_nodes) = match query.strip_prefix('#') {
        Some(rest) => (rest, true),
        None => (query, false),
    };
    let entities: Vec<Choice> = if only_nodes {
        Vec::new()
    } else {
        let all = sections.iter().flat_map(|s| s.entities.iter().map(move |e| (s, e))).map(|(s, e)| Choice::Studio {
            kind: MentionKind::Entity,
            target: e.id.clone(),
            name: if e.name.trim().is_empty() { e.id.clone() } else { e.name.clone() },
            detail: capitalized(&model::singular(&s.title)),
        });
        let found = best(all.collect(), query, ENTITIES);
        if query.trim().is_empty() {
            found
        } else {
            // References found by their name only (not every look of an entity found by its own name).
            let looks: Vec<Choice> = sections.iter().flat_map(|s| &s.entities).flat_map(|e| look_rows(e, true)).collect();
            let looks = best_by_name(looks, query, true, LOOKS);
            under(found, looks)
        }
    };
    let mut nodes = offered(canvas, card.unwrap_or_default(), sections);
    nodes.sort_by_key(|(group, at, _)| (*group, *at));
    let nodes = best(nodes.into_iter().map(|(.., c)| c).collect(), query, NODES);
    entities.into_iter().chain(nodes).collect()
}

/// `rows` matching `query` (fuzzy on the name and the target without its `#`), best first, else in order; at most
/// `limit`.
fn best(rows: Vec<Choice>, query: &str, limit: usize) -> Vec<Choice> {
    let mut hits: Vec<(i32, usize, Choice)> = rows
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let Choice::Studio { target, name, .. } = &c else { return None };
            let score =
                [name.as_str(), target.trim_start_matches('#')].into_iter().filter_map(|t| crate::commands::fuzzy(query, t)).map(|(s, _)| s).max()?;
            Some((score, i, c))
        })
        .collect();
    hits.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
    hits.into_iter().take(limit).map(|(.., c)| c).collect()
}

/// References' `rows` matching `query` by their own name (`Crown off` of `Dhritarashtra · Crown off`) or id
/// (`crown-off`), best first; at most `limit`. `whole`: the query as typed must be in it (rows found among the
/// entities', where a scattered match would be noise), else a scattered match does.
fn best_by_name(rows: Vec<Choice>, query: &str, whole: bool, limit: usize) -> Vec<Choice> {
    let needle = query.trim().to_lowercase();
    let mut hits: Vec<(i32, usize, Choice)> = rows
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let Choice::Studio { name, target, .. } = &c else { return None };
            let own = name.rsplit(" \u{b7} ").next().unwrap_or(name);
            let id = target.rsplit('#').next().unwrap_or_default();
            if whole && ![own, id].iter().any(|t| t.to_lowercase().contains(&needle)) {
                return None;
            }
            let score = [own, id].into_iter().filter_map(|t| crate::commands::fuzzy(query, t)).map(|(s, _)| s).max()?;
            Some((score, i, c))
        })
        .collect();
    hits.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
    hits.into_iter().take(limit).map(|(.., c)| c).collect()
}

/// `looks` (references' rows) each right under its entity's row in `entities`, else after them.
fn under(entities: Vec<Choice>, looks: Vec<Choice>) -> Vec<Choice> {
    let target = |c: &Choice| match c {
        Choice::Studio { target, .. } => target.split('#').next().unwrap_or_default().to_owned(),
        _ => String::new(),
    };
    let mut out = entities;
    for l in looks {
        let owner = target(&l);
        match out.iter().rposition(|c| target(c) == owner) {
            Some(at) => out.insert(at + 1, l),
            None => out.push(l),
        }
    }
    out
}

/// The chips of an editor's text with the one being drilled into left out: a whole entity's whose text is followed
/// at once by `#` up to the caret (`@characters/mara#cr|`), so the `@…` is a query again for its references.
#[must_use]
pub fn drilled(text: &str, caret: usize, chips: Vec<Chip>) -> Vec<Chip> {
    chips
        .into_iter()
        .filter(|c| {
            let after = text.get(c.range.end..caret.max(c.range.end)).unwrap_or_default();
            !(c.entity() && caret > c.range.end && after.starts_with('#') && !after.contains(char::is_whitespace))
        })
        .collect()
}

/// The nodes of `canvas` a prompt of `card` can mention, as rows with their group (cards, notes, documents, other
/// files) and canvas order.
fn offered(canvas: &Canvas, card: &str, sections: &[Section]) -> Vec<(u8, usize, Choice)> {
    canvas
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(at, n)| {
            let (group, kind) = match &n.body {
                NodeBody::Gen { makes, .. } => (0, format!("{} card", makes.label())),
                NodeBody::Note { text } if !text.trim().is_empty() => (1, "Note".to_owned()),
                NodeBody::Asset { media: StudioMedia::Text, .. } => (2, "Document".to_owned()),
                NodeBody::Asset { media, .. } => (3, media_kind(*media).to_owned()),
                _ => return None,
            };
            let name = canvas.mention_label(&Target::Node(n.id.clone()), card, sections)?;
            let frame = n.parent.as_deref().and_then(|p| canvas.node(p)).and_then(|f| match &f.body {
                NodeBody::Frame { title, .. } if !title.trim().is_empty() => Some(title.trim().to_owned()),
                _ => None,
            });
            let detail = frame.map_or_else(|| kind.clone(), |f| format!("{kind} \u{b7} {f}"));
            Some((group, at, Choice::Studio { kind: MentionKind::Canvas, target: format!("#{}", n.id), name, detail }))
        })
        .collect()
}

fn media_kind(media: StudioMedia) -> &'static str {
    match media {
        StudioMedia::Image => "Image",
        StudioMedia::Video => "Video",
        StudioMedia::Audio => "Audio",
        StudioMedia::Text => "Document",
        StudioMedia::Unknown => "File",
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Whether a source of `target` is wired into `card` (at any port), as the server counts it wired.
#[must_use]
pub fn wired(canvas: &Canvas, card: &str, target: &Target, sections: &[Section]) -> bool {
    canvas.mention_sources(target, card, sections).iter().any(|s| canvas.edges.iter().any(|e| e.from == *s && e.to == card && e.role.is_port()))
}

/// What a quiet chip's tooltip says.
#[must_use]
pub fn unwired_tip(label: &str) -> String {
    format!("{label} is mentioned but not wired into this card")
}

/// The chips of `text`, a prompt of `card` (`None`: the canvas prompt): one per token that resolves
/// ([`Canvas::mention_label`]), reading the entity's name, a named reference's entity and name (`Dhritarashtra ·
/// Crown off`, the icon of its media) or the node's label with its kind's icon; the rest is plain text. Tokens of
/// `settled` (the prompt as saved; `None`: none) whose source has no wire into the card are quiet ([`wired`]):
/// mentioned, not wired. The chip's text is the token exactly.
#[must_use]
pub fn chips(text: &str, canvas: &Canvas, card: Option<&str>, sections: &[Section], settled: Option<&str>) -> Vec<Chip> {
    let settled: Vec<Target> = settled.map(mention::tokens).unwrap_or_default().into_iter().map(|t| t.target).collect();
    mention::tokens(text)
        .into_iter()
        .filter_map(|t| {
            let label = match t.target.look(sections) {
                Some((e, l)) => ref_label(&model::entity_name(e), &model::ref_caption(l).0),
                None => canvas.mention_label(&t.target, card.unwrap_or_default(), sections)?,
            };
            let (kind, target, icon) = match &t.target {
                Target::Entity(id) => (MentionKind::Entity, id.clone(), row_icon(MentionKind::Entity, id, "")),
                Target::Ref { entity, id } => {
                    let icon = t.target.look(sections).map_or("image", |(_, l)| model::RefMedia::of_look(l).icon());
                    (MentionKind::Entity, format!("{entity}#{id}"), icon)
                }
                Target::Node(id) => (MentionKind::Canvas, format!("#{id}"), node_icon(canvas, id)),
            };
            let quiet = card.is_some_and(|c| settled.contains(&t.target) && !wired(canvas, c, &t.target, sections));
            Some(Chip { range: t.range, kind, target, missing: false, face: Some(Face { label, icon, quiet }) })
        })
        .collect()
}

/// A node chip's icon: its kind's, as its popup row shows it.
fn node_icon(canvas: &Canvas, id: &str) -> &'static str {
    match canvas.node(id).map(|n| &n.body) {
        Some(NodeBody::Gen { makes, .. }) => tiles::kind_glyph(*makes),
        Some(NodeBody::Note { .. }) => "sticky-note",
        Some(NodeBody::Asset { media, .. }) => row_icon(MentionKind::Canvas, "#", media_kind(*media)),
        Some(NodeBody::Entity { id }) => row_icon(MentionKind::Entity, id, ""),
        _ => row_icon(MentionKind::Canvas, "#", ""),
    }
}

#[cfg(test)]
mod tests;
