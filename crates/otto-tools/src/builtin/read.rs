use super::*;
use crate::{OutputPart, ToolOutput, attachments, images};
use base64::Engine;
tool!(
    Read,
    spec(
        "read",
        "Read a UTF-8 file with 1-based line numbers. offset is a 1-based starting line; limit bounds lines. Images (PNG, JPEG, GIF, WebP, TIFF, BMP) come back as image content, downscaled to max_edge (default 1568) px on the long edge; PDFs as a document. attachment_id (from an [attachment att_…] note) reads a file the user attached instead of path. Text over 20 MiB and binary text are rejected.",
        json!({
            "path":{"type":"string"},
            "attachment_id":{"type":"string","description":"An attachment id such as att_3f2a9c1b, instead of path"},
            "offset":{"type":"integer","minimum":1},
            "limit":{"type":"integer","minimum":1},
            "max_edge":{"type":"integer","minimum":64,"maximum":8000,"description":"Images: long edge in px (never upscaled)"},
            "quality":{"type":"integer","minimum":1,"maximum":100,"description":"Images: JPEG quality when recompressed (default 85)"}
        }),
        &[],
        Effects::READ
    ),
    run
);
const MAX_TEXT: usize = 20 * 1024 * 1024;
const MAX_FILE: usize = 25 * 1024 * 1024;
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let attached = input.get("attachment_id").and_then(Value::as_str).is_some();
    let (mut output, name) = read(input, cx).await?;
    // An attachment is named by its note, never by where it's stored.
    if attached {
        match &mut output.metadata {
            Value::Object(m) => {
                m.insert("name".into(), name.into());
            }
            meta => *meta = json!({"name": name}),
        }
    }
    Ok(output)
}
async fn read(input: Value, cx: ToolCx) -> Result<(ToolOutput, String), ToolError> {
    let (path, name) = match input.get("attachment_id").and_then(Value::as_str) {
        Some(id) => {
            let root = cx.attachments.as_ref().ok_or_else(|| ToolError::invalid("This session has no attachments"))?;
            let (attachment, path) =
                attachments::find(root, &cx.sessions, id).map_err(|e| ToolError::new(crate::ToolErrorKind::NotFound, e.to_string()))?;
            (path, attachment.name)
        }
        None => {
            let path = path(&input, &cx)?;
            let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            (path, name)
        }
    };
    let read = tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let file = regular_open(&path).map_err(io)?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE as u64 + 1).read_to_end(&mut bytes).map_err(io)?;
        Ok::<_, ToolError>(bytes)
    });
    let bytes = tokio::select! {
        result = read => result.map_err(|e| ToolError::failed(e.to_string()))??,
        () = cx.cancel.cancelled() => return Err(ToolError::new(crate::ToolErrorKind::Cancelled, "Read cancelled")),
    };
    if bytes.len() > MAX_FILE {
        return Err(ToolError::invalid("File exceeds 25 MiB; use search or a smaller file"));
    }
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    if bytes.starts_with(b"%PDF-") {
        if bytes.len() > MAX_TEXT {
            return Err(ToolError::invalid("PDF exceeds 20 MiB; split it or extract its text with the shell"));
        }
        let text = format!("{name}: PDF, {}", attachments::size(bytes.len() as u64));
        let output = ToolOutput {
            content: vec![
                OutputPart::Text { text },
                OutputPart::Document { mime: "application/pdf".into(), data_base64: b64(&bytes), name: Some(name.clone()) },
            ],
            ..Default::default()
        };
        return Ok((output, name));
    }
    if images::decodable(&bytes) && !cx.vision {
        let size = attachments::size(bytes.len() as u64);
        let text = match images::describe(&bytes) {
            Some(((w, h), format)) => format!("{name}: {w}×{h} {format}, {size}; this model can't view images"),
            None => format!("{name}: image, {size}; this model can't view images"),
        };
        return Ok((ToolOutput::text(text).with_metadata(json!({"bytes": bytes.len()})), name));
    }
    if images::decodable(&bytes) {
        let max_edge = input.get("max_edge").and_then(Value::as_u64).map_or(images::MAX_EDGE, |e| e.clamp(64, 8000) as u32);
        let quality = input.get("quality").and_then(Value::as_u64).map_or(images::QUALITY, |q| q.clamp(1, 100) as u8);
        let prepared = tokio::task::spawn_blocking(move || images::prepare(&bytes, max_edge, quality))
            .await
            .map_err(|e| ToolError::failed(e.to_string()))?
            .map_err(|e| ToolError::invalid(format!("{name}: {e}")))?;
        let text = format!("{name} {}", prepared.dimensions());
        let output = ToolOutput {
            content: vec![OutputPart::Text { text }, OutputPart::Image { mime: prepared.mime.into(), data_base64: b64(&prepared.bytes) }],
            metadata: json!({"original":[prepared.original.0, prepared.original.1],"sent":[prepared.sent.0, prepared.sent.1],"bytes":prepared.bytes.len()}),
            ..Default::default()
        };
        return Ok((output, name));
    }
    if bytes.len() > MAX_TEXT {
        return Err(ToolError::invalid("File exceeds 20 MiB; use search or a smaller file"));
    }
    if bytes.contains(&0) {
        return Err(ToolError::invalid("Binary file cannot be read as text"));
    }
    let text = String::from_utf8(bytes).map_err(|_| ToolError::invalid("File is not UTF-8"))?;
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
    let output =
        crate::output::limit(text.lines().enumerate().skip(offset - 1).take(limit).map(|(i, line)| format!("{}: {line}\n", i + 1)).collect(), &cx)?;
    Ok((output, name))
}
