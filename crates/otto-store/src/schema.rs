pub(crate) const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        r#"
CREATE TABLE projects (id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL);
CREATE TABLE sessions (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), parent_id TEXT REFERENCES sessions(id),
 agent TEXT NOT NULL, model TEXT NOT NULL, provider TEXT NOT NULL, title TEXT NOT NULL, cwd TEXT NOT NULL,
 status TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, archived INTEGER NOT NULL DEFAULT 0,
 last_seq INTEGER NOT NULL DEFAULT 0, meta TEXT NOT NULL CHECK(json_valid(meta))
);
CREATE TABLE events (
 session_id TEXT NOT NULL REFERENCES sessions(id), seq INTEGER NOT NULL, turn INTEGER NOT NULL, step INTEGER NOT NULL,
 kind TEXT NOT NULL, payload TEXT NOT NULL CHECK(json_valid(payload)), at INTEGER NOT NULL,
 PRIMARY KEY(session_id, seq)
);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TABLE items (
 session_id TEXT NOT NULL REFERENCES sessions(id), item_id TEXT NOT NULL, seq INTEGER NOT NULL,
 role TEXT NOT NULL, json TEXT NOT NULL CHECK(json_valid(json)), PRIMARY KEY(session_id,item_id)
);
CREATE TABLE item_versions (
 session_id TEXT NOT NULL REFERENCES sessions(id), item_id TEXT NOT NULL, seq INTEGER NOT NULL,
 role TEXT NOT NULL, json TEXT NOT NULL CHECK(json_valid(json)), PRIMARY KEY(session_id,item_id,seq)
);
CREATE TABLE tool_calls (
 session_id TEXT NOT NULL REFERENCES sessions(id), call_id TEXT NOT NULL, name TEXT NOT NULL,
 input TEXT NOT NULL CHECK(json_valid(input)), status TEXT NOT NULL CHECK(status IN ('intent','running','done','error','unknown')),
 output TEXT CHECK(output IS NULL OR json_valid(output)), started_at INTEGER NOT NULL, finished_at INTEGER,
 PRIMARY KEY(session_id,call_id)
);
CREATE TABLE usage (
 session_id TEXT NOT NULL REFERENCES sessions(id), turn INTEGER NOT NULL, step INTEGER NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
 input INTEGER NOT NULL, output INTEGER NOT NULL, cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL, reasoning INTEGER NOT NULL,
 cost_usd REAL NOT NULL, PRIMARY KEY(session_id,turn,step,provider,model)
);
CREATE TABLE outbox (
 id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), from_session TEXT NOT NULL REFERENCES sessions(id),
 version INTEGER NOT NULL, payload TEXT NOT NULL CHECK(json_valid(payload)), delivered_at INTEGER
);
CREATE TABLE turns (
 session_id TEXT NOT NULL REFERENCES sessions(id), turn INTEGER NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('running','done','cancelled','interrupted','error')),
 started_at INTEGER NOT NULL, finished_at INTEGER, PRIMARY KEY(session_id,turn)
);
CREATE INDEX sessions_project ON sessions(project_id,updated_at);
CREATE INDEX outbox_pending ON outbox(session_id,delivered_at);
"#,
    ),
    (
        2,
        r#"
DELETE FROM outbox WHERE rowid NOT IN (SELECT min(rowid) FROM outbox GROUP BY session_id,from_session,version);
CREATE UNIQUE INDEX outbox_version ON outbox(session_id,from_session,version);
CREATE INDEX sessions_parent ON sessions(parent_id);
"#,
    ),
    (
        3,
        r#"
ALTER TABLE turns ADD COLUMN base_tree TEXT;
ALTER TABLE turns ADD COLUMN end_tree TEXT;
"#,
    ),
];
