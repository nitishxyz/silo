use otto_tools::{Tool, ToolCx, builtin::*};
use serde_json::json;
use std::path::Path;

fn cx(dir: &Path, session: &str) -> ToolCx {
    let mut cx = ToolCx::new("undo", dir);
    cx.sessions = vec![session.into()];
    cx
}

#[tokio::test]
async fn undo_takes_back_a_sessions_edits_newest_first() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("a.txt");
    let read = || std::fs::read_to_string(&file).unwrap();
    let undo = undo_edit::UndoEdit::default();
    assert_eq!(undo.spec().name, "undo_edit");
    let nothing = undo.call(json!({"path":"a.txt"}), cx(d.path(), "s1")).await.unwrap_err().to_string();
    assert!(nothing.contains("Nothing to undo"), "{nothing}");
    write::Write::default().call(json!({"path":"a.txt","content":"one\n"}), cx(d.path(), "s1")).await.unwrap();
    edit::Edit::default().call(json!({"path":"a.txt","old_string":"one","new_string":"two"}), cx(d.path(), "s1")).await.unwrap();
    multiedit::Multiedit::default()
        .call(json!({"path":"a.txt","edits":[{"old_string":"two","new_string":"three"}]}), cx(d.path(), "s1"))
        .await
        .unwrap();
    // Another session has nothing of its own to take back.
    assert!(undo.call(json!({"path":"a.txt"}), cx(d.path(), "s2")).await.unwrap_err().to_string().contains("Nothing to undo"));
    let out = undo.call(json!({"path":"a.txt"}), cx(d.path(), "s1")).await.unwrap();
    assert!(out.metadata["diff"].as_str().unwrap().contains("+two"));
    assert_eq!(read(), "two\n");
    undo.call(json!({"path":"a.txt"}), cx(d.path(), "s1")).await.unwrap();
    assert_eq!(read(), "one\n");
    // The write created the file: undoing it removes the file.
    undo.call(json!({"path":"a.txt"}), cx(d.path(), "s1")).await.unwrap();
    assert!(!file.exists());
    assert!(undo.call(json!({"path":"a.txt"}), cx(d.path(), "s1")).await.unwrap_err().to_string().contains("Nothing to undo"));
}

#[tokio::test]
async fn undo_refuses_a_file_changed_since_and_covers_patches() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("b.txt");
    std::fs::write(&file, "keep\n").unwrap();
    let undo = undo_edit::UndoEdit::default();
    edit::Edit::default().call(json!({"path":"b.txt","old_string":"keep","new_string":"mine"}), cx(d.path(), "p1")).await.unwrap();
    std::fs::write(&file, "theirs\n").unwrap();
    let changed = undo.call(json!({"path":"b.txt"}), cx(d.path(), "p1")).await.unwrap_err().to_string();
    assert!(changed.contains("changed after"), "{changed}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
    apply_patch::ApplyPatch::default()
        .call(json!("*** Begin Patch\n*** Update File: b.txt\n@@\n-theirs\n+patched\n*** End Patch"), cx(d.path(), "p1"))
        .await
        .unwrap();
    undo.call(json!({"path":"b.txt"}), cx(d.path(), "p1")).await.unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
}
