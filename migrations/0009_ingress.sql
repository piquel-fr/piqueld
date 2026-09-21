-- Routing intent is durable before gateway I/O. The accepted projection retains
-- hostname ownership until a removal has actually reached the gateway.
CREATE TABLE application_routes (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    desired_json TEXT NOT NULL CHECK(json_valid(desired_json)),
    applied_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(applied_json))
) STRICT;

CREATE TABLE hostname_reservations (
    hostname TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE
) STRICT;
CREATE INDEX hostname_reservations_application ON hostname_reservations(application_id);
