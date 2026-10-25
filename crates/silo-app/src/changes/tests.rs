//! The Changes pane, drawn and driven without a server: a status and diffs
//! put in place as the server would answer them.

use std::rc::Rc;

use gpui::{Entity, TestAppContext, VisualTestContext};
use silo_proto::{
    changes::{ChangedFile, ChangesStatus, ChangesTotals, DiffLine, FileDiff, FileStatus, Hunk, LineKind},
    model::{AgentLink, BlockInfo, BlockKind, ChangesScope, ChangesState, ReviewComment, Side},
};

use super::{ChangesPane, ChangesPaneEvent, HunkAction, Loaded, model};

fn info(state: ChangesState) -> BlockInfo {
    let mut info: BlockInfo = serde_json::from_value(
        serde_json::json!({"id": "c1", "session_id": "s", "kind": "changes", "title": "app", "size": {"columns": 80, "rows": 24}}),
    )
    .unwrap();
    info.kind = BlockKind::Changes;
    info.changes = Some(Box::new(state));
    info
}

fn pane(cx: &mut TestAppContext, state: ChangesState) -> (Entity<ChangesPane>, &mut VisualTestContext, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let handle = rt.handle().clone();
    let (pane, cx) = cx.add_window_view(|window, cx| ChangesPane::new("c1".into(), Some(info(state)), handle, None, window, cx));
    (pane, cx, rt)
}

fn file(path: &str, status: FileStatus, staged: bool) -> ChangedFile {
    ChangedFile { path: path.into(), old_path: None, status, staged, unstaged: !staged, added: 2, removed: 1, binary: false }
}

fn status(files: Vec<ChangedFile>) -> ChangesStatus {
    ChangesStatus {
        repo: true,
        branch: Some("otto".into()),
        head: Some("abc1234".into()),
        upstream: None,
        ahead: 1,
        behind: 0,
        totals: ChangesTotals { files: u32::try_from(files.len()).unwrap(), added: 4, removed: 2 },
        files,
        truncated: false,
    }
}

/// `body` in unified diff form, from line `at` on both sides.
fn hunk(id: &str, at: u32, body: &str) -> Hunk {
    let (mut o, mut n) = (at, at);
    let lines = body
        .lines()
        .map(|raw| {
            let (mark, text) = raw.split_at(1);
            let (kind, old, new) = match mark {
                "-" => (LineKind::Removed, Some(o), None),
                "+" => (LineKind::Added, None, Some(n)),
                _ => (LineKind::Context, Some(o), Some(n)),
            };
            o += u32::from(old.is_some());
            n += u32::from(new.is_some());
            DiffLine { kind, old, new, text: text.into() }
        })
        .collect();
    Hunk { id: id.into(), old_start: at, old_lines: o - at, new_start: at, new_lines: n - at, header: format!("@@ -{at} +{at} @@ fn x"), lines }
}

fn diff(path: &str, hunks: Vec<Hunk>) -> FileDiff {
    FileDiff { path: path.into(), old_path: None, status: FileStatus::Modified, binary: false, hunks, truncated: false }
}

fn put(pane: &mut ChangesPane, diffs: Vec<FileDiff>, cx: &mut gpui::Context<ChangesPane>) {
    for d in diffs {
        let words = model::words(&d);
        pane.diffs.insert(d.path.clone(), Rc::new(Loaded { diff: d, words, syntax: None }));
    }
    pane.rebuild(cx);
}

fn working() -> ChangesState {
    ChangesState::new("/work/app".into())
}

fn fixture(pane: &mut ChangesPane, cx: &mut gpui::Context<ChangesPane>) {
    pane.took_status(Ok(status(vec![file("src/a.rs", FileStatus::Modified, false), file("b.md", FileStatus::Untracked, false)])), cx);
    let long: String = (0..20).map(|i| format!(" ctx {i}\n")).collect();
    put(
        pane,
        vec![
            diff(
                "src/a.rs",
                vec![hunk("h1", 3, &format!(" fn a() {{\n-    let x = 1;\n+    let x = 2;\n{long}+    more();\n")), hunk("h2", 80, " y\n-z\n+Z\n")],
            ),
            diff("b.md", vec![hunk("h3", 1, "+# Notes\n+text\n")]),
        ],
        cx,
    );
}

#[gpui::test]
fn the_summary_split_and_states_draw(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx, working());
    let draw = |cx: &mut VisualTestContext| cx.update(|window, cx| _ = window.draw(cx));
    draw(cx);
    pane.update(cx, fixture);
    draw(cx);
    pane.update(cx, |pane, cx| {
        let rows = &pane.shown.rows;
        assert_eq!(rows.iter().filter(|r| matches!(r, model::Row::File { .. })).count(), 2, "the summary: a header per file");
        assert!(rows.iter().any(|r| matches!(r, model::Row::Fold { .. })), "the long run is folded");
        pane.set_split(true, cx);
        assert!(pane.shown.rows.iter().any(|r| matches!(r, model::Row::Pair { .. })));
        pane.open("src/a.rs", cx);
        assert_eq!(pane.state.active.as_deref(), Some("src/a.rs"));
        assert_eq!(pane.state.open, ["src/a.rs"]);
    });
    draw(cx);
    pane.update(cx, |pane, cx| {
        assert!(!pane.shown.rows.iter().any(|r| matches!(r, model::Row::File { .. })), "a tab: no file headers");
        pane.toggle_list(cx);
        pane.notice = Some(("The file changed: review it again".into(), true));
    });
    draw(cx);
    // Not a repository.
    pane.update(cx, |pane, cx| pane.took_status(Ok(ChangesStatus { repo: false, ..status(Vec::new()) }), cx));
    draw(cx);
    pane.update(cx, |pane, _| assert!(!pane.repo() && !pane.commits()));
}

#[gpui::test]
fn keys_walk_hunks_and_act_on_the_one_gone_to(cx: &mut TestAppContext) {
    let (pane, cx, _rt) = pane(cx, working());
    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_, cx| {
        cx.subscribe(&pane, move |_, event: &ChangesPaneEvent, _| {
            if let ChangesPaneEvent::Revert(ask) = event {
                seen.borrow_mut().push(ask.question.clone());
            }
        })
        .detach();
    });
    pane.update(cx, |pane, cx| {
        fixture(pane, cx);
        pane.step_hunk(1, cx);
        assert_eq!(pane.current, Some(("src/a.rs".into(), "h1".into())));
        pane.step_hunk(1, cx);
        assert_eq!(pane.current, Some(("src/a.rs".into(), "h2".into())));
        pane.step_hunk(1, cx);
        assert_eq!(pane.current, Some(("b.md".into(), "h3".into())));
        pane.step_hunk(-1, cx);
        assert_eq!(pane.target_hunk(), Some(("src/a.rs".into(), "h2".into())));
        pane.hunk_action(HunkAction::Revert, cx);
        // A fold opens and closes.
        let fold =
            pane.shown.rows.iter().find_map(|r| if let model::Row::Fold { file, hunk, at, .. } = r { Some((*file, *hunk, *at)) } else { None });
        let (file, hunk, at) = fold.expect("a fold");
        let before = pane.shown.rows.len();
        pane.toggle_fold(file, hunk, at, cx);
        assert!(pane.shown.rows.len() > before);
        pane.toggle_fold(file, hunk, at, cx);
        assert_eq!(pane.shown.rows.len(), before);
    });
    cx.run_until_parked();
    assert_eq!(*events.borrow(), ["Discard 2 changed lines in a.rs?"]);
    // A turn's scope is read-only: the keys say so instead of writing.
    pane.update(cx, |pane, cx| {
        pane.set_scope(ChangesScope::Tree { base: "b".into(), head: Some("e".into()), label: "Turn 4".into() }, cx);
        assert_eq!(pane.mode(), model::ListMode::ReadOnly);
        assert!(!pane.commits());
        pane.took_status(Ok(status(vec![file("src/a.rs", FileStatus::Modified, false)])), cx);
        put(pane, vec![diff("src/a.rs", vec![hunk("h1", 3, "-a\n+b\n")])], cx);
        pane.step_hunk(1, cx);
        pane.hunk_action(HunkAction::Stage, cx);
        assert!(pane.notice.as_ref().is_some_and(|(n, error)| *error && n.contains("read-only")));
    });
}

#[gpui::test]
fn comments_build_the_review_sent_to_the_linked_agent(cx: &mut TestAppContext) {
    let mut state = working();
    state.scope = ChangesScope::Tree { base: "b".into(), head: Some("e".into()), label: "Turn 4".into() };
    state.agent = Some(AgentLink { otto_session: "sess".into(), block_id: Some("agent1".into()) });
    let (pane, cx, _rt) = pane(cx, state);
    let sent = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = sent.clone();
    cx.update(|_, cx| {
        cx.subscribe(&pane, move |_, event: &ChangesPaneEvent, _| {
            if let ChangesPaneEvent::SendReview { text, link } = event {
                seen.borrow_mut().push((text.clone(), link.clone()));
            }
        })
        .detach();
    });
    pane.update_in(cx, |pane, window, cx| {
        pane.took_status(Ok(status(vec![file("src/a.rs", FileStatus::Modified, false)])), cx);
        put(pane, vec![diff("src/a.rs", vec![hunk("h1", 10, " keep\n-old()\n+new()\n")])], cx);
        pane.pick_line("src/a.rs", Side::New, 11, false, cx);
        pane.start_comment(window, cx);
        let composing = pane.composing.as_ref().expect("the comment field");
        assert_eq!((composing.side, composing.line, composing.end), (Side::New, 11, None));
        composing.field.update(cx, |f, cx| f.set_text("why new?", cx));
        pane.add_comment("why new?".into(), cx);
        assert_eq!(pane.state.comments.len(), 1);
        assert!(pane.shown.rows.contains(&model::Row::Comment { file: 0, index: 0 }), "shown under its line");
        pane.send_review(cx);
    });
    cx.run_until_parked();
    let (text, link) = sent.borrow()[0].clone();
    assert_eq!(text, "Review of your changes (Turn 4):\n\nsrc/a.rs:11 (new)\n> new()\nwhy new?");
    assert_eq!(link.map(|l| l.otto_session), Some("sess".into()));
    pane.update(cx, |pane, cx| {
        pane.review_sent(Ok(()), cx);
        assert!(pane.state.comments.is_empty(), "sent: the comments go");
        pane.commit_message(Ok("Sure:\n```\napp: x\n```".into()), cx);
        assert_eq!(pane.commit.read(cx).text(), "app: x");
    });
    // The server's later state wins once nothing is in flight.
    pane.update(cx, |pane, cx| {
        pane.in_flight = 0;
        let mut state = pane.state.clone();
        state.comments = vec![ReviewComment {
            id: "x".into(),
            path: "src/a.rs".into(),
            side: Side::Old,
            line: 11,
            end_line: None,
            text: "from elsewhere".into(),
            created_at: 0,
        }];
        pane.set_info(info(state), cx);
        assert_eq!(pane.state.comments.len(), 1);
    });
}
