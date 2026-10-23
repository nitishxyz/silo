use otto_proto::{AuthMode, LoginCallback, LoginResult, Method, Request};
use serde_json::json;

#[test]
fn device_fields_are_additive() {
    let request: Request =
        serde_json::from_value(json!({"id":1,"method":"auth.login","params":{"provider":"openai","mode":"subscription"}})).unwrap();
    assert!(matches!(request.method, Method::AuthLogin { device: false, .. }));
    let method =
        Method::AuthLogin { provider: "openai".into(), mode: AuthMode::Subscription, api_key: None, device: true, base_url: None, paste: false };
    assert_eq!(serde_json::to_value(method).unwrap()["params"]["device"], true);
    let old: LoginResult = serde_json::from_value(json!({"status":"pending","url":"https://example.test","login_id":"l"})).unwrap();
    assert!(matches!(
        old,
        LoginResult::Pending { user_code: None, verification_uri: None, interval_secs: None, needs_code: false, callback: None, .. }
    ));
    let device = LoginResult::Pending {
        url: "https://example.test".into(),
        login_id: "l".into(),
        user_code: Some("ABCD".into()),
        verification_uri: Some("https://example.test/device".into()),
        interval_secs: Some(5),
        needs_code: false,
        callback: None,
        accepts_code: false,
    };
    let wire = serde_json::to_value(device).unwrap();
    assert_eq!(wire["user_code"], "ABCD");
    assert!(wire.get("callback").is_none());
    assert!(matches!(serde_json::from_value::<LoginResult>(wire).unwrap(), LoginResult::Pending { interval_secs: Some(5), .. }));
    let old = serde_json::to_value(old).unwrap();
    assert!(old.get("user_code").is_none() && old.get("needs_code").is_none());
    let browser: LoginResult = serde_json::from_value(
        json!({"status":"pending","url":"https://example.test","login_id":"l","callback":{"port":1455,"path":"/auth/callback"}}),
    )
    .unwrap();
    assert!(matches!(browser, LoginResult::Pending { callback: Some(LoginCallback { port: 1455, ref path }), .. } if path == "/auth/callback"));
}

#[test]
fn project_open_and_hello_fields_are_additive() {
    let request: Request = serde_json::from_value(json!({"id":1,"method":"project.open","params":{"root":"/r"}})).unwrap();
    assert!(matches!(request.method, Method::ProjectOpen { ref root, cwd: None } if root == "/r"));
    let request: Request = serde_json::from_value(json!({"id":1,"method":"project.open","params":{"cwd":"/r/src"}})).unwrap();
    assert!(matches!(request.method, Method::ProjectOpen { ref root, cwd: Some(ref cwd) } if root.is_empty() && cwd == "/r/src"));
    let hello: otto_proto::ServerHello =
        serde_json::from_value(json!({"protocol":1,"version":"0","server_id":"s","pid":1,"started_at":0,"client_id":"c"})).unwrap();
    assert!(hello.home.is_empty());
}
