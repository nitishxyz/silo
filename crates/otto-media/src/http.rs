//! Requests to providers: cancellable, their errors made readable and scrubbed of the key.
use crate::{Call, Generated, MediaError, MediaErrorKind};
use serde_json::Value;
use std::time::Duration;

pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        // The slowest image models think for minutes.
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap_or_default()
}

/// `request` sent, cancellable: its JSON body, or the provider's error as `<name>: <message>`.
pub(crate) async fn send(call: &Call, name: &str, key: &str, request: reqwest::RequestBuilder) -> Result<Value, MediaError> {
    let (body, _) = send_raw(call, name, key, request).await?;
    serde_json::from_slice(&body).map_err(|_| MediaError::failed(format!("{name}: unreadable response")))
}

/// `request` sent, its body a file (speech, sound): the bytes and their type.
pub(crate) async fn send_file(call: &Call, name: &str, key: &str, request: reqwest::RequestBuilder) -> Result<Generated, MediaError> {
    let (body, mime) = send_raw(call, name, key, request).await?;
    if body.is_empty() {
        return Err(MediaError::failed(format!("{name} returned no file")));
    }
    Ok(generated(body, &mime))
}

/// The body and content type of a successful response.
async fn send_raw(call: &Call, name: &str, key: &str, request: reqwest::RequestBuilder) -> Result<(Vec<u8>, String), MediaError> {
    let response = tokio::select! {
        response = request.send() => response.map_err(|e| MediaError::failed(scrub(&format!("{name}: {}", without_url(&e)), key)))?,
        () = call.cancel.cancelled() => return Err(MediaError::cancelled()),
    };
    let status = response.status();
    let mime = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    let body = tokio::select! {
        body = response.bytes() => body.map_err(|e| MediaError::failed(scrub(&format!("{name}: {}", without_url(&e)), key)))?,
        () = call.cancel.cancelled() => return Err(MediaError::cancelled()),
    };
    if !status.is_success() {
        return Err(api_error(name, status.as_u16(), &body, key));
    }
    Ok((body.to_vec(), mime))
}

fn without_url(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "can't connect".into()
    } else {
        let text = e.to_string();
        e.url().map_or(text.clone(), |url| text.replace(&format!(" for url ({url})"), ""))
    }
}

/// `<name>: <the provider's message> (HTTP <status>)`; 401/403 also say the key was refused.
pub(crate) fn api_error(name: &str, status: u16, body: &[u8], key: &str) -> MediaError {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let message = [&value["error"]["message"], &value["error"], &value["detail"], &value["message"]]
        .into_iter()
        .find_map(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Array(items) => {
                Some(items.iter().map(|i| i["msg"].as_str().map_or_else(|| i.to_string(), str::to_owned)).collect::<Vec<_>>().join("; "))
            }
            _ => None,
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(300).collect::<String>().trim().to_owned());
    let message = if message.is_empty() { "no details".into() } else { message };
    let kind = if matches!(status, 401 | 403) { MediaErrorKind::Auth } else { MediaErrorKind::Failed };
    let text = match status {
        401 => format!("{name} refused the API key: {message}"),
        429 => format!("{name}: rate limited: {message}"),
        _ => format!("{name}: {message} (HTTP {status})"),
    };
    MediaError { kind, message: scrub(&text, key) }
}

/// `text` without `key` or either half of a two-part key.
pub(crate) fn scrub(text: &str, key: &str) -> String {
    let mut text = text.to_owned();
    for part in std::iter::once(key).chain(key.split(':')).filter(|p| p.len() >= 8) {
        text = text.replace(part, "[key]");
    }
    text
}

/// The bytes at `url` (a provider's output link, no credentials; or a `data:` URI).
pub(crate) async fn download(call: &Call, name: &str, url: &str) -> Result<Generated, MediaError> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (mime, data) = rest.split_once(";base64,").ok_or_else(|| MediaError::failed(format!("{name}: unreadable file data")))?;
        return decoded(name, data, mime);
    }
    fetch(call, name, call.http.get(url)).await
}

/// The file `request` gets (a download, keyed where the provider's links need it).
pub(crate) async fn fetch(call: &Call, name: &str, request: reqwest::RequestBuilder) -> Result<Generated, MediaError> {
    let response = tokio::select! {
        r = request.send() => r.map_err(|e| MediaError::failed(format!("{name}: download failed: {}", without_url(&e))))?,
        () = call.cancel.cancelled() => return Err(MediaError::cancelled()),
    };
    if !response.status().is_success() {
        return Err(MediaError::failed(format!("{name}: download failed (HTTP {})", response.status().as_u16())));
    }
    let mime = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    let bytes = tokio::select! {
        b = response.bytes() => b.map_err(|e| MediaError::failed(format!("{name}: download failed: {}", without_url(&e))))?,
        () = call.cancel.cancelled() => return Err(MediaError::cancelled()),
    };
    Ok(generated(bytes.to_vec(), &mime))
}

/// Base64 `data` as a file; the mime from its bytes when they say.
pub(crate) fn decoded(name: &str, data: &str, mime: &str) -> Result<Generated, MediaError> {
    let bytes = otto_tools::attachments::decode(data).map_err(|_| MediaError::failed(format!("{name}: unreadable file data")))?;
    Ok(generated(bytes, mime))
}

pub(crate) fn generated(bytes: Vec<u8>, mime: &str) -> Generated {
    let mime = otto_tools::images::sniff(&bytes)
        .or_else(|| crate::probe::sniff(&bytes))
        .map_or_else(|| mime.split(';').next().unwrap_or("").trim().to_owned(), str::to_owned);
    Generated { bytes, mime: if mime.is_empty() { "application/octet-stream".into() } else { mime } }
}

/// Runs `n` of `one` at once (providers that make one image a request).
pub(crate) async fn times<F, Fut>(n: u8, one: F) -> Result<Vec<Generated>, MediaError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Generated>, MediaError>>,
{
    let all = futures::future::try_join_all((0..n.max(1)).map(|_| one())).await?;
    Ok(all.into_iter().flatten().collect())
}

/// The files a finished request links (any provider's result shape): `url`/`…_url` strings and `…urls` lists,
/// not the request's own links (`status_url`, an echoed `input`) or previews.
pub(crate) fn links(value: &Value) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    collect_links(value, "", &mut out);
    out
}

fn collect_links(value: &Value, key: &str, out: &mut Vec<(String, Option<String>)>) {
    const SKIP: &[&str] = &["input", "arguments", "params", "request", "logs", "metrics", "status_url", "cancel_url", "response_url"];
    if SKIP.contains(&key) || ["thumbnail", "preview", "poster"].iter().any(|w| key.contains(w)) {
        return;
    }
    let linkish = |s: &str| s.starts_with("https://") || s.starts_with("http://") || s.starts_with("data:");
    match value {
        Value::String(s) if (key == "url" || key.ends_with("_url") || key.ends_with("urls")) && linkish(s) => {
            if !out.iter().any(|(u, _)| u == s) {
                out.push((s.clone(), None));
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_links(i, key, out)),
        Value::Object(map) => {
            let mime = map.get("content_type").or_else(|| map.get("mime_type")).and_then(Value::as_str).map(str::to_owned);
            let before = out.len();
            for (k, v) in map {
                collect_links(v, k, out);
            }
            if let (Some(mime), [one]) = (mime, &mut out[before..]) {
                one.1 = Some(mime);
            }
        }
        _ => {}
    }
}

/// The images, videos and audio a finished request links, downloaded (files of other types are left out).
pub(crate) async fn outputs(call: &Call, name: &str, value: &Value) -> Result<Vec<Generated>, MediaError> {
    let media = |m: &str| ["image/", "video/", "audio/"].iter().any(|p| m.starts_with(p));
    let mut files = Vec::new();
    for (link, mime) in links(value) {
        if mime.as_deref().is_some_and(|m| !media(m)) {
            continue;
        }
        let file = download(call, name, &link).await?;
        if media(&file.mime) {
            files.push(file);
        }
    }
    if files.is_empty() {
        return Err(MediaError::failed(format!("{name} returned no media")));
    }
    Ok(files)
}
