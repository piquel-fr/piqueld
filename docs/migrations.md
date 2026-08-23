# Database migrations

`piqueld` uses SQLx's SQLite driver as its sole persistence engine. The current
product has one baseline migration, `migrations/0001_control_plane.sql`, which
creates the fresh Plan 06B schema: instance metadata, applications, application
status, durable operations, operation steps, and mutation idempotency bindings
for create, replace, and delete requests.

The product has never been deployed. The baseline may therefore be edited or
consolidated while this branch is finalized; no compatibility migrations are
needed for the abandoned internal schemas. After deployment, normal forward
migration discipline applies.

The applications table requires canonical desired JSON, resolved runtime JSON,
the specification hash, generation, deletion intent, and a tombstone timestamp.
The schema contains no build, source, registry, secret, route, or published-port
tables. Operation and status diagnostics are bounded safe strings and never raw
backend errors.

On startup the daemon reads `PRAGMA user_version`, rejects a newer schema, applies
missing embedded migrations, and verifies the singleton instance metadata row. The
final migration transaction also writes the instance metadata row, so a crash can
never commit a schema version without instance identity. Retention pruning can
delete terminal operations older than a configured cutoff together with their
steps and idempotency bindings in one transaction; the partial index
`operations_finished_retention_idx` serves that cutoff scan. The daemon does not
yet schedule the pass itself.
Before opening SQLite the daemon prepares its single private `server.data_dir`
(creating missing components with mode 0700, refusing symlinks anywhere in the
path, and rejecting non-private final directories); the store itself only
verifies that the database target inside it is absent or a regular file.
The database file `<data_dir>/piqueld.db` is the sole authoritative state location.
The build script provisions a disposable migrated SQLite database before SQLx
compile-time queries are checked. It never opens an operator database.

Run the focused fresh-database coverage with:

```console
cargo test -p piqueld --test persistence
cargo test -p piqueld --test sqlx_stack
```
