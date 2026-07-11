# ADR 0001: libSQL at runtime, SQLx as isolated validation tooling

- Status: accepted
- Date: 2026-07-11

## Context

The prototype requires the official `libsql` SDK in embedded mode, explicit SQL
migrations, and SQLx checking. Both libraries bundle a SQLite-family C engine by
default. Linking them into one Rust artifact fails with duplicate `sqlite3_*`
symbols. Even if platform linker behavior masked that collision, two engines and
two pools writing one file would have unclear transaction and migration ownership.

## Decision

The official `libsql` SDK is the sole production database runtime. It owns the
embedded connection, writes, transactions, and application of explicit migration
files. No production target links SQLx.

SQLx is isolated validation and test tooling. It validates migrations and statically
checked repository SQL against disposable databases during development and CI; it
must never receive the production database path or run inside the daemon artifact.
Schema-bearing Plan 04 will commit SQLx offline metadata for query macros together
with the real migrations. Keeping SQLx in a separate test/build target also makes
the no-second-writer rule mechanically visible in Cargo dependency graphs.

This is a deliberate interpretation of “use SQLx for compile-time type-safe
queries”: SQLx checks the SQL and result contracts, while the equivalent production
execution adapter uses the official SDK. It avoids weakening the explicit embedded
libSQL requirement merely to share SQLx's runtime connection type.

## Executable evidence

The two integration-test artifacts prove the split without ever linking both C
engines into one process:

- `apps/piqueld/tests/libsql_stack.rs` creates and queries an embedded database via
  the official SDK.
- `apps/piqueld/tests/sqlx_stack.rs` uses SQLx's `query!` macro, so compilation
  describes and type-checks a query against a disposable in-memory database before
  the test executes it. Workspace Cargo configuration forcibly isolates that macro
  from any ambient `DATABASE_URL`. Plan 04 will replace this schema-free foundation
  spike with offline metadata generated from the real migrations.

The initial same-artifact spike was rejected because the linker reported duplicate
SQLite symbols, directly confirming the unsafe topology this decision forbids.

## Consequences

- There is exactly one production database and transaction authority.
- Embedded libSQL remains the actual production engine.
- Migrations remain explicit SQL files and gain isolated SQLx validation in Plan 04.
- Query definitions need a small official-SDK execution adapter after SQLx checking.
- SQLx upgrades cannot accidentally introduce a second production write pool.
