//! OAuth for HTTP servers (MCP authorization, protocol `2025-06-18`): discovery from a
//! `401` (protected resource and authorization server metadata), dynamic client
//! registration, PKCE S256 with a loopback redirect, the `resource` parameter, and
//! token refresh. Tokens live in otto's credential store (a store of their own,
//! `<home>/auth/mcp`), keyed by server URL.
pub mod discover;
pub mod login;

use otto_llm::auth::{Credential, CredentialStore, oauth};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::OnceLock};

/// Stored OAuth tokens for every server, refreshed when they near expiry.
pub struct TokenStore {
    dir: PathBuf,
    store: OnceLock<CredentialStore>,
    /// One refresh at a time.
    refreshing: tokio::sync::Mutex<()>,
    pub(crate) http: reqwest::Client,
}

fn key(url: &str) -> String {
    format!("mcp:{}", discover::canonical(url))
}

impl TokenStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), store: OnceLock::new(), refreshing: tokio::sync::Mutex::new(()), http: reqwest::Client::new() }
    }

    fn store(&self) -> Result<&CredentialStore, String> {
        if let Some(store) = self.store.get() {
            return Ok(store);
        }
        let store = CredentialStore::open(&self.dir).map_err(|e| e.to_string())?;
        Ok(self.store.get_or_init(|| store))
    }

    pub fn get(&self, url: &str) -> Option<Credential> {
        // Nothing stored yet: don't create the store just to look.
        if self.store.get().is_none() && !self.dir.join("auth.json").exists() {
            return None;
        }
        self.store().ok()?.get(&key(url)).ok().flatten()
    }

    pub fn set(&self, url: &str, credential: Credential) -> Result<(), String> {
        self.store()?.set(&key(url), credential).map_err(|e| e.to_string())
    }

    pub fn remove(&self, url: &str) -> Result<(), String> {
        if self.get(url).is_none() {
            return Ok(());
        }
        self.store()?.remove(&key(url)).map_err(|e| e.to_string())
    }

    /// The access token for `url`, refreshed first when it is about to expire.
    pub async fn bearer(&self, url: &str) -> Option<String> {
        let credential = self.get(url)?;
        let Credential::OAuth { access, .. } = &credential else { return None };
        if credential.is_fresh() || !self.refresh(url, access).await {
            // A failed refresh still tries the old token; its `401` asks for sign-in.
            return Some(access.clone());
        }
        self.access(url)
    }

    fn access(&self, url: &str) -> Option<String> {
        match self.get(url)? {
            Credential::OAuth { access, .. } => Some(access),
            Credential::ApiKey { .. } => None,
        }
    }

    /// Replace the token `used` (rejected or expiring); true when a newer one is stored.
    pub async fn refresh(&self, url: &str, used: &str) -> bool {
        let _one = self.refreshing.lock().await;
        let Some(Credential::OAuth { access, refresh, extra, account_id, .. }) = self.get(url) else { return false };
        if access != used {
            return true;
        }
        let (Some(endpoint), Some(client_id)) = (extra["token_endpoint"].as_str(), extra["client_id"].as_str()) else { return false };
        if refresh.is_empty() {
            return false;
        }
        let mut fields = BTreeMap::from([("refresh_token".to_owned(), refresh.clone()), ("client_id".to_owned(), client_id.to_owned())]);
        for name in ["client_secret", "resource"] {
            if let Some(value) = extra[name].as_str() {
                fields.insert(name.into(), value.into());
            }
        }
        match oauth::refresh_token(&self.http, endpoint, &fields, oauth::TokenEncoding::Form, &Default::default()).await {
            Ok(token) => {
                let credential = credential(&token, Some(refresh), account_id, extra);
                self.set(url, credential).is_ok()
            }
            Err(error) => {
                tracing::warn!(url, %error, "MCP token refresh failed");
                false
            }
        }
    }
}

/// A token response as a stored credential; a response without a refresh token keeps `previous`.
pub(crate) fn credential(token: &oauth::TokenResponse, previous: Option<String>, account_id: Option<String>, extra: Value) -> Credential {
    let expires_at_ms = match token.expires_in {
        0 => u64::MAX,
        seconds => otto_llm::auth::now_ms().saturating_add(seconds.saturating_mul(1000)),
    };
    Credential::OAuth {
        access: token.access.clone(),
        refresh: token.refresh.clone().or(previous).unwrap_or_default(),
        expires_at_ms,
        account_id,
        extra: if extra.is_null() { json!({}) } else { extra },
    }
}
