//! Undo history (`undo_edit`): each session's file edits, content in `blobs`, capped by
//! [`UNDO_PER_FILE`](crate::retention::UNDO_PER_FILE) and [`UNDO_SESSION_BYTES`](crate::retention::UNDO_SESSION_BYTES).

use crate::{Error, Result, Store, now, refs, retention};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

/// One edit kept for undo: a file's content before and after it (`None`: absent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoEdit {
    pub id: i64,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl Store {
    /// Keeps `path` going from `before` to `after` by an edit of the session, dropping its oldest edits beyond the
    /// caps. Its id.
    /// # Errors
    /// Returns database errors, including a missing session.
    pub fn undo_push(&mut self, session_id: &str, path: &str, before: Option<&str>, after: Option<&str>) -> Result<i64> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (b, a) = (before.map(|t| refs::put(&tx, t.as_bytes())).transpose()?, after.map(|t| refs::put(&tx, t.as_bytes())).transpose()?);
        let bytes = i64::try_from(before.map_or(0, str::len) + after.map_or(0, str::len)).unwrap_or(i64::MAX);
        let id: i64 = tx.query_row(
            "INSERT INTO undo_edits(session_id,path,before,after,bytes,at) VALUES(?1,?2,?3,?4,?5,?6) RETURNING id",
            params![session_id, path, b, a, bytes, now()],
            |r| r.get(0),
        )?;
        let per_file = i64::try_from(retention::UNDO_PER_FILE).unwrap_or(i64::MAX);
        retention::drop_undo(
            &tx,
            "session_id=?1 AND path=?2 AND id NOT IN (SELECT id FROM undo_edits WHERE session_id=?1 AND path=?2 ORDER BY id DESC LIMIT ?3)",
            &[&session_id, &path, &per_file],
        )?;
        retention::drop_undo(
            &tx,
            "id IN (SELECT id FROM (SELECT id,sum(bytes) OVER (ORDER BY id DESC) AS total FROM undo_edits WHERE session_id=?1) WHERE total>?2)",
            &[&session_id, &retention::UNDO_SESSION_BYTES],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// The session's last edit to `path` still kept.
    /// # Errors
    /// Returns database errors, or `NotFound` when its content is missing.
    pub fn undo_last(&self, session_id: &str, path: &str) -> Result<Option<UndoEdit>> {
        let row: Option<(i64, Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT id,before,after FROM undo_edits WHERE session_id=?1 AND path=?2 ORDER BY id DESC LIMIT 1",
                params![session_id, path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((id, before, after)) = row else { return Ok(None) };
        let text = |hash: Option<String>| -> Result<Option<String>> {
            let Some(hash) = hash else { return Ok(None) };
            let bytes = refs::get(&self.connection, &hash)?.ok_or_else(|| Error::NotFound(format!("blob {hash}")))?;
            Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
        };
        Ok(Some(UndoEdit { id, before: text(before)?, after: text(after)? }))
    }

    /// Forgets the session's edit `id` (taken back).
    /// # Errors
    /// Returns database errors.
    pub fn undo_remove(&mut self, session_id: &str, id: i64) -> Result<()> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        retention::drop_undo(&tx, "session_id=?1 AND id=?2", &[&session_id, &id])?;
        tx.commit()?;
        Ok(())
    }
}
