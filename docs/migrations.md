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
old target during preparation or continue the new rollout. All runtime SQLite
writers, including build output, commit metadata, completion, and recovery,
share an asynchronous queue. Read-then-write transactions acquire SQLite's write
lock before reading, preventing stale WAL snapshots during concurrent builds.
Request receipts are committed with acceptance and expire after 24 hours,
independently of operation and event retention. Earlier prototype schemas, including those without the distinct `superseded`
operation state, require a fresh database.

`0003_deployment_inputs.sql` adds durable candidate manifests for manual Deploy.
Candidates and fetched commit provenance remain separate from accepted intent
until source preparation succeeds, and are removed with their operation history.
Existing version-1 databases are upgraded while retaining instance identity.

Startup reads `PRAGMA user_version`, rejects an unsupported newer schema, and
applies missing embedded migrations transactionally. Each migration commits its
schema changes, `user_version`, and matching instance metadata together. A
restart or later migration failure can resume from any committed version without
changing the instance identity.

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
application-owned history and request receipts after runtime verification; Docker volumes remain.
Daemon-scoped failures retain optional application context after deletion.
Application events use `retention.event_days`; shared daemon events use
`retention.daemon_event_days` (both default to 90 days; zero disables pruning). Non-deployment operation retention remains configurable.

The daemon prepares its private data directory before opening SQLite. The store
checks the database file path. During builds, the daemon build script provisions
a disposable migrated database for SQLx query checks; it does not open an
operator database.

Migration 0004 adds executor-independent build attempts and bounded output chunks.
Build metadata is owned by the application rather than an operation, so pruning
operation history cannot erase build history. Interrupted running records are
recovered at coordinator startup; output retention leaves metadata intact.

`0005_structured_build_logs.sql` adds capture timestamps and stdout/stderr
identity to output chunks. Old unstructured output is marked expired because
its missing provenance cannot be reconstructed; build metadata remains intact.
The stream index supports filtering before backward pagination.

`0006_observability.sql` adds self-contained diagnostic and action context to
events, active action recovery, retention coverage, and a durable webhook outbox
with activation watermarks and observed health conditions. Existing events keep
application scope; old diagnostic detail is not reconstructed. The migration
starts notification processing after existing events, avoiding historical alerts.
See [observability](observability.md) for the API and deletion contract.

Recovery deliveries are paired with their original failures at each destination
in the same observability migration.

`retention.event_days` defaulted to 30 before this release, and both event
retention settings briefly defaulted to no pruning afterwards. They now default to
90 days, so upgrading an installation that relied on unbounded history prunes
older events on the next reconciliation. Set both to `0` before upgrading to keep it.

`0007_authentication.sql` adds accounts, passkeys, hashed session/API
credentials, invitations, and a durable initial-setup marker. Existing installations require first-account
setup after upgrading; application data and running reconciliation are preserved.
There is no anonymous API compatibility mode. See [authentication](authentication.md).
Follow the [upgrade and rollback checklist](authentication.md#upgrading-an-existing-installation)
before deployment. An older daemon rejects the migrated schema; rollback requires
restoring the pre-upgrade data directory together with the old binary/configuration.

`0008_application_secrets.sql` adds application-secret metadata, encrypted versions,
per-deployment pins and a persistent master-key verifier. Empty pin sets are
recorded too, so retries cannot silently pick up subsequently added values. Secret
value changes do not update application configuration or request deployments.
Deletion reservations survive restarts; retry deletion to finish cleanup. Lost-key
recovery marks discarded versions unavailable without changing pins, and
unavailable values are excluded from retained-value quotas.

`0009_ingress.sql` adds transactional hostname reservations, hostnames reserved
for the installation's own website, and a per-application routing projection. Desired routes are journaled before gateway I/O; the last
accepted projection retains ownership until a removal is acknowledged. Saved
configuration, captured deployment inputs, resolved targets, and both routing
projections participate in reservation checks within the same SQLite transaction.
An interrupted gateway update is reapplied from durable intent. Application deletion
cascades routing records only after gateway withdrawal and runtime cleanup succeed.

`0010_history_pagination.sql` aligns the deployment application index with its
ID-based cursor and ordering. Filtered event/build queries use direct application
equality, and filtered output uses direct stream equality, so their existing
compound indexes bound each page without scanning unrelated history.

## Upgrade and rollback

Migrations are forward-only. An older daemon rejects a database with a newer
schema; reverting the binary alone is insufficient. Before upgrading, create a
SQLite backup in a private directory, for example:

```sh
sqlite3 /var/lib/piqueld/piqueld.db ".backup '/safe/location/piqueld-before-upgrade.db'"
sqlite3 /safe/location/piqueld-before-upgrade.db 'PRAGMA integrity_check; PRAGMA user_version;'
```

The SQLite backup command includes committed WAL data. Preserve the matching
daemon configuration and previous binary too. To roll back, stop the upgraded
daemon, restore the backup as `piqueld.db` in a fresh data directory, and configure
the previous daemon to use that directory. Do not reuse WAL/SHM files from the
upgraded database. Restore also reverts application edits and deployment history
accepted after the backup; reconciliation observes the current Docker state
against that restored intent.

The migration tests cover a populated schema-5 upgrade, retained event identity
and notification activation, and restoration of the pre-upgrade snapshot.
