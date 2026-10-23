# Persistence

SQLx's SQLite driver owns persistence (see [ADR 0001](adr/0001-sqlx-sqlite.md)).
The database at `<server.data_dir>/piqueld.db` contains application manifests,
environments with their resolved targets and status, deployments, releases,
operation and build history, accounts and their grants, encrypted secrets, the
audit trail, and the control-plane instance identity. The store reads and
writes these records; it does not resolve images or plan runtime changes. The
daemon prepares its private data directory before opening SQLite, and the store
checks the database file path.

Accepted manifests may have no resolved target yet; operation preparation
publishes the target after planning checks. Operation promotion is durable, so
restart recovery knows whether to maintain the old target during preparation or
continue the new rollout. All runtime SQLite writers, including build output,
commit metadata, completion, and recovery, share an asynchronous queue.
Read-then-write transactions acquire SQLite's write lock before reading,
preventing stale WAL snapshots during concurrent builds. Request receipts are
committed with acceptance and expire after 24 hours, independently of operation
and event retention.

Saving configuration does not create or supersede an operation. Deployment
preparation reads only its snapshot, including after restart. Prepared image
resolutions remain attached to that operation during retries. A new deployment
resolves image tags again. Empty applications are valid and deploy without
services or networks.

Deployment snapshots, execution records and attempt outcomes are retained until
environment deletion. Deletion removes the environment's database records,
environment-owned history and request receipts after runtime verification;
Docker volumes remain. The application is removed with its last environment
when its deletion was requested. Daemon-scoped failures retain optional
environment context after deletion. Application events use
`retention.event_days`; shared daemon events use `retention.daemon_event_days`
(both default to 90 days; zero disables pruning). Non-deployment operation
retention remains configurable.

## Migrations

Startup reads `PRAGMA user_version`, rejects an unsupported newer schema, and
applies missing embedded migrations transactionally. Each migration commits its
schema changes, `user_version`, and matching instance metadata together. A
restart or later migration failure can resume from any committed version
without changing the instance identity.

Each file in
[`migrations/`](https://github.com/piquel-fr/piqueld/tree/main/migrations)
explains in its comments what it changes and what it does to existing data.
The migration tests cover upgrading populated databases, the data moves of
`0012`, `0014`, `0021` and `0022`, and reopening every committed version after
a later migration fails.

### Adding a migration

Add `migrations/NNNN_<name>.sql` with the next number, starting with a comment
that says what it changes and what happens to existing data. The daemon build
script embeds every file in name order and fails unless the numbers are
contiguous from `0001`; migration `N` upgrades schema `N - 1` to `N`, and the
number of embedded migrations is the schema version. It also applies them all
to a disposable database and points `DATABASE_URL` at it, so `sqlx::query!`
macros are checked against the new schema at compile time. A migration runs as
raw SQL inside the transaction that records its version, with foreign keys
enforced, so it must not start or commit a transaction itself, and
`PRAGMA foreign_keys` has no effect there. Never edit or renumber a released
migration's statements: databases that already applied it would not run it
again. Work that needs the daemon, such as re-encrypting secrets or hashing
releases, runs in `Store::open` after the migrations instead.
