# Database migrations

`piqueld` uses the official `libsql` SDK as the sole production database engine.
Forward-only SQL files live in `migrations/` and use a zero-padded numeric prefix.
Never edit a migration after it has shipped; append the next numbered file instead.

On startup the daemon reads `PRAGMA user_version`, rejects schemas newer than this
binary, and applies each missing migration in its own transaction. The singleton
instance metadata row records the stable instance ID and expected schema version.
There are intentionally no application revision or rollback tables in prototype 1.

For development, create a disposable database by starting `piqueld` against a path
inside a temporary directory, or apply the files in numeric order with a SQLite-
compatible tool. Run `cargo test -p piqueld --test persistence` to exercise fresh,
incremental, incompatible, and corrupt databases. SQLx is confined to a separate
test artifact and validates the production schema without ever opening the daemon's
database path.

When repository query shapes change, create a disposable SQLite database, apply all
files in `migrations/`, and refresh the checked-in metadata with
`cargo sqlx prepare --workspace --database-url sqlite://<disposable-path> -- --tests`. The
workspace sets `SQLX_OFFLINE=true`, so normal builds use `.sqlx/` and cannot
accidentally inspect an operator database. Use the same command with `--check`
after `prepare` for the CI freshness check.

Migration reviews must check constraints, indexes, forward compatibility, and that
no secret fixture or plaintext is present in SQL, diagnostics, snapshots, or logs.
