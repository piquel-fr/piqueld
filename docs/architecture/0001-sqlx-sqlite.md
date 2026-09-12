# ADR 0001: SQLx SQLite as the integrated persistence runtime

- Status: accepted
- Date: 2026-07-11

## Context

The prototype needs one embedded SQLite database, explicit forward migrations,
transactional repository operations, and compile-time query validation. Using a
separate runtime driver and SQLx only in tests duplicated the query surface and
allowed production SQL to drift away from the checked statements.

## Decision

SQLx's integrated SQLite driver is the sole database runtime. It owns the connection
pool, SQLite configuration, migrations, transactions, and repository query
execution.

Production repository statements use `query!`, `query_as!`, or `query_scalar!`
so SQL syntax, bind counts, result columns, types, and nullability are checked
against the migrated schema. The `piqueld` build provisions a fresh SQLite database
under Cargo's build output directory, applies every migration, and directs the SQLx
macros to that database. Builds therefore validate the current migrations directly
and never connect to an operator database or rely on checked-in query metadata.

SQLite does not expose PRAGMA assignment through bind parameters, so each migration
transaction stamps the applied version with one dynamically assembled statement,
`format!("PRAGMA user_version = {version}")`, executed through `sqlx::query`; the
version integer comes from the fixed, embedded migration index. That pragma stamp
is the store's only dynamically assembled statement. Every other production query
is compile-time checked.

## Consequences

- There is one SQLite driver, pool, migration owner, and transaction authority.
- Production repository statements and their checked counterparts are the same
  Rust macro invocations.
- `BEGIN IMMEDIATE` preserves the existing write-serialization and compare-and-swap
  behavior.
- Building the daemon provisions a disposable migrated SQLite database before its
  SQLx query macros are compiled.
- No second SQLite-family driver is linked or allowed to write the database.
- Startup prepares missing parents for the configured database path without
  changing existing directory permissions and rejects symlinked path components.

The current baseline is pre-deployment and may be consolidated as the supported
product model is simplified. Once the daemon is deployed, subsequent schema
changes must use forward migrations.
