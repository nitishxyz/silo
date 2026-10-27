//! Turn snapshots (docs/CHANGES_BLOCK.md §4): a turn in a git work tree records its base and end trees and
//! what it changed, through a copy of the index; outside git the edit records name the files; nothing here
//! fails a turn. Offline (scripted model, a temp repo).
#[path = "snapshot_util.rs"]
mod util;
use otto_core::SessionDefaults;
use otto_llm::Role;
use otto_proto::{TurnChanges, TurnStatus};
use util::*;

#[tokio::test]
async fn a_turn_in_a_repo_records_both_trees_and_its_changes_without_touching_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    repo(&repo_dir);
    let index = std::fs::read(repo_dir.join(".git/index")).unwrap();
    let head_tree = git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
    let t = turn(dir.path(), &repo_dir, edits(), SessionDefaults::default()).await;
    let (status, changes) = t.finished();
    assert_eq!(status, TurnStatus::Done);
    let changes = changes.expect("the turn changed files");
    // The edit tool's file and both of the shell's: tracked and new; ignored and untouched files aren't listed.
    assert_eq!(changes.files, ["made.txt", "tracked.txt", "written.txt"]);
    assert_eq!((changes.added, changes.removed, changes.more), (4, 0, 0));
    let (base, end) = t.trees();
    assert_eq!((changes.base_tree.clone(), changes.end_tree.clone()), (base.clone(), end.clone()));
    let (base, end) = (base.unwrap(), end.unwrap());
    // The base is the work tree as the turn started: HEAD plus the staged file, not the ignored one.
    assert_ne!(base, head_tree);
    assert_eq!(git(&repo_dir, &["ls-tree", "--name-only", &base]).lines().collect::<Vec<_>>(), [".gitignore", "staged.txt", "tracked.txt"]);
    assert_eq!(git(&repo_dir, &["show", &format!("{end}:tracked.txt")]), "one\ntwo");
    assert_eq!(git(&repo_dir, &["show", &format!("{end}:written.txt")]), "a\nb");
    // The user's index is byte for byte as it was, no lock is left, and the status is the user's own.
    assert_eq!(std::fs::read(repo_dir.join(".git/index")).unwrap(), index);
    assert!(!repo_dir.join(".git/index.lock").exists());
    assert_eq!(git(&repo_dir, &["diff", "--cached", "--name-only"]), "staged.txt");
    let untracked = git(&repo_dir, &["ls-files", "--others", "--exclude-standard"]);
    assert_eq!(untracked.lines().collect::<Vec<_>>(), ["made.txt", "written.txt"]);
    // A reopened thread gets the changes under the turn's reply.
    let view = t.runtime.view(&t.session).await.unwrap();
    let reply = view.items.iter().rev().find(|i| i.role == Role::Assistant).unwrap();
    assert_eq!(view.turns.len(), 1);
    assert_eq!((view.turns[0].turn, view.turns[0].item_id.as_deref()), (1, Some(reply.id.as_str())));
    assert_eq!(view.turns[0].changes, changes);
    t.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_turn_that_changes_nothing_reports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    repo(&repo_dir);
    let t = turn(dir.path(), &repo_dir, vec![], SessionDefaults::default()).await;
    assert_eq!(t.finished(), (TurnStatus::Done, None));
    let (base, end) = t.trees();
    assert!(base.is_some() && base == end);
    assert!(t.runtime.view(&t.session).await.unwrap().turns.is_empty());
    t.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_worktree_snapshots_through_its_own_index() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("repo");
    repo(&main);
    let tree = dir.path().join("wt");
    git(&main, &["worktree", "add", "-q", tree.to_str().unwrap()]);
    let index_path = git(&tree, &["rev-parse", "--path-format=absolute", "--git-path", "index"]);
    let (index, main_index) = (std::fs::read(&index_path).unwrap(), std::fs::read(main.join(".git/index")).unwrap());
    let t = turn(dir.path(), &tree, edits(), SessionDefaults::default()).await;
    let changes = t.finished().1.unwrap();
    assert_eq!(changes.files, ["made.txt", "tracked.txt", "written.txt"]);
    assert_eq!(std::fs::read(&index_path).unwrap(), index);
    assert_eq!(std::fs::read(main.join(".git/index")).unwrap(), main_index);
    t.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn outside_git_the_edit_records_name_the_files() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("plain");
    std::fs::create_dir_all(&cwd).unwrap();
    let t = turn(dir.path(), &cwd, edits(), SessionDefaults::default()).await;
    let (status, changes) = t.finished();
    assert_eq!(status, TurnStatus::Done);
    // The shell's changes aren't seen without git; the edit tool's are, with their lines.
    assert_eq!(changes, Some(TurnChanges { files: vec!["written.txt".into()], added: 2, ..Default::default() }));
    assert_eq!(t.trees(), (None, None));
    t.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_broken_repo_still_finishes_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    repo(&repo_dir);
    std::fs::write(repo_dir.join(".git/index"), b"not an index").unwrap();
    let t = turn(dir.path(), &repo_dir, edits(), SessionDefaults::default()).await;
    assert_eq!(t.finished(), (TurnStatus::Done, None));
    assert_eq!(t.trees(), (None, None));
    assert_eq!(std::fs::read(repo_dir.join(".git/index")).unwrap(), b"not an index");
    assert!(repo_dir.join("written.txt").exists() && repo_dir.join("made.txt").exists());
    t.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn untracked_files_over_the_limit_skip_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    repo(&repo_dir);
    let big = std::fs::File::create(repo_dir.join("big.bin")).unwrap();
    big.set_len(51 * 1024 * 1024).unwrap();
    let t = turn(dir.path(), &repo_dir, edits(), SessionDefaults::default()).await;
    assert_eq!(t.finished(), (TurnStatus::Done, None));
    assert_eq!(t.trees(), (None, None));
    t.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn turn_snapshots_off_takes_none() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    repo(&repo_dir);
    let objects = git(&repo_dir, &["count-objects", "-v"]);
    let t = turn(dir.path(), &repo_dir, edits(), SessionDefaults { turn_snapshots: false, ..Default::default() }).await;
    assert_eq!(t.finished(), (TurnStatus::Done, None));
    assert_eq!(t.trees(), (None, None));
    // No tree or blob was written.
    assert_eq!(git(&repo_dir, &["count-objects", "-v"]), objects);
    t.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reopened_thread_has_each_turns_usage_under_its_reply() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    repo(&repo_dir);
    let used = otto_llm::Usage { input: 100, output: 20, cache_read: 300, cache_write: 5, reasoning: 4 };
    let step = vec![otto_llm::StreamEvent::Usage(used.clone()), call("w", "write", serde_json::json!({"path":"a.txt","content":"a\n"}))];
    let t = turn(dir.path(), &repo_dir, vec![step], SessionDefaults::default()).await;
    let view = t.runtime.view(&t.session).await.unwrap();
    let reply = view.items.iter().rev().find(|i| i.role == Role::Assistant).unwrap();
    assert_eq!(view.turn_usage.len(), 1);
    let turn = &view.turn_usage[0];
    // Both steps (the second reported none), under the turn's last reply.
    assert_eq!((turn.turn, turn.item_id.as_deref(), &turn.usage), (1, Some(reply.id.as_str()), &used));
    t.runtime.shutdown().await.unwrap();
}
