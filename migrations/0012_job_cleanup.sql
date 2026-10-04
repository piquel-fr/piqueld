-- Cleanup survives restarts and is independent of the operation's outcome.
-- Retention must keep these operations until their job containers have stopped.
CREATE TABLE job_cleanup (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id) ON DELETE CASCADE
);
