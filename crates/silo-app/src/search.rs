//! What the address bar opens: an address as typed, anything else as a
//! search on the engine picked in Settings › Browser.

use serde::{Deserialize, Serialize};

/// The search engine pages search with (`search_engine` in `app.json`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchEngine {
    #[default]
    Google,
    DuckDuckGo,
    Bing,
    Kagi,
}

impl SearchEngine {
    pub const ALL: [Self; 4] = [Self::Google, Self::DuckDuckGo, Self::Bing, Self::Kagi];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::DuckDuckGo => "DuckDuckGo",
            Self::Bing => "Bing",
            Self::Kagi => "Kagi",
        }
    }

    /// The results page for `query`.
    #[must_use]
    pub fn url(self, query: &str) -> String {
        let base = match self {
            Self::Google => "https://www.google.com/search?q=",
            Self::DuckDuckGo => "https://duckduckgo.com/?q=",
            Self::Bing => "https://www.bing.com/search?q=",
            Self::Kagi => "https://kagi.com/search?q=",
        };
        let mut out = String::from(base);
        for b in query.trim().bytes() {
            match b {
                b' ' => out.push('+'),
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
}

/// The page typed `input` opens: an address (as [`silo_proto::model::browser_url`]
/// normalizes it) when it reads as one, else a search on `engine`. `None`:
/// empty, or a scheme pages may not load (`file:`, `javascript:`…).
#[must_use]
pub fn page_for(input: &str, engine: SearchEngine) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if looks_like_address(input) {
        return silo_proto::model::browser_url(input);
    }
    Some(engine.url(input))
}

/// A scheme (`https://…`, `about:blank`, `data:`), or one word whose host is
/// localhost, an IP, or dotted with a letter TLD (`example.com/a`, `x.dev:3000`).
fn looks_like_address(input: &str) -> bool {
    if input.contains(char::is_whitespace) {
        return false;
    }
    let lower = input.to_ascii_lowercase();
    if lower.contains("://") || lower == "about:blank" || lower.starts_with("data:") {
        return true;
    }
    let authority = input.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if authority.starts_with('[') {
        return true;
    }
    let host = authority.split(':').next().unwrap_or_default().to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() > 1
        && labels.iter().all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && labels.last().is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_open_and_the_rest_is_searched() {
        let g = SearchEngine::Google;
        assert_eq!(page_for("why do cats purr", g).as_deref(), Some("https://www.google.com/search?q=why+do+cats+purr"));
        assert_eq!(page_for("rust", g).as_deref(), Some("https://www.google.com/search?q=rust"));
        assert_eq!(page_for("c++ & go?", SearchEngine::DuckDuckGo).as_deref(), Some("https://duckduckgo.com/?q=c%2B%2B+%26+go%3F"));
        assert_eq!(page_for("example.com/a", g).as_deref(), Some("https://example.com/a"));
        assert_eq!(page_for("localhost:5173", g).as_deref(), Some("http://localhost:5173"));
        assert_eq!(page_for("192.168.1.4:8080/x", g).as_deref(), Some("http://192.168.1.4:8080/x"));
        assert_eq!(page_for("[::1]:5173", g).as_deref(), Some("http://[::1]:5173"));
        assert_eq!(page_for("example.com:8080", g).as_deref(), Some("https://example.com:8080"));
        assert_eq!(page_for("https://x.dev/a b", g).as_deref(), Some("https://www.google.com/search?q=https%3A%2F%2Fx.dev%2Fa+b"));
        assert_eq!(page_for("about:blank", g).as_deref(), Some("about:blank"));
        assert_eq!(page_for("v1.2", g).as_deref(), Some("https://www.google.com/search?q=v1.2"));
        assert_eq!(page_for("file:///etc/passwd", g), None);
        assert_eq!(page_for("  ", g), None);
    }

    #[test]
    fn every_engine_has_a_results_page() {
        for e in SearchEngine::ALL {
            assert!(e.url("a b").starts_with("https://") && e.url("a b").ends_with("a+b"), "{e:?}");
        }
    }
}
