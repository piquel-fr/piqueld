# ADR 0001: SQLx SQLite persistence

Status: accepted. Originally recorded 2026-07-11; updated for the simplified
application and operation model.

The prototype uses SQLx's integrated SQLite driver as its sole database runtime.
SQLx owns the pool, transactions, and query execution; the store applies the
embedded migrations itself, tracking `PRAGMA user_version`. Production
queries use compile-time checking against a disposable database created by the
daemon build script from the migration sequence.

The application service validates commands; the store compares intent inside its
acceptance transaction and records the response receipt atomically. Image resolution
and runtime planning belong to the controller. SQLite transactions also guard
lifecycle writes and queue writers asynchronously; they do
not implement another scheduler or persist an execution plan.

`0001_control_plane.sql` creates the consolidated prototype schema, including
receipts and progress fields; later numbered migrations extend it. Earlier
prototypes require a fresh database.
See [persistence](../persistence.md) for the schema lifecycle.
