//! What crosses the wire is bounded: a view carries the newest items that fit
//! [`VIEW_BUDGET`], and an item, tool result or tool input over [`ITEM_BUDGET`]
//! reaches clients clipped (text head and tail; images and documents as a
//! placeholder naming their mime and size). Only the client's copy is clipped:
//! the stored item and what the model sees are untouched.

use otto_llm::{Item, MediaSource, Part, ResultPart};
use serde_json::Value;

use crate::Event;

/// Serialized items a `session.view` reply carries at most (half of [`crate::MAX_FRAME`]).
pub const VIEW_BUDGET: usize = 8 * 1024 * 1024;
/// An item, tool result or tool input larger than this (serialized) reaches clients clipped.
pub const ITEM_BUDGET: usize = 256 * 1024;
/// Items a client asks for when it loads an earlier page (`session.view` with `before_seq`).
pub const PAGE_ITEMS: u32 = 200;
/// Strings shorter than this are never clipped.
const MIN_KEEP: usize = 1024;

/// Serialized size of `value` in bytes.
#[must_use]
pub fn json_len(value: &impl serde::Serialize) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

/// `2.1 MB`, `340 KB`, `12 bytes`.
#[must_use]
pub fn size_label(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} bytes")
    }
}

fn decoded_len(base64: &str) -> u64 {
    (base64.len() as u64) * 3 / 4
}

/// What a clipped image or document reads as.
#[must_use]
pub fn media_placeholder(mime: &str, bytes: u64) -> String {
    format!("[{mime}, {}: not sent to this client]", size_label(bytes))
}

/// Keep `text`'s head (three quarters of `keep`) and tail, with a note of what
/// was left out between them. `true` when it was clipped.
pub fn clip_text(text: &mut String, keep: usize) -> bool {
    let keep = keep.max(MIN_KEEP);
    if text.len() <= keep {
        return false;
    }
    let mut head = keep * 3 / 4;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - keep / 4;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    let cut = size_label((tail - head) as u64);
    *text = format!("{}\n\n[… {cut} clipped …]\n\n{}", &text[..head], &text[tail..]);
    true
}

/// Clip every string in `value` longer than `keep`. `true` when any was.
pub fn clip_value(value: &mut Value, keep: usize) -> bool {
    match value {
        Value::String(s) => clip_text(s, keep),
        Value::Array(items) => items.iter_mut().fold(false, |any, v| clip_value(v, keep) | any),
        Value::Object(map) => map.values_mut().fold(false, |any, v| clip_value(v, keep) | any),
        _ => false,
    }
}

fn long_strings(value: &Value) -> usize {
    match value {
        Value::String(s) => usize::from(s.len() > MIN_KEEP),
        Value::Array(items) => items.iter().map(long_strings).sum(),
        Value::Object(map) => map.values().map(long_strings).sum(),
        _ => 0,
    }
}

/// How much of each long string to keep so that `count` of them share the item budget.
fn share(count: usize) -> usize {
    (ITEM_BUDGET * 3 / 4 / count.max(1)).max(MIN_KEEP)
}

/// Clip `value` (a tool input or progress output) when it serializes over [`ITEM_BUDGET`].
pub fn clip_large_value(value: &mut Value) -> bool {
    if json_len(value) <= ITEM_BUDGET {
        return false;
    }
    let keep = share(long_strings(value));
    clip_value(value, keep)
}

/// Clip an item over [`ITEM_BUDGET`] for a client: base64 media become
/// placeholders (an empty image or document part; in a tool result, a text
/// part naming its mime and size), provider-opaque data is dropped and long
/// strings keep their head and tail. Sets [`Item::clipped`] to the size it had.
/// `true` when it was clipped.
pub fn clip_item(item: &mut Item) -> bool {
    let size = json_len(item);
    if size <= ITEM_BUDGET || item.clipped.is_some() {
        return false;
    }
    let mut strings = 0;
    for part in &mut item.parts {
        match part {
            Part::Image(media) | Part::Document(media) | Part::Audio(media) => {
                if let MediaSource::Base64 { data } = &mut media.source {
                    data.clear();
                }
            }
            Part::Reasoning { opaque, summary, text } => {
                *opaque = None;
                strings += usize::from(summary.as_ref().is_some_and(|s| s.len() > MIN_KEEP));
                strings += usize::from(text.as_ref().is_some_and(|s| s.len() > MIN_KEEP));
            }
            Part::ToolCall { opaque, input, .. } => {
                *opaque = None;
                strings += long_strings(input);
            }
            Part::HostedResult { opaque, payload, .. } => {
                *opaque = None;
                strings += long_strings(payload);
            }
            Part::Compaction { opaque, summary } => {
                *opaque = None;
                strings += usize::from(summary.as_ref().is_some_and(|s| s.len() > MIN_KEEP));
            }
            Part::ToolResult { content, .. } => {
                for c in content.iter_mut() {
                    match c {
                        ResultPart::Image(media) | ResultPart::Document(media) => {
                            let bytes = match &media.source {
                                MediaSource::Base64 { data } => decoded_len(data),
                                _ => 0,
                            };
                            *c = ResultPart::text(media_placeholder(&media.mime, bytes));
                        }
                        ResultPart::Text { text } => strings += usize::from(text.len() > MIN_KEEP),
                    }
                }
            }
            Part::Text { text, .. } | Part::System { text } => strings += usize::from(text.len() > MIN_KEEP),
            Part::ToolsChanged { .. } => {}
        }
    }
    let keep = share(strings);
    for part in &mut item.parts {
        match part {
            Part::Text { text, .. } | Part::System { text } => {
                clip_text(text, keep);
            }
            Part::Reasoning { summary, text, .. } => {
                for s in [summary, text].into_iter().flatten() {
                    clip_text(s, keep);
                }
            }
            Part::ToolCall { input, .. } => {
                clip_value(input, keep);
            }
            Part::HostedResult { payload, .. } => {
                clip_value(payload, keep);
            }
            Part::Compaction { summary: Some(s), .. } => {
                clip_text(s, keep);
            }
            Part::ToolResult { content, .. } => {
                for c in content {
                    if let ResultPart::Text { text } = c {
                        clip_text(text, keep);
                    }
                }
            }
            _ => {}
        }
    }
    // Thousands of small parts can still add up: keep only what links the item
    // to the rest of the transcript.
    if json_len(item) > ITEM_BUDGET * 2 {
        let note = format!("[{}: too large to show]", size_label(size as u64));
        let mut kept = Vec::new();
        for part in std::mem::take(&mut item.parts) {
            match part {
                Part::ToolCall { call_id, name, kind, .. } => {
                    kept.push(Part::ToolCall { call_id, name, input: Value::Object(serde_json::Map::new()), kind, opaque: None });
                }
                Part::ToolResult { call_id, is_error, .. } => {
                    kept.push(Part::ToolResult { call_id, content: vec![ResultPart::text(&note)], is_error })
                }
                _ => {}
            }
        }
        if kept.is_empty() {
            kept.push(Part::text(note));
        }
        item.parts = kept;
    }
    item.clipped = Some(size as u64);
    true
}

/// Clip a `tool.result` output (a serialized tool output: `content` parts,
/// `metadata`) over [`ITEM_BUDGET`]: images and documents keep their `type`
/// and `mime` with empty data and gain `bytes`; long strings keep their head
/// and tail; the output gains `clipped: {bytes}` with its size before.
pub fn clip_output(output: &mut Value) -> bool {
    let size = json_len(output);
    if size <= ITEM_BUDGET || output.get("clipped").is_some() {
        return false;
    }
    if let Some(parts) = output.get_mut("content").and_then(Value::as_array_mut) {
        for part in parts {
            if let Some(data) = part.get("data_base64").and_then(Value::as_str) {
                let bytes = decoded_len(data);
                part["data_base64"] = Value::String(String::new());
                part["bytes"] = Value::from(bytes);
            }
        }
    }
    let keep = share(long_strings(output));
    clip_value(output, keep);
    if json_len(output) > ITEM_BUDGET * 2 {
        let text = format!("[{}: too large to show]", size_label(size as u64));
        let is_error = output.get("is_error").cloned().unwrap_or(Value::Bool(false));
        *output = serde_json::json!({"content": [{"type": "text", "text": text}], "is_error": is_error, "metadata": null});
    }
    if let Some(map) = output.as_object_mut() {
        map.insert("clipped".into(), serde_json::json!({ "bytes": size }));
    }
    true
}

/// The client's copy of a journal event: items, tool results, inputs and
/// progress clipped as above. `true` when anything was.
pub fn clip_event(event: &mut Event) -> bool {
    match event {
        Event::ItemDone { item } => clip_item(item),
        Event::ToolResult { output, .. } => clip_output(output),
        Event::ToolCall { input, .. } | Event::ApprovalRequested { input, .. } => clip_large_value(input),
        Event::ToolProgress { output: Some(output), .. } => clip_large_value(output),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_llm::{Media, Role};

    fn image(bytes: usize) -> Media {
        Media { mime: "image/png".into(), source: MediaSource::Base64 { data: "A".repeat(bytes * 4 / 3) }, name: Some("shot.png".into()) }
    }

    #[test]
    fn small_items_pass_untouched() {
        let mut item = Item::user_text("u", "hello");
        let before = item.clone();
        assert!(!clip_item(&mut item));
        assert_eq!(item, before);
    }

    #[test]
    fn a_huge_tool_result_keeps_its_head_and_tail_and_names_its_images() {
        let text = format!("HEAD{}TAIL", "x".repeat(3 * 1024 * 1024));
        let mut item = Item::new(
            "r",
            Role::Tool,
            vec![Part::ToolResult {
                call_id: "c1".into(),
                content: vec![ResultPart::text(text), ResultPart::Image(image(2 * 1024 * 1024)), ResultPart::Document(image(10))],
                is_error: false,
            }],
        );
        let size = json_len(&item);
        assert!(clip_item(&mut item));
        assert_eq!(item.clipped, Some(size as u64));
        assert!(json_len(&item) <= ITEM_BUDGET);
        let Part::ToolResult { call_id, content, .. } = &item.parts[0] else { panic!() };
        assert_eq!(call_id, "c1");
        let ResultPart::Text { text } = &content[0] else { panic!() };
        assert!(text.starts_with("HEAD") && text.ends_with("TAIL") && text.contains("clipped"));
        assert!(matches!(&content[1], ResultPart::Text { text } if text == "[image/png, 2.0 MB: not sent to this client]"));
        assert!(matches!(&content[2], ResultPart::Text { text } if text.starts_with("[image/png, ") && text.contains(" bytes:")));
        // Clipping a clipped copy again changes nothing.
        let once = item.clone();
        assert!(!clip_item(&mut item));
        assert_eq!(item, once);
    }

    #[test]
    fn a_user_image_keeps_its_part_without_data() {
        let mut item = Item::new("u", Role::User, vec![Part::text("look"), Part::Image(image(1024 * 1024))]);
        assert!(clip_item(&mut item));
        assert!(
            matches!(&item.parts[1], Part::Image(Media { source: MediaSource::Base64 { data }, name: Some(n), .. }) if data.is_empty() && n == "shot.png")
        );
        assert_eq!(item.text(), "look");
    }

    #[test]
    fn many_parts_share_the_budget_and_multibyte_text_splits_cleanly() {
        let parts = (0..40).map(|i| Part::text(format!("{i}:{}", "é".repeat(100_000)))).collect();
        let mut item = Item::new("a", Role::Assistant, parts);
        assert!(clip_item(&mut item));
        assert!(json_len(&item) <= ITEM_BUDGET * 2, "{}", json_len(&item));
        assert_eq!(item.parts.len(), 40);
        let mut tiny = (0..200_000).map(|i| Part::System { text: format!("{i}") }).collect::<Vec<_>>();
        tiny.push(Part::ToolCall { call_id: "c".into(), name: "x".into(), input: Value::Null, kind: otto_llm::CallKind::Function, opaque: None });
        let mut many = Item::new("m", Role::Assistant, tiny);
        assert!(clip_item(&mut many));
        assert!(json_len(&many) <= ITEM_BUDGET);
        assert!(matches!(&many.parts[..], [Part::ToolCall { call_id, .. }] if call_id == "c"));
    }

    #[test]
    fn tool_result_outputs_and_inputs_are_bounded() {
        let mut output = serde_json::json!({
            "content": [{"type": "text", "text": "y".repeat(1_000_000)}, {"type": "image", "mime": "image/jpeg", "data_base64": "B".repeat(4_000_000)}],
            "is_error": false,
            "metadata": {"exit": 0},
        });
        let size = json_len(&output);
        let mut event = Event::ToolResult { call_id: "c".into(), is_error: false, output: output.clone(), item_id: "i".into() };
        assert!(clip_event(&mut event));
        assert!(clip_output(&mut output));
        assert!(json_len(&output) <= ITEM_BUDGET);
        assert_eq!(output["clipped"]["bytes"], size);
        assert_eq!(output["content"][1]["mime"], "image/jpeg");
        assert_eq!(output["content"][1]["data_base64"], "");
        assert_eq!(output["content"][1]["bytes"], 3_000_000);
        assert_eq!(output["metadata"]["exit"], 0);
        let mut input = serde_json::json!({"path": "a", "content": "z".repeat(2_000_000)});
        assert!(clip_large_value(&mut input));
        assert!(json_len(&input) <= ITEM_BUDGET);
        assert_eq!(input["path"], "a");
        let mut small = Event::ToolResult { call_id: "c".into(), is_error: false, output: serde_json::json!({"content": []}), item_id: "i".into() };
        assert!(!clip_event(&mut small));
    }
}
