use std::collections::{HashMap, HashSet};

use silo_proto::{
    control::{FileEntry, FileKind},
    model::FileHighlight,
};

use super::*;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|&p| p.to_owned()).collect()
}

#[test]
fn tabs_close_to_a_neighbour_and_step_around() {
    let open = s(&["a.rs", "b.rs", "c.rs"]);
    assert_eq!(close_tab(&open, Some("b.rs"), "b.rs"), (s(&["a.rs", "c.rs"]), Some("c.rs".into())), "the right neighbour");
    assert_eq!(close_tab(&open, Some("c.rs"), "c.rs"), (s(&["a.rs", "b.rs"]), Some("b.rs".into())), "the last: its left");
    assert_eq!(close_tab(&open, Some("c.rs"), "a.rs"), (s(&["b.rs", "c.rs"]), Some("c.rs".into())), "another tab keeps the shown one");
    assert_eq!(close_tab(&s(&["a.rs"]), Some("a.rs"), "a.rs"), (vec![], None));
    assert_eq!(close_tab(&open, Some("a.rs"), "x.rs").0, open, "not open: nothing");
    assert_eq!(step_tab(&open, Some("c.rs"), 1).as_deref(), Some("a.rs"));
    assert_eq!(step_tab(&open, Some("a.rs"), -1).as_deref(), Some("c.rs"));
    assert_eq!(step_tab(&[], None, 1), None);
}

#[test]
fn tab_labels_name_the_folder_of_twins() {
    let labels = tab_labels(&s(&["src/agent/mod.rs", "src/files/mod.rs", "Cargo.toml", "mod.rs"]));
    assert_eq!(labels[0], ("mod.rs".into(), Some("agent".into())));
    assert_eq!(labels[1], ("mod.rs".into(), Some("files".into())));
    assert_eq!(labels[2], ("Cargo.toml".into(), None));
    assert_eq!(labels[3], ("mod.rs".into(), None), "at the root: no folder to name");
}

fn entry(name: &str, kind: FileKind) -> FileEntry {
    FileEntry { name: name.into(), kind, target: (kind == FileKind::Symlink).then_some(FileKind::Dir), size: 0, mtime: 0, created: 0, hidden: false }
}

#[test]
fn the_tree_lists_expanded_folders_lazily_without_git() {
    let listings: HashMap<String, Vec<FileEntry>> = [
        (
            "".to_owned(),
            vec![entry(".git", FileKind::Dir), entry("src", FileKind::Dir), entry("docs", FileKind::Symlink), entry(".env", FileKind::File)],
        ),
        ("src".to_owned(), vec![entry("files", FileKind::Dir), entry("main.rs", FileKind::File)]),
    ]
    .into();
    let expanded: HashSet<String> = ["src".to_owned(), "src/files".to_owned()].into();
    let rows = tree_rows(&listings, &expanded);
    let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["src", "src/files", "src/main.rs", "docs", ".env"]);
    assert!(rows[0].expanded && !rows[0].loading && rows[0].depth == 0);
    assert!(rows[1].expanded && rows[1].loading && rows[1].depth == 1, "expanded, not listed yet");
    assert!(rows[3].dir && rows[3].link, "a link to a folder opens as one");
    assert!(rows[4].hidden && !rows[4].dir);
}

#[test]
fn tree_keys_move_open_and_close() {
    let listings: HashMap<String, Vec<FileEntry>> = [
        ("".to_owned(), vec![entry("src", FileKind::Dir), entry("a.txt", FileKind::File)]),
        ("src".to_owned(), vec![entry("main.rs", FileKind::File)]),
    ]
    .into();
    let closed = tree_rows(&listings, &HashSet::new());
    assert_eq!(tree_key(&closed, None, "down"), Some(TreeMove::Select(0)), "nothing selected: the first row");
    assert_eq!(tree_key(&closed, Some(0), "right"), Some(TreeMove::Toggle("src".into(), true)));
    assert_eq!(tree_key(&closed, Some(0), "up"), Some(TreeMove::Select(0)));
    assert_eq!(tree_key(&closed, Some(0), "down"), Some(TreeMove::Select(1)));
    let open = tree_rows(&listings, &["src".to_owned()].into());
    assert_eq!(tree_key(&open, Some(0), "right"), Some(TreeMove::Select(1)), "an open folder: into it");
    assert_eq!(tree_key(&open, Some(1), "left"), Some(TreeMove::Select(0)), "a file: up to its folder");
    assert_eq!(tree_key(&open, Some(0), "left"), Some(TreeMove::Toggle("src".into(), false)));
    assert_eq!(collapse(&s(&["src", "src/a", "srcx", "src/a/b"]), "src"), s(&["srcx"]));
    assert_eq!(reveal(&s(&["docs"]), "src/files/mod.rs"), s(&["docs", "src", "src/files"]));
    // Over the cap, the longest-open folders not above the file make way.
    assert_eq!(reveal_within(&s(&["docs", "lib", "src"]), "src/files/mod.rs", 3), s(&["lib", "src", "src/files"]));
    assert_eq!(reveal_within(&s(&["a", "b"]), "a/b/c/d.rs", 2), s(&["a", "a/b", "a/b/c"]), "never the folders above it");
}

#[test]
fn changes_reread_open_files_and_relist_their_folders() {
    let open = s(&["src/main.rs", "README.md"]);
    let listed = s(&["", "src", "docs"]);
    let (reread, relist) = affected(&s(&["src/main.rs", "src/new.rs"]), false, &open, listed.iter());
    assert_eq!((reread, relist), (s(&["src/main.rs"]), s(&["src"])));
    let (reread, relist) = affected(&s(&["docs"]), false, &open, listed.iter());
    assert_eq!((reread.len(), relist), (0, s(&["", "docs"])), "a folder itself changed: it and its parent");
    let (reread, relist) = affected(&[], true, &open, listed.iter());
    assert_eq!((reread, relist), (open.clone(), listed.clone()), "truncated: everything");
}

#[test]
fn docs_split_lines_and_measure_columns() {
    let doc = Doc::new("fn main() {\r\n\tlet 界 = 1;\n}".into());
    assert_eq!(doc.line_count(), 3);
    assert_eq!((doc.line(0), doc.line(1), doc.line(2)), ("fn main() {", "\tlet 界 = 1;", "}"));
    assert_eq!(columns(doc.line(1)), 4 + 4 + 2 + 5, "a tab to column 4, a wide character two");
    assert_eq!(doc.widest, 1);
    assert_eq!(expand_tabs("\ta\tb"), "    a   b");
    assert_eq!(to_shown("\ta", 1), 4);
    assert_eq!(col_at("\tab", 3.0), 1, "inside the tab's second half: after it");
    assert_eq!(col_at("\tab", 1.0), 0);
    assert_eq!(col_at("ab", 9.0), 2, "past the end: the end");
    assert_eq!(Doc::new(String::new()).line_count(), 1, "an empty file has one empty line");
    assert_eq!(Doc::new("a\n".into()).line_count(), 2);
}

#[test]
fn selections_grow_by_word_and_line_and_copy_text() {
    let doc = Doc::new("let foo_bar = baz;\nsecond line\nthird".into());
    let at = |line, col| Pos { line, col };
    let sel = Selection { anchor: at(0, 6), head: at(1, 3), unit: Unit::Char };
    assert_eq!(sel.text(&doc), "o_bar = baz;\nsec");
    assert_eq!(sel.on_line(&doc, 0), Some(6..19), "the line break counts");
    assert_eq!(sel.on_line(&doc, 1), Some(0..3));
    assert_eq!(sel.on_line(&doc, 2), None);
    let word = Selection::new(at(0, 5), Unit::Word);
    assert_eq!(word.text(&doc), "foo_bar");
    let backwards = Selection { anchor: at(1, 9), head: at(1, 1), unit: Unit::Word };
    assert_eq!(backwards.text(&doc), "second line");
    let line = Selection::new(at(1, 2), Unit::Line);
    assert_eq!(line.text(&doc), "second line\n");
    assert_eq!(Selection::new(at(2, 1), Unit::Line).text(&doc), "third", "the last line has no break");
    assert!(Selection::new(at(0, 1), Unit::Char).is_empty(&doc));
    assert_eq!(word_at("a  b", 1), 1..3, "spaces are a run");
    assert_eq!(word_at("a.b", 1), 1..2);
    assert_eq!(word_at("ab", 2), 0..2, "at the end: the word before");
    assert_eq!(doc.slice(at(0, 0), doc.end()), doc.text);
}

#[test]
fn find_is_smart_case_and_goto_parses_line_and_column() {
    let doc = Doc::new("Foo foo\nbar FOO".into());
    assert_eq!(find_all(&doc, "foo"), [(0, 0..3), (0, 4..7), (1, 4..7)]);
    assert_eq!(find_all(&doc, "Foo"), [(0, 0..3)]);
    assert!(find_all(&doc, "").is_empty());
    let matches = find_all(&doc, "foo");
    assert_eq!(first_from(&matches, 1), Some(2));
    assert_eq!(first_from(&matches, 9), Some(0), "none after: wrap to the first");
    assert_eq!(parse_goto(" 12 "), Some((11, None)));
    assert_eq!(parse_goto("12:5"), Some((11, Some(4))));
    assert_eq!(parse_goto("12:"), Some((11, None)));
    assert_eq!(parse_goto("0"), None);
    assert_eq!(parse_goto("x"), None);
}

#[test]
fn the_finder_prefers_file_names() {
    let paths = s(&["src/files/model.rs", "src/model/files.rs", "docs/FILES_BLOCK.md"]);
    let found = rank("model", paths.clone(), &[]);
    assert_eq!(found[0].path, "src/files/model.rs", "a name match beats a folder match");
    assert_eq!(found[0].ranges, vec![std::ops::Range { start: 10, end: 15 }]);
    assert_eq!(found[1].path, "src/model/files.rs");
    assert!(rank("zzz", paths.clone(), &[]).is_empty());
    assert_eq!(rank("files/m", paths.clone(), &[])[0].path, "src/files/model.rs", "a slash matches across the path");
    let empty = rank("", paths, &s(&["a.rs", "src/model/files.rs"]));
    let order: Vec<&str> = empty.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(order, ["src/model/files.rs", "a.rs", "src/files/model.rs", "docs/FILES_BLOCK.md"], "open tabs first, newest first");
}

#[test]
fn content_kinds_sizes_and_marks() {
    assert_eq!(shown_as("logo.png", "image/png", false), Shown::Image);
    assert_eq!(shown_as("icon.svg", "image/svg+xml", true), Shown::Image);
    assert_eq!(shown_as("README.md", "text/markdown", true), Shown::Markdown);
    assert_eq!(shown_as("main.rs", "text/plain", true), Shown::Text);
    assert_eq!(shown_as("a.zip", "application/zip", false), Shown::Binary);
    assert_eq!(human_size(812), "812 B");
    assert_eq!(human_size(3_200_000), "3.2 MB");
    assert_eq!(human_size(14_000_000), "14 MB");
    assert_eq!(binary_summary("application/zip", 3_200_000), "binary · 3.2 MB · application/zip");
    let marks = [FileHighlight { start: 3, end: 5, note: None }, FileHighlight { start: 9, end: 9, note: Some("x".into()) }];
    assert_eq!(mark_at(&marks, 3), Some((0, true)));
    assert_eq!(mark_at(&marks, 5), Some((0, false)));
    assert_eq!(mark_at(&marks, 9), Some((1, true)));
    assert_eq!(mark_at(&marks, 6), None);
}
