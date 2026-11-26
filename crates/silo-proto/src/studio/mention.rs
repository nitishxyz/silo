//! Mentions in a card's prompt (docs/STUDIO.md §3, "Mentions"): `@characters/mara` names a world entity (its
//! sheet), `@characters/mara#crown-off` one of its named references and `@#n_3f2a9c` a node of the same canvas. A mention is a wire: the server wires its source into the card when
//! the token is written ([`Edge::mention`] marks the wires it makes), and a run expands it to text the model can
//! use (`Mara Venn (image 1)`). One parser for the server, otto and the app; anything that isn't a token (an
//! e-mail, a handle) or doesn't resolve is ordinary text.
//!
//! [`Edge::mention`]: super::Edge::mention

use std::ops::Range;

use super::{Canvas, EdgeRole, EntityInfo, NodeBody, RefInfo, Section, SourceMedia, entity_of};

/// What a token names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// A world entity by id (`characters/mara`): its sheet.
    Entity(String),
    /// One of a world entity's named references (`RefInfo`): `entity` its id (`characters/mara`), `id` the
    /// reference's (`crown-off`).
    Ref { entity: String, id: String },
    /// A node of the same canvas by id (`n_3f2a9c`).
    Node(String),
}

impl Target {
    /// The token as written: `@characters/mara`, `@characters/mara#crown-off`, `@#n_3f2a9c`.
    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Self::Entity(id) => format!("@{id}"),
            Self::Ref { entity, id } => format!("@{entity}#{id}"),
            Self::Node(id) => format!("@#{id}"),
        }
    }

    /// The world entity it names, itself or through one of its references.
    #[must_use]
    pub fn entity(&self) -> Option<&str> {
        match self {
            Self::Entity(id) | Self::Ref { entity: id, .. } => Some(id),
            Self::Node(_) => None,
        }
    }

    /// The entity and reference a `Ref` names, from `sections`.
    #[must_use]
    pub fn look<'a>(&self, sections: &'a [Section]) -> Option<(&'a EntityInfo, &'a RefInfo)> {
        let Self::Ref { entity, id } = self else { return None };
        let e = entity_of(sections, entity)?;
        Some((e, e.look(id)?))
    }
}

/// A token in a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Its bytes in the text, from the `@` to its last character.
    pub range: Range<usize>,
    pub target: Target,
}

/// Characters after which a token may start (besides whitespace and the start of the text): opening brackets
/// and quotes.
const OPENERS: &[char] = &['(', '[', '{', '<', '"', '\'', '“', '‘', '«', '`'];

/// The longest node id (as the server accepts them), and reference id.
const NODE_ID_MAX: usize = 64;

fn node_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Characters that end an entity token besides whitespace: punctuation an id never has (`.` only trails, and
/// is trimmed). Anything else may be in a folder name (letters of any script with their marks, `-`, `_`, `.`).
const STOP: &[char] =
    &[',', ';', ':', '!', '?', '\'', '"', ')', ']', '}', '>', '(', '[', '{', '<', '’', '”', '»', '…', '—', '–', '`', '*', '@', '#', '\\', '|'];

fn entity_char(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !STOP.contains(&c)
}

/// The tokens of `text`, in order. A token starts at the start of the text or after whitespace or an opening
/// bracket or quote, and runs over the characters an id can have, so it ends before whitespace and before
/// trailing sentence punctuation (`@characters/mara.`, `(@#n_3f2a9c),` and `@characters/mara's` hold one each).
/// `@<section>/<id>` (exactly one `/`, neither part empty or starting with `.`) names an entity; followed at once by
/// `#<ref id>` (letters, digits, `_`, `-`) one of its references, the whole a single token (`@characters/mara#`
/// with nothing after the `#` is the entity's token, the `#` text); `@#<id>` (the same characters) a node. Whether
/// it resolves is [`Canvas::mention_label`]'s business: a reference id the entity doesn't have leaves the whole
/// token as text, not the entity's part alone, so a misspelt reference never wires the sheet in its place.
#[must_use]
pub fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let starts = c == '@' && prev.is_none_or(|p| p.is_whitespace() || OPENERS.contains(&p));
        prev = Some(c);
        if !starts {
            continue;
        }
        let rest = &text[at + 1..];
        // A node or reference id after a `#`: its length, when there is one.
        let short_id = |id: &str| {
            let len = id.find(|c: char| !node_char(c)).unwrap_or(id.len());
            (len > 0 && len <= NODE_ID_MAX).then_some(len)
        };
        let found = if let Some(id) = rest.strip_prefix('#') {
            short_id(id).map(|len| (1 + 1 + len, Target::Node(id[..len].to_owned())))
        } else {
            let run = rest.find(|c: char| !entity_char(c)).unwrap_or(rest.len());
            let id = rest[..run].trim_end_matches(['.', '/']);
            let plain = |s: &str| !s.is_empty() && !s.starts_with('.');
            match id.split_once('/') {
                Some((section, name)) if plain(section) && plain(name) && !name.contains('/') => {
                    let look = (id.len() == run).then(|| rest[run..].strip_prefix('#')).flatten().and_then(|r| Some((short_id(r)?, r)));
                    Some(match look {
                        Some((len, r)) => (1 + id.len() + 1 + len, Target::Ref { entity: id.to_owned(), id: r[..len].to_owned() }),
                        None => (1 + id.len(), Target::Entity(id.to_owned())),
                    })
                }
                _ => None,
            }
        };
        if let Some((len, target)) = found {
            out.push(Token { range: at..at + len, target });
            // Skip the token: an `@` inside it starts nothing.
            while chars.peek().is_some_and(|(i, _)| *i < at + len) {
                prev = chars.next().map(|(_, c)| c);
            }
        }
    }
    out
}

/// The port a mention wires its source into, by what the source outputs: an image `reference`, a text `text`.
/// Video and audio: none (which of the model's fields it is for is the user's call).
#[must_use]
pub fn port_for(media: SourceMedia) -> Option<EdgeRole> {
    match media {
        SourceMedia::Image => Some(EdgeRole::Reference),
        SourceMedia::Text => Some(EdgeRole::Text),
        SourceMedia::Video | SourceMedia::Audio | SourceMedia::Unknown => None,
    }
}

/// What a reference token expands to when nothing is wired from it: the entity's name and the reference's
/// (`Dhritarashtra, Crown off`).
#[must_use]
pub fn look_label(entity: &EntityInfo, look: &RefInfo) -> String {
    format!("{}, {}", entity.name, look.name)
}

impl Canvas {
    /// The nodes a mention in card `card`'s prompt can be wired from: an entity's Entity nodes (in canvas
    /// order); a reference's Asset nodes, those showing its file (in canvas order; `sections` says which file);
    /// node `#id` when it has an output and isn't the card itself.
    #[must_use]
    pub fn mention_sources(&self, target: &Target, card: &str, sections: &[Section]) -> Vec<&str> {
        match target {
            Target::Entity(id) => {
                self.nodes.iter().filter(|n| matches!(&n.body, NodeBody::Entity { id: e } if e == id)).map(|n| n.id.as_str()).collect()
            }
            Target::Ref { .. } => {
                let Some((_, look)) = target.look(sections) else { return Vec::new() };
                self.nodes.iter().filter(|n| matches!(&n.body, NodeBody::Asset { path, .. } if *path == look.path)).map(|n| n.id.as_str()).collect()
            }
            Target::Node(id) => {
                self.node(id).filter(|n| n.id != card && self.source_media(&n.id).is_some()).map(|n| n.id.as_str()).into_iter().collect()
            }
        }
    }

    /// What a mention in card `card`'s prompt expands to when nothing is wired from it: the entity's name (an
    /// entity of `sections`); a reference's with the entity's ([`look_label`]: `Mara Venn, Crown off`); the node's
    /// label ([`Canvas::label_of`]: a card's or frame's title, a file's name, a note's first line). `None` when it
    /// doesn't resolve: no such entity, no such reference of it, no such node, a frame, the card itself; such a
    /// token is ordinary text.
    #[must_use]
    pub fn mention_label(&self, target: &Target, card: &str, sections: &[Section]) -> Option<String> {
        match target {
            Target::Entity(id) => entity_of(sections, id).map(|e| e.name.clone()),
            Target::Ref { .. } => target.look(sections).map(|(e, look)| look_label(e, look)),
            Target::Node(id) => (!self.mention_sources(target, card, sections).is_empty()).then(|| self.label_of(id, sections)),
        }
    }

    /// The tokens of card `card`'s prompt that resolve, once each in order, with what each expands to unwired
    /// ([`Self::mention_label`]): what a run needs to expand them.
    #[must_use]
    pub fn mentions_of(&self, card: &str, sections: &[Section]) -> Vec<(Target, String)> {
        let Some(NodeBody::Gen { prompt, .. }) = self.node(card).map(|n| &n.body) else { return Vec::new() };
        let mut out: Vec<(Target, String)> = Vec::new();
        for t in tokens(prompt) {
            if !out.iter().any(|(seen, _)| *seen == t.target)
                && let Some(label) = self.mention_label(&t.target, card, sections)
            {
                out.push((t.target, label));
            }
        }
        out
    }

    /// Whether node `from` is what `target` names: the node itself, an Entity node of the entity, an Asset node
    /// of the reference's file. A reference of an entity `sections` doesn't have (the app's optimistic edits pass
    /// none) names any Asset node of a file in the entity's folder: kept rather than guessed away.
    #[must_use]
    pub fn mention_names(&self, target: &Target, from: &str, sections: &[Section]) -> bool {
        match target {
            Target::Node(id) => id == from,
            Target::Entity(id) => matches!(self.node(from).map(|n| &n.body), Some(NodeBody::Entity { id: e }) if e == id),
            Target::Ref { entity, .. } => {
                let Some(NodeBody::Asset { path, .. }) = self.node(from).map(|n| &n.body) else { return false };
                match target.look(sections) {
                    Some((_, look)) => *path == look.path,
                    None => entity_of(sections, entity).is_none() && path.starts_with(&format!("world/{entity}/")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<(&str, Target)> {
        tokens(text).into_iter().map(|t| (&text[t.range], t.target)).collect()
    }

    fn entity(id: &str) -> Target {
        Target::Entity(id.into())
    }

    fn node(id: &str) -> Target {
        Target::Node(id.into())
    }

    #[test]
    fn tokens_start_at_boundaries() {
        assert_eq!(found("@characters/mara walks"), [("@characters/mara", entity("characters/mara"))]);
        assert_eq!(found("Then @#n_3f2a9c\tand\n@places/harbor"), [("@#n_3f2a9c", node("n_3f2a9c")), ("@places/harbor", entity("places/harbor"))]);
        assert_eq!(
            found("(@characters/mara) \"@#n_1\" [@props/bell] «@places/quay»"),
            [
                ("@characters/mara", entity("characters/mara")),
                ("@#n_1", node("n_1")),
                ("@props/bell", entity("props/bell")),
                ("@places/quay", entity("places/quay")),
            ]
        );
        assert!(found("x@characters/mara").is_empty(), "inside a word");
        assert!(found("@@characters/mara").is_empty());
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_a_token() {
        assert_eq!(found("Look at @characters/mara."), [("@characters/mara", entity("characters/mara"))]);
        assert_eq!(
            found("@characters/mara, @#n_2; @#n_3: @#n_4! @#n_5?"),
            [
                ("@characters/mara", entity("characters/mara")),
                ("@#n_2", node("n_2")),
                ("@#n_3", node("n_3")),
                ("@#n_4", node("n_4")),
                ("@#n_5", node("n_5")),
            ]
        );
        assert_eq!(found("@characters/mara's coat"), [("@characters/mara", entity("characters/mara"))]);
        assert_eq!(found("@characters/mara..."), [("@characters/mara", entity("characters/mara"))]);
        assert_eq!(
            found("@characters/old-bell_2 @characters/mara.venn."),
            [("@characters/old-bell_2", entity("characters/old-bell_2")), ("@characters/mara.venn", entity("characters/mara.venn")),]
        );
    }

    #[test]
    fn emails_handles_and_other_shapes_are_text() {
        assert!(found("mail mara@harbor.com or @mara or @ or @# or @/x or @x/ or @a/b/c or @./x or @a/.b").is_empty());
        assert!(found(&format!("@#{}", "a".repeat(65))).is_empty(), "too long for a node id");
        assert_eq!(found("@#n-1.png"), [("@#n-1", node("n-1"))]);
    }

    #[test]
    fn ranges_are_bytes_and_unicode_ids_work() {
        let text = "Ārjuna — @characters/arjuna and @characters/कृष्ण.";
        let t = tokens(text);
        assert_eq!(t.len(), 2);
        assert_eq!(&text[t[0].range.clone()], "@characters/arjuna");
        assert_eq!(t[1].target, entity("characters/कृष्ण"));
        assert_eq!(t[1].range.end, text.len() - 1);
        assert_eq!(entity("characters/mara").token(), "@characters/mara");
        assert_eq!(node("n_1").token(), "@#n_1");
    }

    fn look(entity: &str, id: &str) -> Target {
        Target::Ref { entity: entity.into(), id: id.into() }
    }

    #[test]
    fn a_reference_token_is_the_entity_and_its_reference_id() {
        assert_eq!(
            found("@characters/dhritarashtra#crown-off sits; @characters/dhritarashtra beside"),
            [
                ("@characters/dhritarashtra#crown-off", look("characters/dhritarashtra", "crown-off")),
                ("@characters/dhritarashtra", entity("characters/dhritarashtra")),
            ]
        );
        // Ends where an id does: before punctuation, a dot, a second `#`, whitespace.
        assert_eq!(found("(@places/quay#reverse_2)."), [("@places/quay#reverse_2", look("places/quay", "reverse_2"))]);
        assert_eq!(found("@places/quay#dawn.png"), [("@places/quay#dawn", look("places/quay", "dawn"))]);
        assert_eq!(found("@places/quay#a#b"), [("@places/quay#a", look("places/quay", "a"))]);
        assert_eq!(found("@props/bell#side's"), [("@props/bell#side", look("props/bell", "side"))]);
        // Nothing after the `#`, or something no id has: the entity's token, the rest text.
        assert_eq!(
            found("@characters/mara# and @characters/mara#!"),
            [("@characters/mara", entity("characters/mara")), ("@characters/mara", entity("characters/mara"))]
        );
        // A trailing dot before the `#` ends the entity's token there.
        assert_eq!(found("@characters/mara.#face"), [("@characters/mara", entity("characters/mara"))]);
        assert!(found(&format!("@characters/mara#{}", "a".repeat(65))).iter().all(|(_, t)| *t == entity("characters/mara")), "too long");
        assert_eq!(look("characters/mara", "crown-off").token(), "@characters/mara#crown-off");
        assert_eq!(look("characters/mara", "x").entity(), Some("characters/mara"));
        assert_eq!(node("n").entity(), None);
        // The token's bytes, with a non-ASCII entity.
        let text = "— @characters/कृष्ण#flute, then";
        let t = tokens(text);
        assert_eq!(&text[t[0].range.clone()], "@characters/कृष्ण#flute");
    }

    #[test]
    fn a_reference_resolves_through_the_world() {
        use crate::studio::{EntityInfo, Node, RefInfo, SectionKind, StudioMedia};
        let look_of = |id: &str, path: &str, name: &str| RefInfo {
            id: id.into(),
            path: path.into(),
            name: name.into(),
            defines: None,
            media: StudioMedia::of_path(path),
        };
        let king = EntityInfo {
            id: "characters/dhritarashtra".into(),
            name: "Dhritarashtra".into(),
            looks: vec![look_of("crown-off", "world/characters/dhritarashtra/refs/crown-off.png", "Crown off")],
            ..EntityInfo::default()
        };
        let sections =
            [Section { id: "characters".into(), title: "Characters".into(), kind: SectionKind::Entities, entities: vec![king], refs: Vec::new() }];
        let asset = |id: &str, path: &str| Node {
            id: id.into(),
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            z: None,
            parent: None,
            body: NodeBody::Asset {
                path: path.into(),
                media: crate::studio::StudioMedia::Image,
                width: None,
                height: None,
                seconds: None,
                prompt: None,
            },
        };
        let mut canvas: Canvas = serde_json::from_value(serde_json::json!({"name": "c", "title": "C"})).unwrap();
        canvas.nodes = vec![
            asset("a", "world/characters/dhritarashtra/refs/crown-off.png"),
            asset("b", "world/characters/dhritarashtra/refs/other.png"),
            asset("c", "world/characters/dhritarashtra/refs/crown-off.png"),
        ];
        let crown = look("characters/dhritarashtra", "crown-off");
        assert_eq!(canvas.mention_label(&crown, "k", &sections).as_deref(), Some("Dhritarashtra, Crown off"));
        assert_eq!(canvas.mention_sources(&crown, "k", &sections), ["a", "c"]);
        assert!(canvas.mention_names(&crown, "a", &sections) && !canvas.mention_names(&crown, "b", &sections));
        // Not a reference it has: plain text, naming nothing.
        let nope = look("characters/dhritarashtra", "nope");
        assert_eq!(canvas.mention_label(&nope, "k", &sections), None);
        assert!(canvas.mention_sources(&nope, "k", &sections).is_empty());
        assert!(!canvas.mention_names(&nope, "a", &sections));
        // Without the world (the app's optimistic edit): any Asset node of the entity's folder, kept.
        assert!(canvas.mention_names(&crown, "b", &[]));
        assert!(!canvas.mention_names(&look("characters/other", "x"), "b", &[]));
    }

    #[test]
    fn ports_by_media() {
        assert_eq!(port_for(SourceMedia::Image), Some(EdgeRole::Reference));
        assert_eq!(port_for(SourceMedia::Text), Some(EdgeRole::Text));
        assert_eq!(port_for(SourceMedia::Video), None);
        assert_eq!(port_for(SourceMedia::Audio), None);
    }
}
