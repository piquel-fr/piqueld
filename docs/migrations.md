# Database migrations

`piqueld` uses SQLx's integrated SQLite driver as its sole database engine.
Forward-only SQL files live in `migrations/` and use a zero-padded numeric prefix.
Never edit a migration after it has shipped; append the next numbered file instead.

On startup the daemon reads `PRAGMA user_version`, rejects schemas newer than this
binary, and applies each missing migration in its own transaction. The singleton
instance metadata row records the stable instance ID and expected schema version.
There are intentionally no application revision or rollback tables in prototype 1.

For development, create a disposable database by starting `piqueld` against a path
inside a temporary directory, or apply the files in numeric order with a SQLite-
compatible tool. Run `cargo test -p piqueld --test persistence` to exercise fresh,
incremental, incompatible, and corrupt databases. Production repository queries use
SQLx compile-time macros checked against a provisioned database.

The `piqueld` build script creates a disposable database under Cargo's build output
directory and applies every file in `migrations/` before compiling repository query
macros. Consequently `cargo check`, `cargo build`, `cargo test`, CI, and Nix builds
all validate queries directly against the current migrated schema. No `DATABASE_URL`
or SQLx offline cache is needed, and the provisioned database cannot be an operator
database.

Migration reviews must check constraints, indexes, forward compatibility, and that
no secret fixture or plaintext is present in SQL, diagnostics, snapshots, or logs.
Create idempotency records retain only SHA-256 key/request hashes and stable resource
identities. Their referenced create operations are excluded from ordinary operation
retention so a lost-response retry remains durable.

Migration 0004 adds deletion tombstones. Completed applications disappear from the
live repository and release their logical name, while the row remains as the parent
of its durable operation journal. Retained Docker volumes are independent of this
control-plane tombstone.
