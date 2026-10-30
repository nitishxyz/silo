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

#[tokio::test]
async fn edited_since_lists_a_sessions_files_after_a_mark() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("old.txt"), "a\nb\n").unwrap();
    write::Write::default().call(json!({"path":"before.txt","content":"x\n"}), cx(d.path(), "m1")).await.unwrap();
    let mark = undo_edit::mark();
    edit::Edit::default().call(json!({"path":"old.txt","old_string":"b","new_string":"c\nd"}), cx(d.path(), "m1")).await.unwrap();
    write::Write::default().call(json!({"path":"new.txt","content":"1\n2\n"}), cx(d.path(), "m1")).await.unwrap();
    write::Write::default().call(json!({"path":"new.txt","content":"1\n2\n3\n"}), cx(d.path(), "m1")).await.unwrap();
    write::Write::default().call(json!({"path":"other.txt","content":"z\n"}), cx(d.path(), "m2")).await.unwrap();
    let edited = undo_edit::edited_since("m1", mark);
    let names: Vec<_> = edited.iter().map(|e| (e.path.file_name().unwrap().to_str().unwrap(), e.added, e.removed)).collect();
    assert_eq!(names, [("old.txt", 2, 1), ("new.txt", 3, 0)]);
    // Taken back: it no longer counts.
    undo_edit::UndoEdit::default().call(json!({"path":"old.txt"}), cx(d.path(), "m1")).await.unwrap();
    assert_eq!(undo_edit::edited_since("m1", mark).len(), 1);
    assert!(undo_edit::edited_since("m1", undo_edit::mark()).is_empty());
}

/// A persisted history (otto's store): what an earlier process kept.
#[derive(Default)]
struct Log(std::sync::Mutex<Vec<(String, std::path::PathBuf, undo_edit::Kept)>>);
#[async_trait::async_trait]
impl undo_edit::UndoLog for Log {
    async fn push(&self, session: &str, path: &Path, before: Option<&str>, after: Option<&str>) {
        let mut log = self.0.lock().unwrap();
        let kept = undo_edit::Kept { id: i64::try_from(log.len()).unwrap(), before: before.map(Into::into), after: after.map(Into::into) };
        log.push((session.into(), path.into(), kept));
    }
    async fn last(&self, session: &str, path: &Path) -> Result<Option<undo_edit::Kept>, otto_tools::ToolError> {
        Ok(self.0.lock().unwrap().iter().rev().find(|(s, p, _)| s == session && p == path).map(|e| e.2.clone()))
    }
    async fn remove(&self, _: &str, id: i64) -> Result<(), otto_tools::ToolError> {
        self.0.lock().unwrap().retain(|e| e.2.id != id);
        Ok(())
    }
}

#[tokio::test]
async fn a_persisted_history_is_taken_back_without_memory_of_it() {
    use undo_edit::UndoLog;
    let d = tempfile::tempdir().unwrap();
    let file = std::fs::canonicalize(d.path()).unwrap().join("p.txt");
    let log = std::sync::Arc::new(Log::default());
    // What an earlier process kept: only the log knows these edits.
    log.push("persisted", &file, None, Some("one\n")).await;
    log.push("persisted", &file, Some("one\n"), Some("two\n")).await;
    std::fs::write(&file, "two\n").unwrap();
    let with_log = || {
        let mut cx = cx(d.path(), "persisted");
        cx.undo = Some(log.clone());
        cx
    };
    let undo = undo_edit::UndoEdit::default();
    undo.call(json!({"path":"p.txt"}), with_log()).await.unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
    undo.call(json!({"path":"p.txt"}), with_log()).await.unwrap();
    assert!(!file.exists(), "the first edit created it");
    assert!(undo.call(json!({"path":"p.txt"}), with_log()).await.unwrap_err().to_string().contains("Nothing to undo"));
    // New edits go to the log too.
    write::Write::default().call(json!({"path":"p.txt","content":"three\n"}), with_log()).await.unwrap();
    assert_eq!(log.last("persisted", &file).await.unwrap().unwrap().after.as_deref(), Some("three\n"));
}
