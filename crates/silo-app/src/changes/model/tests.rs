use std::collections::HashSet;

use super::*;
use crate::changes::wire::{ChangedFile, DiffLine, FileDiff, FileStatus, Hunk, LineKind, ReviewComment, Side};

/// A hunk from unified diff lines (` `, `-`, `+`, `\`), starting at `old` / `new`.
fn hunk(id: &str, old: u32, new: u32, body: &str) -> Hunk {
    let (mut o, mut n) = (old, new);
    let mut lines = Vec::new();
    for raw in body.lines() {
        let (mark, text) = raw.split_at(1);
        let line = match mark {
            " " => {
                o += 1;
                n += 1;
                DiffLine { kind: LineKind::Context, old: Some(o - 1), new: Some(n - 1), text: text.into() }
            }
            "-" => {
                o += 1;
                DiffLine { kind: LineKind::Removed, old: Some(o - 1), new: None, text: text.into() }
            }
            "+" => {
                n += 1;
                DiffLine { kind: LineKind::Added, old: None, new: Some(n - 1), text: text.into() }
            }
            _ => DiffLine { kind: LineKind::NoNewline, old: None, new: None, text: text.trim().into() },
        };
        lines.push(line);
    }
    Hunk { id: id.into(), old_start: old, old_lines: o - old, new_start: new, new_lines: n - new, header: format!("@@ -{old} +{new} @@"), lines }
}

fn diff(path: &str, hunks: Vec<Hunk>) -> FileDiff {
    FileDiff { path: path.into(), old_path: None, status: FileStatus::Modified, binary: false, hunks, truncated: false }
}

fn context(n: usize) -> String {
    (0..n).map(|i| format!(" c{i}\n")).collect()
}

#[test]
fn unified_rows_follow_the_hunk_with_a_gap_before_it() {
    let d = diff("a.rs", vec![hunk("h1", 10, 10, " a\n-b\n+B\n c\n")]);
    let rows = rows(&[&d], Layout::default(), &HashSet::new(), &[]);
    assert_eq!(
        rows,
        vec![
            Row::Gap { file: 0, hunk: 0, lines: 9 },
            Row::Hunk { file: 0, hunk: 0 },
            Row::Line { file: 0, hunk: 0, at: 0 },
            Row::Line { file: 0, hunk: 0, at: 1 },
            Row::Line { file: 0, hunk: 0, at: 2 },
            Row::Line { file: 0, hunk: 0, at: 3 },
        ]
    );
    // Both line numbers are on the lines.
    assert_eq!((d.hunks[0].lines[0].old, d.hunks[0].lines[0].new), (Some(10), Some(10)));
    assert_eq!((d.hunks[0].lines[1].old, d.hunks[0].lines[1].new), (Some(11), None));
    assert_eq!((d.hunks[0].lines[2].old, d.hunks[0].lines[2].new), (None, Some(11)));
}

#[test]
fn gaps_between_hunks_count_the_lines_left_out() {
    let d = diff("a.rs", vec![hunk("h1", 1, 1, " a\n-b\n c\n"), hunk("h2", 20, 19, " x\n+y\n z\n")]);
    let rows = rows(&[&d], Layout::default(), &HashSet::new(), &[]);
    // h1 covers old 1..=3; h2 starts at 20: 16 lines between.
    assert!(rows.contains(&Row::Gap { file: 0, hunk: 1, lines: 16 }));
    assert!(!rows.iter().any(|r| matches!(r, Row::Gap { hunk: 0, .. })), "a hunk at the top has no gap");
    // A hunk that only adds after line 5.
    let add = Hunk { old_start: 5, old_lines: 0, ..hunk("h", 5, 6, "+new\n") };
    let rows = super::rows(&[&diff("b.rs", vec![add])], Layout::default(), &HashSet::new(), &[]);
    assert_eq!(rows[0], Row::Gap { file: 0, hunk: 0, lines: 5 });
}

#[test]
fn long_unchanged_runs_fold_and_open() {
    // 3 kept, 10 folded, 3 kept between two changes; the leading run keeps 3 before the change.
    let body = format!("{}-x\n+y\n{}-p\n+q\n", context(9), context(16));
    let h = hunk("h1", 1, 1, &body);
    assert_eq!(folds(&h), vec![(0, 6), (14, 10)]);
    let d = diff("a.rs", vec![h]);
    let closed = rows(&[&d], Layout::default(), &HashSet::new(), &[]);
    assert!(closed.contains(&Row::Fold { file: 0, hunk: 0, at: 0, lines: 6 }));
    assert!(closed.contains(&Row::Fold { file: 0, hunk: 0, at: 14, lines: 10 }));
    assert!(!closed.contains(&Row::Line { file: 0, hunk: 0, at: 15 }));
    let open = rows(&[&d], Layout::default(), &HashSet::from([(0, 0, 14)]), &[]);
    assert!(open.contains(&Row::Line { file: 0, hunk: 0, at: 15 }));
    assert!(open.contains(&Row::Fold { file: 0, hunk: 0, at: 0, lines: 6 }), "the other fold stays");
    // Short runs never fold.
    assert!(folds(&hunk("h", 1, 1, &format!("{}-x\n{}", context(3), context(3)))).is_empty());
    assert!(folds(&hunk("h", 1, 1, &format!("-x\n{}-y\n", context(9)))).is_empty(), "9 = 3 + 3 + 3 hidden: too few");
    assert_eq!(folds(&hunk("h", 1, 1, &format!("-x\n{}-y\n", context(10)))), vec![(4, 4)]);
}

#[test]
fn split_pairs_removed_with_added_lines() {
    let h = hunk("h", 1, 1, " a\n-b\n-c\n-d\n+B\n+C\n e\n+f\n\\ No newline at end of file\n");
    assert_eq!(
        pairs(&h),
        vec![(Some(0), Some(0)), (Some(1), Some(4)), (Some(2), Some(5)), (Some(3), None), (Some(6), Some(6)), (None, Some(7)), (Some(8), Some(8)),]
    );
    let d = diff("a.rs", vec![h]);
    let rows = rows(&[&d], Layout { split: true, headers: false }, &HashSet::new(), &[]);
    assert!(rows.contains(&Row::Pair { file: 0, hunk: 0, old: Some(3), new: None }));
    assert!(rows.contains(&Row::Line { file: 0, hunk: 0, at: 8 }), "no newline spans both sides");
}

#[test]
fn word_highlights_mark_what_changed() {
    let (a, b) = word_diff("    div().child(msg.clone())", "    div().child(text)").unwrap();
    let old = "    div().child(msg.clone())";
    let new = "    div().child(text)";
    assert_eq!(a.iter().map(|r| &old[r.clone()]).collect::<Vec<_>>(), ["msg.clone()"]);
    assert_eq!(b.iter().map(|r| &new[r.clone()]).collect::<Vec<_>>(), ["text"]);
    // Nothing in common: no marks (the tint says it).
    assert_eq!(word_diff("fn alpha() {}", "let beta = 42;"), None);
    // Two changed words with a space between join.
    let (_, b) = word_diff("let a = one two;", "let a = three four;").unwrap();
    assert_eq!(b, vec![8..18]);
    // Per line of a hunk: only paired removed/added lines.
    let d = diff("a.rs", vec![hunk("h", 1, 1, " keep\n-let x = 1;\n+let x = 2;\n")]);
    let w = words(&d);
    assert!(w[0][0].is_empty());
    assert_eq!(w[0][1], vec![8..9]);
    assert_eq!(w[0][2], vec![8..9]);
}

#[test]
fn summary_has_a_header_per_file_and_notes_for_the_rest() {
    let a = diff("a.rs", vec![hunk("h", 1, 1, "-x\n+y\n")]);
    let bin = FileDiff { binary: true, ..diff("img.png", Vec::new()) };
    let empty = FileDiff { status: FileStatus::Untracked, ..diff("empty.txt", Vec::new()) };
    let rows = rows(&[&a, &bin, &empty], Layout { split: false, headers: true }, &HashSet::new(), &[]);
    assert_eq!(rows[0], Row::File { file: 0 });
    assert!(rows.contains(&Row::File { file: 1 }) && rows.contains(&Row::Note { file: 1, text: "Binary file".into() }));
    assert!(rows.contains(&Row::Note { file: 2, text: "Empty file".into() }));
}

fn comment(path: &str, side: Side, line: u32, end: Option<u32>, text: &str) -> ReviewComment {
    ReviewComment { id: format!("{path}{line}"), path: path.into(), side, line, end_line: end, text: text.into(), created_at: 0 }
}

#[test]
fn comments_show_under_their_last_line() {
    let d = diff("a.rs", vec![hunk("h", 10, 10, " a\n-b\n+B\n c\n")]);
    let comments =
        [comment("a.rs", Side::New, 11, None, "why?"), comment("a.rs", Side::Old, 1, None, "gone"), comment("b.rs", Side::New, 1, None, "x")];
    let rows = rows(&[&d], Layout::default(), &HashSet::new(), &comments);
    let at = rows.iter().position(|r| *r == Row::Comment { file: 0, index: 0 }).unwrap();
    assert_eq!(rows[at - 1], Row::Line { file: 0, hunk: 0, at: 2 });
    assert_eq!(rows[0], Row::Comment { file: 0, index: 1 }, "a line not shown: at the file's top");
    assert!(!rows.contains(&Row::Comment { file: 0, index: 2 }), "another file's comment");
}

#[test]
fn navigation_steps_between_hunks() {
    let d = diff("a.rs", vec![hunk("h1", 1, 1, "-x\n+y\n"), hunk("h2", 20, 20, "-p\n+q\n")]);
    let rows = rows(&[&d], Layout::default(), &HashSet::new(), &[]);
    let first = hunk_row(&rows, 0, 0).unwrap();
    let second = hunk_row(&rows, 0, 1).unwrap();
    assert_eq!(step_hunk(&rows, first, 1), Some(second));
    assert_eq!(step_hunk(&rows, second, -1), Some(first));
    assert_eq!(step_hunk(&rows, second, 1), None);
    assert_eq!(hunk_at(&rows, second + 1), Some((0, 1)));
    assert_eq!(hunk_at(&rows, 0), Some((0, 0)));
}

fn file(path: &str, status: FileStatus, staged: bool, unstaged: bool) -> ChangedFile {
    ChangedFile { path: path.into(), old_path: None, status, staged, unstaged, added: 1, removed: 0, binary: false }
}

#[test]
fn the_list_groups_staged_changes_untracked_and_conflicts() {
    let files = [
        file("both.rs", FileStatus::Modified, true, true),
        file("new.rs", FileStatus::Untracked, false, true),
        file("staged.rs", FileStatus::Added, true, false),
        file("conflict.rs", FileStatus::Conflicted, false, true),
        file("edit.rs", FileStatus::Modified, false, true),
    ];
    let rows = group(&files, ListMode::Working);
    let f = |file, section, checked| ListRow::File { file, section, checked };
    assert_eq!(
        rows,
        vec![
            ListRow::Section { section: Section::Staged, count: 2 },
            f(0, Section::Staged, Some(true)),
            f(2, Section::Staged, Some(true)),
            ListRow::Section { section: Section::Changes, count: 2 },
            f(0, Section::Changes, Some(false)),
            f(4, Section::Changes, Some(false)),
            ListRow::Section { section: Section::Untracked, count: 1 },
            f(1, Section::Untracked, Some(false)),
            ListRow::Section { section: Section::Conflicts, count: 1 },
            f(3, Section::Conflicts, None),
        ]
    );
    let read_only = group(&files[..2], ListMode::ReadOnly);
    assert_eq!(read_only, vec![ListRow::Section { section: Section::Changes, count: 2 }, f(0, Section::Changes, None), f(1, Section::Changes, None)]);
    let staged = group(&files[2..3], ListMode::Staged);
    assert_eq!(staged[1], f(0, Section::Staged, Some(true)));
    assert!(group(&[], ListMode::Working).is_empty());
}

#[test]
fn counts_read_as_the_list_shows_them() {
    assert_eq!(file_counts(&ChangedFile { added: 28, removed: 4, ..file("a", FileStatus::Modified, false, true) }), "+28 \u{2212}4");
    assert_eq!(file_counts(&ChangedFile { added: 0, ..file("a", FileStatus::Untracked, false, true) }), "new");
    assert_eq!(file_counts(&ChangedFile { binary: true, ..file("a", FileStatus::Modified, false, true) }), "binary");
    assert_eq!(files_count(1), "1 file");
    assert_eq!(files_count(5), "5 files");
}

#[test]
fn the_review_message_reads_as_designed() {
    let d = diff(
        "crates/silo-app/src/agent/view.rs",
        vec![hunk("h", 811, 811, " let text = one_line(&msg);\n-div().child(msg.clone())\n+div().child(text)\n")],
    );
    let comments = [
        comment("crates/silo-app/src/agent/view.rs", Side::New, 812, None, "why not truncate at 80?"),
        comment("docs/BLOCKS.md", Side::New, 14, Some(20), "say what a kind brings first"),
    ];
    let text =
        review_message("Turn 4", &comments, |path, side, line| (path == d.path).then(|| line_text(&d, side, line)).flatten().map(str::to_owned));
    assert_eq!(
        text,
        "Review of your changes (Turn 4):\n\
         \n\
         crates/silo-app/src/agent/view.rs:812 (new)\n\
         > div().child(text)\n\
         why not truncate at 80?\n\
         \n\
         docs/BLOCKS.md:14-20 (new)\n\
         say what a kind brings first"
    );
    assert_eq!(line_text(&d, Side::Old, 812), Some("div().child(msg.clone())"));
    assert_eq!(review_message("", &comments[..1], |_, _, _| None).lines().next(), Some("Review of your changes:"));
    // Long ranges quote their first lines and say there's more.
    let long = review_message("Turn 1", &[comment("a", Side::Old, 1, Some(9), "x")], |_, _, n| Some(format!("l{n}")));
    assert!(long.contains("> l6\n> …\nx") && !long.contains("l7"));
}

#[test]
fn commit_messages_come_from_the_first_fence() {
    assert_eq!(commit_message_from("Here:\n```\napp: fix the thing\n\nbody\n```\nmore"), "app: fix the thing\n\nbody");
    assert_eq!(commit_message_from("```text\none\n```"), "one");
    assert_eq!(commit_message_from("  just text \n"), "just text");
}

#[test]
fn the_scope_picker_lists_scopes_turns_and_the_session() {
    let t = |turn, base: &str, end: &str| TurnRef { turn, base_tree: base.into(), end_tree: end.into(), files: 2 };
    let turns = [t(3, "b3", "e3"), t(4, "b4", "e4")];
    let rows = picker_rows(&turns, true);
    let labels: Vec<String> = rows.iter().map(Pick::label).collect();
    assert_eq!(labels, ["Working", "Staged", "Unstaged", "Turn 4", "Turn 3", "This session", "Branch\u{2026}"]);
    assert_eq!(rows[5], Pick::Session { base: "b3".into(), head: "e4".into() });
    assert_eq!(rows[3].detail().as_deref(), Some("2 files"));
    // Not a repository: only the agent's turns.
    let labels: Vec<String> = picker_rows(&turns[..1], false).iter().map(Pick::label).collect();
    assert_eq!(labels, ["Turn 3", "This session"]);
    assert_eq!(picker_rows(&[], true).len(), 4, "no turns: no session row");
}

#[test]
fn branch_scopes_parse() {
    assert_eq!(parse_range("main"), Some(("main".into(), None)));
    assert_eq!(parse_range(" main..otto "), Some(("main".into(), Some("otto".into()))));
    assert_eq!(parse_range("main...otto"), Some(("main".into(), Some("otto".into()))));
    assert_eq!(parse_range("main.."), Some(("main".into(), None)));
    assert_eq!(parse_range(""), None);
    assert_eq!(parse_range("a b"), None);
}

#[test]
fn reverts_name_what_goes() {
    let h = hunk("h", 1, 1, " a\n-b\n+B\n+C\n");
    assert_eq!(changed_lines(&[&h]), 3);
    assert_eq!(revert_question("crates/x/view.rs", Some(3)), "Discard 3 changed lines in view.rs?");
    assert_eq!(revert_question("view.rs", Some(1)), "Discard 1 changed line in view.rs?");
    assert_eq!(revert_question("a/view.rs", None), "Discard every change to view.rs?");
    assert_eq!(hunk_place(&h), (Side::New, 2, Some(3)));
    assert_eq!(hunk_place(&hunk("h", 5, 4, "-x\n-y\n")), (Side::Old, 5, Some(6)));
}
