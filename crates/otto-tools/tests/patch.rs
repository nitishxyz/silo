use otto_tools::{Tool, ToolCx, builtin::apply_patch::ApplyPatch};
use serde_json::json;
#[tokio::test]
async fn add_update_move_delete_and_freeform() {
    let d = tempfile::tempdir().unwrap();
    let tool = ApplyPatch::default();
    let cx = || ToolCx::new("patch", d.path());
    tool.call(json!("*** Begin Patch\n*** Add File: dir/a\n+one\n+two\n+three\n*** End Patch"), cx()).await.unwrap();
    tool.call(
        json!({"input":"*** Begin Patch\n*** Update File: dir/a\n*** Move to: b\n@@ one\n-two\n+TWO\n three\n*** End of File\n*** End Patch"}),
        cx(),
    )
    .await
    .unwrap();
    assert!(!d.path().join("dir/a").exists());
    assert_eq!(std::fs::read_to_string(d.path().join("b")).unwrap(), "one\nTWO\nthree\n");
    let out = tool.call(json!({"patch":"*** Begin Patch\n*** Delete File: b\n*** End Patch"}), cx()).await.unwrap();
    assert_eq!(out.metadata["files"][0]["removed"], 3);
    assert!(!d.path().join("b").exists());
    let openai = tool.spec().presentation("openai").unwrap();
    assert_eq!((openai.name.as_str(), &openai.freeform), ("apply_patch", &Some(json!({"type":"text"}))));
    assert!(tool.spec().presentation("anthropic").is_none());
}
#[tokio::test]
async fn validation_is_atomic_across_files() {
    let d = tempfile::tempdir().unwrap();
    let tool = ApplyPatch::default();
    let cx = || ToolCx::new("patch", d.path());
    std::fs::write(d.path().join("a"), "old\n").unwrap();
    std::fs::write(d.path().join("b"), "other\n").unwrap();
    assert!(
        tool.call(json!("*** Begin Patch\n*** Update File: a\n@@\n-old\n+new\n*** Update File: b\n@@\n-missing\n+bad\n*** End Patch"), cx())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(d.path().join("a")).unwrap(), "old\n");
    assert!(tool.call(json!("*** Begin Patch\n*** Add File: c\n+new\n*** Delete File: absent\n*** End Patch"), cx()).await.is_err());
    assert!(!d.path().join("c").exists());
}
#[tokio::test]
async fn whitespace_matching_multiple_hunks_and_eof() {
    let d = tempfile::tempdir().unwrap();
    let tool = ApplyPatch::default();
    let cx = || ToolCx::new("patch", d.path());
    let p = d.path().join("a");
    std::fs::write(&p, "alpha   \n  middle\ngamma\nbeta\n").unwrap();
    tool.call(
        json!("*** Begin Patch\n*** Update File: a\n@@\n-alpha\n+ALPHA\n@@\n-middle\n+MIDDLE\n@@\n-beta\n+END\n*** End of File\n*** End Patch"),
        cx(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "ALPHA\nMIDDLE\ngamma\nEND\n");
}
#[tokio::test]
async fn trimmed_context_and_insertions() {
    let d = tempfile::tempdir().unwrap();
    let tool = ApplyPatch::default();
    let cx = || ToolCx::new("patch", d.path());
    let p = d.path().join("a");
    std::fs::write(&p, "  alpha \nbeta\n").unwrap();
    tool.call(json!("*** Begin Patch\n*** Update File: a\n@@\n-alpha\n+ALPHA\n@@\n+tail\n*** End of File\n*** End Patch"), cx()).await.unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "ALPHA\nbeta\ntail\n");
}
#[tokio::test]
async fn malformed_and_duplicate_operations_rejected() {
    let d = tempfile::tempdir().unwrap();
    let tool = ApplyPatch::default();
    for patch in [
        "",
        "*** Begin Patch\n*** End Patch",
        "*** Begin Patch\n*** Add File: a\nbad\n*** End Patch",
        "*** Begin Patch\n*** Update File: a\n@@\n*** End Patch",
        "*** Begin Patch\n*** Unknown: a\n*** End Patch",
        "*** Begin Patch\n*** Add File: a\n+x\n*** Add File: a\n+y\n*** End Patch",
    ] {
        assert!(tool.call(json!(patch), ToolCx::new("patch", d.path())).await.is_err(), "{patch}");
        assert!(!d.path().join("a").exists());
    }
}
#[tokio::test]
async fn io_failure_rolls_back_previous_files() {
    let d = tempfile::tempdir().unwrap();
    let tool = ApplyPatch::default();
    std::fs::write(d.path().join("a"), "old\n").unwrap();
    std::fs::write(d.path().join("z"), "not a directory").unwrap();
    assert!(
        tool.call(
            json!("*** Begin Patch\n*** Update File: a\n@@\n-old\n+new\n*** Add File: z/file\n+bad\n*** End Patch"),
            ToolCx::new("patch", d.path())
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(d.path().join("a")).unwrap(), "old\n");
}
