-- Indexes for lookups that scanned whole tables: a release's promotions, a
-- secret version's copies and pins (also checked by their foreign keys on
-- delete), an operation's builds, and the builds whose output can still
-- expire. Existing rows are indexed as they are; no data changes.
CREATE INDEX deployment_release ON deployments(release_id) WHERE release_id IS NOT NULL;
CREATE INDEX application_secret_copy_version ON application_secret_copies(application_id,name,generation);
CREATE INDEX deployment_stored_secret_pin_copy ON deployment_stored_secret_pins(environment_id,name,generation);
CREATE INDEX deployment_secret_pin_version ON deployment_secret_pins(environment_id,name,generation);
CREATE INDEX builds_operation ON builds(operation_id);
CREATE INDEX builds_unexpired_finished ON builds(finished_at_ms) WHERE log_expired=0;
