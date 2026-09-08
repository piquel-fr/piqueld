# Database migrations

SQLx's SQLite driver owns persistence. The database at
`<server.data_dir>/piqueld.db` contains application manifests, resolved targets,
application status, operation history, and the control-plane instance identity.
The store reads and writes these records; it does not resolve images or plan
runtime changes.

The migration sequence is:

- `0001_control_plane.sql`: the original prototype schema.
- `0002_simplify_operations.sql`: preserves application IDs, desired and resolved
  state, status, and operation history while removing generations, stored spec
  hashes, idempotency bindings, and the individual operation-step journal.
  Earlier create, replace, and reconcile operations become `apply`; pending and
  recovery states become `requested`. Only the latest operation for an
  application remains active.

Startup reads `PRAGMA user_version`, rejects an unsupported newer schema, and
applies missing embedded migrations transactionally. Existing application data
and operation IDs survive the simplification migration. Fresh databases apply
the same migration sequence.

Deletion marks an application deleted only after runtime verification. Its
operation history remains available. Retention removes eligible terminal history
older than the configured cutoff; `retention.finished_operation_days = 0`
disables pruning. The latest operation is retained because it identifies the
current target and supports duplicate requests and reconciliation.

The daemon prepares its private data directory before opening SQLite. The store
checks the database file path. During builds, the daemon build script provisions
a disposable migrated database for SQLx query checks; it does not open an
operator database.
