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

`0001_control_plane.sql` creates the simplified schema directly for fresh
prototype databases. See [migrations](../migrations.md) for the schema lifecycle.
