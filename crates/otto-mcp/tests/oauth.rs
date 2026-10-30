//! OAuth against the fake authorization server: discovery from the `401`, dynamic
//! registration, PKCE S256 with the `resource` parameter, the loopback redirect or a
//! pasted callback URL, stored tokens, refresh after a `401`, and sign-out.
use otto_mcp::{Event, Mcp, Status, fake::Fake};
use otto_tools::{ProjectSources, ToolCx};
use serde_json::json;
use std::time::Duration;

fn project(url: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".mcp.json"), json!({"mcpServers":{"remote":{"type":"http","url":url}}}).to_string()).unwrap();
    dir
}

async fn status(events: &mut tokio::sync::broadcast::Receiver<Event>, want: Status) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Event::Status { status, .. } = events.recv().await.unwrap()
                && status == want
            {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {want:?}"));
}

async fn echo(mcp: &Mcp, dir: &tempfile::TempDir) -> Result<String, String> {
    let tools = mcp.tools(dir.path()).await;
    let echo = tools.iter().find(|t| t.spec().name == "mcp__remote__echo").ok_or("no echo tool")?;
    echo.call(json!({"text":"signed"}), ToolCx::new("c", dir.path())).await.map(|o| o.joined_text()).map_err(|e| e.message)
}

#[tokio::test]
async fn sign_in_refresh_and_sign_out() {
    let fake = Fake::start(true).await;
    let dir = project(&fake.url);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let mut events = mcp.subscribe();
    let listed = mcp.list(Some(dir.path())).await;
    assert_eq!(listed[0].status, Status::Connecting);
    status(&mut events, Status::NeedsSignIn).await;
    assert_eq!(mcp.list(Some(dir.path())).await[0].status, Status::NeedsSignIn);
    assert!(mcp.tools(dir.path()).await.is_empty());

    let login = mcp.login(Some(dir.path()), "remote").await.unwrap();
    let url = reqwest::Url::parse(&login.url).unwrap();
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(url.path(), "/authorize");
    assert_eq!(query["client_id"], "client-1");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["resource"], fake.url);
    assert_eq!(query["scope"], "mcp");
    assert_eq!(query["redirect_uri"], format!("http://127.0.0.1:{}/callback", login.port));
    assert_eq!(login.path, "/callback");
    assert_eq!(fake.state().registered[0]["token_endpoint_auth_method"], "none");
    let page = Fake::browse(&login.url).await.unwrap();
    assert!(page.contains("Authorization complete"), "{page}");
    let (name, root) = mcp.login_complete(&login.login_id, None).await.unwrap();
    assert_eq!((name.as_str(), root.as_deref()), ("remote", Some(dir.path())));
    status(&mut events, Status::Connected).await;
    assert_eq!(echo(&mcp, &dir).await.unwrap(), "signed");
    assert!(std::fs::read_to_string(dir.path().join("tokens/auth.json")).unwrap().contains(&format!("mcp:{}", fake.url)));

    // A rejected token is refreshed (with the resource) and the request retried.
    fake.expire_tokens();
    assert_eq!(echo(&mcp, &dir).await.unwrap(), "signed");
    let refresh = fake.state().token_requests.last().cloned().unwrap();
    assert_eq!((refresh["grant_type"].as_str(), refresh["resource"].as_str()), ("refresh_token", fake.url.as_str()));

    mcp.logout(Some(dir.path()), "remote").await.unwrap();
    status(&mut events, Status::NeedsSignIn).await;
    mcp.shutdown().await;
}

#[tokio::test]
async fn a_pasted_callback_url_finishes_sign_in() {
    let fake = Fake::start(true).await;
    let dir = project(&fake.url);
    let mcp = Mcp::new(dir.path().join("user"), dir.path().join("tokens"));
    let login = mcp.login(Some(dir.path()), "remote").await.unwrap();
    let browser = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let response = browser.get(&login.url).send().await.unwrap();
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    assert!(mcp.login_complete(&login.login_id, Some("http://127.0.0.1:1/callback?code=x&state=wrong")).await.is_err());
    mcp.login_complete(&login.login_id, Some(&location)).await.unwrap();
    assert!(mcp.login_complete(&login.login_id, None).await.is_err(), "spent");
    assert_eq!(echo(&mcp, &dir).await.unwrap(), "signed");
    // A server with its own Authorization header does not sign in.
    let config = json!({"mcpServers":{"remote":{"type":"http","url":fake.url,"headers":{"Authorization":"Bearer x"}}}});
    std::fs::write(dir.path().join(".mcp.json"), config.to_string()).unwrap();
    assert!(mcp.login(Some(dir.path()), "remote").await.unwrap_err().contains("Authorization"));
    mcp.shutdown().await;
}
