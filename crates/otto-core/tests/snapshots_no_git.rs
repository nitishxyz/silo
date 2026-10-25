//! A turn with no `git` on PATH still finishes `done` (a process of its own: it changes PATH).
#[path = "snapshot_util.rs"]
mod util;
use otto_core::SessionDefaults;
use otto_proto::{TurnChanges, TurnStatus};
use util::*;

#[tokio::test]
async fn a_turn_without_git_on_path_finishes_from_the_edit_records() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    repo(&repo_dir);
    let empty = dir.path().join("bin");
    std::fs::create_dir_all(&empty).unwrap();
    // SAFETY: the only test in this binary, set before the runtime starts any thread.
    unsafe { std::env::set_var("PATH", &empty) };
    let steps = vec![vec![call("w", "write", serde_json::json!({"path":"written.txt","content":"a\n"}))]];
    let t = turn(dir.path(), &repo_dir, steps, SessionDefaults::default()).await;
    let (status, changes) = t.finished();
    assert_eq!(status, TurnStatus::Done);
    assert_eq!(changes, Some(TurnChanges { files: vec!["written.txt".into()], added: 1, ..Default::default() }));
    assert_eq!(t.trees(), (None, None));
    t.runtime.shutdown().await.unwrap();
}
