# Database migrations

SQLx's SQLite driver owns persistence. The database at
`<server.data_dir>/piqueld.db` contains application manifests, resolved targets,
application status, operation history, and the control-plane instance identity.
The store reads and writes these records; it does not resolve images or plan
runtime changes.

`0001_control_plane.sql` creates the current prototype schema directly. It
contains five tables: instance metadata, applications, application status,
operations, and informational events. Accepted manifests may have no resolved
target yet; operation preparation publishes the target after planning checks. There is no upgrade path from the earlier prototype schemas; use a
fresh database with this version.

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
