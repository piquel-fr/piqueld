-- One-shot job runs share build history, bounded output storage, and retention.
ALTER TABLE builds ADD COLUMN job TEXT;
ALTER TABLE builds ADD COLUMN exit_code INTEGER;
