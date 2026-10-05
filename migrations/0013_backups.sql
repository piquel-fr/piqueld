-- Time of the last successful `piqueld backup`, reported by daemon status.
ALTER TABLE instance_metadata ADD COLUMN last_backup_at_ms INTEGER CHECK (last_backup_at_ms IS NULL OR last_backup_at_ms > 0);
