# Operation Scheduling in piqueld — Architecture, State Machines, and Race Analysis

*Analysis date: 2026-08-23. All `file:line` references are against the working tree at HEAD.
Purpose: a complete, verified map of how durable operations are scheduled, executed, and
recovered — with an explicit catalog of race conditions and fragile couplings — to serve as
the factual basis for a refactor that simplifies state transitions.*

---

## Table of contents

1. [Executive summary](#1-executive-summary)
2. [Component map](#2-component-map)
3. [Data model and schema-enforced invariants](#3-data-model-and-schema-enforced-invariants)
4. [State machines](#4-state-machines)
5. [End-to-end operation lifecycle](#5-end-to-end-operation-lifecycle)
6. [Concurrency-control inventory (defense in depth)](#6-concurrency-control-inventory-defense-in-depth)
7. [Cancellation, shutdown, and crash recovery](#7-cancellation-shutdown-and-crash-recovery)
8. [Race condition catalog](#8-race-condition-catalog)
9. [Client-observable anomalies](#9-client-observable-anomalies)
10. [Refactor directions](#10-refactor-directions)

---

## 1. Executive summary

piqueld is a single-node Docker Swarm control plane. Intent (application manifests) is stored
normalized in SQLite; every mutation creates a **durable operation** row plus ordered
**operation steps** derived from a plan; a single in-process scheduler claims and executes
operations against Docker; a coordinator drives startup recovery, wake-triggered scans
(after API mutations), and periodic drift scans.

The design is *correct by construction at the storage layer*: all multi-statement mutations
run in `BEGIN IMMEDIATE` transactions, every terminal write is a predicate-CAS (`UPDATE ...
WHERE id=? AND state=?`), the schema pins a partial unique index allowing **at most one
`running` operation per application**, and Docker mutations are verify-then-act so that
crash-redo is safe.

The recurring bugs live **not in the store but at its boundaries**:

- **Read–then–CAS sequences spanning multiple store calls** in the handler and coordinator
  (`status()` read → conditional `set_status` write) can lose races against concurrent
  generation bumps or scan-driven status writes, surfacing as spurious
  `journal_unavailable` operation failures for perfectly healthy deployments.
- **Check-then-act generation guards** (`operation_is_current`) bound staleness to one step
  but never eliminate it: an old-generation step can still mutate the runtime after a newer
  generation commits.
- **Two different concurrency disciplines coexist**: the API's process-wide `mutation_lock`
  (held across minutes-long Docker image resolution → head-of-line blocking) versus pure
  store-CAS discipline for reconcile/scan paths.
- **Cancellation semantics are racy by construction**: whether SIGTERM durably produces
  `Recovery` (resumable) or terminal `Cancelled` depends on which of two racing checks wins;
  in-flight Docker requests are not actually aborted on the drop path.
- **Status vocabulary leaks scheduling internals** to clients: ops report `running` while the
  app still reads `pending`; accepts rewind `ready → pending`; supersede yields either
  `succeeded` or `cancelled` depending on timing; failed deletes resurrect on the same
  operation ID.
- **Liveness coupling**: batch-wave dispatch joins an entire snapshot before re-fetching, so
  one slow convergence wait (up to 2 min per step, unbounded overall) starves unrelated
  applications despite free semaphore permits.

Section 8 catalogs each issue with interleavings and impact; Section 10 sketches refactor
directions keyed to them.

---

## 2. Component map

```
HTTP handlers (api/applications.rs, api/operations.rs)
    │  validate manifest · resolve images via runtime.prepare() · build plan
    │  commit mutation + operation + steps + idempotency binding   ← BEGIN IMMEDIATE
    │  trigger_reconciliation()  ──► Notify (wake permit)
    ▼
SqliteStore (store/{mod,application,operation,status}.rs)          SQLite (WAL)
    │  CAS transitions, guarded status writes, durable queue query
    ▼
OperationScheduler (operations.rs)
    │  run_until_idle(): snapshot pending_operations(100) → JoinSet wave → join → repeat
    │  global Semaphore(4) · per-app Mutex map · claim = CAS Pending|Recovery→Running
    ▼
ReconcileHandler (reconcile/handler.rs)   implements OperationHandler
    │  generation guard · start_deployment · per-step: re-observe → re-plan → act
    ▼
DockerRuntime (reconcile/runtime.rs)  ──► BollardDocker (docker/*.rs)
    │  observe(): networks+volumes+services+tasks snapshot (multi-call, non-atomic)
    ▼
Coordinator (reconcile/coordinator.rs)   owns the scheduler loop
    startup recovery (recover_interrupted + drain)
    select { cancel | wake.notified() | scan.tick(60 s) } → scan_and_run()
```

Key files and sizes:

| File | Role |
| --- | --- |
| `apps/piqueld/src/operations.rs` (330 L) | `OperationScheduler`, `OperationError`, claim/finish logic |
| `apps/piqueld/src/reconcile/coordinator.rs` (269 L) | recovery loop, scans, wake handling |
| `apps/piqueld/src/reconcile/handler.rs` (370 L) | step execution, status writes, supersede logic |
| `apps/piqueld/src/reconcile/runtime.rs` (101 L) | `DockerRuntime`: prepare/observe/wake boundary |
| `apps/piqueld/src/reconcile/actions.rs` (173 L) | per-action retry wrapper, convergence polling |
| `apps/piqueld/src/reconcile/mod.rs` (139 L) | `RetryPolicy`, error mapping |
| `apps/piqueld/src/store/operation.rs` (387 L) | queue query, transitions, recovery, delete finalize |
| `apps/piqueld/src/store/application.rs` (1054 L) | mutation transactions, idempotency, tombstones |
| `apps/piqueld/src/store/status.rs` (88 L) | guarded `set_status` CAS |
| `apps/piqueld/src/api/applications.rs` / `mod.rs` | endpoints, `mutation_lock`, plan-at-submit |
| `migrations/0001_control_plane.sql` (107 L) | entire schema |

Bootstrap order (`main.rs:34-119`): DB open → Docker connect + swarm gate → shared `Notify`
→ `ReconcileHandler` → `OperationScheduler(max_parallel_operations=4)` → `ApiState`
→ coordinator task spawned **before** HTTP listeners bind (but tasks run concurrently, so
HTTP serving overlaps startup recovery — see §8-R16) → signal handler → graceful shutdown
awaits API listeners then controller.

---

## 3. Data model and schema-enforced invariants

### 3.1 Tables

**`applications`** (`0001_control_plane.sql:15-31`)
`id TEXT PK` (shape-checked), `name UNIQUE`, `generation > 0`,
`desired_json`/`resolved_json` (JSON-valid), `spec_hash sha256:*`, `delete_intent ∈ {0,1}`,
nullable `deleted_at_ms` (tombstone), timestamps with monotonicity CHECKs.
Tombstoning renames the row (`"deleted-{id}-{now}-{uuid}"`) rather than deleting it.

**`application_status`** (`:33-40`) — 1:1 with applications
```sql
state CHECK IN ('pending','deploying','ready','degraded','deleting','failed'),
observed_generation INTEGER NULL,
CHECK (observed_generation IS NULL OR state IN ('ready','degraded','failed'))
```

**`operations`** (`:42-62`)
```sql
kind CHECK IN ('create','replace','delete','reconcile'),
state CHECK IN ('pending','running','recovery','succeeded','failed','cancelled'),
CHECK (terminal ⇒ finished_at_ms NOT NULL, non-terminal ⇒ NULL),
CHECK (running ⇒ started_at_ms NOT NULL),
CHECK ((error_code IS NULL) = (error_message IS NULL)),
CHECK (error_code IS NULL OR state = 'failed')
```

**`operation_steps`** (`:64-85`) — same family, plus `position ≥ 0` (`UNIQUE(operation_id,
position)`), `action TEXT ≤64`, `attempt ≥ 0`, and an extra state value `'skipped'`.

**`mutation_idempotency`** (`:87-95`) — PK `key_hash` (sha256 of Idempotency-Key),
`request_hash`, `application_id`, `operation_id`, `generation`, `kind`. One binding per key;
bindings are never garbage-collected.

### 3.2 The keystone invariant

```sql
CREATE UNIQUE INDEX operations_one_running_per_app_idx ON operations(application_id)
    WHERE state = 'running';                                   -- :101-102
```

This is the **only schema-level scheduling invariant**, and it is the strongest one in the
system: even if every in-process lock were removed, two operations of one application could
never be `running` simultaneously. Everything else (per-app mutexes, sibling-running SQL
guards) is redundancy layered on top.

### 3.3 What the schema does *not* enforce

Enforced only in Rust (and therefore only as good as their call sites): the legal-transition
matrices (§4), monotonic `observed_generation`, ready-requires-matching-generation,
delete-intent gating, generation increment discipline, `delete_intent ⇔ deleted_at_ms`
coupling. There are no triggers anywhere in the migration.

### 3.4 SQLite runtime configuration

`store/mod.rs:474-487`: WAL journal, `busy_timeout(5s)`, foreign keys on, pool max 8
connections. Every mutating entry point uses `BEGIN IMMEDIATE` (`begin_immediate`,
`store/mod.rs:579-584`), so writers queue on the single SQLite write lock and losers park in
the busy handler up to 5 s. **There is no retry logic at the store layer** — any sqlx error
(including busy-timeout expiry) maps to `StoreError::DatabaseSource`, indistinguishable from
corruption downstream. Retries exist only above the layer, per-subsystem: coordinator scan
loop retries with exponential backoff (100 ms → 2 s cap, `coordinator.rs:47-74`), startup
recovery retries every 1 s. Store transactions never span Docker I/O.

---

## 4. State machines

Three independent state machines, all defined in `store/mod.rs` and enforced only by code +
CAS predicates:

### 4.1 Application lifecycle status (`ApplicationState::can_transition_to`, mod.rs:161-182)

Self-transitions always allowed. Otherwise:

```
Pending   → Deploying | Deleting | Failed
Deploying → Ready | Degraded | Failed | Deleting
Ready     → Pending | Degraded | Deleting | Failed
Degraded  → Pending | Ready | Deleting | Failed
Failed    → Pending | Ready | Degraded | Deleting
Deleting  → Degraded | Failed        (terminal-ish; nothing returns to Pending)
```

Notably illegal: `Ready→Deploying` (a replace must go through `pending`),
`Failed→Deploying`, anything out of `Deleting` except `Degraded|Failed`.
Additional rules enforced inside the `set_status` UPDATE WHERE clause (`status.rs:69-81`):

- (b) `observed_generation` is monotonic non-decreasing;
- (c) may not observe a generation ahead of desired;
- (d) `ready` requires `observed_generation == applications.generation` exactly;
- (e) `deleting` requires `delete_intent=1`;
- (f) once `delete_intent=1`, only `deleting|degraded|failed` are reachable.

A fifth writer bypasses all of this: the private raw helper `set_application_status`
(`application.rs:822-843`) sets `state=?, observed_generation=NULL, message=NULL` with **no
CAS, no transition matrix** — used at exactly four sites, always inside a mutation's
`BEGIN IMMEDIATE` transaction so the reset is atomic with the operation insert:
`replace_with_key`→`'pending'` (:323), `request_delete_with_key`→`'deleting'` (:581),
`request_reconcile` inline →`'pending'` (:413-421), `reset_failed_delete`→`'deleting'`
(:1037-1045). This is the sanctioned "mutation resets the status slate" channel; it silently
overwrites whatever the executor last wrote (see §8-O).

### 4.2 Operation states (`WorkState::can_transition_to`, mod.rs:263-275)

```
Pending | Recovery → Running | Cancelled
Running            → Recovery | Succeeded | Failed | Cancelled
Succeeded | Failed | Cancelled      → terminal
```

Extra SQL guards beyond the matrix (`transition_operation`, store/operation.rs:299-309):

- entering `running` refused if **any sibling op of the same app is already running**
  (subquery resolves the app id from the target row);
- entering `succeeded` refused unless every step is `succeeded|skipped`;
- `started_at_ms=COALESCE(started_at_ms,?)` preserves first start across recoveries;
- `finished_at_ms` set only on terminal states.

Failure modes are distinguished post-hoc by `transition_miss` (`store/mod.rs:586-623`):
a follow-up existence probe turns a zero-row UPDATE into `NotFound` (row gone) vs
`IllegalTransition` (row in unexpected state). Callers use this to swallow benign claim
losses (`operations.rs:283-287`).

Routing pre-SQL (`operation.rs:274-296`): `error` payload mandatory iff target is `failed`;
`Running→Recovery` delegates to `recover_operation` (own IMMEDIATE tx, clears error fields);
`→{Failed,Cancelled}` delegates to `finish_operation`, whose IMMEDIATE tx first sweeps all
non-terminal steps to `cancelled`, then performs the guarded op UPDATE — a guard miss rolls
back the sweep.

Delete success bypasses the generic path entirely: `finish_delete_operation`
(`operation.rs:25-55`) atomically flips the op to `succeeded` **and** tombstones the
application (rename + `deleted_at_ms`) in one transaction, guarded by kind/generation/
steps-complete predicates. The app can never be hidden while its delete op is unsucceeded,
and vice versa. Crash windows around it are enumerated in §7.3.

### 4.3 Step states (`StepState::can_transition_to`, mod.rs:329-341)

Same shape as operations plus `skipped` (terminal):

```
Pending | Recovery → Running | Cancelled | Skipped
Running            → Recovery | Succeeded | Failed | Cancelled
```

`transition_step` SQL (`store/operation.rs:342-353`) adds three guards:

- **parent-running coupling**: the step may only change while its operation is `running`
  (this is why stray step writes fail after the op terminalizes);
- at most one `running` step per operation;
- **left-to-right ordering**: a step may enter `running` only when no earlier-position step
  is non-terminal. Asymmetry: skipping later positions is unrestricted, which
  `skip_superseded_steps` relies on.

`attempt` increments once per entry into `running` (`attempt=attempt+(to='running' AND
from≠'running')`, :338) — including recovery restarts. Note this counts *step entries*, not
Docker attempts: the 4 in-`retry()` attempts per action are invisible to the journal.

### 4.4 Who writes what, when

| Writer | Transition | Location |
| --- | --- | --- |
| create/replace commit | → `pending` (obs_gen NULL) | store/application.rs:139-146, :323 |
| delete commit | → `deleting` | :581 |
| reconcile insert (new op) | → `pending`, obs_gen NULL, msg NULL | :413-424 |
| executor start (non-delete) | `pending → deploying` | reconcile/handler.rs:111-130 |
| executor success | `→ ready`, obs_gen = op.generation | handler.rs:317-339 |
| executor failure | `deploying→degraded` / `deleting→failed` / `ready\|degraded→degraded` | handler.rs:229-285 |
| drift scan, blocked plan | `ready → degraded` (**no operation created**) | reconcile/coordinator.rs:176-194 |
| drift scan, converged | `degraded\|failed → ready` | coordinator.rs:196-216 |

---

## 5. End-to-end operation lifecycle

### 5.1 Submission (API layer)

All mutating endpoints share one skeleton (`api/applications.rs`):

1. **Create** (`:132-196`): requires `Idempotency-Key`; application ID is a pure function of
   the key (`idempotent_application_id = "app-" ++ sha256("piqueld-create/v1\0"+key)[..16]`,
   api/mod.rs:540-544), so a retry targets the same row by construction. Parse + strict
   validation → take **global `mutation_lock`** (`:153`) → advisory idempotency lookup →
   name-collision pre-check → `runtime.prepare(&app)` (**live registry resolution, up to
   ~5 min, while holding the lock**) → compute `Plan::from_request` against fresh observation
   → blocked plan ⇒ 409 `plan_blocked` → flatten plan actions into step-name strings →
   `store.create_idempotent(...)` commits app row (generation 1) + status `pending` +
   operation `pending` + steps + idempotency binding in ONE transaction →
   `trigger_reconciliation()` (wake) → **202 Accepted** `{operation_id, application_id,
   generation}`. Replay with same key returns the identical `operation_id`.

2. **Replace** (`:198-307`): optimistic concurrency via `expected_generation` (JSON body or
   `X-Expected-Generation` header); optional idempotency key folded into
   `request_hash = sha256("piqueld-mutation/v1\0{kind}\0{id}\0{gen}\0{spec_hash}")`
   (api/mod.rs:401-412). Same lock + prepare + plan skeleton; store side
   (`replace_with_key`, application.rs:288-353) re-checks everything inside its tx, CASes
   `UPDATE ... WHERE id=? AND generation=? AND delete_intent=0`, resets status to `pending`
   (obs_gen NULL), inserts the op at `expected+1`.

3. **Delete** (`:331-411`): observes live Docker for the deletion plan; store side
   (`request_delete_with_key`, :498-601) sets `delete_intent=1` + status `deleting`, inserts
   the delete op. If a delete op already exists: active (`pending|running|recovery`) ones are
   reused; **`failed|cancelled` deletes are reset in place** — `reset_failed_delete`
   (:999-1053) flips the same row back to `pending`, deletes all steps, re-inserts steps from
   the fresh plan. Clients holding the old ID watch it go `failed → pending` (see §9).

4. **Reconcile** (`POST /{id}/reconcile`, :606-656): dedups on any active
   (`pending|running|recovery`) reconcile op for the generation; does **not** take
   `mutation_lock`; safety rests entirely on the in-tx dedupe + generation CAS inside
   `request_reconcile` (application.rs:361-430). Two racers converge on one operation.

Handlers **never block until execution starts** — they return 202 immediately after commit.
But create/replace do block for the full image-resolution window *inside* `mutation_lock`
(§8-R14). Plans are computed **twice**: once at submit (persisted as step names), then again
per step at execution time (§5.3).

### 5.2 Wake-up and dispatch

Every successful mutation calls `trigger_reconciliation()` → `Notify::notify_one`
(`runtime.rs:34-36`). The coordinator selects over `{cancel, wake.notified(), scan.tick()}`
with a 60 s default interval, missed ticks skipped (`coordinator.rs:36-44`). `Notify` stores
one permit, so N rapid wakes collapse to one rescan (intended). Wakes consumed during a
scan's internal error-retry backoff are serviced only after the backoff completes (§8-K).

Dispatch itself is **batch-wave, not a worker pool**. `run_until_idle` (`operations.rs:237-329`):

```rust
loop {
    let operations = self.repository.pending_operations(MAX_PAGE_SIZE).await?; // ≤100
    if operations.is_empty() { return Ok(()); }
    let mut tasks = JoinSet::new();
    for operation in operations {
        // spawn task: acquire per-app mutex → acquire global semaphore permit
        //   (both selects biased toward cancellation)
        // claim: transition_operation(snapshot.state → Running)   [CAS]
        //        IllegalTransition => return Ok(())                // lost race, benign
        // execute: select! { biased cancel → park Running→Recovery ;
        //                    execute_claimed(handler) }
        // finish_claimed(result)
    }
    join_all(tasks);           // <-- whole wave must drain before refetch
}
```

The durable queue query (`pending_operations`, store/operation.rs:256-265) exposes only
`pending|recovery` rows and applies the crucial filter:

> Only the **oldest queued generation per application** is visible — an op is eligible only
> if no older-or-equal-generation sibling of the same app is itself
> `pending|recovery|running` (ties broken by `created_at_ms`, then UUIDv7 `id`).
> Comment: *"This makes ordering durable rather than depending on task scheduling or mutex
> acquisition order."*

Consequences: per-app FIFO is guaranteed by data, not by locks; a newer queued op waits for
the currently-*running* elder too (total per-app serialization); within one snapshot each
app appears at most once, so the per-app tokio mutex is uncontended intra-wave and only
guards across waves/restarts. No permanent starvation exists (superseded elders terminate
quickly via generation guards), but see §8-E for latency coupling.

Claim failure surfaces as `StoreError::IllegalTransition` and is deliberately swallowed
(`operations.rs:284-287`) — the op stays queued for the next wave.

### 5.3 Execution (handler)

`execute_operation` (`handler.rs:36-71`):

1. Load application. `NotFound` tolerated **only for Delete** ("Finalization tombstones the
   application before the scheduler marks the operation successful. A crash in that tiny
   window resumes here safely", :99-103) — early `Ok(())`, and the scheduler still calls
   `finish_delete_operation`, whose guards make replay a safe no-op.
2. **Generation guard** (non-delete): `application.generation != operation.generation` →
   skip remaining steps, return. *"A newer generation owns the application now."*
3. `start_deployment`: read status; if `pending` (and non-delete) CAS-write
   `pending → deploying`. No generation guard on this write (§8-H).
4. Build `PlanRequest` from the **persisted resolution** (images are resolved at submit, not
   at execute).
5. `execute_steps`: iterate persisted steps; skip `succeeded|skipped`; before each step
   re-check generation currency and cancellation.

Per-step (`execute_step`, :159-207):

```
deadline = now + convergence_timeout (2 min)
observed = observe_with_retry(...)                 // full Docker snapshot, retried
current  = Plan::from_request(request, &observed)  // RE-PLAN from scratch
if current.is_blocked() → fail op (OwnershipConflict / DockerConfigurationConflict)
action = current.actions.find(|a| a.operation_step() == step.action)  // string match!
else → transition_step(step → Skipped)             // plan no longer needs it
transition_step(step → Running)                    // attempt += 1
execute_action(action)                             // retry-wrapped Docker call
  Ok  → transition_step(Running → Succeeded)
  Err → record_step_failure: cancel? → Recovery : Failed(with error); propagate
```

Load-bearing subtleties:

- **The persisted step row is a checkpoint marker, not a payload.** The action executed is
  always freshly derived from current observation + persisted desired state. This is what
  makes crash-resume and replay idempotent — redo takes the verify-then-(maybe-)act path.
- **Action lookup is by stable display string** (`PlanAction::operation_step()`,
  planner.rs:239-256, e.g. `"ENSURE SERVICE web"`; sha256-suffixed if >64 chars). If the
  fresh plan contains actions absent from the persisted steps, they silently don't run in
  this operation (picked up by the next scan); if it contains fewer, leftovers get `Skipped`.
- Retry classification (`actions.rs:61-106`): retryable = transport/unavailability/image
  resolution errors (4 attempts, 100 ms doubling to 2 s cap, per action); non-retryable =
  ownership conflict, immutable config conflict, not-manager, topology, local validation.
- Convergence waiting (`wait_service`, actions.rs:107-130) is **polling**: full
  `observe()` every 250 ms until `Convergence::Converged`, deadline 2 min →
  `ConvergenceTimeout`; Docker update paused → `ServiceUpdateFailed` (immediate fail).
- There is **no overall operation deadline**: worst case per step ≈ observe-retry window +
  4 × 30 s request timeout + a full 2 min convergence wait, times N steps, while holding a
  semaphore permit and the app mutex.

After steps: final currency check → `mark_ready` (non-delete; CAS to `ready` with
observed_generation = op.generation, message `"runtime converged"`), or `finish_delete`
(re-observe, re-plan; anything left ⇒ `DeletionNotConverged`).

Superseding (`skip_superseded_steps`, :287-315): `pending|recovery` steps → `skipped`;
any `running|failed|cancelled` step ⇒ return `Err(Superseded)` → scheduler journals the op
`Cancelled`. If *nothing* was in flight, returns `Ok(())` and the empty-handed op is
journaled **`Succeeded`** (§8-G).

Failure recording (`record_operation_failure`, :229-285): guarded by a currency check;
maps `(kind, current status)` to a target (`deleting→failed`, `deploying→degraded`,
`ready|degraded→degraded`), stamps observed_generation = op.generation — i.e. "observed"
really means "last attempted". Errors here are logged, swallowed, and never affect the op's
own outcome.

### 5.4 Finish

`finish_claimed` (`operations.rs:162-202`):

| Result | Journal write |
| --- | --- |
| `Ok` + kind Delete | `finish_delete_operation` — atomic succeed+tombstone |
| `Ok` | `Running → Succeeded` (steps-complete guard) |
| `Err(Cancelled \| Superseded)` | `Running → Cancelled` (sweeps non-terminal steps to `cancelled`) |
| other `Err(e)` | `Running → Failed(code,message)` (sanitized) |

Note: `journal_error` (handler.rs:86-90) collapses **every** `StoreError` — including
benign CAS misses — into `OperationError::JournalUnavailable`, which lands in the `Failed`
branch. This conversion is the root of several spurious-failure bugs below.

---

## 6. Concurrency-control inventory (defense in depth)

For one application, mutual exclusion is enforced **five independent ways**, plus ordering
by data:

| # | Mechanism | Where | Scope |
| --- | --- | --- | --- |
| 1 | Partial unique index `operations_one_running_per_app_idx` | schema :101-102 | cross-process, absolute |
| 2 | Sibling-running predicate in `transition_operation` | store/operation.rs:300 | per-app, per-claim |
| 3 | Per-app `tokio::sync::Mutex` map | operations.rs:145, 257-264 | in-process, across waves |
| 4 | Handler generation equality checks | handler.rs:44-50, 144-149, 362-369 | logical (check-then-act, racy) |
| 5 | Status CAS guards incl. ready⇔generation match | status.rs:69-81 | catches stale success writes |
| + | Oldest-generation-per-app visibility filter | store/operation.rs:256-265 | durable FIFO ordering |

Plus API-level serialization:

- Process-wide `mutation_lock` held across idempotency lookup **through Docker prepare**
  for create/replace/delete (api/applications.rs:153, 248, 351; rationale comment
  api/mod.rs:103-104). Deliberate: prevents duplicate registry work for concurrent retries
  of the same key. Cost: head-of-line blocking of ALL mutations behind one slow registry
  (minutes). Reconcile/scan paths don't participate — two disciplines coexist.
- Idempotency table: advisory out-of-tx lookups + authoritative in-tx re-checks +
  unique-violation-on-insert mapped to `IdempotencyConflict`. Cross-process safe.
- Optimistic generation CAS on every application-row rewrite, producing precise
  `GenerationConflict{expected, actual}` 409s.

And Docker-side mitigations:

- Act-time ownership re-verification inside every `ensure_*`/`remove_*`;
- delete-by-ID (not name) with explicit TOCTOU comments (resources.rs:424-425, :458);
- bounded retry loop on Docker's transient `update out of sequence` (string-matched,
  engine.rs:200-211 — fragile, §8-L).

The redundancy is mostly harmless today but is exactly the surface where bugs breed: three
of the four in-process mechanisms (mutex map, generation checks, status CAS) implement
*policy*, while only #1/#2 implement *mechanism*. A refactor that trusts the DB layer more
could delete most of the rest (§10).

---

## 7. Cancellation, shutdown, and crash recovery

### 7.1 Cancellation points

Root `CancellationToken` (SIGINT/SIGTERM) → child tokens per scheduler task and per
scan/recovery run. All scheduler selects are `biased` toward `token.cancelled()`
(operations.rs:265-298). Where cancellation can land:

| Await point | Durable outcome |
| --- | --- |
| Before permit/mutex acquired | none — op stays `pending|recovery`, redispatched next boot/run |
| After claim, before/during execution | scheduler select wins → `Running → Recovery` (parked) |
| Handler notices first (`retry()`/`wait_service` loops check `is_cancelled()`) | returns `Err(Cancelled)` → op journaled terminal **`Cancelled`** |

**The same SIGTERM produces two different durable outcomes depending on which side wins the
race between the scheduler's select and the handler's internal checks.** Terminal
`Cancelled` ops are never automatically resumed (only deletes can be reset, and only by an
explicit new request); `Recovery` ops resume on next boot or scan drain. See §8-C.

Additionally, dropping `execute_claimed` at an await point does not abort the underlying
Docker HTTP driver task on the drop path (`engine.rs:90-92` aborts only on normal
completion) — an in-flight mutation may complete server-side after the op was parked as
interrupted. Resume re-verifies, so it converges, but "cancelled" ≠ "nothing happened"
(§8-C2).

### 7.2 Crash recovery

Crash leaves `running` rows. At boot, `recover_interrupted` (`store/operation.rs:365-386`)
in one IMMEDIATE tx flips **all** `running` ops and steps to `recovery`, clears
`started_at_ms`, preserves attempt counters and partial step progress. Recovered ops are
indistinguishable from fresh work to the dispatcher (same eligibility set), so there is no
separate replay mechanism — `recover_and_run` = `recover_interrupted` + `run_until_idle`.
Running it twice is a no-op returning 0. Startup retries every 1 s on store failure
(coordinator.rs:23-34).

Because HTTP serving starts concurrently with recovery, a restarted daemon can briefly
report crashed-run ops as `running` before demotion to `recovery` (§8-R16).

### 7.3 Delete-finalization atomicity

`succeeded`-flip and tombstone share one transaction (§4.2). Crash windows: before stmt 1 →
op stays `running` → recovered → teardown re-runs (idempotent) → refinalize; mid-tx → rolled
back atomically; after commit → done. `load_application` tolerating `NotFound` for delete
ops covers the post-commit-but-pre-scheduler-write corner. Verified by
tests/fake_docker.rs:277-326 and tests/persistence.rs:119-141.

---

## 8. Race condition catalog

Consolidated and deduplicated from all layers. Severity reflects likelihood × blast radius
for the single-node prototype.

### Correctness-affecting (misleading failures / lost work)

**R1 — Supersede race misclassified as `journal_unavailable` operation failure.**
`mark_ready` (handler.rs:317-339) reads status, checks currency (handler.rs:62-65), then
CAS-writes `→ ready` requiring `observed_generation == applications.generation`
(status.rs:70, guard d). Interleaving: currency check passes → user commits a replace
(generation+1, raw status reset to `pending`) → `set_status(→ready, obs_gen=old)` matches
zero rows → `IllegalTransition` → `journal_error` → op journaled
**`Failed("journal_unavailable")`** although deployment succeeded and the new generation
will redo the work. Same shape in `start_deployment` (R-H) and `record_operation_failure`
(R-O). *Impact: misleading history; alert noise; the exact class of bug the refactor should
eliminate.* Mitigation today: none; the guard fires after the decision was already made.

**R2 — Orphaned `running` operation on terminal-write failure.**
If `finish_claimed`'s store write fails transiently (busy timeout under load), the op stays
`running` forever in-process: `pending_operations` selects only `pending|recovery`, and
`recover_interrupted` runs only at boot (`operations.rs:301`, `coordinator.rs:24`). Same
exposure for the cancel-path `Running→Recovery` write whose `?` propagates first
(operations.rs:290-297). *Impact: app wedged until daemon restart; delete of that app queues
behind a ghost (R-J).*

**R3 — Cancellation outcome race: `Recovery` vs terminal `Cancelled`.**
Two racing cancellation detectors (scheduler select, operations.rs:288-299 vs handler
checks in `retry()`/`wait_service`, actions.rs) decide between resumable parking and
permanent terminalization. Non-deterministic per event; also inconsistent with the
documented restart-safe contract since `Cancelled` is never auto-resumed.

**R4 — In-flight Docker request survives cancellation (driver leak on drop).**
The spawned hyper connection driver (engine.rs:90-92) is aborted only on the normal
completion path (engine.rs:131-132); dropping the future mid-request leaves it detached, so
a mutation can apply after the op was parked `recovery`/`cancelled`. Safe on resume
(verify-then-act) but semantically muddy and an unbounded-task leak per cancelled request.

**R5 — Old-generation step mutates runtime after a new generation commits.**
Per-step currency check (handler.rs:144-149) is check-then-act: replace commits after the
check, then `execute_action` applies the old spec (payload planned from old `app.resolved`
captured at :53). Runtime transiently diverges from both generations until the new op
re-plans. Bounded to one step; inherent to the design without a transactional claim on
(app, generation).

**R6 — `reset_failed_delete` step-swap safety is call-pattern-dependent.**
`reset_failed_delete` (store/application.rs:999-1053) DELETEs and re-INSERTs all steps of a
`failed|cancelled` delete op. Any concurrent holder of old step IDs would hit `NotFound` →
`JournalUnavailable`. Currently unreachable because reset requires the op non-running and
claimed ops are `running` — but nothing structural enforces it; a future caller resetting a
running op's steps breaks silently. Encode the precondition in SQL (guard the steps DELETE
with `state IN ('failed','cancelled')` on the parent) or forbid step deletion.

### Liveness / performance

**R7 — Batch-barrier head-of-line blocking + no operation deadline.**
`run_until_idle` joins the entire ≤100-op wave before re-fetching (operations.rs:305-320);
each step may hold its permit through observe-retry + 4×30 s attempts + a 2 min convergence
wait, with no overall deadline. Four slow apps saturate `max_parallel_operations=4` and
delay everyone; ops created mid-batch wait for full drain even with free permits. Tests run
with `max_parallel_operations=1`, fully serializing (fake_docker.rs:240, 543).

**R8 — Delete latency absorbs a stale running reconcile.**
One-running-per-app means a newly requested delete cannot be claimed until the elder
reconcile notices supersession at its next per-step check — potentially its full 2-minute
convergence horizon (store/operation.rs:300 + handler currency checks). Correct but
perceived as "delete hung".

**R9 — Wake collapse + retry-backoff blind spot.**
Single-slot `Notify` collapses N wakes (intended), but a wake arriving during
`run_scan_until_success`'s exponential backoff sleeps is serviced only after the backoff
completes (coordinator.rs:56-73); no wake re-check inside the sleep.

**R10 — Per-app mutex map grows unboundedly.**
`locks.entry().or_insert_with()` is never pruned (operations.rs:145, 257-264) — one leaked
`Arc<Mutex<()>>` per application ever seen. Minor; symptom of policy living in-process.

### Consistency / observability

**R11 — Observation snapshots are non-atomic (torn reads).**
`BollardDocker::observe` (docker/resources.rs:91-218) runs list-networks → list-volumes →
list-services → inspect-each-service → list-tasks sequentially with no consistency point;
mid-observation changes yield torn snapshots (unresolvable network IDs stay as raw IDs at
:206-212 and read as drift). Every 250 ms convergence poll repeats the full snapshot, so
tears can extend waits or spuriously time out near deadlines. The "one boundary operation"
comment (:88-89) covers ID normalization, not atomicity.

**R12 — Status flapping from scan vs mutation interleave.**
Scan's `record_blocked_status` (`ready→degraded`) can land just before a concurrent
mutation's raw status reset wipes it to `pending` (coordinator.rs:176-194 vs
store/application.rs:323/:413-421/:581). Harmless for correctness (the new op re-decides)
but client-visible churn. Root cause: two writers with different disciplines writing the
same cell.

**R13 — Supersede terminal-state asymmetry.**
Depending on whether a step was in flight when supersession hit, an operation that did
nothing is journaled `Succeeded` (all steps `skipped`) or `Cancelled`
(handler.rs:287-315). Same causal event, two terminal codes; `succeeded` history can
contain no-op executions.

**R14 — Global mutation lock held across minutes of Docker I/O.**
Create/replace hold `mutation_lock` through registry resolution (up to 5 min prepare
timeout; per-image 30 s, 4-way concurrent — runtime.rs:10-11, 69). One slow registry stalls
every mutation's preparation phase. Undocumented latency profile; deliberate per comments
(api/applications.rs:150-153).

**R15 — Fragile string coupling to Docker error text.**
Update-conflict retry triggers only on the exact body `"rpc error: code = Unknown desc =
update out of sequence"` with status 500 (engine.rs:200-211). Any rewording converts routine
optimistic-concurrency conflicts into hard failures. Test-locked, hence intentional — but
brittle.

**R16 — HTTP serving overlaps startup recovery.**
Coordinator spawns before listeners bind but runs concurrently; orphaned `running` ops from
a crash are briefly visible as `running` to early clients before `recover_interrupted`
demotes them (`main.rs:89-117`, store/operation.rs:365-386).

**R17 — Error-class mismatch on the index backstop.**
The unique-index backstop fires only if some future path bypasses the sibling-running SQL
guard; the resulting raw unique-violation maps to `DatabaseSource` → HTTP **503** instead of
409-class `IllegalTransition` — a logical scheduling conflict misreported as storage outage
(store/operation.rs:299-313 vs migrations :101-102).

**R18 — Retention/GC gaps interacting with filters.**
Tombstoned apps keep their status row (invisible via JOIN filter, status.rs:18, never
deleted), all operations/steps, and idempotency bindings; `operations_finished_retention_idx`
(migration :103-104) anticipates GC that doesn't exist. Any GC must respect
`pending_operations` NOT EXISTS semantics and the one-running index.

**R19 — Timestamp sourcing.**
In-tx functions correctly share one `now_ms()`; cross-function sequences (claim T1, step
writes T2..) interleave with wall-clock adjustments; only `>=` CHECKs catch pathology;
`now_ms` saturates at `i64::MAX` rather than failing (store/mod.rs:745-751).

---

## 9. Client-observable anomalies

Direct consequences of the above, worth fixing as UX-level acceptance criteria for the
refactor:

1. Op reports `running` while app status still says `pending` — the scheduler flips the op
   before the handler commits `deploying` (operations.rs:275-287 vs handler.rs:111-130).
2. Accepting a replace instantly rewinds `ready → pending` and nulls `observed_generation`
   before anything executes (store/application.rs:323) — clients see progress lost.
3. Drift scans silently rewind `ready → pending` when creating a reconcile op, and flip
   `ready → degraded` (blocked plans) **with no operation existing at all** — status changes
   with only a message, no operation_id surfaced anywhere (coordinator.rs:140-146, :176-194).
4. `degraded` carries `observed_generation` equal to a generation that never converged —
   "observed" actually means "last attempted" (handler.rs:256-281).
5. `recovery` is client-visible and non-terminal; clients must know to keep polling.
6. A retried failed delete resurrects the **same operation ID** from `failed` back to
   `pending` with brand-new steps (store/application.rs:999-1053; test
   persistence.rs:324-351).
7. Successful-looking operations can be journaled `failed("journal_unavailable")` due to
   R1/R-H races.
8. CLI default `--timeout` (30 s) bounds the whole command while server-side convergence may
   legitimately take up to 2 min per service — documented but a persistent foot-gun
   (docs/piquelctl.md:24-27).
9. Defensive client vocabulary accepts `completed`/`canceled` spellings the server never
   emits (support.rs:216-221 vs store/mod.rs:240-249) — harmless drift, signals vocabulary
   isn't shared via types.

---

## 10. Refactor directions

Ordered by expected bug-class elimination per unit of disruption. Each item names the races
it retires.

### 10.1 Make the database own all policy (retires R1, R2-partially, R12, R17)

The strongest move available: the schema already guarantees the hard invariants. Push the
remaining soft ones into single-statement or single-transaction primitives so callers stop
doing read-then-write dances:

- **Replace read-status-then-CAS-status with one conditional UPDATE** that encodes the
  decision in SQL. E.g. a single `complete_operation_successfully(op)` statement that sets
  `ready` only if `applications.generation = op.generation` and otherwise reports
  `Superseded` as a *distinct outcome* rather than `IllegalTransition` → `journal_unavailable`.
  The handler should never need to read status at all: `start_deployment`, `mark_ready`,
  and `record_operation_failure` become intent statements ("try to advance; tell me if the
  world moved on"), with `Superseded` treated as a normal, non-error result.
- **Stop conflating CAS misses with journal failure**: introduce distinct outcomes
  (`AlreadyClaimed`, `SupersededDuringExecution`, `StatusMovedOn`) instead of collapsing
  every `StoreError` into `JournalUnavailable` (handler.rs:86-90). Most of §8's
  "spurious failure" bugs are this one mapping.
- Give `transition_operation` a generation-aware variant so the scheduler's claim and the
  handler's currency checks are the same mechanism, not two.

### 10.2 Unify the two mutation disciplines (retires R14, R7-partially)

Either (a) drop the API `mutation_lock` and rely solely on store idempotency/CAS — paying
duplicate registry resolution only for genuinely concurrent same-key retries, which the
idempotency machinery already arbitrates — or (b) keep the lock but shrink its critical
section to exclude `prepare()` I/O (resolve images outside the lock, re-validate spec_hash
at commit). Option (b) preserves the stated goal (no duplicated registry work) while
removing head-of-line blocking. Either way, document one discipline instead of two.

### 10.3 Replace batch-waves with slot-based dispatch (retires R7, R8-partially)

Swap `snapshot → JoinSet → join-all → repeat` for a long-lived dispatcher: maintain ≤N
in-flight tasks, refill one slot at a time from `pending_operations(1..k)` as tasks finish.
Ordering is already durable in SQL (oldest-generation-per-app filter), so the wave barrier
buys nothing except latency coupling. Add an operation-level deadline (sum of per-step
budgets) so permits can't be held indefinitely.

### 10.4 Make cancellation deterministic (retires R3, R4)

Pick one owner of cancellation truth: the simplest is to have the handler be the only party
that consults the token during execution and *always* map cancellation to `Recovery`
(resumable), reserving terminal `Cancelled` exclusively for supersession. Additionally:
abort the in-flight Docker driver task on drop (hold the `JoinHandle`/`AbortHandle` in a
guard), or route all Docker calls through a cancellable request-scoped context so
"cancellation" has a single meaning end-to-end.

### 10.5 Normalize the status vocabulary for clients (retires §9 items)

Decide what each public field means and make writers conform:
- either hide `recovery` behind `running` in views, or document it as pollable;
- stop resetting status at accept time (let the op carry "queued/deploying" progress) or
  expose an explicit `target_generation` so clients can distinguish regressions;
- require every status change to reference an operation (kills the anonymous
  scan-induced `degraded`);
- define `observed_generation` as "last converged" and stamp `attempted_generation`
  separately on failure;
- forbid terminal-state resurrection (new operation ID for delete retries; keep the old ID
  immutable).

### 10.6 Structural cleanups

- Delete the per-app mutex map once claims are fully DB-mediated (#10.1); it is redundant
  with the sibling-running guard and leaks memory (R10).
- Guard `reset_failed_delete`'s step swap with the parent-state predicate in SQL, or switch
  to append-only re-planning (never DELETE steps) (R6).
- Snapshot atomicity: either accept torn observations explicitly (document + jitter polls)
  or fetch services+tasks in one filtered pass where possible (R11).
- Replace the string-matched Docker retry trigger with version-conflict detection based on
  inspect-refresh semantics rather than error text (R15).
- Add retention for finished operations/status/bindings, respecting queue-query semantics
  (R18).
- Consider deferring listener bind until after startup recovery completes (trivial fix for
  R16), and mapping index-violations distinctly (R17).

### 10.7 Testing gaps worth closing

- No test currently exercises R1-style interleavings (currency-check-passes-then-bump);
  a deterministic race harness (tokio pause/time control or fault-injecting store wrapper)
  would lock in the fixed behavior.
- Attempt counters don't reflect Docker retries; if operator-facing retry metrics matter,
  journal per-action attempts.
- The `update out of sequence` test pins the fragile string (engine.rs:274-296) — keep, but
  add a negative test for reworded bodies to force conscious handling.

---

## Appendix A — Verified defaults

| Knob | Value | Source |
| --- | --- | --- |
| Scan interval | 60 s, missed ticks skipped | config/mod.rs:173, coordinator.rs:36-37 |
| Max parallel operations | 4 | config/mod.rs:174, main.rs wiring |
| Dispatch page size | 100 (`MAX_PAGE_SIZE`) | operations.rs:245, store/mod.rs:372 |
| Retry attempts / backoff | 4, 100 ms → 2 s cap (per action) | reconcile/mod.rs:100-102 |
| Convergence timeout | 2 min (per step, polling @250 ms) | reconcile/mod.rs:103, actions.rs:107-130 |
| Prepare timeout / image timeout | 5 min / 30 s (4-way concurrent) | reconcile/runtime.rs:10-11, 69 |
| Docker request timeout | 30 s (fresh connection per request) | docker/engine.rs:9 |
| SQLite | WAL, busy_timeout 5 s, pool ≤ 8 | store/mod.rs:474-487 |
| CLI poll / default timeout | 250 ms / 30 s whole-command | piquelctl support.rs:21, cli.rs:21 |

## Appendix B — Behavioral evidence index

| Behavior | Test |
| --- | --- |
| Create replay returns identical `operation_id`; status `pending` with no executor attached | tests/api_contract.rs:125-166 |
| Replace bumps generation; plan preview reports `proposed_generation` | tests/api_contract.rs:169-197 |
| Idempotent replays value-equal; stale replace → `GenerationConflict{expected,actual}` | tests/persistence.rs:145-238 |
| Active delete reuse (`pending|running|recovery`); failed/cancelled reset with fresh steps | tests/persistence.rs:277-352 |
| Interrupted ops recover to `Succeeded`/`Ready` via `recover_and_run`; drift repair at same generation | tests/fake_docker.rs:277-326, 424-477 |
| Matching reconcile performs no Docker update | tests/docker_integration.rs:160-169 |
| `finalize_delete` refuses before op success | tests/persistence.rs:119-133 |
