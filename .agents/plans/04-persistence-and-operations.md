# Plan 04 — Persistence, migrations, and durable operation scheduling

## Goal

Persist desired/resolved state and operational progress transactionally in embedded
libSQL, while enforcing optimistic concurrency and restart-safe per-application
serialization.

## Deliverables

- Explicit forward SQL migrations for instance metadata, applications, status,
  operations, operation steps, logical secret metadata/encrypted payload columns,
  and builds, with keys, constraints, and useful indexes.
- Repository traits and libSQL/SQLx-backed implementations isolated in
  `apps/piqueld/src/store/`, following Plan 01's ADR.
- Transactional create/full-replacement/delete-intent operations with monotonically
  increasing generation and `expected_generation` conflicts.
- Durable operation/step state machine and bounded in-process scheduler enforcing
  one mutation per application plus global operation/build limits.
- Startup recovery that turns interrupted running work into retryable/recovery state
  rather than reporting false success.
- Instance ID creation/load and schema-version checks.
- Status and build repositories; retention hooks may exist, but no revision-history
  or rollback tables.

## Work

1. Store canonical desired/resolved JSON and hash. Decode through core validation on
   read so corrupt rows fail safely.
2. Atomically write desired/resolved state, generation, initial status, operation,
   and initial steps. Never expose a desired generation without its operation.
3. Provide database-assisted validation that every referenced logical secret exists.
4. Define legal state transitions for operation, step, build, application status,
   and delete intent. Guard transitions in code and, where useful, SQL constraints.
5. Keep errors structured and sanitized. Map uniqueness, missing rows, generation
   mismatch, schema mismatch, and corruption to stable core/API codes.
6. Use cancellation-aware scheduling; dropping a future must not erase durable work.
7. Document migration development and test database creation.

## Verification

- Migration-up tests on a fresh database and schema-version rejection tests.
- Transaction rollback/fault-injection tests for each atomic write path.
- Concurrent generation-update and per-application scheduling tests.
- Restart recovery tests for resolving/building/deploying/deleting operations.
- Repository round trips preserve canonical hashes and timestamps.
- No database/log/error snapshot contains secret plaintext fixtures.

## Done when

Applications and operations survive daemon restart, concurrent writers receive a
clean conflict, and the scheduler can resume durable pending work. Docker/API work is
not part of this plan.

