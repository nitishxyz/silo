use super::render::{RenderError, RenderRequest};
use super::web_fetch::{Verdict, Web, http, needs_render};
use super::*;
use crate::{Tool, ToolOutput, html};
use base64::Engine as _;
use reqwest::Url;

/// The results page `web_search` reads (`search_engine` in otto's config).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// The HTML endpoint: light, no JavaScript, works over plain HTTP.
    #[default]
    Duckduckgo,
    Bing,
    Google,
}

impl Engine {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Duckduckgo => "duckduckgo",
            Self::Bing => "bing",
            Self::Google => "google",
        }
    }
    /// The results page for `query`.
    #[must_use]
    pub fn url(self, query: &str) -> Url {
        let (base, extra): (&str, &[(&str, &str)]) = match self {
            Self::Duckduckgo => ("https://html.duckduckgo.com/html/", &[]),
            Self::Bing => ("https://www.bing.com/search", &[]),
            Self::Google => ("https://www.google.com/search", &[("hl", "en")]),
        };
        Url::parse_with_params(base, std::iter::once(("q", query)).chain(extra.iter().copied())).expect("static url")
    }
    /// The organic results on one of its pages, ads dropped and redirect
    /// wrappers resolved.
    #[must_use]
    pub fn results(self, html: &str) -> Vec<Hit> {
        match self {
            Self::Duckduckgo => duckduckgo(html),
            Self::Bing => bing(html),
            Self::Google => google(html),
        }
    }
}

impl std::str::FromStr for Engine {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" | "ddg" => Ok(Self::Duckduckgo),
            "bing" => Ok(Self::Bing),
            "google" => Ok(Self::Google),
            other => Err(format!("unknown search engine {other:?}; use duckduckgo, bing or google")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// The real target of a result link: relative links resolved against
/// `base`, DuckDuckGo `/l/?uddg=`, Google `/url?q=` and Bing `/ck/a?u=a1…`
/// unwrapped; `None` for anything but HTTP(S).
#[must_use]
pub fn unwrap(href: &str, base: &str) -> Option<Url> {
    let url = Url::parse(base).ok()?.join(href.trim()).ok()?;
    let host = url.host_str().unwrap_or("").to_owned();
    let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned()).filter(|v| !v.is_empty());
    let inner = if host.ends_with("duckduckgo.com") && url.path() == "/l/" {
        param("uddg")
    } else if is_google(&host) && url.path() == "/url" {
        param("q").or_else(|| param("url"))
    } else if host.ends_with("bing.com") && url.path() == "/ck/a" {
        param("u").and_then(|u| u.strip_prefix("a1").map(str::to_owned)).and_then(|u| {
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(u.trim_end_matches('=')).ok()?;
            String::from_utf8(bytes).ok()
        })
    } else {
        None
    };
    let url = match inner {
        Some(inner) => Url::parse(&inner).ok()?,
        None => url,
    };
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn is_google(host: &str) -> bool {
    let host = host.strip_prefix("www.").unwrap_or(host);
    host.strip_prefix("google.").is_some_and(|tld| tld.split('.').all(|p| p.len() <= 3))
}

/// DuckDuckGo's HTML endpoint: `div.result` with `a.result__a` and
/// `.result__snippet`; ads are `result--ad` (their links go to `y.js`).
#[must_use]
pub fn duckduckgo(page: &str) -> Vec<Hit> {
    let doc = html::Doc::parse(page);
    doc.select("div.result")
        .into_iter()
        .filter(|r| !r.has_class("result--ad"))
        .filter_map(|r| {
            let link = *doc.select_in(Some(r), "a.result__a").first()?;
            let url = unwrap(&link.attr("href")?, "https://duckduckgo.com/")?;
            if url.host_str().is_some_and(|h| h.ends_with("duckduckgo.com")) && url.path() == "/y.js" {
                return None;
            }
            let snippet = doc.select_in(Some(r), ".result__snippet").first().map(|s| doc.text(s)).unwrap_or_default();
            hit(doc.text(link), url, snippet)
        })
        .collect()
}

/// Bing: `li.b_algo` with `h2 a` and a caption paragraph; ads (`b_ad`)
/// are other list items, and `aclick` links are dropped.
#[must_use]
pub fn bing(page: &str) -> Vec<Hit> {
    let icon = regex::Regex::new(r#"(?is)<span\b[^>]*class="[^"]*algoSlug_icon[^"]*"[^>]*>.*?</span\s*>"#).expect("static regex");
    let doc = html::Doc::parse(page);
    doc.select("li.b_algo")
        .into_iter()
        .filter(|r| doc.closest(r, ".b_ad").is_none())
        .filter_map(|r| {
            let link = *doc.select_in(Some(r), "h2 a").first()?;
            let url = unwrap(&link.attr("href")?, "https://www.bing.com/")?;
            if url.host_str().is_some_and(|h| h.ends_with("bing.com")) {
                return None;
            }
            let snippet = doc
                .select_in(Some(r), ".b_caption p, p.b_lineclamp2, p.b_lineclamp3, p.b_lineclamp4, p.b_algoSlug")
                .first()
                .map(|p| html::text(&icon.replace_all(doc.inner(p), "")));
            hit(doc.text(link), url, snippet.unwrap_or_default())
        })
        .collect()
}

/// Google (rendered or basic HTML): links holding an `h3`, outside the ad
/// blocks (`#tads`, `#tvcap`, `#bottomads`, `[data-text-ad]`), with the
/// snippet from their result block. Rendered pages link through opaque
/// `/goto?url=` redirects, kept as they are for [`resolve`].
#[must_use]
pub fn google(page: &str) -> Vec<Hit> {
    let doc = html::Doc::parse(page);
    let ads = doc.select("#tads, #tvcap, #bottomads, [data-text-ad]");
    doc.select("a h3")
        .into_iter()
        .filter_map(|h3| {
            let link = doc.closest(h3, "a")?;
            if ads.iter().any(|ad| ad.start <= link.start && link.end <= ad.end) {
                return None;
            }
            let url = unwrap(&link.attr("href")?, "https://www.google.com/")?;
            let host = url.host_str().unwrap_or("");
            if (is_google(host) && !is_goto(&url)) || host.ends_with("googleadservices.com") {
                return None;
            }
            let block = ["div.g", "div.MjjYud", "div.Gx5Zad", "div[data-hveid]"].iter().find_map(|s| doc.closest(link, s));
            let snippet = block
                .and_then(|b| {
                    doc.select_in(Some(b), ".VwiC3b, [data-sncf], .s3v9rd, .st")
                        .into_iter()
                        .find(|s| !(link.start <= s.start && s.end <= link.end))
                        .map(|s| doc.text(s))
                })
                .unwrap_or_default();
            hit(doc.text(h3), url, snippet)
        })
        .collect()
}

fn hit(title: String, url: Url, snippet: String) -> Option<Hit> {
    (!title.is_empty()).then(|| Hit { title, url: url.into(), snippet })
}

fn is_goto(url: &Url) -> bool {
    is_google(url.host_str().unwrap_or("")) && url.path() == "/goto"
}

/// `hits` with Google's `/goto` redirects replaced by their targets (each
/// redirect read without following it); a hit whose target can't be read
/// is dropped.
pub async fn resolve(hits: Vec<Hit>) -> Vec<Hit> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .expect("static client");
    let mut tasks = tokio::task::JoinSet::new();
    for (i, mut hit) in hits.into_iter().enumerate() {
        let client = client.clone();
        tasks.spawn(async move {
            if Url::parse(&hit.url).is_ok_and(|u| is_goto(&u)) {
                let response = client.get(&hit.url).send().await.ok()?;
                let target = response.headers().get(reqwest::header::LOCATION)?.to_str().ok()?;
                let target = Url::parse(target).ok().filter(|u| matches!(u.scheme(), "http" | "https") && !is_goto(u))?;
                hit.url = target.into();
            }
            Some((i, hit))
        });
    }
    let mut out: Vec<(usize, Hit)> = tasks.join_all().await.into_iter().flatten().collect();
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, hit)| hit).collect()
}

/// DuckDuckGo turned a plain request away (its bot check answers 202).
#[must_use]
pub fn duckduckgo_blocked(status: u16, body: &str) -> bool {
    status != 200 || body.contains("anomaly-modal") || body.contains("anomaly.js") || needs_render(status, body) == Some(Verdict::Blocked)
}

pub struct WebSearch {
    spec: ToolSpec,
    web: Web,
}

impl Default for WebSearch {
    fn default() -> Self {
        Self::new(Web::default())
    }
}

impl WebSearch {
    #[must_use]
    pub fn new(web: Web) -> Self {
        let mut spec = spec(
            "web_search",
            "Search the web and return result titles, URLs and snippets. It reads a search engine's results page (DuckDuckGo unless otto's config sets search_engine to bing or google) in a hidden browser in the Silo app; without the app it asks DuckDuckGo over plain HTTP. Ads are dropped and redirect links resolved. Read a result with web_fetch.",
            json!({
                "query":{"type":"string"},
                "limit":{"type":"integer","minimum":1,"maximum":20,"description":"Most results (default 8)"}
            }),
            &["query"],
            Effects::NETWORK,
        );
        spec.loading = crate::Loading::Deferred;
        spec.mode = crate::CallMode::Async;
        Self { spec, web }
    }
}

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        crate::policy::validate_prepared(&input, &cx)?;
        tokio::select! {
            result = run(&self.web, &input, &cx) => result,
            () = cx.cancel.cancelled() => Err(ToolError::new(crate::ToolErrorKind::Cancelled, "Search cancelled")),
        }
    }
}

async fn run(web: &Web, input: &Value, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
    let query = string(input, "query")?.trim();
    if query.is_empty() {
        return Err(ToolError::invalid("query must not be empty"));
    }
    let limit = input.get("limit").and_then(Value::as_u64).map_or(8, |n| n.clamp(1, 20)) as usize;
    let engine = web.engine;
    let request = RenderRequest { url: engine.url(query).into(), ..RenderRequest::default() };
    let (engine, rendered, hits, status, page) = match web.render(request, cx).await {
        Ok(page) => {
            let mut hits = engine.results(&page.html);
            if engine == Engine::Google {
                // A few spare for duplicates.
                hits.truncate(limit + 4);
                hits = resolve(hits).await;
            }
            (engine, true, hits, page.status.unwrap_or(200), page.html)
        }
        Err(RenderError::NoApp(_)) => {
            let page = http(Engine::Duckduckgo.url(query)).await?;
            if duckduckgo_blocked(page.status, &page.body) {
                return Ok(ToolOutput::error("Search needs the Silo app: DuckDuckGo turned the plain request away."));
            }
            (Engine::Duckduckgo, false, duckduckgo(&page.body), page.status, page.body)
        }
        Err(e) => return Ok(ToolOutput::error(format!("Search failed: {e}"))),
    };
    let mut seen = std::collections::HashSet::new();
    let hits: Vec<Hit> = hits.into_iter().filter(|h| seen.insert(h.url.clone())).take(limit).collect();
    let metadata = json!({"engine": engine.name(), "rendered": rendered, "results": hits});
    if hits.is_empty() {
        let text = if needs_render(status, &page) == Some(Verdict::Blocked) {
            format!("No results: {} showed a bot check.", engine.name())
        } else {
            format!("No results for {query:?}.")
        };
        return Ok(ToolOutput::text(text).with_metadata(metadata));
    }
    let text = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            if h.snippet.is_empty() {
                format!("{}. {}\n   {}", i + 1, h.title, h.url)
            } else {
                format!("{}. {}\n   {}\n   {}", i + 1, h.title, h.url, h.snippet)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(crate::output::limit(text, cx)?.with_metadata(metadata))
}
