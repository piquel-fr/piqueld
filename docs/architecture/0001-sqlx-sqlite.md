# ADR 0001: SQLx SQLite persistence

Status: accepted. Originally recorded 2026-07-11; updated for the simplified
application and operation model.

The prototype uses SQLx's integrated SQLite driver as its sole database runtime.
SQLx owns the pool, transactions, migrations, and query execution. Production
queries use compile-time checking against a disposable database created by the
daemon build script from the migration sequence.

The application service validates commands; the store compares intent inside its
acceptance transaction and records the response receipt atomically. Image resolution
and runtime planning belong to the controller. SQLite transactions also guard
lifecycle writes and queue writers asynchronously; they do
not implement another scheduler or persist an execution plan.

`0001_control_plane.sql` creates the consolidated prototype schema, including
receipts and progress fields. Earlier prototypes require a fresh database.
See [migrations](../migrations.md) for the schema lifecycle.
