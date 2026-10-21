use super::*;
use crate::ToolOutput;
tool!(
    WebFetch,
    spec(
        "web_fetch",
        "Fetch an HTTP(S) URL as readable text. HTML tags, scripts and styles are removed. Requests time out after 30 seconds; responses over 5 MiB are rejected. Does not execute JavaScript.",
        json!({"url":{"type":"string","format":"uri"}}),
        &["url"],
        Effects::NETWORK
    ),
    run
);
pub(crate) fn readable(html: &str) -> String {
    let suppressed = regex::Regex::new(r"(?is)<(script|style)\b[^>]*>.*?</(?:script|style)\s*>").expect("static regex");
    let text = suppressed.replace_all(html, "");
    let breaks = regex::Regex::new(r"(?i)</?(?:p|div|br|h[1-6]|li|tr|section|article)\b[^>]*>").expect("static regex");
    let text = breaks.replace_all(&text, "\n");
    let tags = regex::Regex::new(r"(?s)<[^>]*>").expect("static regex");
    let text = tags.replace_all(&text, "");
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let url = reqwest::Url::parse(string(&input, "url")?).map_err(|e| ToolError::invalid(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::invalid("Only HTTP(S) URLs are supported"));
    }
    let fetch = async {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| ToolError::failed(e.to_string()))?;
        let mut response =
            client.get(url).send().await.and_then(reqwest::Response::error_for_status).map_err(|e| ToolError::failed(e.to_string()))?;
        let html = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|s| s.contains("html"));
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| ToolError::failed(e.to_string()))? {
            if data.len() + chunk.len() > 5 * 1024 * 1024 {
                return Err(ToolError::failed("Response exceeds 5 MiB"));
            }
            data.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&data);
        crate::output::limit(if html { readable(&text) } else { text.into_owned() }, &cx)
    };
    tokio::select! {result=fetch=>result,()=cx.cancel.cancelled()=>Err(ToolError::new(crate::ToolErrorKind::Cancelled,"Fetch cancelled"))}
}

#[cfg(test)]
mod tests {
    #[test]
    fn html_to_readable_text() {
        assert_eq!(super::readable("<h1>Title</h1><script>bad()</script><style>.bad{}</style><p>A &amp; B<br>Next</p>"), "Title\n\nA & B\n\nNext");
    }
}
