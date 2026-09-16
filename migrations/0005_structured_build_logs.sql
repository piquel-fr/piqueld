ALTER TABLE build_log_chunks ADD COLUMN stream TEXT CHECK(stream IN ('stdout','stderr'));
ALTER TABLE build_log_chunks ADD COLUMN timestamp_ms INTEGER NOT NULL DEFAULT 0;
CREATE INDEX build_log_stream ON build_log_chunks(build_id,stream,offset);
