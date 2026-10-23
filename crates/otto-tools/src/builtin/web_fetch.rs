use super::render::{RenderError, RenderRequest, Rendered, Renderer};
use super::*;
use crate::{OutputPart, Tool, ToolOutput, html};
use reqwest::Url;
use std::sync::Arc;

const MAX_BODY: usize = 5 * 1024 * 1024;
const USER_AGENT: &str = concat!("Mozilla/5.0 (compatible; otto/", env!("CARGO_PKG_VERSION"), ")");

/// What the web tools share: silo's off-screen browser, when connected, and
/// the search engine (`search_engine` in otto's config).
#[derive(Clone, Default)]
pub struct Web {
    pub renderer: Option<Arc<dyn Renderer>>,
    pub engine: super::web_search::Engine,
}

impl Web {
    pub(crate) async fn render(&self, request: RenderRequest, cx: &ToolCx) -> Result<Rendered, RenderError> {
        match &self.renderer {
            Some(renderer) => renderer.render(request, cx).await,
            None => Err(RenderError::NoApp("no Silo app".into())),
        }
    }
}

pub struct WebFetch {
    spec: ToolSpec,
    web: Web,
}

impl Default for WebFetch {
    fn default() -> Self {
        Self::new(Web::default())
    }
}

impl WebFetch {
    #[must_use]
    pub fn new(web: Web) -> Self {
        let mut spec = spec(
            "web_fetch",
            "Fetch an HTTP(S) URL and return its title, final URL and main content as markdown. It uses plain HTTP and, when the page looks blocked by bot protection or built by JavaScript, renders it in a hidden browser in the Silo app (without the app it returns the plain result and says so). render:true always renders, render:false never does. screenshot:true renders and adds an image of the viewport. selector keeps only matching elements (tag, #id, .class, [attr=value], descendants separated by spaces). Plain requests time out after 30 seconds; responses over 5 MiB are rejected.",
            json!({
                "url":{"type":"string","format":"uri"},
                "render":{"type":"boolean","description":"true: always render in the browser; false: plain HTTP only; omitted: render only when the plain result looks blocked or empty"},
                "screenshot":{"type":"boolean","description":"Render and add an image of the viewport"},
                "selector":{"type":"string","description":"CSS selector limiting the content, e.g. main article or #content"}
            }),
            &["url"],
            Effects::NETWORK,
        );
        // Slow and independent: models with async tool calling keep working meanwhile.
        spec.mode = crate::CallMode::Async;
        Self { spec, web }
    }
}

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
        crate::policy::validate_prepared(&input, &cx)?;
        tokio::select! {
            result = run(&self.web, &input, &cx) => result,
            () = cx.cancel.cancelled() => Err(ToolError::new(crate::ToolErrorKind::Cancelled, "Fetch cancelled")),
        }
    }
}

/// Why a plain fetch should be rendered instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A bot wall or challenge page.
    Blocked,
    /// A page JavaScript fills in: almost no text without it.
    Empty,
}

/// Challenge markers; with 403, 429 or 503 any of them means blocked.
const CHALLENGE: &[&str] = &[
    "just a moment",
    "cf-chl",
    "cf_chl",
    "challenge-platform",
    "attention required",
    "cf-browser-verification",
    "datadome",
    "captcha-delivery.com",
    "perimeterx",
    "px-captcha",
    "_pxappid",
    "access denied",
    "errors.edgesuite.net",
    "captcha",
    "checking your browser",
    "ddos-guard",
    "verify you are human",
    "unusual traffic",
];
/// Markers specific enough to mean blocked on a short page whatever its status.
const WALL: &[&str] = &["cf-chl", "cf_chl", "challenge-platform", "captcha-delivery.com", "px-captcha", "_pxappid", "checking your browser"];
const NO_JS: &[&str] = &[
    "enable javascript",
    "javascript is required",
    "requires javascript",
    "javascript is disabled",
    "turn on javascript",
    "need javascript",
    "javascript to run this app",
];
/// Under this many characters of text a page counts as short.
const SHORT: usize = 200;

/// Whether a plain HTTP result (`status`, HTML `body`) looks blocked or
/// empty: a challenge page (403/429/503 with a bot-wall marker, or a short
/// page with an unmistakable one), or an HTML shell with under 200
/// characters of text and script-heavy markup, an empty app root or an
/// "enable JavaScript" notice.
#[must_use]
pub fn needs_render(status: u16, body: &str) -> Option<Verdict> {
    let lower = body.to_ascii_lowercase();
    let has = |markers: &[&str]| markers.iter().any(|m| lower.contains(m));
    if matches!(status, 403 | 429 | 503) && has(CHALLENGE) {
        return Some(Verdict::Blocked);
    }
    let short = html::text(body).chars().count() < SHORT;
    if short && has(WALL) {
        return Some(Verdict::Blocked);
    }
    let root = regex::Regex::new(r#"<div\s+id=["'](?:root|app|__next|__nuxt|svelte)["'][^>]*>\s*</div>|<app-root[^>]*>\s*</app-root>"#)
        .expect("static regex");
    (short && (lower.matches("<script").count() >= 3 || root.is_match(&lower) || has(NO_JS))).then_some(Verdict::Empty)
}

/// A plain HTTP response.
pub(crate) struct Http {
    pub url: Url,
    pub status: u16,
    pub html: bool,
    pub body: String,
}

pub(crate) async fn http(url: Url) -> Result<Http, ToolError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| ToolError::failed(e.to_string()))?;
    let mut response = client.get(url).send().await.map_err(|e| ToolError::failed(e.to_string()))?;
    let (url, status) = (response.url().clone(), response.status().as_u16());
    let html = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|s| s.contains("html"));
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| ToolError::failed(e.to_string()))? {
        if data.len() + chunk.len() > MAX_BODY {
            return Err(ToolError::failed("Response exceeds 5 MiB"));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(Http { url, status, html, body: String::from_utf8_lossy(&data).into_owned() })
}

async fn run(web: &Web, input: &Value, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
    let url = Url::parse(string(input, "url")?).map_err(|e| ToolError::invalid(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::invalid("Only HTTP(S) URLs are supported"));
    }
    let render = input.get("render").and_then(Value::as_bool);
    let screenshot = input.get("screenshot").and_then(Value::as_bool).unwrap_or(false);
    let selector = input.get("selector").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    if screenshot && render == Some(false) {
        return Err(ToolError::invalid("screenshot needs the browser; drop render:false"));
    }
    let request = RenderRequest { url: url.to_string(), timeout_ms: None, screenshot, selector: selector.clone() };
    if render == Some(true) || screenshot {
        return match web.render(request, cx).await {
            Ok(page) => rendered(page, selector.as_deref(), cx),
            Err(RenderError::NoApp(_)) => Ok(ToolOutput::error("Rendering needs the Silo app.")),
            Err(e) => Ok(ToolOutput::error(format!("Rendering failed: {e}"))),
        };
    }
    let page = http(url).await?;
    let verdict = if page.html && render.is_none() { needs_render(page.status, &page.body) } else { None };
    let Some(verdict) = verdict else {
        if !(200..300).contains(&page.status) {
            return Err(ToolError::failed(format!("HTTP {} for {}", page.status, page.url)));
        }
        return plain(&page, selector.as_deref(), None, cx);
    };
    match web.render(request, cx).await {
        Ok(rendered_page) => rendered(rendered_page, selector.as_deref(), cx),
        Err(RenderError::NoApp(_)) => {
            let note = match verdict {
                Verdict::Blocked => "bot check; rendering needs the Silo app",
                Verdict::Empty => "built by JavaScript; rendering needs the Silo app",
            };
            plain(&page, selector.as_deref(), Some(note.into()), cx)
        }
        Err(e) => plain(&page, selector.as_deref(), Some(format!("rendering failed: {e}")), cx),
    }
}

/// Markdown of `body`: the elements matching `selector`, or the main content.
fn content(body: &str, base: Option<&Url>, selector: Option<&str>) -> Result<String, String> {
    let doc = html::Doc::parse(body);
    match selector {
        Some(selector) => {
            let found = doc.select(selector);
            // Nested matches are already inside their ancestors' text.
            let top: Vec<_> = found.iter().filter(|n| !found.iter().any(|a| a.start < n.start && n.end <= a.end)).collect();
            if top.is_empty() {
                return Err(format!("No element matches selector {selector}"));
            }
            Ok(top.iter().map(|n| html::markdown(doc.outer(n), base)).filter(|m| !m.is_empty()).collect::<Vec<_>>().join("\n\n"))
        }
        None => Ok(html::markdown(&doc.main_content(), base)),
    }
}

fn plain(page: &Http, selector: Option<&str>, note: Option<String>, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
    let ok = (200..300).contains(&page.status);
    if !page.html {
        let mut out = crate::output::limit(page.body.clone(), cx)?;
        out.metadata = json!({"url": page.url.as_str(), "status": page.status, "rendered": false});
        return Ok(out);
    }
    let title = html::Doc::parse(&page.body).title();
    let (body, error) = match content(&page.body, Some(&page.url), selector) {
        Ok(body) => (body, false),
        Err(e) => (e, true),
    };
    let notes: Vec<String> = note.into_iter().collect();
    finish(Head { title, url: page.url.as_str(), status: Some(page.status), notes: &notes }, body, None, false, !ok || error, cx)
}

fn rendered(page: Rendered, selector: Option<&str>, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
    let base = Url::parse(&page.url).ok();
    // With a selector the page is that element already.
    let (body, error) = match selector {
        Some(_) => (html::markdown(&page.html, base.as_ref()), false),
        None => match content(&page.html, base.as_ref(), None) {
            Ok(body) => (body, false),
            Err(e) => (e, true),
        },
    };
    let mut notes = vec!["rendered in browser".to_owned()];
    if page.blocked {
        notes.push(blocked_note(&page.html).into());
    } else if page.timed_out {
        notes.push("not settled by the deadline; may be partial".into());
    }
    let title = Some(page.title.trim().to_owned()).filter(|t| !t.is_empty()).or_else(|| html::Doc::parse(&page.html).title());
    let failed = page.status.is_some_and(|s| !(200..300).contains(&s));
    finish(Head { title, url: &page.url, status: page.status, notes: &notes }, body, page.screenshot, true, failed || error, cx)
}

/// The note for a page stuck on a bot check, naming Cloudflare's.
#[must_use]
pub fn blocked_note(html: &str) -> &'static str {
    let lower = html.to_ascii_lowercase();
    if ["cf-chl", "cf_chl", "challenge-platform", "cloudflare"].iter().any(|m| lower.contains(m)) {
        "blocked by a bot check (Cloudflare)"
    } else {
        "blocked by a bot check"
    }
}

struct Head<'a> {
    title: Option<String>,
    url: &'a str,
    status: Option<u16>,
    notes: &'a [String],
}

fn finish(head: Head<'_>, body: String, image: Option<(String, String)>, rendered: bool, error: bool, cx: &ToolCx) -> Result<ToolOutput, ToolError> {
    let mut text = String::new();
    if let Some(title) = &head.title {
        text.push_str(&format!("Title: {title}\n"));
    }
    text.push_str(&format!("URL: {}\n", head.url));
    if let Some(status) = head.status.filter(|s| !(200..300).contains(s)) {
        text.push_str(&format!("Status: {status}\n"));
    }
    for note in head.notes {
        text.push_str(&format!("Note: {note}\n"));
    }
    text.push('\n');
    text.push_str(if body.is_empty() { "(no text content)" } else { &body });
    let mut out = crate::output::limit(text, cx)?;
    if let Some((mime, data_base64)) = image {
        out.content.push(OutputPart::Image { mime, data_base64 });
    }
    out.is_error = error;
    out.metadata = json!({"url": head.url, "title": head.title, "status": head.status, "rendered": rendered});
    Ok(out)
}
