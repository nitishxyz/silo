//! Durable session journal and projections.
//!
//! `Store` is `Send`, not `Sync`. Share it as `Arc<Mutex<Store>>` and perform
//! database work inside `tokio::task::spawn_blocking`, never on an executor thread.
//! Each successful append commits its event and projection with SQLite FULL
//! synchronous durability. Commit a tool intent before execution, then commit its
//! outcome before appending the result item. Recovery must run with actors stopped.

mod coalescer;
mod migrate;
mod refs;
mod retention;
mod schema;
mod undo;

pub use coalescer::Coalescer;
pub use otto_llm::{event::Usage, types::Item};
pub use retention::{Maintained, UNDO_IDLE_MS, UNDO_PER_FILE, UNDO_SESSION_BYTES, VACUUM_PAGES};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub use undo::UndoEdit;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unsupported schema version {0}")]
    SchemaVersion(i64),
    #[error("invalid transition: {0}")]
    InvalidTransition(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    pub root: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewSession {
    pub project_id: String,
    pub parent_id: Option<String>,
    pub agent: String,
    pub model: String,
    pub provider: String,
    pub title: String,
    pub cwd: String,
    #[serde(default)]
    pub meta: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub parent_id: Option<String>,
    pub agent: String,
    pub model: String,
    pub provider: String,
    pub title: String,
    pub cwd: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived: bool,
    pub last_seq: i64,
    pub meta: Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionUpdate {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub status: Option<String>,
    pub archived: Option<bool>,
    pub meta: Option<Value>,
    /// Moves the session to another project (with `cwd`).
    pub project_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub turn: i64,
    pub step: i64,
    pub kind: String,
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub session_id: String,
    pub seq: i64,
    pub turn: i64,
    pub step: i64,
    pub kind: String,
    pub payload: Value,
    pub at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Intent,
    Running,
    Done,
    Error,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Done,
    Cancelled,
    Interrupted,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub session_id: String,
    pub call_id: String,
    pub name: String,
    pub input: Value,
    pub status: ToolStatus,
    pub output: Option<Value>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub usage: Usage,
    pub cost_usd: f64,
}

/// One stored step's usage (a `usage` row): what it ran on, its tokens and billed cost.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRow {
    pub session_id: String,
    pub turn: i64,
    pub step: i64,
    /// When its turn started (ms since the Unix epoch); else its last event in that turn.
    pub at: i64,
    pub record: UsageRecord,
}

#[derive(Clone, Debug)]
pub enum Projection {
    PutItem(Item),
    ToolIntent { call_id: String, name: String, input: Value },
    ToolOutcome { call_id: String, status: ToolStatus, output: Value },
    Usage(UsageRecord),
    TurnStatus(TurnStatus),
    None,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub id: String,
    pub session_id: String,
    pub from_session: String,
    pub version: i64,
    pub payload: Value,
    pub delivered_at: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Recovery {
    pub tool_calls: Vec<ToolCall>,
    pub interrupted_turns: Vec<(String, i64)>,
}

pub struct Store {
    connection: Connection,
}

/// How a schema migration moves on ([`Store::open_with`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Migration {
    /// `done` of `of` steps.
    Step { done: u32, of: u32 },
    /// Giving the space it freed back to the file system (`VACUUM`), last.
    Compacting,
}

impl Store {
    /// # Errors
    /// Returns database, filesystem, or migration errors.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, &|_| {})
    }

    /// [`Store::open`], telling `progress` how a schema migration moves on.
    /// # Errors
    /// Returns database, filesystem, or migration errors.
    pub fn open_with(path: impl AsRef<Path>, progress: &dyn Fn(Migration)) -> Result<Self> {
        Self::initialize(Connection::open(path)?, progress)
    }

    /// # Errors
    /// Returns database or migration errors. In-memory SQLite uses a memory journal.
    pub fn open_in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?, &|_| {})
    }

    fn initialize(mut connection: Connection, progress: &dyn Fn(Migration)) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        // Takes effect in a new database; an existing one gets it from the `VACUUM` below.
        connection.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > schema::MIGRATIONS.last().map_or(0, |m| m.0) {
            return Err(Error::SchemaVersion(version));
        }
        let stored = version > 0 && tx.query_row("SELECT EXISTS(SELECT 1 FROM sessions)", [], |r| r.get::<_, bool>(0))?;
        // The pending migrations' steps are told as one count.
        let steps = |number| match number {
            migrate::STORED_ONCE => migrate::STEPS,
            retention::BOUNDED => retention::STEPS,
            _ => 0,
        };
        let total = schema::MIGRATIONS.iter().filter(|m| m.0 > version).map(|m| steps(m.0)).sum();
        let mut base = 0;
        for &(number, sql) in schema::MIGRATIONS {
            if number > version {
                tx.execute_batch(sql)?;
                let told = |m: Migration| match m {
                    Migration::Step { done, .. } if base == 0 || done > 0 => progress(Migration::Step { done: base + done, of: total }),
                    Migration::Step { .. } => {}
                    Migration::Compacting => progress(m),
                };
                match number {
                    migrate::STORED_ONCE => migrate::stored_once(&tx, &told)?,
                    retention::BOUNDED => retention::bounded(&tx, &told)?,
                    _ => {}
                }
                base += steps(number);
                tx.pragma_update(None, "user_version", number)?;
            }
        }
        tx.commit()?;
        let incremental = connection.pragma_query_value(None, "auto_vacuum", |r| r.get::<_, i64>(0))? == 2;
        if (stored && version < migrate::STORED_ONCE) || !incremental {
            progress(Migration::Compacting);
            migrate::vacuum(&connection)?;
        }
        Ok(Self { connection })
    }

    /// Creates a project, or returns the existing row for this exact root.
    /// # Errors
    /// Returns database errors.
    pub fn create_project(&mut self, root: &str) -> Result<Project> {
        self.connection
            .execute("INSERT INTO projects VALUES(lower(hex(randomblob(16))),?1,?2) ON CONFLICT(root) DO NOTHING", params![root, now()])?;
        self.project_by_root(root)?.ok_or_else(|| Error::NotFound(root.into()))
    }

    /// # Errors
    /// Returns database errors.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.connection.prepare("SELECT id,root,created_at FROM projects ORDER BY root,id")?;
        Ok(stmt.query_map([], |r| Ok(Project { id: r.get(0)?, root: r.get(1)?, created_at: r.get(2)? }))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Projects with their latest session `updated_at` (else `created_at`), most recent first.
    /// # Errors
    /// Returns database errors.
    pub fn recent_projects(&self) -> Result<Vec<(Project, i64)>> {
        let mut stmt = self.connection.prepare(
            "SELECT p.id,p.root,p.created_at,coalesce(max(s.updated_at),p.created_at) AS used FROM projects p LEFT JOIN sessions s ON s.project_id=p.id GROUP BY p.id ORDER BY used DESC,p.root,p.id",
        )?;
        Ok(stmt
            .query_map([], |r| Ok((Project { id: r.get(0)?, root: r.get(1)?, created_at: r.get(2)? }, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// # Errors
    /// Returns database errors.
    pub fn project_by_root(&self, root: &str) -> Result<Option<Project>> {
        Ok(self
            .connection
            .query_row("SELECT id,root,created_at FROM projects WHERE root=?", [root], |r| {
                Ok(Project { id: r.get(0)?, root: r.get(1)?, created_at: r.get(2)? })
            })
            .optional()?)
    }

    /// # Errors
    /// Returns database errors, including missing project or parent references.
    pub fn create_session(&mut self, new: NewSession) -> Result<Session> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = insert_session(&tx, &new)?;
        tx.commit()?;
        self.session(&id)?.ok_or(Error::NotFound(id))
    }

    /// # Errors
    /// Returns database or decoding errors.
    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self.connection.query_row("SELECT * FROM sessions WHERE id=?", [id], session_row).optional()?)
    }

    /// # Errors
    /// Returns database or decoding errors.
    pub fn all_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.connection.prepare("SELECT * FROM sessions ORDER BY id")?;
        Ok(stmt.query_map([], session_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// # Errors
    /// Returns database or decoding errors.
    pub fn project(&self, id: &str) -> Result<Option<Project>> {
        Ok(self
            .connection
            .query_row("SELECT id,root,created_at FROM projects WHERE id=?", [id], |r| {
                Ok(Project { id: r.get(0)?, root: r.get(1)?, created_at: r.get(2)? })
            })
            .optional()?)
    }

    /// # Errors
    /// Returns database or decoding errors.
    pub fn list_sessions(&self, project: &str, include_archived: bool) -> Result<Vec<Session>> {
        let mut stmt = self.connection.prepare("SELECT * FROM sessions WHERE project_id=?1 AND (?2 OR NOT archived) ORDER BY updated_at DESC,id")?;
        Ok(stmt.query_map(params![project, include_archived], session_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// # Errors
    /// Returns database errors or `NotFound`.
    pub fn update_session(&mut self, id: &str, update: SessionUpdate) -> Result<()> {
        let meta = update.meta.map(|v| serde_json::to_string(&v)).transpose()?;
        let count = self.connection.execute("UPDATE sessions SET agent=coalesce(?2,agent),model=coalesce(?3,model),provider=coalesce(?4,provider),title=coalesce(?5,title),cwd=coalesce(?6,cwd),status=coalesce(?7,status),archived=coalesce(?8,archived),meta=coalesce(?9,meta),project_id=coalesce(?11,project_id),updated_at=?10 WHERE id=?1",
            params![id,update.agent,update.model,update.provider,update.title,update.cwd,update.status,update.archived,meta,now(),update.project_id])?;
        require_row(count, id)
    }

    /// Replaces the session's meta, keeping `updated_at` (the list order).
    /// # Errors
    /// Returns database errors or `NotFound`.
    pub fn set_meta(&mut self, id: &str, meta: &Value) -> Result<()> {
        let count = self.connection.execute("UPDATE sessions SET meta=?2 WHERE id=?1", params![id, serde_json::to_string(meta)?])?;
        require_row(count, id)
    }

    /// Commits the event and projection atomically, assigning a gap-free sequence.
    /// Usage is cumulative per turn/step/provider/model: the last record wins.
    /// # Errors
    /// Returns database, encoding, missing-session, or invalid-transition errors.
    pub fn append_with_session(&mut self, session_id: &str, event: EventRecord, projection: Projection, update: SessionUpdate) -> Result<i64> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq = append_tx(&tx, session_id, &event, &projection)?;
        let meta = update.meta.map(|v| serde_json::to_string(&v)).transpose()?;
        tx.execute("UPDATE sessions SET agent=coalesce(?2,agent),model=coalesce(?3,model),provider=coalesce(?4,provider),title=coalesce(?5,title),cwd=coalesce(?6,cwd),status=coalesce(?7,status),archived=coalesce(?8,archived),meta=coalesce(?9,meta),project_id=coalesce(?10,project_id) WHERE id=?1",
            params![session_id,update.agent,update.model,update.provider,update.title,update.cwd,update.status,update.archived,meta,update.project_id])?;
        tx.commit()?;
        Ok(seq)
    }

    /// Commits an event and projection atomically.
    /// # Errors
    /// Returns database or invalid-transition errors.
    pub fn append(&mut self, session_id: &str, event: EventRecord, projection: Projection) -> Result<i64> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq = append_tx(&tx, session_id, &event, &projection)?;
        tx.commit()?;
        Ok(seq)
    }

    /// # Errors
    /// Returns database or decoding errors.
    pub fn events_since(&self, session_id: &str, after_seq: i64, limit: usize) -> Result<Vec<Event>> {
        self.events_where("seq>?2 ORDER BY seq LIMIT ?3", params![session_id, after_seq, i64::try_from(limit).unwrap_or(i64::MAX)])
    }

    /// The session's events of turn `turn` and `kinds`, in order.
    /// # Errors
    /// Returns database or decoding errors.
    pub fn turn_events(&self, session_id: &str, turn: i64, kinds: &[&str]) -> Result<Vec<Event>> {
        self.events_where(
            "turn=?2 AND kind IN (SELECT value FROM json_each(?3)) ORDER BY seq",
            params![session_id, turn, serde_json::to_string(kinds)?],
        )
    }

    fn events_where(&self, filter: &str, params: impl rusqlite::Params) -> Result<Vec<Event>> {
        let conn = &self.connection;
        let mut stmt = conn.prepare(&format!("SELECT session_id,seq,turn,step,kind,payload,at FROM events WHERE session_id=?1 AND {filter}"))?;
        Ok(stmt
            .query_map(params, |r| {
                let (session_id, seq): (String, i64) = (r.get(0)?, r.get(1)?);
                let mut payload = json_column(r, 5)?;
                refs::resolve_event(conn, &session_id, seq, &mut payload)?;
                Ok(Event { session_id, seq, turn: r.get(2)?, step: r.get(3)?, kind: r.get(4)?, payload, at: r.get(6)? })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The session's events after its last one of `kinds` (all of them when there is none).
    /// # Errors
    /// Returns database or decoding errors.
    pub fn events_after_last(&self, session_id: &str, kinds: &[&str]) -> Result<Vec<Event>> {
        let kinds = serde_json::to_string(kinds)?;
        let after: i64 = self.connection.query_row(
            "SELECT coalesce(max(seq),0) FROM events WHERE session_id=? AND kind IN (SELECT value FROM json_each(?))",
            params![session_id, kinds],
            |r| r.get(0),
        )?;
        self.events_since(session_id, after, usize::MAX)
    }

    /// # Errors
    /// Returns database or decoding errors.
    pub fn items(&self, session_id: &str) -> Result<Vec<Item>> {
        let conn = &self.connection;
        let mut stmt = conn.prepare("SELECT json FROM items WHERE session_id=? ORDER BY seq,item_id")?;
        Ok(stmt.query_map([session_id], |r| refs::decode(conn, &r.get::<_, String>(0)?))?.collect::<rusqlite::Result<_>>()?)
    }

    /// The session's items newest first with their item sequence, those before
    /// `before_seq` when given, until `visit` returns `false`.
    /// # Errors
    /// Returns database or decoding errors.
    pub fn items_newest(&self, session_id: &str, before_seq: Option<i64>, mut visit: impl FnMut(i64, Item) -> bool) -> Result<()> {
        let mut stmt = self.connection.prepare("SELECT seq,json FROM items WHERE session_id=?1 AND seq<?2 ORDER BY seq DESC,item_id DESC")?;
        let mut rows = stmt.query(params![session_id, before_seq.unwrap_or(i64::MAX)])?;
        while let Some(row) = rows.next()? {
            if !visit(row.get(0)?, refs::decode(&self.connection, &row.get::<_, String>(1)?)?) {
                break;
            }
        }
        Ok(())
    }

    /// The input of the latest call to tool `name` in an item stored before `before_seq`.
    /// # Errors
    /// Returns database or decoding errors.
    pub fn latest_call_before(&self, session_id: &str, before_seq: i64, name: &str) -> Result<Option<serde_json::Value>> {
        let pattern = format!("%{}%", serde_json::to_string(&serde_json::json!({ "name": name }))?.trim_matches(['{', '}']));
        let mut stmt = self
            .connection
            .prepare("SELECT json FROM items WHERE session_id=?1 AND seq<?2 AND role='assistant' AND json LIKE ?3 ORDER BY seq DESC,item_id DESC")?;
        let mut rows = stmt.query(params![session_id, before_seq, pattern])?;
        while let Some(row) = rows.next()? {
            let item: Item = refs::decode(&self.connection, &row.get::<_, String>(0)?)?;
            if let Some((_, _, input)) = item.tool_calls().filter(|(_, n, _)| *n == name).last() {
                return Ok(Some(input.clone()));
            }
        }
        Ok(None)
    }

    /// # Errors
    /// Returns database or decoding errors.
    pub fn tool_calls(&self, session_id: &str) -> Result<Vec<ToolCall>> {
        tool_rows(&self.connection, Some(session_id), false)
    }

    /// The session's events of `kinds` in order, without payloads: kind, time,
    /// item id (`item_id`, else `item.id`) and call id.
    /// # Errors
    /// Returns database errors.
    #[allow(clippy::type_complexity)]
    pub fn marks(&self, session_id: &str, kinds: &[&str]) -> Result<Vec<(String, i64, Option<String>, Option<String>)>> {
        let mut stmt = self.connection.prepare(
            "SELECT kind,at,coalesce(json_extract(payload,'$.item_id'),json_extract(payload,'$.item.id'),CASE WHEN kind IN ('item.done','item.forked') THEN (SELECT item_id FROM items i WHERE i.session_id=e.session_id AND i.seq=e.seq UNION ALL SELECT item_id FROM item_versions v WHERE v.session_id=e.session_id AND v.seq=e.seq) END),json_extract(payload,'$.call_id') FROM events e WHERE session_id=? AND kind IN (SELECT value FROM json_each(?)) ORDER BY seq",
        )?;
        let kinds = serde_json::to_string(kinds)?;
        Ok(stmt.query_map(params![session_id, kinds], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// When the session's running turn started, if one runs.
    /// # Errors
    /// Returns database errors.
    pub fn turn_started(&self, session_id: &str) -> Result<Option<i64>> {
        Ok(self
            .connection
            .query_row("SELECT started_at FROM turns WHERE session_id=? AND status='running' ORDER BY turn DESC LIMIT 1", [session_id], |r| r.get(0))
            .optional()?)
    }

    /// Records the work tree snapshots of a turn (`git write-tree` ids): `base` at its start, `end` before it
    /// finished. `None` leaves a recorded id as it is.
    /// # Errors
    /// Returns database errors or `NotFound` when the turn has no row.
    pub fn set_turn_trees(&mut self, session_id: &str, turn: i64, base: Option<&str>, end: Option<&str>) -> Result<()> {
        let count = self.connection.execute(
            "UPDATE turns SET base_tree=coalesce(?3,base_tree),end_tree=coalesce(?4,end_tree) WHERE session_id=?1 AND turn=?2",
            params![session_id, turn, base, end],
        )?;
        require_row(count, &format!("{session_id} turn {turn}"))
    }

    /// A turn's snapshots: `(base_tree, end_tree)`, each `None` when not taken.
    /// # Errors
    /// Returns database errors.
    pub fn turn_trees(&self, session_id: &str, turn: i64) -> Result<(Option<String>, Option<String>)> {
        Ok(self
            .connection
            .query_row("SELECT base_tree,end_tree FROM turns WHERE session_id=?1 AND turn=?2", params![session_id, turn], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
            .unwrap_or_default())
    }

    /// The session's first snapshot: the oldest turn's `base_tree` that was taken.
    /// # Errors
    /// Returns database errors.
    pub fn session_base_tree(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT base_tree FROM turns WHERE session_id=?1 AND base_tree IS NOT NULL ORDER BY turn LIMIT 1", params![session_id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// The session's finished turns that changed files, oldest first: the turn, the last assistant item stored
    /// in it (the reply a client shows the changes under) and the `changes` its `turn.finished` event carries.
    /// # Errors
    /// Returns database or decoding errors.
    pub fn turn_changes(&self, session_id: &str) -> Result<Vec<(i64, Option<String>, Value)>> {
        let mut stmt = self.connection.prepare(
            "SELECT f.turn,(SELECT i.item_id FROM items i JOIN events e ON e.session_id=i.session_id AND e.seq=i.seq WHERE i.session_id=f.session_id AND e.turn=f.turn AND i.role='assistant' ORDER BY i.seq DESC LIMIT 1),json_extract(f.payload,'$.changes') FROM events f WHERE f.session_id=?1 AND f.kind='turn.finished' AND json_type(f.payload,'$.changes')='object' ORDER BY f.seq",
        )?;
        Ok(stmt
            .query_map([session_id], |r| {
                let changes = refs::decode(&self.connection, &r.get::<_, String>(2)?)?;
                Ok((r.get(0)?, r.get(1)?, changes))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The usage recorded for the session's last step, if it ran one.
    /// # Errors
    /// Returns database errors, including integer overflow.
    pub fn last_usage(&self, session_id: &str) -> Result<Option<Usage>> {
        Ok(self
            .connection
            .query_row(
                "SELECT input,output,cache_read,cache_write,reasoning FROM usage WHERE session_id=? ORDER BY turn DESC,step DESC,rowid DESC LIMIT 1",
                [session_id],
                |r| Ok(Usage { input: r.get(0)?, output: r.get(1)?, cache_read: r.get(2)?, cache_write: r.get(3)?, reasoning: r.get(4)? }),
            )
            .optional()?)
    }

    /// # Errors
    /// Returns database errors, including integer overflow.
    pub fn usage_totals(&self, session_id: &str) -> Result<UsageTotals> {
        Ok(self.connection.query_row("SELECT coalesce(sum(input),0),coalesce(sum(output),0),coalesce(sum(cache_read),0),coalesce(sum(cache_write),0),coalesce(sum(reasoning),0),coalesce(sum(cost_usd),0) FROM usage WHERE session_id=?",[session_id],|r| Ok(UsageTotals {
            usage:Usage { input:r.get(0)?,output:r.get(1)?,cache_read:r.get(2)?,cache_write:r.get(3)?,reasoning:r.get(4)? },cost_usd:r.get(5)?
        }))?)
    }

    /// The session's stored steps, oldest first: what each ran on, its tokens and billed cost.
    /// # Errors
    /// Returns database errors, including integer overflow.
    pub fn usage_rows(&self, session_id: &str) -> Result<Vec<UsageRow>> {
        self.usage_query("u.session_id=?1", params![session_id])
    }

    /// Every session's steps whose turn started at or after `since_ms` (ms since the Unix epoch), oldest first.
    /// # Errors
    /// Returns database errors, including integer overflow.
    pub fn usage_since(&self, since_ms: i64) -> Result<Vec<UsageRow>> {
        self.usage_query("at>=?1", params![since_ms])
    }

    fn usage_query(&self, filter: &str, params: impl rusqlite::Params) -> Result<Vec<UsageRow>> {
        // A step is dated by its turn's start; a row without a turn (none recorded) by its turn's last event.
        let sql = format!(
            "SELECT * FROM (SELECT u.session_id,u.turn,u.step,u.provider,u.model,u.input,u.output,u.cache_read,u.cache_write,u.reasoning,u.cost_usd,\
             coalesce(t.started_at,(SELECT max(e.at) FROM events e WHERE e.session_id=u.session_id AND e.turn=u.turn),0) AS at \
             FROM usage u LEFT JOIN turns t ON t.session_id=u.session_id AND t.turn=u.turn) u WHERE {filter} ORDER BY at,u.session_id,u.turn,u.step"
        );
        let mut stmt = self.connection.prepare(&sql)?;
        Ok(stmt
            .query_map(params, |r| {
                let usage = Usage { input: r.get(5)?, output: r.get(6)?, cache_read: r.get(7)?, cache_write: r.get(8)?, reasoning: r.get(9)? };
                Ok(UsageRow {
                    session_id: r.get(0)?,
                    turn: r.get(1)?,
                    step: r.get(2)?,
                    at: r.get(11)?,
                    record: UsageRecord { provider: r.get(3)?, model: r.get(4)?, usage, cost_usd: r.get(10)? },
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Each turn's usage summed over its steps, oldest first, with the turn's last assistant item (the reply it
    /// shows under; `None` when it has none yet).
    /// # Errors
    /// Returns database errors, including integer overflow.
    pub fn turn_usage(&self, session_id: &str) -> Result<Vec<(i64, Option<String>, UsageTotals)>> {
        let mut stmt = self.connection.prepare(
            "SELECT u.turn,(SELECT i.item_id FROM items i JOIN events e ON e.session_id=i.session_id AND e.seq=i.seq WHERE i.session_id=u.session_id AND e.turn=u.turn AND i.role='assistant' ORDER BY i.seq DESC LIMIT 1),sum(u.input),sum(u.output),sum(u.cache_read),sum(u.cache_write),sum(u.reasoning),sum(u.cost_usd) FROM usage u WHERE u.session_id=?1 GROUP BY u.turn ORDER BY u.turn",
        )?;
        Ok(stmt
            .query_map([session_id], |r| {
                let usage = Usage { input: r.get(2)?, output: r.get(3)?, cache_read: r.get(4)?, cache_write: r.get(5)?, reasoning: r.get(6)? };
                Ok((r.get(0)?, r.get(1)?, UsageTotals { usage, cost_usd: r.get(7)? }))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Marks unfinished calls unknown and running turns interrupted, journaling both.
    /// The runtime must emit error result items for returned calls, never execute them.
    /// # Errors
    /// Returns database or decoding errors. Run only with session actors stopped.
    pub fn recover(&mut self, session_id: &str) -> Result<Recovery> {
        self.recover_inner(Some(session_id))
    }

    /// # Errors
    /// Returns database or decoding errors. Run only with all session actors stopped.
    pub fn recover_all(&mut self) -> Result<Recovery> {
        self.recover_inner(None)
    }

    fn recover_inner(&mut self, session: Option<&str>) -> Result<Recovery> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut tool_calls = tool_rows(&tx, session, true)?;
        let interrupted_turns = {
            let mut stmt =
                tx.prepare("SELECT session_id,turn FROM turns WHERE status='running' AND (?1 IS NULL OR session_id=?1) ORDER BY session_id,turn")?;
            stmt.query_map([session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for call in &mut tool_calls {
            let output = serde_json::json!({"is_error":true,"text":"outcome unknown"});
            let event = EventRecord {
                turn: 0,
                step: 0,
                kind: "tool.recovered".into(),
                payload: serde_json::json!({"call_id":call.call_id,"status":"unknown","output":output}),
            };
            append_tx(
                &tx,
                &call.session_id,
                &event,
                &Projection::ToolOutcome { call_id: call.call_id.clone(), status: ToolStatus::Unknown, output: output.clone() },
            )?;
            call.status = ToolStatus::Unknown;
            call.output = Some(output);
            call.finished_at =
                tx.query_row("SELECT finished_at FROM tool_calls WHERE session_id=? AND call_id=?", params![call.session_id, call.call_id], |r| {
                    r.get(0)
                })?;
        }
        for (id, turn) in &interrupted_turns {
            append_tx(
                &tx,
                id,
                &EventRecord { turn: *turn, step: 0, kind: "turn.interrupted".into(), payload: serde_json::json!({"status":"interrupted"}) },
                &Projection::TurnStatus(TurnStatus::Interrupted),
            )?;
        }
        tx.commit()?;
        Ok(Recovery { tool_calls, interrupted_turns })
    }

    /// Records a versioned outbox entry once: pushing the same
    /// `(session, from_session, version)` again returns the existing id.
    /// # Errors
    /// Returns database errors, including missing session references.
    pub fn outbox_push(&mut self, session_id: &str, from_session: &str, version: i64, payload: Value) -> Result<String> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO outbox VALUES(lower(hex(randomblob(16))),?1,?2,?3,?4,NULL) ON CONFLICT(session_id,from_session,version) DO NOTHING",
            params![session_id, from_session, version, serde_json::to_string(&payload)?],
        )?;
        let id = tx.query_row(
            "SELECT id FROM outbox WHERE session_id=?1 AND from_session=?2 AND version=?3",
            params![session_id, from_session, version],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Unarchived sessions with undelivered outbox entries, oldest entry first.
    /// # Errors
    /// Returns database errors.
    pub fn outbox_pending_sessions(&self) -> Result<Vec<String>> {
        let mut stmt = self.connection.prepare(
            "SELECT o.session_id FROM outbox o JOIN sessions s ON s.id=o.session_id WHERE o.delivered_at IS NULL AND NOT s.archived GROUP BY o.session_id ORDER BY min(o.rowid)",
        )?;
        Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Appends the event that delivers outbox entry `outbox_id` and acknowledges
    /// it in the same transaction. An entry delivers exactly once: a second
    /// attempt fails with `InvalidTransition` and appends nothing.
    /// # Errors
    /// Returns database, encoding, missing-row, or invalid-transition errors.
    pub fn append_delivering(
        &mut self,
        session_id: &str,
        event: EventRecord,
        projection: Projection,
        update: SessionUpdate,
        outbox_id: &str,
    ) -> Result<i64> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let delivered: Option<i64> = tx
            .query_row("SELECT delivered_at FROM outbox WHERE id=?1 AND session_id=?2", params![outbox_id, session_id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound(outbox_id.into()))?;
        if delivered.is_some() {
            return Err(Error::InvalidTransition(format!("outbox {outbox_id} already delivered")));
        }
        let seq = append_tx(&tx, session_id, &event, &projection)?;
        let meta = update.meta.map(|v| serde_json::to_string(&v)).transpose()?;
        tx.execute("UPDATE sessions SET agent=coalesce(?2,agent),model=coalesce(?3,model),provider=coalesce(?4,provider),title=coalesce(?5,title),cwd=coalesce(?6,cwd),status=coalesce(?7,status),archived=coalesce(?8,archived),meta=coalesce(?9,meta),project_id=coalesce(?10,project_id) WHERE id=?1",
            params![session_id,update.agent,update.model,update.provider,update.title,update.cwd,update.status,update.archived,meta,update.project_id])?;
        tx.execute("UPDATE outbox SET delivered_at=?2 WHERE id=?1", params![outbox_id, now()])?;
        tx.commit()?;
        Ok(seq)
    }

    /// Direct children of `parent`, oldest first.
    /// # Errors
    /// Returns database or decoding errors.
    pub fn children(&self, parent: &str) -> Result<Vec<Session>> {
        let mut stmt = self.connection.prepare("SELECT * FROM sessions WHERE parent_id=? ORDER BY created_at,id")?;
        Ok(stmt.query_map([parent], session_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// # Errors
    /// Returns database or decoding errors.
    pub fn outbox_pending(&self, session_id: &str) -> Result<Vec<OutboxEntry>> {
        let mut stmt = self.connection.prepare(
            "SELECT id,session_id,from_session,version,payload,delivered_at FROM outbox WHERE session_id=? AND delivered_at IS NULL ORDER BY rowid",
        )?;
        Ok(stmt
            .query_map([session_id], |r| {
                Ok(OutboxEntry {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    from_session: r.get(2)?,
                    version: r.get(3)?,
                    payload: json_column(r, 4)?,
                    delivered_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Idempotently acknowledges delivery; callers own injection/ack coordination.
    /// # Errors
    /// Returns database errors or `NotFound`.
    pub fn outbox_mark_delivered(&mut self, id: &str) -> Result<()> {
        require_row(self.connection.execute("UPDATE outbox SET delivered_at=coalesce(delivered_at,?2) WHERE id=?1", params![id, now()])?, id)
    }

    /// Forks the historical transcript into a fresh event sequence and provider lane.
    /// Parent metadata is not copied because it may contain provider response caches.
    /// # Errors
    /// Returns database/decoding errors or an invalid sequence boundary.
    pub fn fork_session(&mut self, id: &str, at_seq: i64) -> Result<String> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let parent = tx.query_row("SELECT * FROM sessions WHERE id=?", [id], session_row).optional()?.ok_or_else(|| Error::NotFound(id.into()))?;
        if at_seq < 0 || at_seq > parent.last_seq {
            return Err(Error::InvalidTransition("fork sequence out of range".into()));
        }
        let new_id = insert_session(
            &tx,
            &NewSession {
                project_id: parent.project_id,
                parent_id: Some(id.into()),
                agent: parent.agent,
                model: parent.model,
                provider: parent.provider,
                title: parent.title,
                cwd: parent.cwd,
                meta: serde_json::json!({"forked_at_seq":at_seq}),
            },
        )?;
        let items: Vec<Item> = {
            let mut stmt = tx.prepare("SELECT json FROM (SELECT seq,item_id,json,row_number() OVER (PARTITION BY item_id ORDER BY seq DESC) AS n FROM (SELECT seq,item_id,json FROM items WHERE session_id=?1 AND seq<=?2 UNION ALL SELECT seq,item_id,json FROM item_versions WHERE session_id=?1 AND seq<=?2)) WHERE n=1 ORDER BY seq,item_id")?;
            stmt.query_map(params![id, at_seq], |r| refs::decode(&tx, &r.get::<_, String>(0)?))?.collect::<rusqlite::Result<_>>()?
        };
        for item in items {
            append_tx(
                &tx,
                &new_id,
                &EventRecord { turn: 0, step: 0, kind: "item.forked".into(), payload: serde_json::to_value(&item)? },
                &Projection::PutItem(item),
            )?;
        }
        tx.commit()?;
        Ok(new_id)
    }
}

fn insert_session(tx: &Transaction<'_>, new: &NewSession) -> Result<String> {
    Ok(tx.query_row(
        "INSERT INTO sessions VALUES(lower(hex(randomblob(16))),?1,?2,?3,?4,?5,?6,?7,'idle',?8,?8,0,0,?9) RETURNING id",
        params![new.project_id, new.parent_id, new.agent, new.model, new.provider, new.title, new.cwd, now(), serde_json::to_string(&new.meta)?],
        |r| r.get(0),
    )?)
}

fn append_tx(tx: &Transaction<'_>, id: &str, event: &EventRecord, projection: &Projection) -> Result<i64> {
    let at = now();
    let seq: i64 = tx
        .query_row("UPDATE sessions SET last_seq=last_seq+1,updated_at=?2 WHERE id=?1 RETURNING last_seq", params![id, at], |r| r.get(0))
        .optional()?
        .ok_or_else(|| Error::NotFound(id.into()))?;
    // What the projection stores is not repeated in the event (`refs`).
    let mut payload = event.payload.clone();
    let (slot, value) = match projection {
        Projection::PutItem(item) => ("item", Some(serde_json::to_value(item)?)),
        Projection::ToolIntent { input, .. } => ("input", Some(input.clone())),
        Projection::ToolOutcome { output, .. } => ("output", Some(output.clone())),
        _ => ("", None),
    };
    if let Some(value) = &value {
        if slot == "item" && payload == *value {
            payload = refs::reference(slot);
        } else if payload.get(slot) == Some(value) {
            payload[slot] = refs::reference(slot);
        }
    }
    // Deltas stay inline: compaction drops them, which would orphan their blobs.
    if !refs::DELTAS.contains(&event.kind.as_str()) {
        refs::externalize(tx, &mut payload)?;
    }
    tx.execute(
        "INSERT INTO events VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![id, seq, event.turn, event.step, event.kind, serde_json::to_string(&payload)?, at],
    )?;
    let stored = |mut value: Value| -> Result<String> {
        refs::externalize(tx, &mut value)?;
        Ok(serde_json::to_string(&value)?)
    };
    match projection {
        Projection::None => {}
        Projection::PutItem(item) => {
            let role = enum_name(&item.role)?;
            let json = stored(value.unwrap_or_default())?;
            // The version it replaces stays readable by its seq (forks, its event).
            tx.execute(
                "INSERT INTO item_versions SELECT session_id,item_id,seq,role,json FROM items WHERE session_id=?1 AND item_id=?2",
                params![id, item.id],
            )?;
            tx.execute("INSERT INTO items VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session_id,item_id) DO UPDATE SET seq=excluded.seq,role=excluded.role,json=excluded.json",params![id,item.id,seq,role,json])?;
        }
        Projection::ToolIntent { call_id, name, input } => {
            tx.execute("INSERT INTO tool_calls VALUES(?1,?2,?3,?4,'intent',NULL,?5,NULL)", params![id, call_id, name, stored(input.clone())?, at])?;
        }
        Projection::ToolOutcome { call_id, status, output } => {
            if !matches!(status, ToolStatus::Done | ToolStatus::Error | ToolStatus::Unknown) {
                return Err(Error::InvalidTransition("outcome must be terminal".into()));
            }
            let count = tx.execute(
                "UPDATE tool_calls SET status=?3,output=?4,finished_at=?5 WHERE session_id=?1 AND call_id=?2 AND status IN ('intent','running')",
                params![id, call_id, enum_name(status)?, stored(output.clone())?, at],
            )?;
            if count != 1 {
                return Err(Error::InvalidTransition(format!("call {call_id} is missing or already terminal")));
            }
        }
        Projection::Usage(record) => {
            if !record.cost_usd.is_finite() || record.cost_usd < 0.0 {
                return Err(Error::InvalidTransition("cost must be finite and nonnegative".into()));
            }
            let u = &record.usage;
            tx.execute("INSERT INTO usage VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(session_id,turn,step,provider,model) DO UPDATE SET input=excluded.input,output=excluded.output,cache_read=excluded.cache_read,cache_write=excluded.cache_write,reasoning=excluded.reasoning,cost_usd=excluded.cost_usd",params![id,event.turn,event.step,record.provider,record.model,u.input,u.output,u.cache_read,u.cache_write,u.reasoning,record.cost_usd])?;
        }
        Projection::TurnStatus(status) => {
            let finished = if *status == TurnStatus::Running { None } else { Some(at) };
            tx.execute("INSERT INTO turns(session_id,turn,status,started_at,finished_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session_id,turn) DO UPDATE SET status=excluded.status,finished_at=excluded.finished_at",params![id,event.turn,enum_name(status)?,at,finished])?;
            if finished.is_some() {
                compact_turn(tx, id, event.turn)?;
            }
        }
    }
    Ok(seq)
}

/// Drops the deltas a finished turn streamed that its stored items and calls make redundant (`refs::redundant`):
/// replay and late joiners read the items; a client resuming inside the turn sees a gap and reloads its view.
fn compact_turn(tx: &Transaction<'_>, id: &str, turn: i64) -> Result<usize> {
    let mut rows = Vec::new();
    {
        let mut stmt = tx.prepare_cached(&format!(
            "SELECT seq,kind,turn,CASE WHEN kind IN ({}) THEN coalesce(json_extract(payload,'$.item_id'),json_extract(payload,'$.call_id')) END FROM events WHERE session_id=? ORDER BY seq DESC",
            refs::DELTAS.map(|k| format!("'{k}'")).join(",")
        ))?;
        let mut cursor = stmt.query([id])?;
        while let Some(r) = cursor.next()? {
            let (seq, kind, at_turn, key): (i64, String, i64, Option<String>) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            if kind == "turn.started" && at_turn == turn {
                break;
            }
            rows.push((seq, kind, key));
        }
    }
    rows.reverse();
    let mut item = tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM items WHERE session_id=?1 AND item_id=?2)")?;
    let mut call = tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM tool_calls WHERE session_id=?1 AND call_id=?2)")?;
    let mut known = std::collections::HashMap::new();
    let drop = refs::redundant(rows, |kind, key| {
        *known.entry((kind == "tool.input_delta", key.to_owned())).or_insert_with(|| {
            let stmt = if kind == "tool.input_delta" { &mut call } else { &mut item };
            stmt.query_row(params![id, key], |r| r.get(0)).unwrap_or(false)
        })
    });
    // Deltas stored before they stayed inline may hold blobs.
    let mut delete = tx.prepare_cached("DELETE FROM events WHERE session_id=?1 AND seq=?2 RETURNING payload")?;
    for seq in &drop {
        let payload: String = delete.query_row(params![id, seq], |r| r.get(0))?;
        retention::forget(tx, refs::hashes(&payload))?;
    }
    Ok(drop.len())
}

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        project_id: r.get(1)?,
        parent_id: r.get(2)?,
        agent: r.get(3)?,
        model: r.get(4)?,
        provider: r.get(5)?,
        title: r.get(6)?,
        cwd: r.get(7)?,
        status: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        archived: r.get(11)?,
        last_seq: r.get(12)?,
        meta: json_column(r, 13)?,
    })
}
fn tool_rows(conn: &Connection, session: Option<&str>, unfinished: bool) -> Result<Vec<ToolCall>> {
    let mut stmt = conn.prepare("SELECT session_id,call_id,name,input,status,output,started_at,finished_at FROM tool_calls WHERE (?1 IS NULL OR session_id=?1) AND (NOT ?2 OR status IN ('intent','running')) ORDER BY session_id,started_at,call_id")?;
    Ok(stmt
        .query_map(params![session, unfinished], |r| {
            let status: String = r.get(4)?;
            let status = serde_json::from_value(Value::String(status))
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
            let output: Option<String> = r.get(5)?;
            let output = output.map(|s| refs::decode(conn, &s)).transpose()?;
            Ok(ToolCall {
                session_id: r.get(0)?,
                call_id: r.get(1)?,
                name: r.get(2)?,
                input: refs::decode(conn, &r.get::<_, String>(3)?)?,
                status,
                output,
                started_at: r.get(6)?,
                finished_at: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}
fn json_column<T: serde::de::DeserializeOwned>(r: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = r.get(index)?;
    serde_json::from_str(&text).map_err(|e| rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e)))
}
fn enum_name<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_value(value)?.as_str().expect("string enum").to_owned())
}
fn require_row(count: usize, id: &str) -> Result<()> {
    if count == 0 { Err(Error::NotFound(id.into())) } else { Ok(()) }
}
fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().try_into().unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
