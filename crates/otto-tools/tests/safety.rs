use otto_tools::{
    Tool, ToolCx,
    builtin::{
        apply_patch::ApplyPatch,
        edit::Edit,
        multiedit::Multiedit,
        read::Read,
        shell::{Job, Jobs, Shell},
        write::Write,
    },
    policy::Policy,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn prepared_paths_reject_symlink_retargeting() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("inside")).unwrap();
    std::fs::write(root.path().join("inside/file"), "safe").unwrap();
    std::fs::write(outside.path().join("file"), "secret").unwrap();
    let link = root.path().join("link");
    std::os::unix::fs::symlink(root.path().join("inside"), &link).unwrap();
    let cx = ToolCx::new("guard", root.path());
    let policy = Policy::default();
    let tools: Vec<(Box<dyn Tool>, serde_json::Value)> = vec![
        (Box::new(Read::default()), json!({"path":"link/file"})),
        (Box::new(Write::default()), json!({"path":"link/file","content":"changed"})),
        (Box::new(Edit::default()), json!({"path":"link/file","old_string":"safe","new_string":"changed"})),
        (Box::new(Multiedit::default()), json!({"path":"link/file","edits":[{"old_string":"safe","new_string":"changed"}]})),
        (Box::new(ApplyPatch::default()), json!("*** Begin Patch\n*** Update File: link/file\n@@\n-safe\n+changed\n*** End Patch")),
    ];
    let prepared: Vec<_> = tools.iter().map(|(t, i)| policy.prepare(t.spec(), i, &cx).unwrap()).collect();
    assert!(prepared[0].input["path"].as_str().unwrap().ends_with("inside/file"));
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    for ((tool, _), prepared) in tools.iter().zip(prepared) {
        let error = tool.call(prepared.input, cx.clone()).await.unwrap_err();
        assert!(error.message.contains("Approved path changed"), "{error}");
    }
    assert_eq!(std::fs::read_to_string(outside.path().join("file")).unwrap(), "secret");
}
#[tokio::test]
async fn prepared_parent_identity_is_checked() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    let cx = ToolCx::new("guard", root.path());
    let tool = Write::default();
    let prepared = Policy::default().prepare(tool.spec(), &json!({"path":"dir/new","content":"no"}), &cx).unwrap();
    std::fs::rename(root.path().join("dir"), root.path().join("old")).unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    assert!(tool.call(prepared.input, cx).await.unwrap_err().message.contains("Approved path changed"));
}
#[tokio::test(flavor = "current_thread")]
async fn fifo_targets_fail_promptly_without_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::tempdir().unwrap();
    let p = root.path().join("fifo");
    let name = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let cx = ToolCx::new("fifo", root.path());
    let tools: Vec<(Box<dyn Tool>, serde_json::Value)> = vec![
        (Box::new(Read::default()), json!({"path":"fifo"})),
        (Box::new(Write::default()), json!({"path":"fifo","content":"x"})),
        (Box::new(Edit::default()), json!({"path":"fifo","old_string":"x","new_string":"y"})),
        (Box::new(Multiedit::default()), json!({"path":"fifo","edits":[{"old_string":"x","new_string":"y"}]})),
        (Box::new(ApplyPatch::default()), json!("*** Begin Patch\n*** Delete File: fifo\n*** End Patch")),
    ];
    for (tool, input) in tools {
        let start = std::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(2), tool.call(input, cx.clone())).await.unwrap().unwrap_err();
        assert!(error.message.contains("regular file"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
#[tokio::test]
async fn shell_large_output_spills_and_stays_bounded() {
    let root = tempfile::tempdir().unwrap();
    let mut cx = ToolCx::new("large", root.path());
    cx.spill_dir = Some(root.path().join("spill"));
    let output = Shell::default().call(json!({"command":"yes x | head -c 50000000","timeout_ms":20000}), cx).await.unwrap();
    assert!(!output.is_error, "{output:?}");
    assert_eq!(output.metadata["output_bytes"], 50_000_000_u64);
    assert!(output.metadata["peak_buffer_capacity_bytes"].as_u64().unwrap() <= 4 * otto_tools::output::MAX_BYTES as u64);
    assert!(output.metadata["peak_buffer_bytes"].as_u64().unwrap() <= otto_tools::output::MAX_BYTES as u64);
    assert!(output.metadata["progress_chunks"].as_u64().unwrap() <= 64);
    let path = output.truncated.unwrap().full_path.unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().len(), 50_000_000);
    let mut cx = ToolCx::new("timeout", root.path());
    cx.spill_dir = Some(root.path().join("spill"));
    let output = Shell::default().call(json!({"command":"yes x","timeout_ms":100}), cx).await.unwrap();
    assert_eq!(output.metadata["termination"], "timeout");
    assert!(output.metadata["peak_buffer_bytes"].as_u64().unwrap() <= otto_tools::output::MAX_BYTES as u64);
    assert_eq!(std::fs::metadata(output.metadata["spill_path"].as_str().unwrap()).unwrap().len(), output.metadata["output_bytes"].as_u64().unwrap());
}
#[tokio::test]
async fn shell_decodes_split_utf8_per_stream_and_removes_consumed_jobs() {
    let root = tempfile::tempdir().unwrap();
    let jobs = Arc::new(Jobs::default());
    let shell = Shell::new(jobs.clone());
    let job = Job::new(jobs);
    let mut cx = ToolCx::new("utf8", root.path());
    cx.spill_dir = Some(root.path().join("spill"));
    let output = shell
        .call(json!({"command":r"printf '\342'; printf '\360' >&2; sleep 0.1; printf '\202\254'; printf '\237\231\202' >&2"}), cx.clone())
        .await
        .unwrap();
    assert!(output.joined_text().contains('€'));
    assert!(output.joined_text().contains('🙂'));
    assert!(!output.joined_text().contains('�'));
    let id = shell.call(json!({"command":"printf done","background":true}), cx.clone()).await.unwrap().metadata["job_id"].clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = job.call(json!({"job_id":id,"operation":"status"}), cx.clone()).await.unwrap();
            if status.metadata["completed"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(job.call(json!({"job_id":id,"operation":"output"}), cx.clone()).await.unwrap().joined_text(), "done");
    assert!(job.call(json!({"job_id":id,"operation":"status"}), cx).await.is_err());
}

#[tokio::test]
async fn background_capture_is_bounded_with_temporary_spill_fallback() {
    let root = tempfile::tempdir().unwrap();
    let cx = ToolCx::new("background", root.path());
    let jobs = Arc::new(Jobs::default());
    let shell = Shell::new(jobs.clone());
    let job = Job::new(jobs);
    let id = shell.call(json!({"command":"yes b | head -c 5000000","background":true}), cx.clone()).await.unwrap().metadata["job_id"].clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = job.call(json!({"job_id":id,"operation":"status"}), cx.clone()).await.unwrap();
            if status.metadata["completed"] == true {
                break;
            }
            let output = job.call(json!({"job_id":id,"operation":"output"}), cx.clone()).await.unwrap();
            assert!(output.joined_text().len() <= otto_tools::output::MAX_BYTES);
            // Output may consume a job that completed between status and output.
            if output.metadata["exit_code"].is_number() {
                return output;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        job.call(json!({"job_id":id,"operation":"output"}), cx.clone()).await.unwrap()
    })
    .await
    .map(|output| {
        assert_eq!(output.metadata["output_bytes"], 5_000_000);
        assert!(output.metadata["peak_buffer_bytes"].as_u64().unwrap() <= otto_tools::output::MAX_BYTES as u64);
        let path = output.metadata["spill_path"].as_str().unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().len(), 5_000_000);
        std::fs::remove_file(path).unwrap();
    })
    .unwrap();
}
