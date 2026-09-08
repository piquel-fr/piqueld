# ADR 0001: SQLx SQLite persistence

Status: accepted. Originally recorded 2026-07-11; updated for the simplified
application and operation model.

The prototype uses SQLx's integrated SQLite driver as its sole database runtime.
SQLx owns the pool, transactions, migrations, and query execution. Production
queries use compile-time checking against a disposable database created by the
daemon build script from the migration sequence.

The store is a persistence boundary. Image resolution, target comparison, and
runtime planning belong to the application service and controller. SQLite
transactions provide atomic acceptance and guarded lifecycle writes; they do
not implement another scheduler or persist an execution plan.

Forward migration `0002_simplify_operations.sql` preserves existing application
state and operation history while removing the old generation, key, and step
bookkeeping. See [migrations](../migrations.md) for the current schema lifecycle.
