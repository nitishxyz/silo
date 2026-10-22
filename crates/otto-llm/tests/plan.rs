//! Subscription plan labels from sign-in responses.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use otto_llm::{anthropic, openai};
use serde_json::json;

fn jwt(claims: serde_json::Value) -> String {
    format!("{}.{}.sig", URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}"), URL_SAFE_NO_PAD.encode(claims.to_string()))
}

#[test]
fn claude_plan_from_token_or_profile() {
    let plan = |value| anthropic::subscription::plan(&value);
    assert_eq!(plan(json!({"organization":{"uuid":"o","organization_type":"claude_max"}})).as_deref(), Some("Max"));
    assert_eq!(plan(json!({"organization":{"organization_type":"claude_pro"}})).as_deref(), Some("Pro"));
    assert_eq!(plan(json!({"organization":{"organization_type":"claude_team"}})).as_deref(), Some("Team"));
    assert_eq!(plan(json!({"organization":{"organization_type":"claude_enterprise"}})).as_deref(), Some("Enterprise"));
    assert_eq!(plan(json!({"account":{"has_claude_max":true,"has_claude_pro":false}})).as_deref(), Some("Max"));
    assert_eq!(plan(json!({"account":{"has_claude_max":false,"has_claude_pro":true}})).as_deref(), Some("Pro"));
    assert_eq!(plan(json!({"access_token":"a","account":{"uuid":"u"}})), None);
}

#[test]
fn chatgpt_plan_from_id_token_claim() {
    let token = |kind: &str| jwt(json!({"https://api.openai.com/auth":{"chatgpt_plan_type":kind,"chatgpt_account_id":"acct"}}));
    assert_eq!(openai::subscription::plan(&token("plus")).as_deref(), Some("Plus"));
    assert_eq!(openai::subscription::plan(&token("pro")).as_deref(), Some("Pro"));
    assert_eq!(openai::subscription::plan(&token("team")).as_deref(), Some("Team"));
    assert_eq!(openai::subscription::plan(&token("enterprise")).as_deref(), Some("Enterprise"));
    assert_eq!(openai::subscription::account_id(&token("plus")).as_deref(), Some("acct"));
    assert_eq!(openai::subscription::plan(&jwt(json!({"https://api.openai.com/auth":{}}))), None);
    assert_eq!(openai::subscription::plan("not-a-jwt"), None);
}

#[test]
fn status_reports_stored_plans() {
    let dir = std::env::temp_dir().join(format!("otto-plan-{}-{}", std::process::id(), otto_llm::auth::now_ms()));
    let store = otto_llm::auth::CredentialStore::open(dir.join("auth")).unwrap();
    let oauth =
        |extra| otto_llm::auth::Credential::OAuth { access: "a".into(), refresh: "r".into(), expires_at_ms: u64::MAX, account_id: None, extra };
    store.set("anthropic", oauth(json!({"plan":"Max"}))).unwrap();
    store.set("openai", oauth(json!({"id_token":jwt(json!({"https://api.openai.com/auth":{"chatgpt_plan_type":"plus"}}))}))).unwrap();
    store.set("xai", otto_llm::auth::Credential::ApiKey { key: "k".into() }).unwrap();
    let providers = otto_llm::Providers::new(dir.join("auth"), dir.clone()).unwrap();
    let plan = |id: &str| providers.status().into_iter().find(|s| s.provider == id).unwrap().plan;
    assert_eq!(plan("anthropic").as_deref(), Some("Max"));
    assert_eq!(plan("openai").as_deref(), Some("Plus"));
    assert_eq!(plan("xai"), None);
    std::fs::remove_dir_all(dir).unwrap();
}
