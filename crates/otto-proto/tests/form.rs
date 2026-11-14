//! Elicitation forms: a requested schema as fields, answers checked against it.

use otto_proto::{FieldKind, PendingQuestion, QuestionForm};
use serde_json::{Value, json};

fn content(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn sample() -> QuestionForm {
    QuestionForm::from_schema(&json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "title": "Name", "description": "Your name", "minLength": 2, "default": "Ann"},
            "email": {"type": "string", "format": "email"},
            "age": {"type": "integer", "minimum": 0, "maximum": 150},
            "ratio": {"type": "number", "default": 0.5},
            "subscribe": {"type": "boolean", "title": "Subscribe", "default": true},
            "size": {"type": "string", "enum": ["s", "m", "l"], "enumNames": ["Small", "Medium", "Large"]},
            "color": {"type": "string", "oneOf": [{"const": "#f00", "title": "Red"}, {"const": "#00f", "title": "Blue"}]}
        },
        "required": ["name", "size"]
    }))
    .unwrap()
}

#[test]
fn schema_becomes_fields() {
    let form = sample();
    let kinds: Vec<(&str, FieldKind, bool)> = form.fields.iter().map(|f| (f.name.as_str(), f.kind, f.required)).collect();
    assert_eq!(
        kinds,
        [
            ("name", FieldKind::Text, true),
            ("email", FieldKind::Text, false),
            ("age", FieldKind::Integer, false),
            ("ratio", FieldKind::Number, false),
            ("subscribe", FieldKind::Boolean, false),
            ("size", FieldKind::Choice, true),
            ("color", FieldKind::Choice, false),
        ]
    );
    let name = form.field("name").unwrap();
    assert_eq!((name.label(), name.description.as_deref(), name.default_text().as_str()), ("Name", Some("Your name"), "Ann"));
    assert_eq!(form.field("email").unwrap().format.as_deref(), Some("email"));
    assert_eq!(form.field("email").unwrap().label(), "email", "no title: the property name");
    let size = form.field("size").unwrap();
    assert_eq!(size.choices.iter().map(|c| (c.value.as_str(), c.label())).collect::<Vec<_>>(), [("s", "Small"), ("m", "Medium"), ("l", "Large")]);
    assert_eq!(form.field("color").unwrap().choices[1].label(), "Blue");
    assert_eq!(form.field("subscribe").unwrap().default, Some(json!(true)));
}

#[test]
fn empty_and_odd_schemas() {
    assert!(QuestionForm::from_schema(&json!({"type":"object","properties":{}})).unwrap().fields.is_empty());
    assert!(QuestionForm::from_schema(&Value::Null).unwrap().fields.is_empty());
    assert!(QuestionForm::from_schema(&json!({"type":"object"})).unwrap().fields.is_empty());
    let nested = QuestionForm::from_schema(&json!({"type":"object","properties":{"tags":{"type":"array"}}}));
    assert!(nested.unwrap_err().contains("tags"));
    assert!(QuestionForm::from_schema(&json!({"type":"string"})).is_err());
    // A default that does not fit its field is dropped.
    let form = QuestionForm::from_schema(&json!({"properties":{"n":{"type":"integer","default":"many"}}})).unwrap();
    assert_eq!(form.fields[0].default, None);
}

#[test]
fn validation() {
    let form = sample();
    let ok = form.validate(&content(json!({"name":"Bo","size":"m","age":3.0,"extra":1,"email":""}))).unwrap();
    assert_eq!(Value::Object(ok), json!({"name":"Bo","size":"m","age":3}), "integral numbers, blank text and unknown keys");
    let missing = form.validate(&content(json!({"name":"  "}))).unwrap_err();
    assert!(missing.contains("Name is required") && missing.contains("size is required"), "{missing}");
    let bad = |value: Value| form.validate(&content(value)).unwrap_err();
    assert!(bad(json!({"name":"B","size":"m"})).contains("at least 2 characters"));
    assert!(bad(json!({"name":"Bo","size":"xl"})).contains("choose one of Small, Medium, Large"));
    assert!(bad(json!({"name":"Bo","size":"s","age":200})).contains("at most 150"));
    assert!(bad(json!({"name":"Bo","size":"s","age":2.5})).contains("whole number"));
    assert!(bad(json!({"name":"Bo","size":"s","email":"not-an-email"})).contains("valid email"));
    assert!(bad(json!({"name":"Bo","size":"s","subscribe":"yes"})).contains("yes or no"));
    assert!(form.validate(&content(json!({"name":"Bo","size":"s","email":"bo@example.com","subscribe":false}))).is_ok());
}

#[test]
fn formats() {
    let field = |format: &str| QuestionForm::from_schema(&json!({"properties":{"v":{"type":"string","format":format}}})).unwrap();
    let ok = |format: &str, text: &str| field(format).validate(&content(json!({"v":text}))).is_ok();
    assert!(ok("uri", "https://example.com/x") && !ok("uri", "example dot com"));
    assert!(ok("date", "2026-11-14") && !ok("date", "2026-13-01") && !ok("date", "14/11/2026"));
    assert!(ok("date-time", "2026-11-14T09:30:00Z") && !ok("date-time", "2026-11-14"));
}

#[test]
fn typed_text_parses_per_field() {
    let form = sample();
    let field = |name: &str| form.field(name).unwrap();
    assert_eq!(field("age").parse(" 42 ").unwrap(), Some(json!(42)));
    assert!(field("age").parse("4.5").is_err());
    assert_eq!(field("ratio").parse("0.25").unwrap(), Some(json!(0.25)));
    assert_eq!(field("subscribe").parse("No").unwrap(), Some(json!(false)));
    assert_eq!(field("size").parse("large").unwrap(), Some(json!("l")), "a choice by its label");
    assert_eq!(field("size").parse("m").unwrap(), Some(json!("m")));
    assert_eq!(field("name").parse("   ").unwrap(), None);
}

#[test]
fn text_answers_for_clients_without_forms() {
    let single = QuestionForm::from_schema(&json!({"properties":{"ok":{"type":"boolean"}},"required":["ok"]})).unwrap();
    assert_eq!(single.options(), ["Yes", "No"]);
    assert_eq!(Value::Object(single.text_answer("Yes").unwrap()), json!({"ok":true}));
    assert!(single.text_answer("maybe").is_err());
    let none = QuestionForm::default();
    assert!(none.text_answer("sure").unwrap().is_empty());
    let form = sample();
    assert!(form.options().is_empty());
    assert!(form.text_answer("Bo").unwrap_err().contains("JSON object"));
    assert!(form.text_answer(r#"{"name":"Bo","size":"Small"}"#).is_err(), "JSON values are checked as is");
    assert!(form.text_answer(r#"{"name":"Bo","size":"s"}"#).is_ok());
    assert_eq!(form.summary(&content(json!({"name":"Bo","size":"s","subscribe":true}))), "Name: Bo · Subscribe: yes · size: Small");
}

#[test]
fn older_questions_still_parse() {
    let q: PendingQuestion = serde_json::from_value(json!({"question_id":"q","call_id":"c","question":"Which?","options":["a"]})).unwrap();
    assert!(q.form.is_none() && q.server.is_none());
    let wire = serde_json::to_value(&q).unwrap();
    assert!(wire.get("form").is_none() && wire.get("server").is_none(), "absent fields stay off the wire");
}
