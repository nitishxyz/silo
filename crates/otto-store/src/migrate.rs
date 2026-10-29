//! Migration 4: existing data stored once (`refs`). One transaction: duplicate item versions go, media and long
//! strings move into `blobs`, and `events` is rebuilt without its projections' copies and redundant deltas. Then `VACUUM`.

use crate::{Result, refs};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use std::{collections::HashSet, time::Instant};

pub(crate) const STORED_ONCE: i64 = 4;

const EVENTS: &str = "CREATE TABLE events_new (
 session_id TEXT NOT NULL REFERENCES sessions(id), seq INTEGER NOT NULL, turn INTEGER NOT NULL, step INTEGER NOT NULL,
 kind TEXT NOT NULL, payload TEXT NOT NULL CHECK(json_valid(payload)), at INTEGER NOT NULL,
 PRIMARY KEY(session_id, seq)
)";
/// Events are append-only, but for compaction dropping redundant deltas.
const TRIGGERS: &str = "CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events WHEN old.kind NOT IN ('text.delta','reasoning.delta','tool.input_delta') BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;";

pub(crate) fn stored_once(tx: &Transaction<'_>) -> Result<()> {
    let start = Instant::now();
    let log = |phase: &str| tracing::info!(phase, elapsed_ms = start.elapsed().as_millis(), "store migration");
    tx.pragma_update(None, "cache_size", -262_144)?;
    log("start");
    tx.execute("DELETE FROM item_versions WHERE (session_id,item_id,seq) IN (SELECT session_id,item_id,seq FROM items)", [])?;
    log("item versions");
    for (table, column) in [("items", "json"), ("item_versions", "json"), ("tool_calls", "input"), ("tool_calls", "output")] {
        let mut changed = Vec::new();
        {
            let mut stmt = tx.prepare(&format!("SELECT rowid,{column} FROM {table} WHERE {column} IS NOT NULL"))?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let text = &r.get::<_, String>(1)?;
                if text.len() < refs::TEXT && !text.contains("base64") {
                    continue;
                }
                let mut value: Value = serde_json::from_str(text)?;
                if refs::externalize(tx, &mut value)? {
                    changed.push((r.get::<_, i64>(0)?, serde_json::to_string(&value)?));
                }
            }
        }
        let mut update = tx.prepare(&format!("UPDATE {table} SET {column}=?2 WHERE rowid=?1"))?;
        for (rowid, text) in changed {
            update.execute(params![rowid, text])?;
        }
        log(&format!("{table}.{column}"));
    }
    events(tx, &log)?;
    tx.pragma_update(None, "cache_size", -2000)?;
    log("done");
    Ok(())
}

fn events(tx: &Transaction<'_>, log: &impl Fn(&str)) -> Result<()> {
    let pairs = |sql: &str| -> Result<HashSet<(String, String)>> {
        let mut stmt = tx.prepare(sql)?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    };
    let (items, calls) = (pairs("SELECT session_id,item_id FROM items")?, pairs("SELECT session_id,call_id FROM tool_calls")?);
    let puts: HashSet<(String, i64)> = {
        let mut stmt = tx.prepare("SELECT session_id,seq FROM items UNION SELECT session_id,seq FROM item_versions")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    tx.execute(EVENTS, [])?;
    let (mut read, mut dropped) = (0_u64, 0_u64);
    {
        let mut insert = tx.prepare("INSERT INTO events_new VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
        let mut item = tx.prepare(
            "SELECT json FROM items WHERE session_id=?1 AND seq=?2 UNION ALL SELECT json FROM item_versions WHERE session_id=?1 AND seq=?2",
        )?;
        let mut call = tx.prepare("SELECT input,output FROM tool_calls WHERE session_id=?1 AND call_id=?2")?;
        // Rowid order is append order: each session's events come in seq order.
        let mut stmt = tx.prepare("SELECT session_id,seq,turn,step,kind,payload,at FROM events ORDER BY rowid")?;
        let mut rows = stmt.query([])?;
        let mut previous = std::collections::HashMap::new();
        while let Some(r) = rows.next()? {
            read += 1;
            if read.is_multiple_of(500_000) {
                log(&format!("events: {read} read, {dropped} dropped"));
            }
            let (session, seq, kind): (String, i64, String) = (r.get(0)?, r.get(1)?, r.get(4)?);
            let text = &r.get::<_, String>(5)?;
            let key = if refs::DELTAS.contains(&kind.as_str()) {
                let payload: Value = serde_json::from_str(text)?;
                payload.get("item_id").or_else(|| payload.get("call_id")).and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            };
            let set = if kind == "tool.input_delta" { &calls } else { &items };
            if refs::run(previous.entry(session.clone()).or_default(), kind.clone(), key, &mut |_, k| set.contains(&(session.clone(), k.to_owned())))
            {
                dropped += 1;
                continue;
            }
            let put = puts.contains(&(session.clone(), seq));
            let tool = matches!(kind.as_str(), "tool.call" | "tool.result" | "tool.recovered");
            let payload = if put || tool || text.len() >= refs::TEXT || text.contains("base64") {
                let mut payload: Value = serde_json::from_str(text)?;
                refs::externalize(tx, &mut payload)?;
                if put && let Some(stored) = item.query_row(params![session, seq], |r| r.get::<_, String>(0)).optional()? {
                    let stored: Value = serde_json::from_str(&stored)?;
                    if payload == stored {
                        payload = refs::reference("item");
                    } else if payload.get("item") == Some(&stored) {
                        payload["item"] = refs::reference("item");
                    }
                }
                if tool && let Some(id) = payload.get("call_id").and_then(Value::as_str).map(str::to_owned) {
                    let found: Option<(String, Option<String>)> = call.query_row(params![session, id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    for (slot, stored) in found.into_iter().flat_map(|(input, output)| [("input", Some(input)), ("output", output)]) {
                        if let Some(stored) = stored
                            && payload.get(slot) == Some(&serde_json::from_str::<Value>(&stored)?)
                        {
                            payload[slot] = refs::reference(slot);
                        }
                    }
                }
                serde_json::to_string(&payload)?
            } else {
                text.to_owned()
            };
            insert.execute(params![session, seq, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, kind, payload, r.get::<_, i64>(6)?])?;
        }
    }
    log(&format!("events: {read} read, {dropped} dropped"));
    tx.execute_batch(&format!("DROP TABLE events; ALTER TABLE events_new RENAME TO events; {TRIGGERS}"))?;
    log("events");
    Ok(())
}

/// Gives the space the migration freed back to the file system.
pub(crate) fn vacuum(connection: &Connection) -> Result<()> {
    let start = Instant::now();
    connection.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
    tracing::info!(elapsed_ms = start.elapsed().as_millis(), "store migration vacuum");
    Ok(())
}
