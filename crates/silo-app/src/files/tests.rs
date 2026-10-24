//! The pane's state against the server's: tabs, highlights, changes on
//! disk, find, and drawing every kind of content. No server: calls are
//! skipped, contents are put in place as a read would.

use gpui::{TestAppContext, VisualTestContext};
use silo_proto::model::{BlockInfo, BlockKind, FileHighlight, FilesState};

use super::{Content, FilesPane, marks_of, model::Shown};

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

fn text(pane: &mut FilesPane, path: &str, body: &str, cx: &mut gpui::Context<FilesPane>) {
    let shown = super::model::shown_as(path, "text/plain", true);
    pane.set_text(path.into(), body.into(), shown, false, body.len() as u64, cx);
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
