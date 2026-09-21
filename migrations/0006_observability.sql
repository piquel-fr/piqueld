-- Events carry their own context; application deletion only removes application scope.
ALTER TABLE events ADD COLUMN scope TEXT NOT NULL DEFAULT 'application' CHECK(scope IN ('application','daemon'));
ALTER TABLE events ADD COLUMN action_id TEXT;
ALTER TABLE events ADD COLUMN retry INTEGER;
ALTER TABLE events ADD COLUMN retry_delay_ms INTEGER;
ALTER TABLE events ADD COLUMN duration_ms INTEGER;
ALTER TABLE events ADD COLUMN diagnostic_id TEXT;
ALTER TABLE events ADD COLUMN request_id TEXT;
ALTER TABLE events ADD COLUMN diagnostic_json TEXT CHECK(diagnostic_json IS NULL OR json_valid(diagnostic_json));
CREATE INDEX event_diagnostic ON events(diagnostic_id) WHERE diagnostic_id IS NOT NULL;
CREATE INDEX event_operation ON events(operation_id,id);
CREATE INDEX event_action ON events(action_id,id);
CREATE INDEX event_scope ON events(scope,id);
CREATE INDEX event_error ON events(error_code,id);
-- Only persist executing actions here. History remains in immutable events.
CREATE TABLE active_actions (
    id TEXT PRIMARY KEY,
    operation_id TEXT,
    application_id TEXT,
    generation INTEGER,
    phase TEXT NOT NULL,
    resource TEXT,
    attempt INTEGER,
    retry INTEGER,
    started_at_ms INTEGER NOT NULL
);
CREATE TABLE history_coverage (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    started_at_ms INTEGER NOT NULL,
    pruned_through_ms INTEGER,
    pruned_through_id INTEGER NOT NULL DEFAULT 0
);
INSERT INTO history_coverage(singleton,started_at_ms) VALUES(1,CAST(strftime('%s','now') AS INTEGER)*1000);
-- A durable cursor makes event processing and outbox creation atomic and replayable.
CREATE TABLE notification_cursor (singleton INTEGER PRIMARY KEY CHECK(singleton=1), event_id INTEGER NOT NULL);
INSERT INTO notification_cursor VALUES(1,COALESCE((SELECT MAX(id) FROM events),0));
CREATE TABLE notification_conditions (
    key TEXT PRIMARY KEY,
    category TEXT NOT NULL,
    application_id TEXT,
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    first_seen_ms INTEGER NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    notified INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE notification_deliveries (
    id TEXT PRIMARY KEY,
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    destination TEXT NOT NULL,
    destination_fingerprint TEXT NOT NULL,
    category TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending','delivered','failed','cancelled')),
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    retry_started_at_ms INTEGER NOT NULL,
    next_attempt_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    last_error TEXT,
    UNIQUE(event_id,destination,category)
);
CREATE INDEX delivery_pending ON notification_deliveries(state,next_attempt_ms);
ALTER TABLE application_status ADD COLUMN health_observed_at_ms INTEGER;
CREATE TABLE dependency_observations (
    key TEXT PRIMARY KEY,
    failed INTEGER NOT NULL,
    observed_at_ms INTEGER NOT NULL
);
-- Activation watermarks prevent replay after a category/destination is re-enabled.
CREATE TABLE notification_routes (
    category TEXT NOT NULL,
    destination TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    after_event_id INTEGER NOT NULL,
    PRIMARY KEY(category,destination)
);
