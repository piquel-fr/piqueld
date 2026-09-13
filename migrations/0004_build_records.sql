CREATE TABLE builds (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
 operation_id TEXT NOT NULL,
 service TEXT NOT NULL,
 source_json TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('running','succeeded','failed','interrupted')),
 started_at_ms INTEGER NOT NULL,
 finished_at_ms INTEGER,
 commit_hash TEXT,
 image_id TEXT,
 log_bytes INTEGER NOT NULL DEFAULT 0,
 log_truncated INTEGER NOT NULL DEFAULT 0,
 log_expired INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX builds_application ON builds(application_id,id);
CREATE TABLE build_log_chunks (
 build_id INTEGER NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
 offset INTEGER NOT NULL,
 data BLOB NOT NULL,
 PRIMARY KEY(build_id,offset)
);
