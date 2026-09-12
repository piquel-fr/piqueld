# Database migrations

SQLx's SQLite driver owns persistence. The database at
`<server.data_dir>/piqueld.db` contains application manifests, resolved targets,
application status, operation history, and the control-plane instance identity.
The store reads and writes these records; it does not resolve images or plan
runtime changes.

`0001_control_plane.sql` creates the consolidated prototype schema with six
tables: instance metadata, applications, application status, operations,
informational events, and request receipts. Accepted manifests may have no resolved
target yet; operation preparation publishes the target after planning checks.
Operation promotion is durable, so restart recovery knows whether to maintain the
old target during preparation or continue the new rollout. SQLite writers queue
asynchronously to avoid contention between concurrent application futures.
Request receipts are committed with acceptance and expire after 24 hours,
independently of operation and event retention. Earlier prototype schemas, including those without the distinct `superseded`
operation state, require a fresh database.

Startup reads `PRAGMA user_version`, rejects an unsupported newer schema, and
applies missing embedded migrations transactionally.

Deletion marks an application deleted only after runtime verification. Its
operation history remains available. Retention removes eligible terminal history
older than the configured cutoff; `retention.finished_operation_days = 0`
disables operation pruning. Event retention is separate: `retention.event_days`
defaults to 30 and zero disables it. Events survive operation pruning and never
reconstruct runtime state. The latest operation is retained because it identifies the
current target and supports duplicate requests and reconciliation.

The daemon prepares its private data directory before opening SQLite. The store
checks the database file path. During builds, the daemon build script provisions
a disposable migrated database for SQLx query checks; it does not open an
operator database.
