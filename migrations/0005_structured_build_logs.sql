-- Old output has no stream or capture time and cannot satisfy the new contract.
-- Expire those logs while retaining build metadata.
UPDATE builds SET log_expired=1,log_bytes=0
WHERE id IN (SELECT build_id FROM build_log_chunks);
DROP TABLE build_log_chunks;
CREATE TABLE build_log_chunks (
 build_id INTEGER NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
 offset INTEGER NOT NULL,
 data BLOB NOT NULL,
 stream TEXT NOT NULL CHECK(stream IN ('stdout','stderr')),
 timestamp_ms INTEGER NOT NULL,
 PRIMARY KEY(build_id,offset)
);
CREATE INDEX build_log_stream ON build_log_chunks(build_id,stream,offset);
