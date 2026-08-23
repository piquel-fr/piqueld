# Scheduler & operations engine — review findings for the refactor

Reference document for the planned scheduler refactor. Findings come from the
2026-08 full-stack review of `plan/06b-cli` (45 commits over `main`). Items
already fixed in this branch are marked; open items are the refactor's inputs.
Line numbers refer to the review-time tree and may have shifted slightly.

## Context

The engine is: `OperationScheduler` (operations.rs) dispatching durable
operations from SQLite through a global semaphore + per-application mutex to
`ReconcileHandler` (reconcile/handler.rs, actions.rs), driven by
`run_coordinator` (coordinator.rs). The store enforces state machines via CAS
transitions and a partial unique index (`operations_one_running_per_app_idx`).

What already works well and should be preserved:

- DB-level transition guards (guarded CAS UPDATEs for ops and steps,
  one-running-per-app index, step ordering guards).
- Step-level re-planning against fresh observation (handler.rs `execute_step`),
  which makes retries convergent and stale actions become `Skipped`.
- Idempotent Docker effects (ensure_*/remove_* with ownership re-verification,
  ID-pinned deletes), so re-execution after recovery is safe.
- Oldest-generation dispatch gate with deterministic tie-breaks.

## Open items (the refactor's job)

### O1. Cancellation outcome is race-dependent — H2

`finish_claimed` (operations.rs:181-190) maps handler-observed
`Cancelled | Superseded` to **terminal** `cancelled`, while the scheduler's
select-arm (operations.rs:288-298) maps its own observation of cancellation to
**resumable** `recovery`. Which one wins depends on where the cancel is noticed
first. Consequences when the terminal path wins:

- Startup recovery only resurrects `running`, so a cancelled op never resumes.
- If all steps had applied but `mark_ready` had not run, drift scan sees an
  empty plan and `record_recovered_status` (coordinator.rs:196-216) only fixes
  `Degraded|Failed` — status stays `deploying` indefinitely.
- Step journal records `Recovery` (handler.rs `record_step_failure`) while the
  operation row says `Cancelled`: contradictory persisted story.
- Main's deleted test `cancellation_returns_running_work_to_durable_recovery`
  pinned the old guarantee; it silently weakened.

Refactor goal: uniform mapping — any shutdown/supersede-driven cancellation
lands in `recovery`; reserve terminal `cancelled` for explicit user action.
Add a regression test: SIGINT mid-operation leaves resumable state.

### O2. No runtime watchdog — M12 (fixed mechanically in this branch, revisit design)

A single failed journal write during execution left ops stuck `running` forever:
dispatch ignores them, drift scans skip them, the oldest-generation gate blocks
all newer work for the application until restart. This branch added
`SqliteStore::reclaim_expired_running(lease)` plus a coordinator sweep every
tick with a 30-minute lease (coordinator.rs `RUNNING_LEASE_MS`). The refactor
should consider replacing the coarse lease with explicit ownership/lease
semantics or heartbeat timestamps if execution budgets grow configurable.

### O3. Scheduler concurrency guarantees are untested — M15/M6

The deleted `tests/scheduler.rs` pinned: durable claim semantics (Running +
started_at_ms before handler runs), global bound, per-application serialization
and generation ordering, claim-once under concurrent dispatchers, and
cancellation → durable recovery. None are tested today. The refactor should
restore these as unit/integration tests against the new design (TrackingHandler
style), including the O1 behavior.

### O4. Transient infrastructure errors terminally fail user operations — L2

`JournalUnavailable`/`StateUnavailable` map to op `Failed`; a momentary SQLite
error converts a create/replace into a terminal failure (status Degraded).
Drift repair eventually recreates the work, but the record says permanent.
Consider retryable classification at the scheduler level (leave in `recovery`)
for infra-class errors, keeping terminal failures for validation/permanent
classes.

### O5. Superseded-before-start reports Succeeded with Skipped steps — L3

Generation mismatch before any step ran yields every step `Skipped` + op
`Succeeded` (handler.rs `skip_superseded_steps` returning Ok), while partially-
run superseded ops become `Cancelled`. Same semantic event ("a newer generation
owns this app"), two terminal states. Unify (e.g. always superseded→cancelled)
or justify.

### O6. Minor edges to fold in

- Per-application lock map grows without bound (operations.rs:145,256-264);
  drain entries when strong count reaches 1.
- `run_coordinator`'s `Result` is effectively infallible yet main treats Err as
  fatal; make the contract honest.
- Scan-failure backoff uses `RetryPolicy::default()` instead of the configured
  policy (coordinator.rs:53).
- `operation_is_current` TOCTOU accepts one transient stale update between check
  and mutation; documented as accepted — keep documenting.
- Fixed 250 ms convergence poll without jitter — fine single-node; note for the
  future.

## Already fixed in this branch (do not re-litigate)

- Blocked-plan error misclassification: `service_update_failed` now maps to
  `ServiceUpdateFailed`; unknown diagnostics map to the distinct
  `PlanBlocked("...")` variant instead of impersonating ownership conflicts
  (M13).
- Image preparation moved out of the API mutation lock; prepare/convergence
  timeouts are configurable under `[reconciliation]` (M14).
- Retention pruning exists (`prune_finished_operations`, default 10 days,
  includes idempotency bindings) and a stale-running watchdog sweeps each
  coordinator tick (M12/M4 mechanics).
- Store-level: atomic delete finalization, keyed-replay resurrection for
  cancelled/failed deletes, step-history-preserving delete reset, corrupt-row
  quarantine in list().

## Suggested refactor checklist

1. Decide the canonical cancellation state machine (O1) and encode it in the
   store's transition matrix so illegal mappings are rejected by the DB.
2. Restore the concurrency/cancellation suite against the new dispatcher (O3).
3. Classify errors: infra-retryable vs permanent (O4); unify superseded
   terminal states (O5).
4. Fold O6 cleanups into the same PR.
5. Re-run the fake-docker integration suite plus the restored scheduler suite;
   add a SIGINT-mid-operation end-to-end test.
