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

`0002_deployments.sql` separates editable configuration from deployment inputs.
Each deployment references its execution operation and captures a manifest and
configuration revision. Attempt outcomes survive ordinary event pruning, and
successful convergence is retained independently of later runtime repair.
The migration captures the latest recoverable legacy intent; older operations
lack source manifests and are not presented as reconstructed deployments.

Saving configuration does not create or supersede an operation. Deployment
preparation reads only its snapshot, including after restart. Prepared image
resolutions remain attached to that operation during retries. A new deployment
resolves image tags again. Empty applications are valid and deploy without
services or networks.

Deployment snapshots, execution records and attempt outcomes are retained until
application deletion. Deletion removes the application's database records,
history and request receipts after runtime verification; Docker volumes remain.
Ordinary operational events still use `retention.event_days` (30 by default,
zero disables pruning). Non-deployment operation retention remains configurable.

The daemon prepares its private data directory before opening SQLite. The store
checks the database file path. During builds, the daemon build script provisions
a disposable migrated database for SQLx query checks; it does not open an
operator database.
