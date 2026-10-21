use otto_proto::{AuthMode, LoginResult, Method, Request};
use serde_json::json;

#[test]
fn device_fields_are_additive() {
    let request: Request =
        serde_json::from_value(json!({"id":1,"method":"auth.login","params":{"provider":"openai","mode":"subscription"}})).unwrap();
    assert!(matches!(request.method, Method::AuthLogin { device: false, .. }));
    let method = Method::AuthLogin { provider: "openai".into(), mode: AuthMode::Subscription, api_key: None, device: true };
    assert_eq!(serde_json::to_value(method).unwrap()["params"]["device"], true);
    let old: LoginResult = serde_json::from_value(json!({"status":"pending","url":"https://example.test","login_id":"l"})).unwrap();
    assert!(matches!(old, LoginResult::Pending { user_code: None, verification_uri: None, interval_secs: None, .. }));
    let device = LoginResult::Pending {
        url: "https://example.test".into(),
        login_id: "l".into(),
        user_code: Some("ABCD".into()),
        verification_uri: Some("https://example.test/device".into()),
        interval_secs: Some(5),
    };
    let wire = serde_json::to_value(device).unwrap();
    assert_eq!(wire["user_code"], "ABCD");
    assert!(matches!(serde_json::from_value::<LoginResult>(wire).unwrap(), LoginResult::Pending { interval_secs: Some(5), .. }));
    assert!(serde_json::to_value(old).unwrap().get("user_code").is_none());
}
