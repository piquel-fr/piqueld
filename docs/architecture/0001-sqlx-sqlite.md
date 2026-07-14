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
against the migrated schema. Offline metadata under `.sqlx/` makes normal builds
deterministic and prevents build-time access to an operator database. CI regenerates
the schema in a disposable database and checks that metadata remains current.

Schema-version PRAGMA reads and assignments are the only dynamically described SQL:
SQLite does not expose PRAGMA assignment through bind parameters. All application
and repository queries are compile-time checked.

## Consequences

- There is one SQLite driver, pool, migration owner, and transaction authority.
- Production and checked queries are the same Rust macro invocations.
- `BEGIN IMMEDIATE` preserves the existing write-serialization and compare-and-swap
  behavior.
- Checked-in SQLx metadata must be refreshed whenever migrations or queries change.
- No second SQLite-family driver is linked or allowed to write the database.
