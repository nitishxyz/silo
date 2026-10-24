//! The copied text across blocks and messages; what each row paints; overlays and offsets.

use gpui::{FontWeight, HighlightStyle, Hsla};

use super::{super::pane::edge_speed, *};

fn copy(src: &dyn Source, sel: &Selection) -> Option<String> {
    copy_text(src, sel)
}

#[test]
fn copying_across_paragraphs_lists_code_tables_and_bubbles() {
    let doc = sample();
    let code = doc.leaves(2)[4].text.clone();
    assert!(code.starts_with("fn x() {\n\tok\n}"), "verbatim, tabs kept: {code:?}");
    // Everything: blank lines between messages, newlines between blocks, tabs between a row's cells; tool rows skipped.
    let all = everything(&doc).expect("something to select");
    assert_eq!(
        copy(&doc, &all).as_deref(),
        Some(format!("Make a table\nplease\n\nTitle\nFirst para.\none\ntwo\n{code}\nA\tB\n1\t2\nquoted\n\nthanks").as_str())
    );
    // From inside a user bubble into the middle of a paragraph.
    let sel = drag(&doc, at("u1", USER, 7), at("a1", "a1-1", 5));
    assert_eq!(copy(&doc, &sel).as_deref(), Some("table\nplease\n\nTitle\nFirst"));
    // Within one leaf; backwards too.
    assert_eq!(copy(&doc, &drag(&doc, at("a1", "a1-1", 10), at("a1", "a1-1", 6))).as_deref(), Some("para"));
    // Part of a table: a cell, then the next row's first.
    assert_eq!(copy(&doc, &drag(&doc, at("a1", "a1-4-r0-c1", 0), at("a1", "a1-4-r1-c0", 1))).as_deref(), Some("B\n1"));
    // Part of a code block, verbatim.
    assert_eq!(copy(&doc, &drag(&doc, at("a1", "a1-3", 3), at("a1", "a1-3", 12))).as_deref(), Some("x() {\n\tok"));
    // A list's items into a quote.
    assert_eq!(
        copy(&doc, &drag(&doc, at("a1", "a1-2-1-0", 0), at("a1", "a1-5-q0", 3))).as_deref(),
        Some(format!("two\n{code}\nA\tB\n1\t2\nquo").as_str())
    );
    // Nothing selected: nothing to copy.
    assert_eq!(copy(&doc, &Selection::new(&doc, at("u2", USER, 2), Unit::Char)), None);
    assert_eq!(everything(&Doc(vec![tool("g")])), None);
}

#[test]
fn words_and_blocks_extend_by_their_unit() {
    let doc = sample();
    let word = Selection::new(&doc, at("a1", "a1-1", 7), Unit::Word);
    assert_eq!(copy(&doc, &word).as_deref(), Some("para"));
    let mut fwd = word.clone();
    fwd.head = at("a1", "a1-2-1-0", 1);
    assert_eq!(copy(&doc, &fwd).as_deref(), Some("para.\none\ntwo"));
    let mut back = word.clone();
    back.head = at("a1", "a1-1", 1);
    assert_eq!(copy(&doc, &back).as_deref(), Some("First para"));
    // Back inside the first word: just it.
    let mut inside = word;
    inside.head = at("a1", "a1-1", 8);
    assert_eq!(copy(&doc, &inside).as_deref(), Some("para"));
    // A triple click: the bubble's line; a paragraph whole; a code line.
    assert_eq!(copy(&doc, &Selection::new(&doc, at("u1", USER, 2), Unit::Block)).as_deref(), Some("Make a table"));
    assert_eq!(copy(&doc, &Selection::new(&doc, at("a1", "a1-1", 2), Unit::Block)).as_deref(), Some("First para."));
    assert_eq!(copy(&doc, &Selection::new(&doc, at("a1", "a1-3", 10), Unit::Block)).as_deref(), Some("\tok"));
}

#[test]
fn each_row_paints_its_part() {
    let doc = sample();
    let sel = drag(&doc, at("u1", USER, 7), at("a1", "a1-1", 5));
    let frame = sel.bounds(&doc).expect("resolves");
    assert_eq!(ranges(&doc, frame, 0), [(USER.to_owned(), 7..19)].into());
    assert!(ranges(&doc, frame, 1).is_empty());
    assert_eq!(ranges(&doc, frame, 2), [("a1-0".to_owned(), 0..5), ("a1-1".to_owned(), 0..5)].into());
    assert!(ranges(&doc, frame, 4).is_empty());
    // A streaming re-render changes nothing it holds: the same anchors, the same text.
    let mut longer = sample();
    longer.0[2] = md("a1", "# Title\n\nFirst *para*. And more now.\n\nNew block");
    assert_eq!(copy(&longer, &sel).as_deref(), Some("table\nplease\n\nTitle\nFirst"));
    // Its row gone (a session change, rows rebuilt): it no longer resolves.
    assert_eq!(sel.bounds(&Doc(vec![user("u9", "x")])), None);
}

#[test]
fn the_selection_overlays_other_highlights() {
    let bg = Hsla { h: 0.5, s: 0.5, l: 0.5, a: 0.6 };
    let bold = HighlightStyle { font_weight: Some(FontWeight::BOLD), ..HighlightStyle::default() };
    let with_bg = |h: HighlightStyle| HighlightStyle { background_color: Some(bg), ..h };
    assert_eq!(overlay(vec![(2..5, bold)], 4..8, bg), vec![(2..4, bold), (4..5, with_bg(bold)), (5..8, with_bg(HighlightStyle::default()))]);
    assert_eq!(overlay(vec![(2..5, bold)], 3..3, bg), vec![(2..5, bold)], "nothing selected");
    assert_eq!(overlay(Vec::new(), 0..3, bg), vec![(0..3, with_bg(HighlightStyle::default()))]);
}

#[test]
fn code_offsets_map_through_expanded_tabs() {
    use super::super::text::{to_shown, to_source};
    assert_eq!(to_shown("\tok", 1), 4);
    assert_eq!(to_shown("a\tb", 2), 4);
    assert_eq!(to_source("\tok", 4), 1);
    assert_eq!(to_source("\tok", 2), 0, "inside a tab: before it");
    assert_eq!(to_source("\tok", 6), 3);
    assert_eq!(to_source("plain", 3), 3);
    assert_eq!(view::line_starts("a\nbb\r\n\nc"), [0, 2, 6, 7]);
    assert_eq!(view::line_starts(""), Vec::<usize>::new());
}

#[test]
fn dragging_past_an_edge_scrolls_faster_further_out() {
    assert!(edge_speed(50.0, 0.0, 100.0).abs() < f32::EPSILON);
    assert!(edge_speed(-1.0, 0.0, 100.0) < 0.0);
    assert!(edge_speed(-60.0, 0.0, 100.0) < edge_speed(-10.0, 0.0, 100.0));
    assert!(edge_speed(101.0, 0.0, 100.0) > 0.0);
    assert!((edge_speed(1000.0, 0.0, 100.0) - 40.0).abs() < f32::EPSILON, "capped");
}
