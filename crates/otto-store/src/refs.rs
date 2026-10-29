//! Everything is stored once, in `blobs` keyed by the sha256 of its bytes: base64 media (`{"kind":"base64",
//! "data":…}` sources, tool outputs' `data_base64`) as its bytes, its string becoming `blob:sha256:<hex>`, and any
//! other string of [`TEXT`] bytes or more (a tool's output text, also in its result item; a written file, also in
//! the call) as `{"$otto_text":"<hex>"}`. A journal event repeats nothing its projection holds: the item an event
//! puts (`item.done`, `item.forked`) and a call's input and output (`tool.call`, `tool.result`, `tool.recovered`)
//! become `{"$otto_ref": "item"|"input"|"output"}`, read back from `items`/`item_versions` (by the event's seq) and
//! `tool_calls` (by its `call_id`). Readers of the store's API always get the full JSON.

use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params, types::Type};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const BLOB: &str = "blob:sha256:";
const TEXT_KEY: &str = "$otto_text";
/// Strings this long or longer are stored as blobs.
pub(crate) const TEXT: usize = 2048;
const REF: &str = "$otto_ref";
/// Streamed deltas: redundant once their item is stored (or their call journaled), but for the first of each
/// run, which keeps when the run started (`otto_proto::timing`).
pub(crate) const DELTAS: [&str; 3] = ["text.delta", "reasoning.delta", "tool.input_delta"];

/// Moves the media and long strings in `value` into `blobs`. Returns whether anything moved.
pub(crate) fn externalize(conn: &Connection, value: &mut Value) -> rusqlite::Result<bool> {
    let mut moved = false;
    let mut put = |bytes: &[u8]| -> rusqlite::Result<String> {
        let hash = Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        });
        conn.prepare_cached("INSERT INTO blobs VALUES(?1,?2) ON CONFLICT DO NOTHING")?.execute(params![hash, bytes])?;
        moved = true;
        Ok(hash)
    };
    walk(value, &mut |slot, media| {
        let Value::String(text) = slot else { return Ok(true) };
        if media {
            // The standard engine only takes canonical base64, so the bytes encode back to the same string.
            if let Ok(bytes) = STANDARD.decode(text.as_bytes()) {
                *text = format!("{BLOB}{}", put(&bytes)?);
                return Ok(false);
            }
        }
        if text.len() >= TEXT {
            *slot = serde_json::json!({ TEXT_KEY: put(text.as_bytes())? });
        }
        Ok(false)
    })?;
    Ok(moved)
}

/// Puts the blobs `value` refers to back in place. A missing blob leaves its ref.
pub(crate) fn resolve(conn: &Connection, value: &mut Value) -> rusqlite::Result<()> {
    let get = |hash: &str| -> rusqlite::Result<Option<Vec<u8>>> {
        let bytes = conn.prepare_cached("SELECT data FROM blobs WHERE sha256=?")?.query_row([hash], |r| r.get(0)).optional()?;
        if bytes.is_none() {
            tracing::warn!(hash, "missing blob");
        }
        Ok(bytes)
    };
    walk(value, &mut |slot, media| {
        if let Some(hash) = slot.as_object().filter(|o| o.len() == 1).and_then(|o| o.get(TEXT_KEY)).and_then(Value::as_str) {
            if let Some(text) = get(hash)?.and_then(|b| String::from_utf8(b).ok()) {
                *slot = Value::String(text);
            }
            return Ok(false);
        }
        match slot {
            Value::String(text) if media => {
                if let Some(bytes) = text.strip_prefix(BLOB).map(get).transpose()?.flatten() {
                    *text = STANDARD.encode(bytes);
                }
                Ok(false)
            }
            _ => Ok(true),
        }
    })
}

/// Visits every value with whether it is a media slot; descends where `visit` says so.
fn walk(value: &mut Value, visit: &mut impl FnMut(&mut Value, bool) -> rusqlite::Result<bool>) -> rusqlite::Result<()> {
    match value {
        Value::Object(map) => {
            let source = map.get("kind").and_then(Value::as_str) == Some("base64");
            for (key, v) in map.iter_mut() {
                let media = key == "data_base64" || (source && key == "data");
                if visit(v, media && v.is_string())? {
                    walk(v, visit)?;
                }
            }
        }
        Value::Array(list) => {
            for v in list {
                if visit(v, false)? {
                    walk(v, visit)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Text JSON with its blobs resolved, decoded.
pub(crate) fn decode<T: serde::de::DeserializeOwned>(conn: &Connection, text: &str) -> rusqlite::Result<T> {
    let fail = |e: serde_json::Error| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e));
    if !text.contains(BLOB) && !text.contains(TEXT_KEY) {
        return serde_json::from_str(text).map_err(fail);
    }
    let mut value: Value = serde_json::from_str(text).map_err(fail)?;
    resolve(conn, &mut value)?;
    serde_json::from_value(value).map_err(fail)
}

pub(crate) fn reference(name: &str) -> Value {
    serde_json::json!({ REF: name })
}

fn referred(value: &Value) -> Option<&str> {
    value.as_object().filter(|o| o.len() == 1)?.get(REF)?.as_str()
}

/// A stored event's payload as it was appended: its refs and blobs resolved.
pub(crate) fn resolve_event(conn: &Connection, session: &str, seq: i64, payload: &mut Value) -> rusqlite::Result<()> {
    if referred(payload) == Some("item") {
        *payload = item_at(conn, session, seq)?;
    } else if let Some(map) = payload.as_object_mut() {
        let call = map.get("call_id").and_then(Value::as_str).map(str::to_owned);
        for v in map.values_mut() {
            match (referred(v), call.as_deref()) {
                (Some("item"), _) => *v = item_at(conn, session, seq)?,
                (Some("input"), Some(call)) => *v = call_column(conn, session, call, "input")?,
                (Some("output"), Some(call)) => *v = call_column(conn, session, call, "output")?,
                _ => {}
            }
        }
    }
    resolve(conn, payload)
}

fn item_at(conn: &Connection, session: &str, seq: i64) -> rusqlite::Result<Value> {
    let text: String = conn
        .prepare_cached(
            "SELECT json FROM items WHERE session_id=?1 AND seq=?2 UNION ALL SELECT json FROM item_versions WHERE session_id=?1 AND seq=?2",
        )?
        .query_row(params![session, seq], |r| r.get(0))?;
    serde_json::from_str(&text).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e)))
}

fn call_column(conn: &Connection, session: &str, call: &str, column: &str) -> rusqlite::Result<Value> {
    let sql = format!("SELECT {column} FROM tool_calls WHERE session_id=?1 AND call_id=?2");
    let text: Option<String> = conn.prepare_cached(&sql)?.query_row(params![session, call], |r| r.get(0))?;
    text.map_or(Ok(Value::Null), |t| serde_json::from_str(&t).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e))))
}

/// The seqs among `rows` (one session's events in order: seq, kind, the item or call a delta streams into)
/// that compaction drops: a delta whose item or call is `done`, after another of the same kind into the same.
pub(crate) fn redundant(rows: impl IntoIterator<Item = (i64, String, Option<String>)>, mut done: impl FnMut(&str, &str) -> bool) -> Vec<i64> {
    let mut previous = None;
    rows.into_iter().filter_map(|(seq, kind, key)| run(&mut previous, kind, key, &mut done).then_some(seq)).collect()
}

/// Steps `previous` (the last event's kind and the key it streams into, if a delta) to this event; whether it is a
/// redundant delta.
pub(crate) fn run(
    previous: &mut Option<(String, Option<String>)>,
    kind: String,
    key: Option<String>,
    done: &mut impl FnMut(&str, &str) -> bool,
) -> bool {
    let key = key.filter(|_| DELTAS.contains(&kind.as_str()));
    if let Some(k) = key.as_deref()
        && previous.as_ref().is_some_and(|(pk, pkey)| *pk == kind && pkey.as_deref() == Some(k))
        && done(&kind, k)
    {
        return true;
    }
    *previous = Some((kind, key));
    false
}
