//! The Cloud Code Assist project: `v1internal:loadCodeAssist`, else `v1internal:onboardUser`
//! and polling of the long-running operation it starts.
use super::{client::call, client::endpoints};
use crate::{ErrorClass, LlmError, http::HttpClient};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const FREE_TIER: &str = "free-tier";
const LEGACY_TIER: &str = "legacy-tier";
const POLLS: usize = 10;

/// The project id of a Google account, found on first use and kept (clones share it).
#[derive(Clone)]
pub struct Project {
    http: HttpClient,
    bases: Vec<String>,
    poll: Duration,
    id: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl Project {
    /// `known`: a project id from an earlier sign-in, skipping the lookup.
    #[must_use]
    pub fn new(http: HttpClient, known: Option<String>) -> Self {
        Self { http, bases: endpoints(), poll: Duration::from_secs(5), id: Arc::new(tokio::sync::Mutex::new(known)) }
    }
    /// The project id; resolves it with `bearer` the first time.
    ///
    /// # Errors
    /// The account has no project and cannot be onboarded, or the service fails.
    pub async fn get(&self, bearer: &str) -> Result<String, LlmError> {
        let mut id = self.id.lock().await;
        if let Some(id) = id.as_ref() {
            return Ok(id.clone());
        }
        let found = resolve(self.http.client(), &self.bases, bearer, self.poll).await?;
        *id = Some(found.clone());
        Ok(found)
    }
}

/// `cloudaicompanionProject` as a string or `{id}`.
fn project_id(value: &Value) -> Option<String> {
    let project = &value["cloudaicompanionProject"];
    project.as_str().or_else(|| project["id"].as_str()).filter(|id| !id.is_empty()).map(String::from)
}

/// Look the project up, or onboard the account's default tier and wait for the operation.
///
/// # Errors
/// A tier that needs a project of the user's own, an operation that never finishes, and service errors.
pub(super) async fn resolve(http: &reqwest::Client, bases: &[String], bearer: &str, poll: Duration) -> Result<String, LlmError> {
    let metadata = json!({"ideType":"ANTIGRAVITY"});
    let loaded = call(http, bases, "/v1internal:loadCodeAssist", bearer, Some(&json!({"metadata":metadata}))).await?;
    if let Some(id) = project_id(&loaded) {
        return Ok(id);
    }
    let tier = loaded["allowedTiers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["isDefault"] == true)
        .and_then(|t| t["id"].as_str())
        .unwrap_or(LEGACY_TIER);
    if tier != FREE_TIER {
        let why = loaded["ineligibleTiers"][0]["reasonMessage"].as_str().map(|m| format!(": {m}")).unwrap_or_default();
        return Err(LlmError::new(ErrorClass::Auth, format!("Antigravity found no project for this account (tier {tier}){why}")));
    }
    let mut operation = call(http, bases, "/v1internal:onboardUser", bearer, Some(&json!({"tierId":tier,"metadata":metadata}))).await?;
    for _ in 0..POLLS {
        if operation["done"] == true {
            break;
        }
        let Some(name) = operation["name"].as_str() else { break };
        tokio::time::sleep(poll).await;
        operation = call(http, bases, &format!("/v1internal/{name}"), bearer, None).await?;
    }
    if operation["done"] != true {
        return Err(LlmError::new(ErrorClass::Timeout, "Antigravity onboarding did not finish"));
    }
    project_id(&operation["response"]).ok_or_else(|| LlmError::new(ErrorClass::Other, "Antigravity onboarding returned no project"))
}
