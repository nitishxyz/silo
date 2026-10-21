use otto_tools::{
    Tool, ToolCx,
    builtin::{apply_patch::ApplyPatch, shell::Shell, web_fetch::WebFetch},
    policy::*,
};
use serde_json::json;
#[test]
fn patch_rules_cover_every_path_and_move_destination() {
    let d = tempfile::tempdir().unwrap();
    let cx = ToolCx::new("policy", d.path());
    let tool = ApplyPatch::default();
    let policy = Policy {
        mode: ApprovalMode::AskOnEffects,
        rules: vec![
            Rule { tool: "apply_patch".into(), pattern: Some("**/secret".into()), decision: RuleDecision::Deny },
            Rule { tool: "apply_patch".into(), pattern: None, decision: RuleDecision::Allow },
        ],
    };
    for patch in [
        "*** Begin Patch\n*** Add File: secret\n+x\n*** End Patch",
        "*** Begin Patch\n*** Update File: safe\n*** Move to: secret\n@@\n-a\n+b\n*** End Patch",
    ] {
        assert!(matches!(policy.decide(tool.spec(), &json!(patch), &cx), Decision::Deny { .. }));
    }
    assert!(matches!(
        policy.decide(tool.spec(), &json!({"patch":"*** Begin Patch\n*** Add File: ../outside\n+x\n*** End Patch"}), &cx),
        Decision::Ask { .. }
    ));
}
#[test]
fn shell_and_url_subject_rules() {
    let d = tempfile::tempdir().unwrap();
    let cx = ToolCx::new("policy", d.path());
    let policy = Policy {
        mode: ApprovalMode::AskOnEffects,
        rules: vec![
            Rule { tool: "shell".into(), pattern: Some("cargo test*".into()), decision: RuleDecision::Allow },
            Rule { tool: "web_*".into(), pattern: Some("https://private.*".into()), decision: RuleDecision::Deny },
        ],
    };
    assert_eq!(policy.decide(Shell::default().spec(), &json!({"command":"cargo test -p tools"}), &cx), Decision::Allow);
    assert!(matches!(policy.decide(Shell::default().spec(), &json!({"command":"rm file"}), &cx), Decision::Ask { .. }));
    assert!(matches!(policy.decide(WebFetch::default().spec(), &json!({"url":"https://private.example"}), &cx), Decision::Deny { .. }));
}
