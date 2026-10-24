use super::{
    text::{line_at, unit_at, word_at},
    *,
};
use crate::agent::markdown::Doc as Markdown;

mod copy;
mod drag;

/// A transcript of rows (key, leaves).
pub(super) struct Doc(pub Vec<(String, Vec<Leaf>)>);

impl Source for Doc {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn key(&self, row: usize) -> &str {
        &self.0[row].0
    }
    fn leaves(&self, row: usize) -> Vec<Leaf> {
        self.0[row].1.clone()
    }
}

pub(super) fn md(key: &str, text: &str) -> (String, Vec<Leaf>) {
    let mut doc = Markdown::default();
    doc.update(text, false);
    (key.to_owned(), block_leaves(key, doc.blocks()))
}

fn user(key: &str, text: &str) -> (String, Vec<Leaf>) {
    (key.to_owned(), user_leaf(text).into_iter().collect())
}

/// A tool row: nothing selectable.
fn tool(key: &str) -> (String, Vec<Leaf>) {
    (key.to_owned(), Vec::new())
}

fn at(row: &str, leaf: &str, offset: usize) -> Anchor {
    Anchor { row: row.to_owned(), leaf: leaf.to_owned(), offset }
}

fn drag(src: &dyn Source, from: Anchor, to: Anchor) -> Selection {
    let mut sel = Selection::new(src, from, Unit::Char);
    sel.head = to;
    sel
}

fn sample() -> Doc {
    Doc(vec![
        user("u1", "Make a table\nplease"),
        tool("g1"),
        md("a1", "# Title\n\nFirst *para*.\n\n- one\n- two\n\n```rust\nfn x() {\n\tok\n}\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n> quoted"),
        tool("g2"),
        user("u2", "thanks"),
    ])
}

#[test]
fn leaves_follow_the_drawn_ids_in_reading_order() {
    let doc = sample();
    let ids: Vec<String> = doc.leaves(2).into_iter().map(|l| l.id).collect();
    assert_eq!(ids, ["a1-0", "a1-1", "a1-2-0-0", "a1-2-1-0", "a1-3", "a1-4-r0-c0", "a1-4-r0-c1", "a1-4-r1-c0", "a1-4-r1-c1", "a1-5-q0"]);
    assert_eq!(doc.leaves(0)[0].id, USER);
    assert!(doc.leaves(1).is_empty(), "tool rows have nothing to select");
    assert!(user_leaf("").is_none(), "a message of only files");
}

#[test]
fn anchors_order_as_the_document_reads() {
    let doc = sample();
    let pos = |a: Anchor| find(&doc, &a).map(|(p, _)| p);
    let p = |row, leaf, offset| Some(Pos { row, leaf, offset });
    assert_eq!(pos(at("a1", "a1-3", 3)), p(2, 4, 3));
    assert!(pos(at("u1", USER, 9)) < pos(at("a1", "a1-0", 0)));
    assert!(pos(at("a1", "a1-0", 4)) < pos(at("a1", "a1-1", 0)), "a later leaf of a row comes after any offset of an earlier one");
    assert!(pos(at("a1", "a1-1", 2)) < pos(at("a1", "a1-1", 3)));
    // Offsets are kept inside the leaf and on a char boundary; unknown rows and leaves don't resolve.
    assert_eq!(pos(at("u2", USER, 99)), p(4, 0, 6));
    assert_eq!(pos(at("a1", "nope", 0)), None);
    assert_eq!(pos(at("gone", USER, 0)), None);
    let doc = Doc(vec![user("u", "é")]);
    assert_eq!(find(&doc, &at("u", USER, 1)).map(|(p, _)| p.offset), Some(0));
    // A drag backwards reads the same as forwards.
    let doc = sample();
    let fwd = drag(&doc, at("u1", USER, 5), at("a1", "a1-1", 5));
    let back = drag(&doc, at("a1", "a1-1", 5), at("u1", USER, 5));
    assert_eq!(fwd.bounds(&doc), back.bounds(&doc));
    assert_eq!(fwd.bounds(&doc), Some((Pos { row: 0, leaf: 0, offset: 5 }, Pos { row: 2, leaf: 1, offset: 5 })));
}

#[test]
fn words_are_unicode_aware() {
    fn w(s: &str, at: usize) -> &str {
        &s[word_at(s, at)]
    }
    assert_eq!(w("hello wörld!", 0), "hello");
    assert_eq!(w("hello wörld!", 7), "wörld");
    assert_eq!(w("hello wörld!", 11), "wörld");
    assert_eq!(w("hello wörld!", 12), "!");
    assert_eq!(w("hello wörld!", 13), "!", "at the end, the last character's word");
    assert_eq!(w("hello world", 5), "hello", "at a word's end, that word");
    assert_eq!(w("a   b", 2), "   ");
    assert_eq!(w("snake_case_name.rs", 3), "snake_case_name");
    // A combining accent is part of its letter's word.
    let s = "cafe\u{301} au lait";
    assert_eq!(w(s, 1), "cafe\u{301}");
    assert_eq!(w("日本語 テキスト", 3), "日本語");
    assert_eq!(w("Привет, мир", 2), "Привет");
    // An emoji sequence (ZWJ, skin tone) is one character's worth.
    let family = "a 👨\u{200D}👩\u{200D}👧 b";
    assert_eq!(w(family, 2), "👨\u{200D}👩\u{200D}👧");
    assert_eq!(w("ok 👍🏽 yes", 3), "👍🏽");
    assert_eq!(word_at("", 0), 0..0);
}

#[test]
fn blocks_are_paragraphs_lines_or_cells() {
    let leaf = |kind, text: &str| Leaf { id: "x".into(), text: text.into(), kind };
    assert_eq!(line_at("one\ntwo\nthree", 5), 4..7);
    assert_eq!(line_at("one\ntwo", 3), 0..3);
    assert_eq!(line_at("one\n\nthree", 4), 4..4);
    assert_eq!(unit_at(&leaf(Kind::Lines, "hi\nthere"), 4, Unit::Block), 3..8, "a user bubble's line");
    assert_eq!(unit_at(&leaf(Kind::Code, "a\nbb\n"), 2, Unit::Block), 2..4, "a code line");
    assert_eq!(unit_at(&leaf(Kind::Text, "a para\nwith a break"), 2, Unit::Block), 0..19, "a paragraph, whole");
    assert_eq!(unit_at(&leaf(Kind::Cell { table: "t".into(), row: 0 }, "cell"), 2, Unit::Block), 0..4);
    assert_eq!(unit_at(&leaf(Kind::Text, "abc"), 2, Unit::Char), 2..2);
}
