use super::*;
use serde_json::json;

/// `a` (read from a prefix) is contained in `b` (read from a longer one): strings only grow, arrays only gain or
/// grow items, objects only gain or grow keys, anything else is final once read.
fn extends(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => b.starts_with(a.as_str()),
        (Value::Array(a), Value::Array(b)) => a.len() <= b.len() && a.iter().zip(b).all(|(x, y)| extends(x, y)),
        (Value::Object(a), Value::Object(b)) => a.iter().all(|(k, v)| b.get(k).is_some_and(|w| extends(v, w))),
        _ => a == b,
    }
}

/// Every prefix of `text` (at char boundaries, as deltas arrive) reads without panicking, grows monotonically
/// towards the full read, and the full text reads exactly as serde does.
fn every_prefix(text: &str) {
    let full: Value = serde_json::from_str(text).unwrap();
    let mut last = json!({});
    let mut ends: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    ends.push(text.len());
    for end in ends {
        let Some(read) = parse(&text[..end]) else {
            assert!(text[..end].trim().is_empty(), "only an empty prefix reads nothing: {:?}", &text[..end]);
            continue;
        };
        assert!(extends(&last, &read.value), "shrank at {end}: {last} then {}", read.value);
        assert!(extends(&read.value, &full), "not a prefix of the full read at {end}: {}", read.value);
        assert_eq!(read.complete, end == text.len(), "complete only at the end");
        last = read.value;
    }
    assert_eq!(last, full);
}

#[test]
fn every_prefix_of_edit_inputs_reads_monotonically() {
    every_prefix(r#"{"path":"src/a.rs","old_string":"fn a() {\n    \"x\"\n}","new_string":"fn a() {\n\t'\\y'\n}"}"#);
    every_prefix(r#"{ "content" : "line 1\nline 2\n\u00e9t\u00e9 \ud83d\ude00 /\/ \r\b\f" , "path" : "b.txt" }"#);
    every_prefix(r#"{"path":"m.rs","edits":[{"old_string":"a","new_string":"b\nc"},{"new_string":"2","old_string":"1","replace_all":true}]}"#);
    every_prefix(r#"{"patch":"*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch"}"#);
    every_prefix("{\n  \"path\": \"日本/ファイル.rs\",\n  \"content\": \"héllo — wörld 😀\\n\",\n  \"n\": -12.5e3,\n  \"b\": [true, false, null]\n}");
    every_prefix(r#"{"a":{"b":{"c":[1,[2,"x"],{}]}},"e":[],"o":{}}"#);
}

#[test]
fn fields_read_as_they_arrive_and_say_whether_they_are_still_open() {
    let p = parse(r#"{"path":"src/a.rs","content":"one\ntw"#).unwrap();
    assert_eq!(p.field("path"), Some(("src/a.rs", false)));
    assert_eq!(p.field("content"), Some(("one\ntw", true)));
    assert_eq!(p.open.as_deref(), Some("content"));
    assert!(!p.complete);
    let p = parse(r#"{"content":"x","path":"a"#).unwrap();
    assert_eq!((p.field("content"), p.field("path")), (Some(("x", false)), Some(("a", true))), "keys in any order");
    let p = parse(r#"{"path":"a","#).unwrap();
    assert_eq!((p.field("path"), p.open.as_deref()), (Some(("a", false)), None), "a finished field is not open");
    let p = parse(r#"{"path":"a","conte"#).unwrap();
    assert_eq!(p.value, json!({"path": "a"}), "a key still arriving is left out");
    let p = parse(r#"{"path":"a","content":"#).unwrap();
    assert_eq!(p.value, json!({"path": "a"}), "a key without its value yet is left out");
    let p = parse(r#"{"path":"a"}"#).unwrap();
    assert!(p.complete && p.open.is_none());
}

#[test]
fn not_an_object_reads_nothing() {
    assert_eq!(parse(""), None);
    assert_eq!(parse("  \n"), None);
    assert_eq!(parse("*** Begin Patch"), None);
    assert_eq!(parse("[1]"), None);
    assert_eq!(parse(" \t{").map(|p| p.value), Some(json!({})), "whitespace before the object");
}

#[test]
fn escapes_cut_off_mid_way_wait_for_the_rest() {
    let read = |t: &str| parse(t).unwrap().field("s").map(|(s, _)| s.to_owned());
    assert_eq!(read(r#"{"s":"a\"#).as_deref(), Some("a"));
    assert_eq!(read(r#"{"s":"a\n"#).as_deref(), Some("a\n"));
    assert_eq!(read(r#"{"s":"a\\"#).as_deref(), Some("a\\"));
    assert_eq!(read(r#"{"s":"a\""#).as_deref(), Some("a\""));
    for cut in [r#"{"s":"a\u"#, r#"{"s":"a\u0"#, r#"{"s":"a\u00"#, r#"{"s":"a\u00e"#] {
        assert_eq!(read(cut).as_deref(), Some("a"), "{cut}");
    }
    assert_eq!(read(r#"{"s":"a\u00e9"#).as_deref(), Some("aé"));
}

#[test]
fn surrogate_pairs_split_across_deltas_wait_for_their_low_half() {
    let full = r#"{"s":"x\ud83d\ude00y"}"#;
    let start = full.find('\\').unwrap();
    for end in start..start + 12 {
        let s = parse(&full[..end]).unwrap().field("s").map(|(s, _)| s.to_owned());
        assert_eq!(s.as_deref(), Some("x"), "no half pair at {end}: {:?}", &full[..end]);
    }
    assert_eq!(parse(&full[..start + 12]).unwrap().field("s"), Some(("x😀", true)));
    // Lone halves read as the replacement character, as serde reads them lossily.
    assert_eq!(parse(r#"{"s":"\ud83dz"}"#).unwrap().field("s"), Some(("\u{fffd}z", false)));
    assert_eq!(parse(r#"{"s":"\ud83d\u0041"}"#).unwrap().field("s"), Some(("\u{fffd}A", false)));
    assert_eq!(parse(r#"{"s":"\ude00"}"#).unwrap().field("s"), Some(("\u{fffd}", false)));
}

#[test]
fn numbers_and_literals_count_once_complete() {
    assert_eq!(parse(r#"{"n":12"#).unwrap().value, json!({}));
    assert_eq!(parse(r#"{"n":12,"#).unwrap().value, json!({"n": 12}));
    assert_eq!(parse(r#"{"b":tru"#).unwrap().value, json!({}));
    assert_eq!(parse(r#"{"b":true}"#).unwrap().value, json!({"b": true}));
}

#[test]
fn malformed_input_stops_the_read_without_panicking() {
    assert_eq!(parse(r#"{"s":"a\x","t":"b"}"#).unwrap().value, json!({}));
    assert_eq!(parse(r#"{"s":"\uZZZZ"}"#).unwrap().value, json!({}));
    assert_eq!(parse(r#"{"a":"1" "b":"2"}"#).unwrap().value, json!({"a": "1"}));
    assert_eq!(parse(r#"{"a" "b"}"#).unwrap().value, json!({}));
    assert_eq!(parse(r#"{"a":nope}"#).unwrap().value, json!({}));
    for garbage in ["{\"\\", "{\"a\":\"\\u\u{e9}", "{[", "{\"a\":[}", "{\"a\":\"\\ud83d\\u\u{e9}\"}"] {
        let _ = parse(garbage);
    }
}

#[test]
fn edits_arrive_pair_by_pair() {
    let p = parse(r#"{"path":"a","edits":[{"old_string":"x","new_string":"y"},{"old_string":"z"#).unwrap();
    assert_eq!(p.edits(), [(Some("x"), Some("y")), (Some("z"), None)]);
    assert_eq!(p.open.as_deref(), Some("edits"));
    assert_eq!(parse(r#"{"edits":[{"#).unwrap().edits(), [(None, None)]);
    assert!(parse(r#"{"path":"a"}"#).unwrap().edits().is_empty());
}

#[test]
fn every_prefix_of_serialized_inputs_reads_monotonically() {
    let tricky = "a\u{1}b\"c\\d\n\r\t\u{8}\u{c}/é日😀\u{7f}\u{2028}";
    every_prefix(&json!({"path": tricky, "content": tricky.repeat(3), "edits": [{"old_string": tricky, "new_string": ""}]}).to_string());
    every_prefix(&serde_json::to_string_pretty(&json!({"command": "ls -la", "timeout_ms": 1000, "background": false})).unwrap());
    every_prefix(r#"{"s":"\uD83D\uDE00\u00E9\u0000"}"#);
}
