//! Google sign-in as the Antigravity CLI does it: PKCE with the installed-app client, offline
//! access, and the Cloud Code Assist project set up on sign-in and kept in the credential.
use super::{client::endpoints, convert::PROVIDER, project::resolve};
use crate::{
    LlmError,
    auth::{
        Credential,
        flows::{self, Callback, Cx, Fut, Port, Refresh, Rotation, SubscriptionSpec},
        oauth::TokenEncoding,
    },
};
use std::time::Duration;

/// The installed-app client of the Antigravity CLI (public; embedded in its open-source plugins).
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "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";
/// The redirect Google shows the code on, for machines the browser cannot reach.
pub const PASTE_REDIRECT: &str = "https://antigravity.google/oauth-callback";

pub static SPEC: SubscriptionSpec = SubscriptionSpec {
    provider: PROVIDER,
    api_key: false,
    client_id: CLIENT_ID,
    client_secret: Some(CLIENT_SECRET),
    authorize: "https://accounts.google.com/o/oauth2/v2/auth",
    console: None,
    token: "https://oauth2.googleapis.com/token",
    scopes: &[
        "https://www.googleapis.com/auth/cloud-platform",
        "https://www.googleapis.com/auth/userinfo.email",
        "https://www.googleapis.com/auth/userinfo.profile",
        "https://www.googleapis.com/auth/cclog",
        "https://www.googleapis.com/auth/experimentsandconfigs",
        "https://www.googleapis.com/auth/aicode",
    ],
    params: &[("access_type", "offline"), ("prompt", "consent")],
    nonce: false,
    callbacks: &[
        Callback::Loopback { host: "localhost", port: Port::Fixed(51121), path: "/oauth-callback" },
        Callback::Paste { redirect: PASTE_REDIRECT, bare_code: true },
    ],
    device: None,
    headers: None,
    exchange_encoding: TokenEncoding::Form,
    exchange_state: false,
    refresh: Refresh {
        encoding: TokenEncoding::Form,
        rotation: Rotation::Keep,
        rejected: flows::rejected_on_invalid_grant,
        rejected_message: Some("Google sign-in expired. Sign in again."),
    },
    credential: flows::plain_credential,
    finish: Some(finish),
    plan: flows::no_plan,
};

/// The Cloud Code Assist project a sign-in stored (`extra.project`).
#[must_use]
pub fn stored_project(credential: &Credential) -> Option<String> {
    match credential {
        Credential::OAuth { extra, .. } => extra["project"].as_str().filter(|p| !p.is_empty()).map(String::from),
        Credential::ApiKey { .. } => None,
    }
}

fn finish<'a>(cx: Cx<'a>, credential: Credential, _: Option<&'a Credential>) -> Fut<'a, Credential> {
    Box::pin(with_project(cx.client, endpoints(), credential, Duration::from_secs(5)))
}

/// `credential` with its project: the one it keeps (a refresh), else looked up or onboarded.
pub(super) async fn with_project(
    http: &reqwest::Client,
    bases: Vec<String>,
    mut credential: Credential,
    poll: Duration,
) -> Result<Credential, LlmError> {
    if stored_project(&credential).is_some() {
        return Ok(credential);
    }
    if let Credential::OAuth { access, extra, .. } = &mut credential {
        let project = resolve(http, &bases, access, poll).await?;
        if !extra.is_object() {
            *extra = serde_json::json!({});
        }
        extra["project"] = project.into();
    }
    Ok(credential)
}
