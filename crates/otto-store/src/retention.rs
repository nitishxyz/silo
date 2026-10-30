//! Bounded growth. Streamed deltas are dropped when their turn finishes (`compact_turn`) and never go to `blobs`;
//! undo history is capped per file and per session and dropped with archived and idle sessions; a blob whose last
//! reference may have gone becomes a candidate (`blob_candidates`) and [`collect`] deletes it once nothing refers to it;
//! the database uses `auto_vacuum=INCREMENTAL` and [`Store::maintain`] gives free pages back in idle time.
//! Migration 5 ([`bounded`]) collects every unreferenced blob once.

use crate::{Migration, Result, Store, now, refs};
use rusqlite::{Connection, ToSql, TransactionBehavior};
use std::collections::HashSet;

pub(crate) const BOUNDED: i64 = 5;
pub(crate) const STEPS: u32 = 1;
/// Undo history kept per file and session: its newest edits.
pub const UNDO_PER_FILE: usize = 50;
/// Undo history kept per session: its newest edits whose content (before and after) adds up to this.
pub const UNDO_SESSION_BYTES: i64 = 16 << 20;
/// Undo history of a session idle this long (ms) is dropped, as is an archived session's.
pub const UNDO_IDLE_MS: i64 = 30 * 24 * 3600 * 1000;
/// Free pages a [`Store::maintain`] gives back at most (64 MiB of 4 KiB pages).
pub const VACUUM_PAGES: i64 = 16_384;

/// What a [`Store::maintain`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Maintained {
    /// Undo edits dropped (archived or idle sessions).
    pub undo_dropped: usize,
    /// Blobs nothing referred to any more, deleted.
    pub blobs_deleted: usize,
    /// Pages given back to the file system.
    pub pages_freed: i64,
}

impl Store {
    /// Idle-time upkeep: drops the undo history of archived and idle sessions ([`UNDO_IDLE_MS`]), deletes
    /// unreferenced blobs and gives up to [`VACUUM_PAGES`] free pages back. Never touches what `session.view` or
    /// event replay reads.
    /// # Errors
    /// Returns database errors.
    pub fn maintain(&mut self) -> Result<Maintained> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let idle = now().saturating_sub(UNDO_IDLE_MS);
        let undo_dropped = drop_undo(&tx, "session_id IN (SELECT id FROM sessions WHERE archived OR updated_at<?1)", &[&idle])?;
        let blobs_deleted = collect(&tx)?;
        tx.commit()?;
        let free = |c: &Connection| c.pragma_query_value(None, "freelist_count", |r| r.get::<_, i64>(0));
        let before = free(&self.connection)?;
        if before > 0 {
            // Each step frees a page.
            let mut stmt = self.connection.prepare(&format!("PRAGMA incremental_vacuum({VACUUM_PAGES})"))?;
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {}
        }
        Ok(Maintained { undo_dropped, blobs_deleted, pages_freed: before - free(&self.connection)? })
    }
}

/// Deletes the undo edits `filter` selects, their blobs becoming candidates. How many went.
pub(crate) fn drop_undo(conn: &Connection, filter: &str, params: &[&dyn ToSql]) -> Result<usize> {
    conn.execute(
        &format!(
            "INSERT OR IGNORE INTO blob_candidates SELECT before FROM undo_edits WHERE before IS NOT NULL AND ({filter}) UNION SELECT after FROM undo_edits WHERE after IS NOT NULL AND ({filter})"
        ),
        params,
    )?;
    Ok(conn.execute(&format!("DELETE FROM undo_edits WHERE {filter}"), params)?)
}

/// Blobs whose last reference may be gone (`hashes`) become candidates.
pub(crate) fn forget<'a>(conn: &Connection, hashes: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut insert = conn.prepare_cached("INSERT OR IGNORE INTO blob_candidates VALUES(?1)")?;
    for hash in hashes {
        insert.execute([hash])?;
    }
    Ok(())
}

/// Deletes the candidates nothing refers to (no stored JSON, no undo edit); how many. Run inside one transaction:
/// a reference added meanwhile would otherwise be missed.
pub(crate) fn collect(conn: &Connection) -> Result<usize> {
    let mut candidates: HashSet<String> =
        conn.prepare("SELECT sha256 FROM blob_candidates")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if candidates.is_empty() {
        return Ok(0);
    }
    for column in ["before", "after"] {
        let mut stmt = conn.prepare(&format!("SELECT {column} FROM undo_edits WHERE {column} IS NOT NULL"))?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            candidates.remove(&r.get::<_, String>(0)?);
        }
    }
    let tables = [
        ("items", "json"),
        ("item_versions", "json"),
        ("tool_calls", "input"),
        ("tool_calls", "output"),
        ("events", "payload"),
        ("sessions", "meta"),
    ];
    for (table, column) in tables {
        if candidates.is_empty() {
            break;
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT {column} FROM {table} WHERE {column} IS NOT NULL AND (instr({column},'{}') OR instr({column},'$otto_text'))",
            refs::BLOB
        ))?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            for hash in refs::hashes(&r.get::<_, String>(0)?) {
                candidates.remove(hash);
            }
        }
    }
    let mut delete = conn.prepare("DELETE FROM blobs WHERE sha256=?1")?;
    let mut deleted = 0;
    for hash in &candidates {
        deleted += delete.execute([hash])?;
    }
    conn.execute("DELETE FROM blob_candidates", [])?;
    Ok(deleted)
}

/// Migration 5: every blob is a candidate once (deltas stored blobs that compaction then orphaned).
pub(crate) fn bounded(conn: &Connection, progress: &dyn Fn(Migration)) -> Result<()> {
    progress(Migration::Step { done: 0, of: STEPS });
    conn.execute("INSERT OR IGNORE INTO blob_candidates SELECT sha256 FROM blobs", [])?;
    let deleted = collect(conn)?;
    tracing::info!(deleted, "store migration: unreferenced blobs");
    progress(Migration::Step { done: 1, of: STEPS });
    Ok(())
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
