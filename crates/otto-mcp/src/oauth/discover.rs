//! Where a server's authorization server is and what it supports: RFC 9728 protected
//! resource metadata (from `WWW-Authenticate` or `.well-known`), then RFC 8414 / OpenID
//! Connect authorization server metadata.
use serde_json::Value;
use std::time::Duration;

/// The canonical resource URI: no fragment, lowercase scheme and host, no lone trailing `/`.
pub fn canonical(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else { return url.to_owned() };
    parsed.set_fragment(None);
    let text = parsed.to_string();
    if parsed.path() == "/" && parsed.query().is_none() { text.trim_end_matches('/').to_owned() } else { text }
}

/// The parameters of a `WWW-Authenticate: Bearer …` challenge that matter here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Challenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
}

pub fn challenge(header: &str) -> Challenge {
    let mut out = Challenge::default();
    let rest = header.trim();
    let mut rest = rest.split_once(char::is_whitespace).filter(|(scheme, _)| !scheme.contains('=')).map_or(rest, |(_, r)| r);
    while let Some((key, after)) = rest.split_once('=') {
        let key = key.trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let after = after.trim_start();
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut end = quoted.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next().map(|(_, c)| c)),
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => value.push(c),
                }
            }
            (value, &quoted[end..])
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim().to_owned(), &after[end..])
        };
        match key.as_str() {
            "resource_metadata" => out.resource_metadata = Some(value),
            "scope" => out.scope = Some(value),
            "error" => out.error = Some(value),
            _ => {}
        }
        rest = next;
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// The `resource` parameter of authorization and token requests.
    pub resource: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub scope: Option<String>,
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let response = http.get(url).header("Accept", "application/json").timeout(Duration::from_secs(15)).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok().filter(Value::is_object)
}

/// `{origin}/.well-known/{name}{path}`, then without the path.
fn well_known(url: &reqwest::Url, name: &str) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let mut urls = vec![];
    if !path.is_empty() {
        urls.push(format!("{origin}/.well-known/{name}{path}"));
    }
    urls.push(format!("{origin}/.well-known/{name}"));
    urls
}

pub async fn discover(http: &reqwest::Client, server_url: &str, challenge: &Challenge) -> Result<Metadata, String> {
    let url = reqwest::Url::parse(server_url).map_err(|e| format!("invalid server URL: {e}"))?;
    let candidates = challenge.resource_metadata.clone().map_or_else(|| well_known(&url, "oauth-protected-resource"), |u| vec![u]);
    let mut resource = None;
    for candidate in candidates {
        if let Some(found) = get_json(http, &candidate).await {
            resource = Some(found);
            break;
        }
    }
    // Servers from before RFC 9728 (protocol 2025-03-26) are their own authorization server.
    let issuer = match resource.as_ref().and_then(|r| r["authorization_servers"][0].as_str()) {
        Some(issuer) => issuer.to_owned(),
        None if resource.is_none() => url.origin().ascii_serialization(),
        None => return Err("the server names no authorization server".into()),
    };
    let issuer_url = reqwest::Url::parse(&issuer).map_err(|e| format!("invalid authorization server {issuer}: {e}"))?;
    let (origin, path) = (issuer_url.origin().ascii_serialization(), issuer_url.path().trim_end_matches('/'));
    let candidates = if path.is_empty() {
        vec![format!("{origin}/.well-known/oauth-authorization-server"), format!("{origin}/.well-known/openid-configuration")]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{origin}{path}/.well-known/openid-configuration"),
        ]
    };
    let mut server = None;
    for candidate in candidates {
        if let Some(found) = get_json(http, &candidate).await {
            server = Some(found);
            break;
        }
    }
    let server = server.ok_or_else(|| format!("no authorization server metadata at {issuer}"))?;
    let methods = server["code_challenge_methods_supported"].as_array();
    if methods.is_some_and(|m| !m.iter().any(|m| m == "S256")) {
        return Err("the authorization server does not support PKCE S256".into());
    }
    let endpoint = |name: &str| server[name].as_str().map(String::from);
    let scopes =
        resource.as_ref().and_then(|r| r["scopes_supported"].as_array()).map(|s| s.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "));
    Ok(Metadata {
        resource: resource.as_ref().and_then(|r| r["resource"].as_str()).map_or_else(|| canonical(server_url), String::from),
        authorization_endpoint: endpoint("authorization_endpoint").ok_or("no authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint").ok_or("no token_endpoint")?,
        registration_endpoint: endpoint("registration_endpoint"),
        scope: challenge.scope.clone().or(scopes).filter(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_and_canonical_urls() {
        let c = challenge(r#"Bearer realm="mcp", resource_metadata="https://x.test/.well-known/oauth-protected-resource/mcp", scope="a b""#);
        assert_eq!(c.resource_metadata.as_deref(), Some("https://x.test/.well-known/oauth-protected-resource/mcp"));
        assert_eq!(c.scope.as_deref(), Some("a b"));
        assert_eq!(challenge(r#"Bearer error=insufficient_scope, scope="x\"y""#).scope.as_deref(), Some("x\"y"));
        assert_eq!(canonical("HTTPS://Example.COM/#frag"), "https://example.com");
        assert_eq!(canonical("https://example.com/mcp/"), "https://example.com/mcp/");
        let url = reqwest::Url::parse("https://a.test/tenant/mcp").unwrap();
        assert_eq!(well_known(&url, "x"), ["https://a.test/.well-known/x/tenant/mcp", "https://a.test/.well-known/x"]);
    }
}
