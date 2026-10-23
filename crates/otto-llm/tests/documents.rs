#![cfg(feature = "client")]
//! A PDF from `read` (a tool result document) goes as a document where the
//! provider takes one, else as a note saying it wasn't sent.
use otto_llm::*;
use serde_json::Value;

fn req() -> Request {
    let pdf = Media { mime: "application/pdf".into(), source: MediaSource::Base64 { data: "JVBERi0=".into() }, name: Some("spec.pdf".into()) };
    Request {
        model: "m".into(),
        items: vec![
            Item::user_text("u", "read it"),
            Item::new(
                "a",
                Role::Assistant,
                vec![Part::ToolCall {
                    call_id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path":"spec.pdf"}),
                    kind: CallKind::Function,
                    opaque: None,
                }],
            ),
            Item::new(
                "t",
                Role::Tool,
                vec![Part::ToolResult {
                    call_id: "c1".into(),
                    content: vec![ResultPart::text("spec.pdf: PDF, 5 B"), ResultPart::Document(pdf)],
                    is_error: false,
                }],
            ),
        ],
        ..Request::default()
    }
}

fn find<'a>(value: &'a Value, kind: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) if map.get("type").and_then(Value::as_str) == Some(kind) => Some(value),
        Value::Object(map) => map.values().find_map(|v| find(v, kind)),
        Value::Array(items) => items.iter().find_map(|v| find(v, kind)),
        _ => None,
    }
}

fn noted(value: &Value) -> bool {
    value.to_string().contains("spec.pdf (application/pdf) not sent")
}

#[test]
fn anthropic_sends_pdfs_and_compatible_endpoints_get_a_note() {
    let body = anthropic::convert::body_for(&req(), "anthropic").unwrap();
    let doc = find(&body, "document").unwrap();
    assert_eq!(doc["source"]["media_type"], "application/pdf");
    assert!(!noted(&body));
    let minimax = anthropic::convert::body_for(&req(), "minimax").unwrap();
    assert!(find(&minimax, "document").is_none());
    assert!(noted(&minimax));
}

#[test]
fn openai_sends_pdfs_as_files_and_xai_gets_a_note() {
    let input = Value::Array(openai::convert::input(&req().items, "openai"));
    let file = find(&input, "input_file").unwrap();
    assert_eq!(file["filename"], "spec.pdf");
    assert!(file["file_data"].as_str().unwrap().starts_with("data:application/pdf;base64,"));
    let xai = Value::Array(openai::convert::input(&req().items, "xai"));
    assert!(find(&xai, "input_file").is_none());
    assert!(noted(&xai));
}

#[test]
fn google_sends_pdfs_as_documents() {
    let body = google::convert::request(&req()).unwrap();
    assert_eq!(find(&body, "document").unwrap()["mime_type"], "application/pdf");
}

#[test]
fn compat_sends_pdfs_to_openrouter_only() {
    let dialect = |id| compat::dialect(id).unwrap();
    let openrouter = compat::convert::request(&req(), dialect("openrouter")).unwrap();
    assert_eq!(find(&openrouter, "file").unwrap()["file"]["filename"], "spec.pdf");
    for id in ["deepseek", "ollama", "custom"] {
        let body = compat::convert::request(&req(), dialect(id)).unwrap();
        assert!(find(&body, "file").is_none(), "{id}");
        assert!(noted(&body), "{id}");
    }
}
