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
missing embedded migrations, and verifies the singleton instance metadata row.
Before opening SQLite it creates only missing parents of `database.path`; existing
directories are left untouched and symlinked parents or database files are
rejected. `database.path` is the sole authoritative state location.
The build script provisions a disposable migrated SQLite database before SQLx
compile-time queries are checked. It never opens an operator database.

Run the focused fresh-database coverage with:

```console
cargo test -p piqueld --test persistence
cargo test -p piqueld --test sqlx_stack
```
