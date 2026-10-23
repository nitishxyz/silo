//! `web_fetch` (plain HTTP, the blocked/empty heuristic, rendering through a
//! fake browser) and `web_search` (SERP parsers over recorded pages).

use otto_tools::{
    Loading, OutputPart, Tool, ToolCx, ToolOutput,
    builtin::{
        render::{RenderError, RenderRequest, Rendered, Renderer},
        web_fetch::{Verdict, Web, WebFetch, needs_render},
        web_search::{self, Engine, WebSearch},
    },
    html,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/web/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

struct FakeRenderer {
    answer: Result<Rendered, RenderError>,
    calls: Mutex<Vec<RenderRequest>>,
}

#[async_trait::async_trait]
impl Renderer for FakeRenderer {
    async fn render(&self, request: RenderRequest, _: &ToolCx) -> Result<Rendered, RenderError> {
        self.calls.lock().unwrap().push(request);
        self.answer.clone()
    }
}

fn renderer(answer: Result<Rendered, RenderError>) -> Arc<FakeRenderer> {
    Arc::new(FakeRenderer { answer, calls: Mutex::new(Vec::new()) })
}

fn web(fake: &Arc<FakeRenderer>, engine: Engine) -> Web {
    Web { renderer: Some(fake.clone()), engine }
}

fn page(html: &str) -> Rendered {
    Rendered { url: "https://example.com/app".into(), title: "Rendered App".into(), status: Some(200), html: html.into(), ..Rendered::default() }
}

/// A local HTTP server answering `(path, status, content type, body)` routes.
async fn serve(routes: Vec<(&'static str, u16, &'static str, String)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 16384];
                let mut n = 0;
                while n < buf.len() {
                    let read = stream.read(&mut buf[n..]).await.unwrap_or(0);
                    n += read;
                    if read == 0 || buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, kind, body) =
                    routes.iter().find(|r| r.0 == path).map_or((404, "text/plain", "not found".to_owned()), |r| (r.1, r.2, r.3.clone()));
                let head = format!("HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

async fn site() -> String {
    serve(vec![
        ("/article", 200, "text/html; charset=utf-8", fixture("article.html")),
        ("/app", 200, "text/html", fixture("spa.html")),
        ("/cf", 403, "text/html", fixture("cloudflare.html")),
        ("/forbidden", 403, "text/html", "<html><body>Forbidden</body></html>".into()),
        ("/data.json", 200, "application/json", r#"{"ok":true}"#.into()),
    ])
    .await
}

fn text(out: &ToolOutput) -> String {
    out.joined_text()
}

fn cx() -> ToolCx {
    ToolCx::new("web", std::env::temp_dir())
}

#[test]
fn blocked_and_empty_pages_are_recognized() {
    for (status, name) in [(403, "cloudflare.html"), (503, "cloudflare.html"), (403, "datadome.html"), (403, "akamai.html")] {
        assert_eq!(needs_render(status, &fixture(name)), Some(Verdict::Blocked), "{name} {status}");
    }
    // A wall served with another status is still a wall.
    assert_eq!(needs_render(200, &fixture("datadome.html")), Some(Verdict::Blocked));
    assert_eq!(needs_render(200, &fixture("spa.html")), Some(Verdict::Empty));
    assert_eq!(needs_render(200, &fixture("nojs.html")), Some(Verdict::Empty));
    assert_eq!(needs_render(200, &format!("<html><body>{}<p>hi</p></body></html>", "<script>x()</script>".repeat(3))), Some(Verdict::Empty));
    // Real pages, and plain errors without a marker, stay plain.
    assert_eq!(needs_render(200, &fixture("article.html")), None);
    assert_eq!(needs_render(200, &fixture("ddg.html")), None);
    assert_eq!(needs_render(403, "<html><body>Forbidden</body></html>"), None);
    assert_eq!(needs_render(200, "<html><body><p>Short and fine.</p></body></html>"), None);
}

#[test]
fn main_content_becomes_markdown() {
    let doc = html::Doc::parse(&fixture("article.html"));
    assert_eq!(doc.title().as_deref(), Some("Ownership & Borrowing | Example Docs"));
    let base = reqwest::Url::parse("https://docs.example.com/book/ownership").unwrap();
    let md = html::markdown(&doc.main_content(), Some(&base));
    assert!(md.starts_with("# Ownership & Borrowing\n\nRust manages memory through a system of **ownership** with"), "{md}");
    assert!(md.contains("the program won't compile."), "{md}");
    assert!(md.contains("## The rules\n\n- Each value has an *owner*.\n- There can only be one owner at a time."), "{md}");
    assert!(md.contains("See [references and borrowing](https://docs.example.com/book/references) for the details, and use `clone` when"), "{md}");
    assert!(md.contains("```\nlet s1 = String::from(\"hello\");\nlet s2 = s1;   // s1 moved\n```"), "{md}");
    for chrome in ["Guide", "Table of contents", "2026 Example Docs", "analytics", "font:"] {
        assert!(!md.contains(chrome), "{chrome} in {md}");
    }
    assert_eq!(html::markdown("<h1>Title</h1><script>bad()</script><style>.bad{}</style><p>A &amp; B<br>Next</p>", None), "# Title\n\nA & B\nNext");
}

#[test]
fn selectors_match_simple_css() {
    let doc = html::Doc::parse(r#"<div id="a" class="x y"><p class="n">one</p><span data-k="v1">two</span></div><p class="n">three</p>"#);
    let texts = |sel: &str| doc.select(sel).iter().map(|n| doc.text(n)).collect::<Vec<_>>();
    assert_eq!(texts("p.n"), ["one", "three"]);
    assert_eq!(texts("#a p"), ["one"]);
    assert_eq!(texts("div.x.y span[data-k=v1]"), ["two"]);
    assert_eq!(texts("[data-k^=v], .missing"), ["two"]);
    assert!(texts("div > p").is_empty());
}

#[tokio::test]
async fn a_normal_page_stays_on_http() {
    let base = site().await;
    let fake = renderer(Ok(page("<p>rendered</p>")));
    let out = WebFetch::new(web(&fake, Engine::default())).call(json!({"url": format!("{base}/article")}), cx()).await.unwrap();
    let body = text(&out);
    assert!(!out.is_error);
    assert!(body.starts_with(&format!("Title: Ownership & Borrowing | Example Docs\nURL: {base}/article\n\n# Ownership")), "{body}");
    assert!(!body.contains("rendered in browser"));
    assert!(fake.calls.lock().unwrap().is_empty());
    assert_eq!(out.metadata["rendered"], false);
    // Non-HTML passes through.
    let out = WebFetch::default().call(json!({"url": format!("{base}/data.json")}), cx()).await.unwrap();
    assert_eq!(text(&out), r#"{"ok":true}"#);
    // A plain error is still an error.
    let error = WebFetch::new(web(&fake, Engine::default())).call(json!({"url": format!("{base}/forbidden")}), cx()).await.unwrap_err();
    assert!(error.message.contains("HTTP 403"), "{error}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_javascript_shell_falls_back_to_the_browser() {
    let base = site().await;
    let shot = ("image/png".to_owned(), "iVBORw0KGgo=".to_owned());
    let fake = renderer(Ok(Rendered { screenshot: Some(shot.clone()), timed_out: true, ..page(&fixture("article.html")) }));
    let out = WebFetch::new(web(&fake, Engine::default())).call(json!({"url": format!("{base}/app")}), cx()).await.unwrap();
    let body = text(&out);
    assert!(
        body.starts_with("Title: Rendered App\nURL: https://example.com/app\nNote: rendered in browser\nNote: the page had not settled"),
        "{body}"
    );
    assert!(body.contains("# Ownership & Borrowing"), "{body}");
    assert!(body.contains("[references and borrowing](https://example.com/book/references)"), "{body}");
    assert!(out.content.contains(&OutputPart::Image { mime: shot.0, data_base64: shot.1 }));
    assert_eq!(out.metadata["rendered"], true);
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].url, format!("{base}/app"));
    assert!(!calls[0].screenshot && calls[0].selector.is_none());
}

#[tokio::test]
async fn render_forces_either_path() {
    let base = site().await;
    let fake = renderer(Ok(page("<main><p>from the browser</p></main>")));
    let tool = WebFetch::new(web(&fake, Engine::default()));
    let out = tool.call(json!({"url": format!("{base}/article"), "render": true, "selector": "main p"}), cx()).await.unwrap();
    assert!(text(&out).contains("from the browser"), "{}", text(&out));
    assert_eq!(fake.calls.lock().unwrap()[0].selector.as_deref(), Some("main p"));
    let out = tool.call(json!({"url": format!("{base}/app"), "render": false}), cx()).await.unwrap();
    assert!(text(&out).starts_with("Title: Dashboard\n"), "{}", text(&out));
    assert!(!text(&out).contains("Note:"));
    // screenshot renders too.
    tool.call(json!({"url": format!("{base}/article"), "screenshot": true}), cx()).await.unwrap();
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
    assert!(fake.calls.lock().unwrap()[1].screenshot);
    assert!(tool.call(json!({"url": format!("{base}/article"), "render": false, "screenshot": true}), cx()).await.is_err());
}

#[tokio::test]
async fn a_selector_applies_to_plain_html() {
    let base = site().await;
    let out = WebFetch::default().call(json!({"url": format!("{base}/article"), "selector": "#content ul"}), cx()).await.unwrap();
    assert!(
        text(&out).ends_with(
            "\n\n- Each value has an *owner*.\n- There can only be one owner at a time.\n- When the owner goes out of scope, the value is dropped."
        ),
        "{}",
        text(&out)
    );
    let out = WebFetch::default().call(json!({"url": format!("{base}/article"), "selector": "table"}), cx()).await.unwrap();
    assert!(out.is_error && text(&out).contains("No element matches selector table"));
}

#[tokio::test]
async fn without_the_app_the_plain_result_says_rendering_needs_it() {
    let base = site().await;
    let no_app = renderer(Err(RenderError::NoApp("no app".into())));
    let out = WebFetch::new(web(&no_app, Engine::default())).call(json!({"url": format!("{base}/cf")}), cx()).await.unwrap();
    let body = text(&out);
    assert!(body.contains("Status: 403\nNote: the page looks blocked by bot protection; rendering it needs the Silo app\n"), "{body}");
    assert!(out.is_error);
    assert_eq!(no_app.calls.lock().unwrap().len(), 1);
    // No renderer at all (otto without silo) reads the same.
    let out = WebFetch::default().call(json!({"url": format!("{base}/app")}), cx()).await.unwrap();
    assert!(text(&out).contains("Note: the page looks built by JavaScript; rendering it needs the Silo app"), "{}", text(&out));
    assert!(!out.is_error);
    let out = WebFetch::default().call(json!({"url": format!("{base}/article"), "render": true}), cx()).await.unwrap();
    assert!(out.is_error && text(&out).starts_with("Rendering needs the Silo app"));
    // Other render failures keep the plain result with the reason.
    let broken = renderer(Err(RenderError::Navigation("DNS failure".into())));
    let out = WebFetch::new(web(&broken, Engine::default())).call(json!({"url": format!("{base}/app")}), cx()).await.unwrap();
    assert!(text(&out).contains("Note: rendering failed: DNS failure"), "{}", text(&out));
}

fn urls(hits: &[web_search::Hit]) -> Vec<&str> {
    hits.iter().map(|h| h.url.as_str()).collect()
}

#[test]
fn duckduckgo_results_unwrap_and_drop_ads() {
    let hits = web_search::duckduckgo(&fixture("ddg.html"));
    assert_eq!(hits.len(), 10, "{hits:#?}");
    assert_eq!(hits[0].title, "Rust Programming Language");
    assert_eq!(hits[0].url, "https://rust-lang.org/");
    assert!(!hits[0].snippet.is_empty());
    assert!(urls(&hits).contains(&"https://en.wikipedia.org/wiki/Rust_(programming_language)"));
    assert!(hits.iter().all(|h| !h.url.contains("duckduckgo.com") && !h.title.contains("Official Rust Book")), "{hits:#?}");
    assert!(web_search::duckduckgo_blocked(202, &fixture("ddg_anomaly.html")));
    assert!(web_search::duckduckgo_blocked(200, &fixture("ddg_anomaly.html")));
    assert!(!web_search::duckduckgo_blocked(200, &fixture("ddg.html")));
    assert!(web_search::duckduckgo(&fixture("ddg_anomaly.html")).is_empty());
}

#[test]
fn bing_results_unwrap_and_drop_ads() {
    let hits = web_search::bing(&fixture("bing.html"));
    assert_eq!(
        urls(&hits),
        ["https://www.rust-lang.org/", "https://doc.rust-lang.org/book/", "https://en.wikipedia.org/wiki/Rust_(programming_language)"]
    );
    assert_eq!(hits[0].title, "Rust Programming Language");
    assert_eq!(hits[0].snippet, "A language empowering everyone to build reliable and efficient software.");
    assert_eq!(hits[1].snippet, "by Steve Klabnik and Carol Nichols & contributions from the Rust Community.");
}

#[test]
fn google_results_unwrap_and_drop_ads() {
    let hits = web_search::google(&fixture("google.html"));
    assert_eq!(
        urls(&hits),
        ["https://www.rust-lang.org/", "https://doc.rust-lang.org/book/", "https://en.wikipedia.org/wiki/Rust_(programming_language)"]
    );
    assert_eq!(hits[0].title, "Rust Programming Language");
    assert_eq!(hits[0].snippet, "A language empowering everyone to build reliable and efficient software.");
    assert_eq!(hits[1].snippet, "by S Klabnik · Learn Rust from the official book.");
    assert_eq!(hits[2].title, "Rust (programming language) - Wikipedia");
    assert_eq!(hits[2].snippet, "Rust is a general-purpose programming language.");
}

#[test]
fn redirect_wrappers_resolve() {
    let unwrap = |href: &str, base: &str| web_search::unwrap(href, base).map(String::from);
    assert_eq!(
        unwrap("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x", "https://duckduckgo.com/").as_deref(),
        Some("https://example.com/a?b=1")
    );
    assert_eq!(unwrap("/url?q=https://example.com/&sa=U", "https://www.google.com/").as_deref(), Some("https://example.com/"));
    assert_eq!(
        unwrap("https://www.bing.com/ck/a?!&&p=1&u=a1aHR0cHM6Ly93d3cucnVzdC1sYW5nLm9yZy8&ntb=1", "https://www.bing.com/").as_deref(),
        Some("https://www.rust-lang.org/")
    );
    assert_eq!(unwrap("https://example.org/x", "https://www.bing.com/").as_deref(), Some("https://example.org/x"));
    assert_eq!(unwrap("javascript:void(0)", "https://duckduckgo.com/"), None);
    assert_eq!("bing".parse::<Engine>(), Ok(Engine::Bing));
    assert!("altavista".parse::<Engine>().is_err());
    assert_eq!(Engine::Duckduckgo.url("rust & go").as_str(), "https://html.duckduckgo.com/html/?q=rust+%26+go");
}

#[test]
fn web_search_is_deferred_and_web_fetch_eager() {
    let search = WebSearch::default();
    assert_eq!(search.spec().loading, Loading::Deferred);
    assert!(search.spec().effects.network && search.spec().effects.read_only());
    assert!(search.spec().description.starts_with("Search the web and return result titles, URLs and snippets. "));
    let tools = otto_tools::registry::tools_for_family("anthropic");
    let spec = |name: &str| tools.iter().find(|t| t.spec().name == name).unwrap().spec().clone();
    assert_eq!(spec("web_search").loading, Loading::Deferred);
    assert_eq!(spec("web_fetch").loading, Loading::Eager);
    assert!(spec("web_fetch").effects.network);
}

#[tokio::test]
async fn web_search_renders_the_configured_engine() {
    let fake = renderer(Ok(page(&fixture("bing.html"))));
    let out = WebSearch::new(web(&fake, Engine::Bing)).call(json!({"query": "rust programming language", "limit": 2}), cx()).await.unwrap();
    assert_eq!(fake.calls.lock().unwrap()[0].url, "https://www.bing.com/search?q=rust+programming+language");
    assert_eq!(
        text(&out),
        "1. Rust Programming Language\n   https://www.rust-lang.org/\n   A language empowering everyone to build reliable and efficient software.\n2. The Rust Programming Language - The Rust Book\n   https://doc.rust-lang.org/book/\n   by Steve Klabnik and Carol Nichols & contributions from the Rust Community."
    );
    assert_eq!(out.metadata["engine"], "bing");
    assert_eq!(out.metadata["results"].as_array().unwrap().len(), 2);
    let empty = renderer(Ok(page(&fixture("cloudflare.html"))));
    let out = WebSearch::new(web(&empty, Engine::Google)).call(json!({"query": "x"}), cx()).await.unwrap();
    assert_eq!(text(&out), "No results: google showed a bot check.");
    let broken = renderer(Err(RenderError::Timeout("nothing loaded".into())));
    let out = WebSearch::new(web(&broken, Engine::Duckduckgo)).call(json!({"query": "x"}), cx()).await.unwrap();
    assert!(out.is_error && text(&out) == "Search failed: nothing loaded");
}

#[tokio::test]
async fn google_goto_links_are_kept_for_resolving() {
    // Recorded from a rendered results page: links are opaque `/goto?url=` redirects.
    let hits = web_search::google(&fixture("google_goto.html"));
    assert_eq!(hits.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(), ["Rust Programming Language", "Rust Tutorial"]);
    assert!(hits.iter().all(|h| h.url.starts_with("https://www.google.com/goto?url=")), "{hits:#?}");
    assert!(hits[0].snippet.starts_with("Rust is blazingly fast and memory-efficient"), "{hits:#?}");
    // Other links pass through untouched (no request).
    let plain = web_search::google(&fixture("google.html"));
    assert_eq!(web_search::resolve(plain.clone()).await, plain);
}
