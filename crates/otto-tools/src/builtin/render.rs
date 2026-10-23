//! Rendered page fetches: a hidden page in the silo app's off-screen
//! browser host (silo's `browser_fetch`, docs/AGENTS.md).
use crate::ToolCx;

/// What to load. `timeout_ms` defaults to 15 s and is capped at 60 s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderRequest {
    pub url: String,
    pub timeout_ms: Option<u64>,
    /// Also capture the viewport.
    pub screenshot: bool,
    /// CSS selector: `html` is that element's `outerHTML`.
    pub selector: Option<String>,
}

/// The page after load and network quiet (or the deadline).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    /// The final URL, after redirects.
    pub url: String,
    pub title: String,
    /// The main document's HTTP status, when known.
    pub status: Option<u16>,
    /// The rendered DOM's `outerHTML` (or the selector's element).
    pub html: String,
    /// The deadline passed before the page settled: `html` is what existed.
    pub timed_out: bool,
    /// A bot check didn't clear: `title` and `html` are the challenge page's.
    pub blocked: bool,
    /// The viewport image: (mime, base64 data).
    pub screenshot: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderError {
    /// No silo app runs pages (silo headless, the app closed, or no silo).
    NoApp(String),
    /// Nothing loaded before the deadline.
    Timeout(String),
    /// DNS, TLS or connection failure.
    Navigation(String),
    /// Anything else (bad selector, engine failure).
    Failed(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::NoApp(m) | Self::Timeout(m) | Self::Navigation(m) | Self::Failed(m)) = self;
        f.write_str(m)
    }
}

impl std::error::Error for RenderError {}

/// Loads pages in a real browser: silo's off-screen host.
#[async_trait::async_trait]
pub trait Renderer: Send + Sync {
    async fn render(&self, request: RenderRequest, cx: &ToolCx) -> Result<Rendered, RenderError>;
}
