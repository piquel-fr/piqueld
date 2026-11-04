-- Where container log export stopped reading, so it resumes after a restart:
-- the JSON form of `ContainerCursors` (apps/piqueld/src/telemetry/containers.rs).
CREATE TABLE container_log_cursors (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    state_json TEXT NOT NULL CHECK (json_valid(state_json))
) STRICT;
