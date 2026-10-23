use otto_tools::{OutputPart, Tool, ToolCx, builtin::*, policy::*, registry::*};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
fn cx(dir: &Path) -> ToolCx {
    ToolCx::new("test", dir)
}
async fn call(tool: &dyn Tool, input: Value, dir: &Path) -> otto_tools::ToolOutput {
    tool.call(input, cx(dir)).await.unwrap()
}
#[tokio::test]
async fn read_write_images_and_guards() {
    let d = tempfile::tempdir().unwrap();
    let out = call(&write::Write::default(), json!({"path":"nested/a","content":"one\ntwo\nthree\n"}), d.path()).await;
    assert_eq!(out.metadata["added"], 3);
    assert!(out.metadata["diff"].as_str().unwrap().contains("+two"));
    let out = call(&read::Read::default(), json!({"path":"nested/a","offset":2,"limit":1}), d.path()).await;
    assert_eq!(out.joined_text(), "2: two\n");
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(4, 3).write_to(&mut png, image::ImageFormat::Png).unwrap();
    std::fs::write(d.path().join("image"), png.into_inner()).unwrap();
    assert!(matches!(call(&read::Read::default(), json!({"path":"image"}), d.path()).await.content[1], OutputPart::Image { .. }));
    std::fs::write(d.path().join("binary"), b"a\0b").unwrap();
    assert!(read::Read::default().call(json!({"path":"binary"}), cx(d.path())).await.is_err());
    let f = std::fs::File::create(d.path().join("huge")).unwrap();
    f.set_len(21 * 1024 * 1024).unwrap();
    assert!(read::Read::default().call(json!({"path":"huge"}), cx(d.path())).await.is_err());
}
#[tokio::test]
async fn edits_exact_fallback_ambiguity_and_atomicity() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("a");
    std::fs::write(&p, "a a\n\tfoo   bar\n").unwrap();
    let edit = edit::Edit::default();
    assert!(edit.call(json!({"path":"a","old_string":"a","new_string":"b"}), cx(d.path())).await.is_err());
    call(&edit, json!({"path":"a","old_string":"a a","new_string":"b b"}), d.path()).await;
    call(&edit, json!({"path":"a","old_string":"foo bar","new_string":"done"}), d.path()).await;
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "b b\ndone\n");
    assert!(
        multiedit::Multiedit::default()
            .call(
                json!({"path":"a","edits":[{"old_string":"done","new_string":"changed"},{"old_string":"missing","new_string":"oops"}]}),
                cx(d.path())
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "b b\ndone\n");
    call(&edit, json!({"path":"a","old_string":"b","new_string":"c","replace_all":true}), d.path()).await;
    call(
        &multiedit::Multiedit::default(),
        json!({"path":"a","edits":[{"old_string":"done","new_string":"ok"},{"old_string":"ok","new_string":"yes"}]}),
        d.path(),
    )
    .await;
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "c c\nyes\n");
    std::fs::write(&p, "  foo  bar\n\tfoo bar\n").unwrap();
    assert!(edit.call(json!({"path":"a","old_string":"foo   bar","new_string":"x","replace_all":true}), cx(d.path())).await.is_err());
    assert!(edit.call(json!({"path":"a","old_string":"","new_string":"x"}), cx(d.path())).await.is_err());
}
#[tokio::test]
async fn discovery_respects_ignore_and_limits() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join("sub")).unwrap();
    for (p, text) in
        [(".gitignore", "ignored\n"), ("ignored", "HELLO"), ("a.rs", "hello\nHELLO\n"), ("sub/b.rs", "Hello\n"), ("sub/c.txt", "Hello\n")]
    {
        std::fs::write(d.path().join(p), text).unwrap();
    }
    let out = call(&ls::Ls::default(), json!({}), d.path()).await.joined_text();
    assert!(out.contains("sub/"));
    assert!(!out.contains("ignored"));
    assert!(!out.contains("b.rs"));
    let out = call(&tree::Tree::default(), json!({"depth":1,"ignore":["*.rs"]}), d.path()).await.joined_text();
    assert!(!out.contains("a.rs"));
    assert!(!out.contains("b.rs"));
    let out = call(&glob::Glob::default(), json!({"pattern":"**/*.rs"}), d.path()).await.joined_text();
    assert!(out.contains("a.rs"));
    assert!(out.contains("b.rs"));
    assert!(!out.contains("c.txt"));
    let out = call(&search::Search::default(), json!({"pattern":"hello","case_insensitive":true,"glob":"**/*.rs","max_results":2}), d.path())
        .await
        .joined_text();
    assert_eq!(out.lines().count(), 2);
    assert!(out.contains("a.rs:1:hello"));
    assert!(search::Search::default().call(json!({"pattern":"["}), cx(d.path())).await.is_err());
    std::fs::create_dir_all(d.path().join(".git/hooks")).unwrap();
    std::fs::write(d.path().join(".git/hooks/pre-commit"), "TODO hook\n").unwrap();
    std::fs::write(d.path().join(".env.example"), "TODO dotfile\n").unwrap();
    let out = call(&search::Search::default(), json!({"pattern":"TODO"}), d.path()).await.joined_text();
    assert!(out.contains(".env.example") && !out.contains(".git/"), "{out}");
    assert!(!call(&ls::Ls::default(), json!({}), d.path()).await.joined_text().contains(".git/"));
    assert_eq!(call(&search::Search::default(), json!({"pattern":"absent-zz"}), d.path()).await.joined_text(), "No matches\n");
    assert_eq!(call(&glob::Glob::default(), json!({"pattern":"**/*.zz"}), d.path()).await.joined_text(), "No matches\n");
}
#[test]
fn output_spills_unicode_head_and_tail() {
    let d = tempfile::tempdir().unwrap();
    let mut context = cx(d.path());
    context.spill_dir = Some(d.path().join("spill"));
    let text = format!("HEAD\n{}\nTAIL", "🙂 a\n".repeat(10000));
    let output = otto_tools::output::limit(text.clone(), &context).unwrap();
    assert!(output.joined_text().starts_with("HEAD"));
    assert!(output.joined_text().ends_with("TAIL"));
    assert!(output.joined_text().len() <= otto_tools::output::MAX_BYTES);
    assert!(output.joined_text().lines().count() <= otto_tools::output::MAX_LINES);
    assert_eq!(std::fs::read_to_string(output.truncated.unwrap().full_path.unwrap()).unwrap(), text);
}
#[test]
fn policy_modes_rules_and_redaction() {
    let d = tempfile::tempdir().unwrap();
    let context = cx(d.path());
    let write = write::Write::default();
    let read = read::Read::default();
    assert_eq!(ApprovalMode::default(), ApprovalMode::Auto);
    let mut policy = Policy { mode: ApprovalMode::AskOnEffects, ..Default::default() };
    assert!(matches!(policy.decide(write.spec(), &json!({"path":"a"}), &context), Decision::Ask { .. }));
    assert_eq!(policy.decide(read.spec(), &json!({"path":"a"}), &context), Decision::Allow);
    policy.rules.push(Rule { tool: "wri*".into(), pattern: None, decision: RuleDecision::Allow });
    assert_eq!(policy.decide(write.spec(), &json!({"path":"a"}), &context), Decision::Allow);
    assert!(matches!(policy.decide(write.spec(), &json!({"path":"../outside"}), &context), Decision::Ask { .. }));
    policy.mode = ApprovalMode::Plan;
    assert!(matches!(policy.decide(write.spec(), &json!({"path":"a"}), &context), Decision::Deny { .. }));
    policy.mode = ApprovalMode::Auto;
    assert_eq!(policy.decide(write.spec(), &json!({"path":"../outside"}), &context), Decision::Allow);
    policy.mode = ApprovalMode::AskAll;
    assert!(matches!(policy.decide(read.spec(), &json!({"path":"a"}), &context), Decision::Ask { .. }));
    assert_eq!(
        redact(&json!({"password":"secret","nested":["hidden"]}), &["/password".into(), "/nested/0".into()]),
        json!({"password":"[redacted]","nested":["[redacted]"]})
    );
    policy.save(&d.path().join("project.json")).unwrap();
    Policy { mode: ApprovalMode::Auto, rules: vec![Rule { tool: "*".into(), pattern: None, decision: RuleDecision::Deny }] }
        .save(&d.path().join("user.json"))
        .unwrap();
    let loaded = Policy::load(&d.path().join("project.json"), &d.path().join("user.json"), ApprovalMode::Auto).unwrap();
    assert_eq!(loaded.rules.len(), 2);
    assert_eq!(loaded.rules[0].tool, "wri*");
}
#[test]
fn paths_follow_existing_symlink_prefixes() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), project.path().join("link")).unwrap();
    let resolved = resolve_path(project.path(), project.path(), "link/new/file").unwrap();
    assert_eq!(resolved.location, Location::Outside);
    assert_eq!(resolved.path, std::fs::canonicalize(outside.path()).unwrap().join("new/file"));
    assert_eq!(resolve_path(project.path(), project.path(), "sub/../a").unwrap().location, Location::Project);
}
#[tokio::test]
async fn shell_output_exit_timeout_cancellation_and_jobs() {
    let d = tempfile::tempdir().unwrap();
    let shell = shell::Shell::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut context = cx(d.path());
    context.progress = Some(tx);
    let out = shell.call(json!({"command":"printf out; printf err >&2; printf \"$OTTO_SILO:$PAGER:$GIT_PAGER\"; exit 7"}), context).await.unwrap();
    assert!(out.is_error);
    assert_eq!(out.metadata["exit_code"], 7);
    assert!(out.joined_text().ends_with("[exit code 7]"));
    assert_eq!(call(&shell, json!({"command":"exit 3"}), d.path()).await.joined_text(), "[exit code 3]");
    assert!(out.joined_text().contains("out"));
    assert!(out.joined_text().contains("err"));
    assert!(out.joined_text().contains("1:cat:cat"));
    assert!(rx.try_recv().is_ok());
    let out = call(&shell, json!({"command":"sleep 10 & wait","timeout_ms":50}), d.path()).await;
    assert!(out.is_error);
    assert_eq!(out.metadata["termination"], "timeout");
    assert!(out.joined_text().ends_with("[stopped: timeout]"));
    let context = cx(d.path());
    context.cancel.cancel();
    let out = shell.call(json!({"command":"sleep 10"}), context).await.unwrap();
    assert_eq!(out.metadata["termination"], "cancelled");
    let jobs = Arc::new(shell::Jobs::default());
    let shell = shell::Shell::new(jobs.clone());
    let job = shell::Job::new(jobs);
    let id = call(&shell, json!({"command":"printf background","background":true}), d.path()).await.metadata["job_id"].clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let out = call(&job, json!({"job_id":id,"operation":"status"}), d.path()).await;
            if out.metadata["completed"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(call(&job, json!({"job_id":id,"operation":"output"}), d.path()).await.joined_text(), "background");
    let id = call(&shell, json!({"command":"sleep 10","background":true}), d.path()).await.metadata["job_id"].clone();
    call(&job, json!({"job_id":id,"operation":"kill"}), d.path()).await;
}
struct Answers;
#[async_trait::async_trait]
impl otto_tools::Approver for Answers {
    async fn approve(&self, _: &str, _: &str, _: Value) -> bool {
        true
    }
    async fn ask(&self, _: &str, question: &str, options: &[String]) -> Option<String> {
        assert_eq!(question, "Choose");
        Some(options[0].clone())
    }
}
#[tokio::test]
async fn ask_and_registry() {
    let d = tempfile::tempdir().unwrap();
    let mut context = cx(d.path());
    context.approver = Some(Arc::new(Answers));
    assert_eq!(ask_user::AskUser::default().call(json!({"question":"Choose","options":["yes"]}), context).await.unwrap().joined_text(), "yes");
    assert!(ask_user::AskUser::default().call(json!({"question":"Choose"}), cx(d.path())).await.is_err());
    for family in ["openai", "anthropic", "generic"] {
        let mut registry = Registry::new();
        for tool in tools_for_family(family) {
            registry.add(tool);
        }
        assert_eq!(registry.lookup("apply_patch").is_some(), family == "openai");
        assert_eq!(registry.lookup("edit").is_some(), family != "openai");
        assert!(registry.lookup("update_todos").is_some());
        assert!(!registry.list_specs().is_empty());
        // Every family gets our full shell, under our name.
        let shell = registry.lookup("shell").unwrap();
        assert!(shell.spec().presentation(family).is_none());
        let properties = &shell.spec().input_schema["properties"];
        assert!(["command", "cwd", "timeout_ms", "background"].iter().all(|k| properties.get(k).is_some()));
        assert!(shell.spec().description.contains("don't cd into it"));
    }
    assert!(web_fetch::WebFetch::default().call(json!({"url":"file:///etc/passwd"}), cx(d.path())).await.is_err());
}
#[tokio::test]
async fn update_todos_acks_progress() {
    let d = tempfile::tempdir().unwrap();
    let todos = json!({"todos":[{"step":"Read","status":"completed"},{"step":"Fix","status":"in_progress"},{"step":"Test","status":"pending"}]});
    assert_eq!(call(&update_todos::UpdateTodos::default(), todos, d.path()).await.joined_text(), "1/3 done");
    let bad = json!({"todos":[{"step":"Read","status":"done"}]});
    assert!(update_todos::UpdateTodos::default().call(bad, cx(d.path())).await.is_err());
}
