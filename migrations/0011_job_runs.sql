-- One-shot job runs share build history, bounded output storage, and retention.
ALTER TABLE builds ADD COLUMN job TEXT;
ALTER TABLE builds ADD COLUMN exit_code INTEGER;

-- Cleanup survives restarts and is independent of the operation's outcome.
-- Retention must keep these operations until their job containers have stopped.
CREATE TABLE job_cleanup (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id) ON DELETE CASCADE
);
