use otto_tools::{Tool, ToolCx, builtin::apply_patch::ApplyPatch};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
#[tokio::test]
async fn optional_initial_header_append_and_preserve_executable_mode() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a");
    std::fs::write(&p, "old\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tool = ApplyPatch::default();
    let cx = || ToolCx::new("patch", d.path());
    tool.call(json!("*** Begin Patch\n*** Update File: a\n-old\n+new\n*** End Patch"), cx()).await.unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755);
    tool.call(json!("*** Begin Patch\n*** Update File: a\n*** Move to: b\n@@\n+tail\n*** End Patch"), cx()).await.unwrap();
    assert_eq!(std::fs::read_to_string(d.path().join("b")).unwrap(), "new\ntail\n");
    assert_eq!(std::fs::metadata(d.path().join("b")).unwrap().permissions().mode() & 0o777, 0o755);
}
#[tokio::test]
async fn exact_context_takes_precedence_over_trimmed_context() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a");
    std::fs::write(&p, "  target\ntarget\n").unwrap();
    ApplyPatch::default()
        .call(json!("*** Begin Patch\n*** Update File: a\n@@\n-target\n+changed\n*** End Patch"), ToolCx::new("patch", d.path()))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "  target\nchanged\n");
}
#[tokio::test]
async fn deleting_all_content_and_empty_add() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a"), "only").unwrap();
    ApplyPatch::default()
        .call(json!("*** Begin Patch\n*** Update File: a\n@@\n-only\n*** Add File: empty\n*** End Patch"), ToolCx::new("patch", d.path()))
        .await
        .unwrap();
    assert_eq!(std::fs::read(d.path().join("a")).unwrap(), b"");
    assert_eq!(std::fs::read(d.path().join("empty")).unwrap(), b"");
}
