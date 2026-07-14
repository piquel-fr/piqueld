# ADR 0001: SQLx owns SQLite persistence and checked queries

- Status: accepted
- Date: 2026-07-11

## Context

The prototype needs one embedded database authority, explicit forward migrations,
and compile-time validation of repository SQL. Splitting execution and validation
between two SQLite-compatible libraries would duplicate connection, transaction,
and migration ownership while allowing the SQL actually executed in production to
drift from the SQL checked by the compiler.

## Decision

`SQLx` with its integrated `sqlite` driver is the only database library. It owns
the production connection pool, transactions, migration execution, and repository
queries.

Production repository statements use `query!`, `query_as!`, or `query_scalar!` so
their bind parameters and result shapes are checked during compilation. Once the
schema arrives in Plan 04, checked-in `.sqlx/` metadata allows ordinary and CI
builds to perform those checks without connecting to an operator database. Explicit
SQL migration files remain the source of truth for schema changes.

## Executable evidence

`apps/piqueld/tests/sqlx_stack.rs` opens a disposable database and executes a
compile-time checked query through the integrated `SQLx` `SQLite` driver. Plan 04
replaces this schema-free foundation spike with the real migration stack and
offline query metadata.

## Consequences

- There is one database, pool, transaction, and migration authority.
- The SQL executed in production is the SQL checked by `SQLx`.
- Repository query changes must refresh and commit `.sqlx/` metadata.
- Runtime builds do not need access to a live validation database.
