use std::time::{Duration, Instant};

use super::*;

fn doc(text: &str) -> Doc {
    Doc::new(text.to_owned())
}

fn at(line: usize, col: usize) -> Pos {
    Pos { line, col }
}

fn sel(a: Pos, b: Pos) -> Selection {
    Selection { anchor: a, head: b, unit: Unit::Char }
}

/// Apply `edit` to `text` the plain way.
fn applied(text: &str, edit: &Edit) -> String {
    let mut out = text.to_owned();
    out.replace_range(edit.range.clone(), &edit.text);
    out
}

#[test]
fn replace_keeps_the_line_table_as_a_fresh_read_would() {
    let cases: &[(&str, std::ops::Range<usize>, &str)] = &[
        ("abc\ndef\nghi", 1..2, "XY"),
        ("abc\ndef\nghi", 2..6, ""),
        ("abc\ndef\nghi", 3..3, "\n"),
        ("abc\ndef\nghi", 11..11, "\nnew"),
        ("abc\ndef\n", 8..8, "x"),
        ("abc\r\ndef\r\nghi", 3..5, ""),
        ("abc\r\ndef", 5..5, "1\r\n2\r\n"),
        ("", 0..0, "a\nb\n"),
        ("a\nb\nc\nd", 0..7, "z"),
        ("short\na much much longer line\nx", 8..12, ""),
        ("short\na much much longer line\nx", 6..30, "y"),
    ];
    for (text, range, new) in cases {
        let mut edited = doc(text);
        let splice = edited.replace(range.clone(), new);
        let mut expected = (*text).to_owned();
        expected.replace_range(range.clone(), new);
        let fresh = doc(&expected);
        assert_eq!(edited, fresh, "{text:?} {range:?} {new:?}");
        assert_eq!(splice.added, splice.removed + new.matches('\n').count() - text[range.clone()].matches('\n').count(), "{text:?}");
    }
    let mut ascii = doc("abc");
    ascii.replace(1..1, "é");
    assert!(!ascii.ascii);
}

#[test]
fn offsets_and_positions_round_trip() {
    let d = doc("ab\r\nçd\n\nx");
    assert_eq!(d.offset(at(1, 1)), 4, "ç is two bytes: col 1 is inside it, clamped back");
    assert_eq!(d.offset(at(1, 2)), 6);
    assert_eq!(d.pos(6), at(1, 2));
    assert_eq!(d.pos(2), at(0, 2));
    assert_eq!(d.pos(3), at(0, 2), "inside \\r\\n is the line's end");
    assert_eq!(d.offset(at(9, 9)), d.text.len());
    assert_eq!(d.pos(d.text.len()), at(3, 1));
    assert_eq!(d.line_of(8), 2);
    assert_eq!(d.line_of(9), 3);
}

#[test]
fn moves_by_characters_words_and_lines() {
    let d = doc("fn main() {\n    let x_y = 1;\n}");
    assert_eq!(prev(&d, at(1, 0)), at(0, 11));
    assert_eq!(next(&d, at(0, 11)), at(1, 0));
    assert_eq!(next(&d, at(2, 1)), at(2, 1), "the end stays");
    assert_eq!(word_right(&d, at(1, 0)), at(1, 7));
    assert_eq!(word_right(&d, at(1, 7)), at(1, 11), "x_y is one word");
    assert_eq!(word_right(&d, at(1, 11)), at(1, 13), "then the =");
    assert_eq!(word_left(&d, at(1, 11)), at(1, 8));
    assert_eq!(word_left(&d, at(0, 9)), at(0, 7), "the () run");
    assert_eq!(word_left(&d, at(1, 4)), at(1, 0));
    assert_eq!(word_left(&d, at(1, 0)), at(0, 11));
    assert_eq!(home(&d, at(1, 9)), at(1, 4), "the first non-blank");
    assert_eq!(home(&d, at(1, 4)), at(1, 0), "then the start");
    assert_eq!(end(&d, at(1, 0)), at(1, 16));
    // Vertical moves keep the column they started at.
    let d = doc("long line here\nab\nanother long one");
    let (p, goal) = vertical(&d, at(0, 10), 1, None);
    assert_eq!((p, goal), (at(1, 2), 10));
    assert_eq!(vertical(&d, p, 1, Some(goal)).0, at(2, 10));
    assert_eq!(vertical(&d, at(0, 3), -1, None).0, at(0, 0), "above the first line: its start");
    assert_eq!(vertical(&d, at(2, 3), 5, None).0, d.end(), "below the last: its end");
    let tabs = doc("\tx\nabcdefgh");
    assert_eq!(vertical(&tabs, at(0, 1), 1, None).0, at(1, 4), "a tab is four columns");
}

#[test]
fn indentation_is_detected() {
    assert_eq!(Indent::detect(&doc("fn a() {\n  x\n  if y {\n    z\n  }\n}")), Indent::Spaces(2));
    assert_eq!(Indent::detect(&doc("a:\n    b\n        c\n    d")), Indent::Spaces(4));
    assert_eq!(Indent::detect(&doc("a {\n\tb\n\t\tc\n}")), Indent::Tab);
    assert_eq!(Indent::detect(&doc("plain\ntext")), Indent::Spaces(4));
    assert_eq!(newline_of(&doc("a\r\nb")), "\r\n");
    assert_eq!(newline_of(&doc("a\nb")), "\n");
    assert_eq!(with_newlines("a\r\nb\nc", "\r\n"), "a\r\nb\r\nc");
}

#[test]
fn typing_replaces_the_selection_with_the_files_line_breaks() {
    let d = doc("hello world\r\nx");
    let e = insert(&d, sel(at(0, 6), at(0, 11)), "there\nfriend", "\r\n");
    assert_eq!(applied(&d.text, &e), "hello there\r\nfriend\r\nx");
    assert_eq!(e.kind, Kind::Other);
    assert_eq!(insert(&d, sel(at(0, 0), at(0, 0)), "y", "\n").kind, Kind::Type);
    // A word selection (double-click) is replaced whole.
    let word = Selection { anchor: at(0, 7), head: at(0, 7), unit: Unit::Word };
    assert_eq!(applied(&d.text, &insert(&d, word, "all", "\n")), "hello all\r\nx");
}

#[test]
fn deletes_reach_characters_words_lines_and_indent_stops() {
    let d = doc("fn x() {\n        let a = 1;\n}");
    let caret = |p| sel(p, p);
    let four = Indent::Spaces(4);
    assert_eq!(applied(&d.text, &backspace(&d, caret(at(1, 8)), By::Char, four).unwrap()), "fn x() {\n    let a = 1;\n}", "back one stop");
    assert_eq!(
        applied(&d.text, &backspace(&d, caret(at(1, 6)), By::Char, four).unwrap()),
        "fn x() {\n      let a = 1;\n}",
        "to the stop, spaces after the caret stay"
    );
    assert_eq!(applied(&d.text, &backspace(&d, caret(at(1, 0)), By::Char, four).unwrap()), "fn x() {        let a = 1;\n}", "joins lines");
    assert_eq!(applied(&d.text, &backspace(&d, caret(at(1, 17)), By::Word, four).unwrap()), "fn x() {\n        let a = ;\n}");
    assert_eq!(applied(&d.text, &backspace(&d, caret(at(1, 13)), By::Word, four).unwrap()), "fn x() {\n        let  = 1;\n}");
    assert_eq!(applied(&d.text, &backspace(&d, caret(at(1, 13)), By::Line, four).unwrap()), "fn x() {\n = 1;\n}");
    assert!(backspace(&d, caret(at(0, 0)), By::Char, four).is_none());
    assert_eq!(applied(&d.text, &delete(&d, caret(at(0, 8)), By::Char).unwrap()), "fn x() {        let a = 1;\n}");
    assert_eq!(applied(&d.text, &delete(&d, caret(at(1, 8)), By::Word).unwrap()), "fn x() {\n         a = 1;\n}");
    assert_eq!(applied(&d.text, &delete(&d, caret(at(1, 12)), By::Line).unwrap()), "fn x() {\n        let \n}");
    assert!(delete(&d, caret(d.end()), By::Char).is_none());
    let s = backspace(&d, sel(at(0, 3), at(0, 4)), By::Word, four).unwrap();
    assert_eq!(applied(&d.text, &s), "fn () {\n        let a = 1;\n}", "a selection goes whole");
}

#[test]
fn return_keeps_and_deepens_the_indent() {
    let four = Indent::Spaces(4);
    let check = |text: &str, p: Pos, colon: bool, want: &str, caret: Pos| {
        let d = doc(text);
        let e = newline(&d, sel(p, p), four, "\n", colon);
        assert_eq!(applied(&d.text, &e), want, "{text:?}");
        let mut edited = doc(text);
        let mut state = EditState::new(FileBase::default(), &edited);
        let (_, after) = state.apply(&mut edited, sel(p, p), e, Instant::now());
        assert_eq!(after.head, caret, "{text:?}");
    };
    check("    let x = 1;", at(0, 14), false, "    let x = 1;\n    ", at(1, 4));
    check("fn a() {", at(0, 8), false, "fn a() {\n    ", at(1, 4));
    check("fn a() {}", at(0, 8), false, "fn a() {\n    \n}", at(1, 4));
    check("  call(a,", at(0, 9), false, "  call(a,\n  ", at(1, 2));
    check("def f():", at(0, 8), true, "def f():\n    ", at(1, 4));
    check("def f():", at(0, 8), false, "def f():\n", at(1, 0));
    check("x =   ", at(0, 6), false, "x =\n", at(1, 0));
    check("    ab", at(0, 2), false, "  \n    ab", at(1, 2));
}

#[test]
fn tab_indents_at_the_caret_or_across_lines() {
    let d = doc("ab\n  cd\n\nef");
    let four = Indent::Spaces(4);
    assert_eq!(applied(&d.text, &tab(&d, sel(at(0, 1), at(0, 1)), four)), "a   b\n  cd\n\nef", "to the next stop");
    assert_eq!(applied(&d.text, &tab(&d, sel(at(0, 1), at(0, 1)), Indent::Tab)), "a\tb\n  cd\n\nef");
    let across = sel(at(0, 1), at(3, 1));
    let e = tab(&d, across, four);
    assert_eq!(applied(&d.text, &e), "    ab\n      cd\n\n    ef", "blank lines stay");
    assert_eq!(e.after, Some(sel(at(0, 5), at(3, 5))));
    // Ending at a line's start leaves that line out.
    assert_eq!(applied(&d.text, &tab(&d, sel(at(0, 0), at(1, 0)), four)), "    ab\n  cd\n\nef");
    let out = shift_lines(&d, sel(at(1, 3), at(1, 3)), four, true).unwrap();
    assert_eq!(applied(&d.text, &out), "ab\ncd\n\nef");
    assert_eq!(out.after, Some(sel(at(1, 1), at(1, 1))));
    assert!(shift_lines(&d, sel(at(0, 0), at(0, 0)), four, true).is_none(), "nothing to take out");
    let tabs = doc("\t\tx");
    assert_eq!(applied(&tabs.text, &shift_lines(&tabs, sel(at(0, 2), at(0, 2)), Indent::Tab, true).unwrap()), "\tx");
}

#[test]
fn undo_groups_typing_and_deletes_and_tracks_the_saved_state() {
    let mut d = doc("");
    let mut state = EditState::new(FileBase::default(), &d);
    let t0 = Instant::now();
    let mut s = caret(at(0, 0));
    let type_ = |d: &mut Doc, state: &mut EditState, s: &mut Selection, text: &str, t: Instant| {
        let e = insert(d, *s, text, "\n");
        *s = state.apply(d, *s, e, t).1;
    };
    for (i, c) in "hello world".chars().enumerate() {
        type_(&mut d, &mut state, &mut s, &c.to_string(), t0 + Duration::from_millis(i as u64 * 50));
    }
    assert!(state.dirty());
    assert_eq!(d.text, "hello world");
    // One step per word.
    let (_, after) = state.step(&mut d, false).unwrap();
    assert_eq!((d.text.as_str(), after.head), ("hello", at(0, 5)));
    state.step(&mut d, false).unwrap();
    assert_eq!(d.text, "");
    assert!(!state.dirty(), "back where it was read");
    assert!(state.step(&mut d, false).is_none());
    state.step(&mut d, true).unwrap();
    state.step(&mut d, true).unwrap();
    assert_eq!(d.text, "hello world");
    // A pause starts a new step; so does a save.
    let mut s = caret(d.end());
    type_(&mut d, &mut state, &mut s, "!", t0 + Duration::from_secs(10));
    state.saved_as(FileBase::default(), state.history.current());
    assert!(!state.dirty());
    type_(&mut d, &mut state, &mut s, "?", t0 + Duration::from_secs(10));
    assert!(state.dirty());
    state.step(&mut d, false).unwrap();
    assert_eq!(d.text, "hello world!");
    assert!(!state.dirty(), "undo back to the save is clean");
    // Backspaces group into one step.
    for i in 0..3 {
        let e = backspace(&d, s, By::Char, Indent::Spaces(4)).unwrap();
        s = state.apply(&mut d, s, e, t0 + Duration::from_secs(11) + Duration::from_millis(i * 10)).1;
    }
    assert_eq!(d.text, "hello wor");
    state.step(&mut d, false).unwrap();
    assert_eq!(d.text, "hello world!");
    // A new edit drops the redo steps.
    let e = insert(&d, caret(at(0, 0)), ">", "\n");
    state.apply(&mut d, caret(at(0, 0)), e, t0 + Duration::from_secs(20));
    assert!(state.step(&mut d, true).is_none());
}

#[test]
fn spans_follow_an_edit() {
    let mut spans = vec![vec![(0..2, 'k'), (3..7, 'f')], vec![(0..3, 's')]];
    // "fn main" → "fn xmain": one byte at col 3, before the name.
    splice_spans(&mut spans, Splice { first: 0, removed: 1, added: 1 }, 3, 0, 1);
    assert_eq!(spans[0], vec![(0..2, 'k'), (4..8, 'f')]);
    // Inside a span: it grows.
    splice_spans(&mut spans, Splice { first: 0, removed: 1, added: 1 }, 1, 0, 2);
    assert_eq!(spans[0], vec![(0..4, 'k'), (6..10, 'f')]);
    // A line break at col 5 of line 0: line 0 keeps what is before it, a new empty line follows.
    splice_spans(&mut spans, Splice { first: 0, removed: 1, added: 2 }, 5, 0, 1);
    assert_eq!(spans, vec![vec![(0..4, 'k')], vec![], vec![(0..3, 's')]]);
    // Joining lines 0 and 1.
    splice_spans(&mut spans, Splice { first: 0, removed: 2, added: 1 }, 4, 1, 0);
    assert_eq!(spans, vec![vec![(0..4, 'k')], vec![(0..3, 's')]]);
}

#[test]
fn utf16_offsets_count_surrogate_pairs() {
    let d = doc("a😀b\né");
    assert_eq!(to_utf16(&d, 5), 3, "😀 is two UTF-16 units, four bytes");
    assert_eq!(from_utf16(&d, 3), 5);
    assert_eq!(from_utf16(&d, 99), d.text.len());
    let ascii = doc("plain");
    assert_eq!((to_utf16(&ascii, 3), from_utf16(&ascii, 3)), (3, 3));
}
