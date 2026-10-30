//! The pane's state against the server's: tabs, highlights, changes on
//! disk, find, editing, and drawing every kind of content. No server: calls
//! are skipped, contents are put in place as a read would, saves are
//! answered by hand.

use gpui::{AppContext as _, EntityInputHandler as _, KeyDownEvent, Keystroke, TestAppContext, VisualTestContext};
use silo_proto::{
    control::{ContentKind, FileRead, FileWritten, RpcError},
    model::{BlockInfo, BlockKind, FileHighlight, FilesState},
};

use super::{
    Content, FilesPane, FilesPaneEvent,
    edit::Disk,
    marks_of,
    model::{Pos, Selection, Shown, Unit},
};

fn info(state: FilesState) -> BlockInfo {
    let mut info: BlockInfo = serde_json::from_value(
        serde_json::json!({"id": "b1", "session_id": "s", "kind": "files", "title": "app", "size": {"columns": 80, "rows": 24}}),
    )
    .unwrap();
    info.kind = BlockKind::Files;
    info.files = Some(Box::new(state));
    info
}

fn state() -> FilesState {
    let mut state = FilesState::new("/work/app".into());
    state.open = vec!["src/main.rs".into(), "README.md".into(), "notes.txt".into()];
    state.active = Some("src/main.rs".into());
    state.highlights.insert("src/main.rs".into(), vec![FileHighlight { start: 2, end: 3, note: Some("here".into()) }]);
    state
}

fn pane(cx: &mut TestAppContext) -> (gpui::Entity<FilesPane>, &mut VisualTestContext, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let handle = rt.handle().clone();
    let (pane, cx) = cx.add_window_view(|window, cx| FilesPane::new("b1".into(), Some(info(state())), handle, None, window, cx));
    (pane, cx, rt)
}

fn read_of(path: &str, body: &str, mtime: u64) -> FileRead {
    FileRead {
        path: path.into(),
        kind: ContentKind::Text,
        text: Some(body.into()),
        mime: "text/plain".into(),
        size: body.len() as u64,
        mtime,
        truncated: false,
        sha256: Some(format!("sha-of-{body}")),
    }
}

fn text(pane: &mut FilesPane, path: &str, body: &str, cx: &mut gpui::Context<FilesPane>) {
    let shown = super::model::shown_as(path, "text/plain", true);
    pane.set_text(path.into(), body.into(), shown, &read_of(path, body, 1), cx);
}

fn key(pane: &mut FilesPane, keys: &str, cx: &mut gpui::Context<FilesPane>) {
    for k in keys.split(' ') {
        let event = KeyDownEvent { keystroke: Keystroke::parse(k).unwrap(), is_held: false, prefer_character_input: false };
        assert!(pane.edit_key(&event, cx), "{k} is an editing key");
    }
}

fn caret(line: usize, col: usize) -> Option<Selection> {
    Some(Selection::new(Pos { line, col }, Unit::Char))
}

fn shown_text(pane: &FilesPane) -> String {
    pane.shown_doc().unwrap().text.clone()
}

#[gpui::test]
fn highlights_go_with_their_tab(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    pane.update(cx, |pane, cx| {
        assert_eq!(marks_of(&pane.state, "src/main.rs").len(), 1);
        pane.close("src/main.rs", cx);
        assert!(marks_of(&pane.state, "src/main.rs").is_empty(), "closing the tab drops its highlights");
        assert_eq!(pane.active(), Some("README.md"), "the right neighbour shows");
        // The server marks lines again (an agent's files_show): they show.
        let mut again = state();
        again.active = Some("notes.txt".into());
        again.highlights = [("notes.txt".to_owned(), vec![FileHighlight { start: 1, end: 1, note: None }])].into();
        pane.set_info(info(again), cx);
        assert_eq!((pane.active(), marks_of(&pane.state, "notes.txt").len()), (Some("notes.txt"), 1));
        assert_eq!(pane.state.open.len(), 3);
    });
}

#[gpui::test]
fn changes_on_disk_mark_hidden_tabs_and_reread_the_shown_one(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    pane.update(cx, |pane, cx| {
        text(pane, "src/main.rs", "fn main() {}\n", cx);
        text(pane, "README.md", "# Hi\n", cx);
        pane.files_changed(&["README.md".into(), "src/main.rs".into()], false, cx);
        assert!(pane.changed.contains("README.md") && !pane.contents.contains_key("README.md"), "hidden: a dot, read when shown");
        assert!(!pane.changed.contains("src/main.rs"), "shown: read again at once, no dot");
        pane.open("README.md", true, cx);
        assert!(!pane.changed.contains("README.md"), "shown: the dot goes");
        pane.step_tab(1, cx);
        assert_eq!(pane.active(), Some("notes.txt"));
        pane.step_tab(1, cx);
        assert_eq!(pane.active(), Some("src/main.rs"), "wraps around");
    });
}

#[gpui::test]
fn find_select_and_copy_in_the_shown_text(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    pane.update_in(cx, |pane, window, cx| {
        text(pane, "src/main.rs", "let foo = 1;\nfoo(foo);\n", cx);
        pane.open_find(window, cx);
        pane.search("foo", cx);
        let find = pane.find.as_ref().unwrap();
        assert_eq!((find.matches.len(), find.current), (3, Some(0)));
        pane.find_step(-1, cx);
        assert_eq!(pane.find.as_ref().unwrap().current, Some(2), "back past the first: the last");
        pane.close_bars(window, cx);
        assert!(pane.find.is_none());
        pane.select_all(cx);
        assert_eq!(pane.selected_text().as_deref(), Some("let foo = 1;\nfoo(foo);\n"));
        // Rendered markdown has no text selection; its source does.
        text(pane, "README.md", "# Hi\n", cx);
        pane.open("README.md", true, cx);
        assert!(pane.source_hidden("README.md") && pane.shown_doc().is_none());
        pane.toggle_source(cx);
        assert_eq!(pane.shown_doc().map(|d| d.text.clone()).as_deref(), Some("# Hi\n"));
    });
}

#[gpui::test]
fn every_kind_of_content_draws(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| _ = window.draw(cx));
    pane.update(cx, |pane, cx| text(pane, "src/main.rs", "fn main() {\n\tprintln!(\"hi\");\n}\n", cx));
    draw(cx);
    pane.update(cx, |pane, cx| {
        assert!(matches!(pane.contents.get("src/main.rs"), Some(Content::Text(f)) if f.shown == Shown::Text), "kept after drawing");
        pane.selection = Some(super::model::Selection::new(super::model::Pos { line: 1, col: 1 }, super::model::Unit::Line));
        text(pane, "README.md", "# Title\n\n- [x] done\n\n```rust\nfn x() {}\n```\n", cx);
        pane.open("README.md", true, cx);
    });
    draw(cx);
    pane.update(cx, |pane, cx| {
        pane.contents.insert("notes.txt".into(), Content::Binary { mime: "application/zip".into(), size: 3_200_000 });
        pane.open("notes.txt", true, cx);
        pane.toggle_tree(cx);
    });
    draw(cx);
    pane.update(cx, |pane, _| _ = pane.contents.insert("notes.txt".into(), Content::Failed("gone".into())));
    draw(cx);
}

#[gpui::test]
fn typing_moving_and_undo_edit_the_shown_text(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    pane.update_in(cx, |pane, window, cx| {
        text(pane, "src/main.rs", "fn main() {\n    x();\n}\n", cx);
        assert!(pane.editing() && pane.unsaved().is_empty());
        pane.selection = caret(1, 8);
        // Characters come through the input handler; keys do the rest.
        for c in ["y", "z"] {
            pane.replace_text_in_range(None, c, window, cx);
        }
        assert_eq!(shown_text(pane), "fn main() {\n    x();yz\n}\n");
        assert_eq!(pane.unsaved(), ["src/main.rs"], "unsaved: the tab says so");
        key(pane, "enter", cx);
        pane.replace_text_in_range(None, "w", window, cx);
        assert_eq!(shown_text(pane), "fn main() {\n    x();yz\n    w\n}\n", "Return keeps the indent");
        key(pane, "cmd-left shift-end backspace", cx);
        assert_eq!(shown_text(pane), "fn main() {\n    x();yz\n    \n}\n");
        key(pane, "alt-backspace up cmd-right", cx);
        assert_eq!(pane.selection.unwrap().head, Pos { line: 1, col: 10 });
        key(pane, "cmd-z cmd-z cmd-z cmd-z", cx);
        assert_eq!(shown_text(pane), "fn main() {\n    x();\n}\n");
        assert!(pane.unsaved().is_empty(), "undone to the file: clean");
        key(pane, "cmd-shift-z", cx);
        assert_eq!(shown_text(pane), "fn main() {\n    x();yz\n}\n");
        // Tab and ⇧Tab across lines, matching the file's four spaces.
        pane.select_all(cx);
        key(pane, "tab", cx);
        assert_eq!(shown_text(pane), "    fn main() {\n        x();yz\n    }\n");
        key(pane, "shift-tab shift-tab", cx);
        assert_eq!(shown_text(pane), "fn main() {\nx();yz\n}\n");
        // ⌘X takes the selection to the clipboard; ⌘V brings it back.
        pane.selection = Some(Selection { anchor: Pos { line: 1, col: 0 }, head: Pos { line: 1, col: 4 }, unit: Unit::Char });
        key(pane, "cmd-x", cx);
        assert_eq!(shown_text(pane), "fn main() {\nyz\n}\n");
        assert_eq!(cx.read_from_clipboard().and_then(|i| i.text()).as_deref(), Some("x();"));
        assert!(pane.paste(cx));
        assert_eq!(shown_text(pane), "fn main() {\nx();yz\n}\n");
    });
}

#[gpui::test]
fn marked_text_composes_then_commits(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    pane.update_in(cx, |pane, window, cx| {
        text(pane, "src/main.rs", "ab\n", cx);
        pane.selection = caret(0, 1);
        pane.replace_and_mark_text_in_range(None, "ni", None, window, cx);
        assert_eq!(shown_text(pane), "anib\n");
        assert_eq!(pane.marked_text_range(window, cx), Some(1..3));
        pane.replace_and_mark_text_in_range(None, "にほ", Some(2..2), window, cx);
        assert_eq!(shown_text(pane), "aにほb\n");
        assert_eq!(pane.marked_text_range(window, cx), Some(1..3), "UTF-16 units");
        pane.replace_text_in_range(None, "日本", window, cx);
        assert_eq!((shown_text(pane).as_str(), pane.marked_text_range(window, cx)), ("a日本b\n", None));
        assert_eq!(pane.selected_text_range(false, window, cx).unwrap().range, 3..3);
    });
}

#[gpui::test]
fn edits_survive_switching_tabs_and_ask_before_closing(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = asked.clone();
    let _sub = cx.update(|_, cx| {
        cx.subscribe(&pane, move |_, event: &FilesPaneEvent, _| {
            if let FilesPaneEvent::Unsaved(paths) = event {
                seen.borrow_mut().push(paths.clone());
            }
        })
    });
    pane.update_in(cx, |pane, window, cx| {
        text(pane, "src/main.rs", "one\n", cx);
        text(pane, "notes.txt", "two\n", cx);
        pane.selection = caret(0, 3);
        pane.replace_text_in_range(None, "!", window, cx);
        pane.open("notes.txt", true, cx);
        assert_eq!(shown_text(pane), "two\n");
        pane.open("src/main.rs", true, cx);
        assert_eq!((shown_text(pane).as_str(), pane.selection.unwrap().head), ("one!\n", Pos { line: 0, col: 4 }), "text and caret kept");
        // A clean tab's change on disk is taken; this one keeps its edits until asked.
        pane.files_changed(&["src/main.rs".into()], false, cx);
        assert_eq!(shown_text(pane), "one!\n");
        // Closed elsewhere (another window): kept open here while it holds edits.
        let mut elsewhere = pane.state.clone();
        elsewhere.open.retain(|p| p != "src/main.rs");
        elsewhere.active = Some("notes.txt".into());
        pane.set_info(info(elsewhere), cx);
        assert!(pane.state.open.contains(&"src/main.rs".to_owned()) && pane.unsaved() == ["src/main.rs"]);
        pane.open("src/main.rs", true, cx);
        pane.close("src/main.rs", cx);
        assert!(pane.state.open.contains(&"src/main.rs".to_owned()), "not closed yet");
    });
    cx.run_until_parked();
    assert_eq!(*asked.borrow(), vec![vec!["src/main.rs".to_owned()]]);
    pane.update(cx, |pane, cx| {
        pane.close_now("src/main.rs", cx);
        assert!(!pane.state.open.contains(&"src/main.rs".to_owned()) && pane.unsaved().is_empty());
    });
}

#[gpui::test]
fn the_disk_bar_saves_and_conflicts(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    pane.update_in(cx, |pane, window, cx| {
        let path = "src/main.rs";
        text(pane, path, "a\n", cx);
        let state = |pane: &FilesPane| match pane.contents.get(path) {
            Some(Content::Text(f)) => (f.edit.as_ref().unwrap().disk, f.edit.as_ref().unwrap().dirty()),
            _ => panic!(),
        };
        // Read again unchanged (its own save, a touch): nothing happens.
        assert!(pane.same_as_disk(path, &read_of(path, "a\n", 2)));
        pane.selection = caret(0, 1);
        pane.replace_text_in_range(None, "b", window, cx);
        // Someone else wrote it: the edits stay, the bar asks.
        pane.disk_read(path, read_of(path, "theirs\n", 3), cx);
        assert_eq!(state(pane), (Disk::Changed, true));
        assert!(pane.disk_bar(cx).is_some());
        pane.keep_mine(path, cx);
        assert_eq!(state(pane), (Disk::Kept, true));
        assert!(pane.disk_bar(cx).is_none());
        // A save refused as a conflict brings the bar back; a saved one is clean.
        let current = match pane.contents.get(path) {
            Some(Content::Text(f)) => f.edit.as_ref().unwrap().history.current(),
            _ => panic!(),
        };
        let refused = Err(RpcError::new(RpcError::CONFLICT, "changed").into());
        assert!(!pane.saved(path, current, refused, cx));
        assert_eq!(state(pane), (Disk::Changed, true));
        let written = FileWritten { path: path.into(), mtime: 4, size: 3, sha256: "sha-of-ab\n".into() };
        assert!(pane.saved(path, current, Ok(written), cx));
        assert_eq!(state(pane), (Disk::Same, false));
        // Read again as saved: still the same file.
        assert!(pane.same_as_disk(path, &read_of(path, "ab\n", 5)));
        // Someone wrote exactly the edits: clean.
        pane.replace_text_in_range(None, "c", window, cx);
        pane.disk_read(path, read_of(path, "abc\n", 6), cx);
        assert_eq!(state(pane), (Disk::Same, false));
    });
}

#[gpui::test]
fn unsaved_tabs_wait_for_the_blocks_next_pane(cx: &mut TestAppContext) {
    let (pane, cx, rt) = pane(cx);
    pane.update_in(cx, |pane, window, cx| {
        text(pane, "src/main.rs", "kept\n", cx);
        pane.selection = caret(0, 4);
        pane.replace_text_in_range(None, "!", window, cx);
        pane.stash(cx);
    });
    let handle = rt.handle().clone();
    let again = cx.update(|window, cx| cx.new(|cx| FilesPane::new("b1".into(), Some(info(state())), handle, None, window, cx)));
    again.update(cx, |pane, _| {
        assert_eq!(pane.unsaved(), ["src/main.rs"]);
        assert_eq!(shown_text(pane), "kept!\n");
    });
}

#[gpui::test]
fn an_edited_tab_draws_its_caret_and_marked_text(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    pane.update_in(cx, |pane, window, cx| {
        text(pane, "src/main.rs", "fn main() {}\n", cx);
        window.focus(&pane.focus, cx);
        pane.selection = caret(0, 11);
        pane.replace_and_mark_text_in_range(None, "x", None, window, cx);
    });
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    pane.update(cx, |pane, _| assert!(pane.caret_on, "a caret while focused"));
}

fn entry(name: &str, dir: bool, size: u64) -> silo_proto::control::FileEntry {
    silo_proto::control::FileEntry {
        name: name.into(),
        kind: if dir { silo_proto::control::FileKind::Dir } else { silo_proto::control::FileKind::File },
        target: None,
        size,
        mtime: 1_761_300_000_000,
        created: 0,
        hidden: name.starts_with('.'),
    }
}

fn press(pane: &mut FilesPane, k: &str, window: &mut gpui::Window, cx: &mut gpui::Context<FilesPane>) -> bool {
    let event = KeyDownEvent { keystroke: Keystroke::parse(k).unwrap(), is_held: false, prefer_character_input: false };
    pane.browse_key(&event, window, cx)
}

#[gpui::test]
fn the_shown_file_is_revealed_in_the_tree(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    pane.update(cx, |pane, cx| {
        // Opening the block: its shown file's folder is open, its row selected.
        assert_eq!(pane.state.expanded, ["src"]);
        assert_eq!(pane.tree_selected.as_deref(), Some("src/main.rs"));
        pane.listings.insert(String::new(), vec![entry("src", true, 0), entry("docs", true, 0), entry("README.md", false, 1)]);
        pane.listings.insert("src".into(), vec![entry("main.rs", false, 5), entry("ui", true, 0)]);
        pane.listings.insert("src/ui".into(), vec![entry("view.rs", false, 5)]);
        // A file deep down: every folder above it opens, and its row is scrolled to once.
        pane.open("src/ui/view.rs", true, cx);
        assert_eq!(pane.state.expanded, ["src", "src/ui"]);
        assert_eq!(pane.tree_selected.as_deref(), Some("src/ui/view.rs"));
        let rows = pane.tree_rows();
        let at = pane.take_tree_reveal(&rows);
        assert_eq!(at.map(|i| rows[i].path.as_str()), Some("src/ui/view.rs"));
        assert_eq!(pane.take_tree_reveal(&rows), None, "scrolled to once");
        // Opening behind (⌘-click) leaves the tree alone; closing the shown tab reveals the next.
        pane.open("README.md", false, cx);
        assert_eq!(pane.tree_selected.as_deref(), Some("src/ui/view.rs"));
        pane.close_now("src/ui/view.rs", cx);
        assert_eq!(pane.tree_selected.as_deref(), pane.active());
    });
}

#[gpui::test]
fn the_browser_lists_navigates_and_selects(cx: &mut TestAppContext) {
    use silo_proto::model::FilesView;
    let (pane, cx, _rt) = pane(cx);
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| _ = window.draw(cx));
    cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.browser.test_ops = true;
            pane.listings
                .insert(String::new(), vec![entry("src", true, 0), entry("b.txt", false, 10), entry("a.md", false, 20), entry(".env", false, 1)]);
            pane.listings.insert("src".into(), vec![entry("main.rs", false, 5), entry("lib.rs", false, 7)]);
            // The shown src/main.rs was revealed (src expanded): start folded.
            assert_eq!(pane.state.expanded, ["src"]);
            pane.set_expanded("src", false, cx);
            pane.set_view(FilesView::Browse, window, cx);
            assert!(pane.browsing());
            let rows = pane.browse_rows();
            assert_eq!(rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["src", "a.md", "b.txt"]);
        });
    });
    draw(cx);
    cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            window.focus(&pane.browser.focus, cx);
            // Arrows, type-to-select, ⇧⌘. for hidden items.
            assert!(press(pane, "down", window, cx));
            assert_eq!(pane.picked_paths(), ["src"]);
            assert!(press(pane, "shift-down", window, cx));
            assert_eq!(pane.picked_paths(), ["src", "a.md"]);
            assert!(press(pane, "b", window, cx));
            assert_eq!(pane.picked_paths(), ["b.txt"]);
            assert!(press(pane, "cmd-shift-.", window, cx));
            assert_eq!(pane.browse_rows().len(), 4);
            // A folder opens in place, then is shown; ⌘[ comes back and selects it.
            pane.toggle_folder("src", cx);
            assert_eq!(pane.browse_rows().iter().filter(|r| r.depth == 1).count(), 2);
            pane.navigate("src".into(), true, cx);
            assert_eq!(pane.state.browse, "src");
            assert_eq!(pane.browse_rows().iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["lib.rs", "main.rs"]);
            pane.go_parent(cx);
            assert_eq!((pane.state.browse.as_str(), pane.picked_paths()), ("", vec!["src".to_owned()]));
            pane.go_back(cx);
            assert_eq!(pane.state.browse, "src");
            pane.go_forward(cx);
            assert_eq!(pane.state.browse, "");
            // Sort by size, largest first; the drag out offers this Mac's paths only.
            pane.set_sort(super::browse::SortKey::Size, cx);
            pane.toggle_folders_first(cx);
            assert_eq!(pane.browse_rows()[0].name, "a.md");
            let drag = pane.drag_of("a.md", &std::rc::Rc::new(vec![]));
            assert!(drag.external().is_none());
            pane.local = true;
            let drag = pane.drag_of("a.md", &std::rc::Rc::new(vec!["a.md".into(), "src".into()]));
            let Some(gpui::ExternalDragPayload::Files(files)) = drag.external() else { panic!("no payload") };
            assert_eq!(files.entries().iter().map(|(p, _)| p.display().to_string()).collect::<Vec<_>>(), ["/work/app/a.md", "/work/app/src"]);
            let items = pane.drag_out_items(&["src".into(), "a.md".into()]);
            assert_eq!(items.iter().map(|i| (i.name.as_str(), i.is_dir, i.size)).collect::<Vec<_>>(), [("src", true, 0), ("a.md", false, 20)]);
            // Space: Quick Look; Return: rename in place.
            pane.browser.picked.only("a.md");
            assert!(press(pane, "space", window, cx));
            assert!(pane.browser.preview.is_some());
            assert!(press(pane, "escape", window, cx));
            assert!(pane.browser.preview.is_none());
            assert!(press(pane, "enter", window, cx));
            assert_eq!(pane.browser.rename.as_ref().map(|r| r.path.as_str()), Some("a.md"));
            pane.browser.menu = Some(super::browser::Menu { at: gpui::point(gpui::px(10.0), gpui::px(10.0)), target: Some("a.md".into()) });
        });
    });
    draw(cx);
    cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.open_browse_search(window, cx);
            pane.browser.search.as_mut().unwrap().query = "main".into();
            pane.browser.search.as_mut().unwrap().hits =
                vec![silo_proto::control::FileHit { path: "src/main.rs".into(), entry: entry("main.rs", false, 5) }];
            pane.browser.dirty();
            assert_eq!(pane.browse_rows()[0].folder.as_deref(), Some("src"));
            pane.set_view(FilesView::Edit, window, cx);
            assert!(!pane.browsing());
        });
    });
    draw(cx);
}

/// ⇧⌘P's finder closes on a press outside it and gives focus back to the
/// viewer; a press inside it keeps it open.
#[gpui::test]
fn the_finder_closes_on_a_press_outside_it(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx);
    cx.update(|window, _| window.activate_window());
    pane.update_in(cx, |pane, window, cx| pane.open_finder(window, cx));
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    let field_focused =
        |pane: &FilesPane, window: &gpui::Window, cx: &gpui::App| pane.finder.as_ref().is_some_and(|f| f.field.read(cx).focus.is_focused(window));
    assert!(pane.update_in(cx, |pane, window, cx| field_focused(pane, window, cx)));
    // Inside: the card is centred 8 pt from the top.
    let inside = cx.update(|window, _| gpui::point(window.viewport_size().width / 2.0, gpui::px(20.0)));
    cx.simulate_mouse_down(inside, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_up(inside, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(pane.read_with(cx, |pane, _| pane.finder.is_some()), "a press inside keeps it");
    // Outside: the bottom-left corner of the pane.
    let outside = cx.update(|window, _| gpui::point(gpui::px(4.0), window.viewport_size().height - gpui::px(4.0)));
    cx.simulate_mouse_down(outside, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.run_until_parked();
    pane.update_in(cx, |pane, window, _| {
        assert!(pane.finder.is_none(), "a press outside closes it");
        assert!(pane.focus.is_focused(window), "focus back on the viewer");
    });
}
