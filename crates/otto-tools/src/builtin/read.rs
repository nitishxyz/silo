use super::*;
use crate::{OutputPart, ToolOutput};
use base64::Engine;
tool!(
    Read,
    spec(
        "read",
        "Read a UTF-8 file with 1-based line numbers. offset is a 1-based starting line; limit bounds lines. Common images are returned as image content. Files over 20 MiB and binary text are rejected.",
        json!({"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}}),
        &["path"],
        Effects::READ
    ),
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let path = path(&input, &cx)?;
    let read = tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let file = regular_open(&path).map_err(io)?;
        let mut bytes = Vec::new();
        file.take(20 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(io)?;
        Ok::<_, ToolError>(bytes)
    });
    let bytes = tokio::select! {
        result = read => result.map_err(|e| ToolError::failed(e.to_string()))??,
        () = cx.cancel.cancelled() => return Err(ToolError::new(crate::ToolErrorKind::Cancelled, "Read cancelled")),
    };
    if bytes.len() > 20 * 1024 * 1024 {
        return Err(ToolError::invalid("File exceeds 20 MiB; use search or a smaller file"));
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    };
    if let Some(mime) = mime {
        return Ok(ToolOutput {
            content: vec![OutputPart::Image { mime: mime.into(), data_base64: base64::engine::general_purpose::STANDARD.encode(bytes) }],
            ..Default::default()
        });
    }
    if bytes.contains(&0) {
        return Err(ToolError::invalid("Binary file cannot be read as text"));
    }
    let text = String::from_utf8(bytes).map_err(|_| ToolError::invalid("File is not UTF-8"))?;
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
    crate::output::limit(text.lines().enumerate().skip(offset - 1).take(limit).map(|(i, line)| format!("{}: {line}\n", i + 1)).collect(), &cx)
}
