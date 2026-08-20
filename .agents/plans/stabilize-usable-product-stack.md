# Stabilize the usable-product stack

## Goal

Make Plan 06C a genuinely usable, independently qualified product while keeping
PRs #7–14 as future feature increments. Correct the existing stack in place: amend
Plans 06A–06C, then semantically restack the existing future PRs without creating a
new product layer after 06C.

The completed Plan 06C product must let a new operator install or build piqueld,
start the daemon with understandable configuration, deploy a prebuilt-image
application with `piquelctl`, and inspect it in the read-only dashboard. Every
manifest feature advertised at that point, including health checks, must work
against a real disposable Docker Swarm.

## Existing stack

| PR | Branch | Responsibility |
| --- | --- | --- |
| #19 | `plan/06a-simplification` | Small, honest prebuilt-image runtime |
| #20 | `plan/06b-cli` | Essential operator CLI |
| #21 | `plan/06c-basic-web-ui` | Basic read-only dashboard and production assets |
| #7–#14 | Existing `plan/07-*` through `plan/14-*` branches | Future features |

Preserve these PRs, their numbers, and their review history. Do not introduce a
Plan 06D that becomes necessary for usability: the fixes belong in #19–#21.

## Non-goals

- Adding secrets, Git builds, ingress, logs, transfer, authentication, or advanced
  web mutations to Plan 06C.
- Replacing the Leptos/WASM dashboard, typed client, SQLite store, or Docker runtime
  with new frameworks.
- Restoring scaffolding removed by Plan 06A.
- Redesigning all future features while moving their ownership to the correct PR.
- Supporting upgrades from any unreleased migration schema.

## Phase 0 — Freeze and protect the current stack

1. Fetch current GitHub refs and require a clean checkout before changing branches.
2. Record exact local and remote heads, merge bases, PR bases, PR descriptions,
   review state, and CI state for #19–#21 and #7–#14.
3. Create a second clearly dated set of local backup refs for every branch before
   this stabilization rewrite. Retain the existing original-stack backups too.
4. Record the current passing #14 CI run and its job results as evidence, not as
   evidence that Plan 06C independently contains the fixes found there.
5. Create a correction ledger mapping every change below to its owning PR and its
   old source commit where applicable.
6. Serialize branch changes in the shared checkout. Recheck status and remote lease
   state before every checkout, rebase, reset, or push. Do not create worktrees.

## Phase 1 — Amend PR #19 / Plan 06A

### Move base Docker corrections down from PR #14

Semantically port the following behavior into #19 without importing future secret,
build, route, log, or qualification code:

1. From `7725ee6`, retry only Docker's exact transient
   `update out of sequence` service-update response. Refresh the current service
   version between attempts and keep the retry bounded.
2. From `e6e8f11`, inspect the complete service representation before semantic
   comparison and ordinary observation. Do not copy secret-specific matching into
   Plan 06A.
3. From `f4f9128`, normalize Docker Swarm's `HealthCheck`/`Healthcheck` key at the
   narrow service wire boundary for create, update, and inspect. Preserve typed
   desired/observed models and sanitized errors.
4. Keep the focused regression tests with the owning code. PR #14 may retain
   qualification scenarios, but it must no longer be the first branch where the
   base behavior is fixed.

### Finish bootstrap and simplification work owned by the daemon

1. Remove the unused `DaemonConfig::data_dir`. Keep `database.path` as the one
   authoritative state location.
2. Ensure the parent of a configured database path can be created safely on a clean
   installation. Do not chmod existing parent directories or follow an unsafe path
   replacement. Preserve detailed startup context.
3. Keep the Unix-socket parent creation and restrictive socket mode. Consolidate
   directory preparation where that reduces duplication without creating a generic
   filesystem framework.
4. Remove `migrations/.gitkeep` now that real migrations exist.
5. Update configuration, migration, architecture, and reconciliation documentation
   for these changes.

### Plan 06A Docker acceptance

Extend the isolated Docker lifecycle test so it covers:

- service creation with a command health check and an HTTP health check where
  practical;
- a repeated matching reconciliation that performs no service update;
- replica or specification update, including the bounded transient retry path;
- complete observation of the health check after create/update;
- owned drift repair and foreign-resource refusal;
- daemon/reconnection recovery;
- deletion ordering and named-volume retention.

The test must continue to refuse the host daemon unless it receives the explicit
isolated-engine attestation.

## Phase 2 — Amend PR #20 / Plan 06B

### Make daemon startup understandable

1. Add a small real daemon command-line surface with `--help`, `--version`, and
   `--config <path>`. Reuse `clap`, which #20 already introduces for `piquelctl`.
2. An explicitly selected missing or invalid configuration file is an error. Choose
   and document one simple behavior for a missing default configuration: either use
   validated defaults or emit a short error that points directly to the shipped
   example and `--config`. Do not silently mix partial configuration sources.
3. Add a complete minimal example configuration with non-root development paths as
   well as documented production defaults.
4. Add a quickstart that goes from build/package to daemon startup, `piquelctl
   status`, plan, apply, show, dashboard, and delete. Include Docker prerequisites
   and the named-volume retention behavior.

### Complete the status contract

1. Add `daemon_version` to `SystemStatus`, sourced from the daemon package version.
2. Display it in `piquelctl status` and its JSON output.
3. Keep API version and instance identity. Do not add a dynamic capability registry;
   Plan 06C's static supported/deferred table remains authoritative.
4. Update OpenAPI, native/WASM DTOs, tests, CLI documentation, and snapshots
   together.

## Phase 3 — Amend PR #21 / Plan 06C

### Make the package run the dashboard it contains

1. Keep the production Leptos bundle in the Nix package.
2. Make a normally started packaged daemon find its packaged dashboard assets
   without requiring the operator to discover and copy a Nix store path. Use the
   smallest transparent mechanism that still permits an explicit `server.ui_dir`
   override.
3. Install only operator-facing binaries. Do not install the OpenAPI generator or a
   native `piqueld-ui` placeholder executable in the normal package output.
4. Keep static fallback from shadowing API, health, and OpenAPI routes.

### Finish the basic dashboard contract

1. Display the daemon version from the corrected status DTO.
2. Preserve the current read-only scope, same-origin transport, single-flight
   polling, hidden-tab pause, stale-data behavior, accessibility, and no-storage/no-
   telemetry properties.
3. If the 20-page application safety bound truncates a response, show that the list
   is incomplete rather than silently presenting it as complete. Detect a repeated
   cursor as an error.
4. Add or run a targeted browser smoke against this exact Plan 06C branch at desktop
   and narrow width. Cover initial load, empty state, application selection/detail,
   refresh, stale-after-error, unreachable/recovery, keyboard flow, and the absence
   of mutation controls.

## Plan 06C standalone acceptance gate

Before touching PR #7, validate the exact #21 head independently:

1. `just` passes and leaves tracked files unchanged.
2. `just ui-check` and strict WASM Clippy pass.
3. The production Nix package builds and contains only intended binaries plus the
   fingerprinted dashboard assets.
4. `piqueld --help`, `piqueld --version`, explicit configuration, missing-default
   behavior, and clean state-directory creation behave as documented.
5. The documented quickstart works from a clean temporary state directory.
6. Essential CLI tests pass over Unix and loopback TCP, including plan-before-apply,
   expected generation, confirmation, idempotent retry, polling, interruption,
   JSON stdout cleanliness, delete, and volume notice.
7. The targeted Plan 06C browser smoke passes.
8. The isolated Docker lifecycle passes with health checks and repeated idempotent
   reconciliation.
9. Source, schema, API, client, CLI, UI, package, and documentation contain no
   future feature implementation.

Record exact commands, environment, Docker version, package contents, and browser
evidence in the correction ledger and PR descriptions.

## Phase 4 — Restore progressive future-feature ownership

Restack each existing future PR on the corrected predecessor. Keep each feature
usable at the increment where it first appears.

### PR #7 — Secrets

1. Rebase the complete existing secret lifecycle onto corrected Plan 06C.
2. Add the minimal safe CLI workflow in #7, not #11: list metadata, set/replace from
   stdin or a protected file, and delete with appropriate confirmation/concurrency.
3. Never accept plaintext as a command argument or expose it in logs, errors,
   serialization, process inspection, browser state, or durable diagnostics.
4. Preserve all lookup, recovery, deployed-state barrier, pruning, and pagination
   fixes.

### PR #8 — Git build and registry

1. Rebase onto the actual corrected #7 head.
2. Keep the one-owner durable build pipeline and fresh build migration.
3. Add the smallest CLI visibility needed to operate the feature in #8: expose the
   build associated with an application/operation and allow its bounded status and
   diagnostics to be inspected. Do not add the full advanced CLI yet.
4. Preserve the later BuildKit session/provider correction from current #14 in #8,
   because it is required for builds to work at the branch where builds appear.

### PR #9 — Ingress, status, and logs

1. Rebase onto corrected #8 and preserve the single shared stream cursor/reconnect
   design.
2. Keep the existing runtime status integration.
3. Add a minimal bounded historical logs command in #9. Advanced follow/watch,
   profile, and stable automation behavior may remain in #11.
4. Keep route/Traefik/log behavior absent from all lower branches.

### PR #10 — State transfer

Keep the existing transfer CLI with the owning feature. Rebase it onto corrected #9
and preserve bounded binary handling, digest-bound confirmation, transactionality,
and dependency reporting.

### PR #11 — Advanced CLI

Extend the corrected essential and feature-owned commands. Remove duplicate secret,
build, log, and transfer implementations. Add only advanced profiles/authentication,
stable output/exits, watch/follow behavior, and other accepted Plan 11 capabilities.

### PRs #12–#14

1. Keep #12 as an expansion of the corrected basic dashboard.
2. Keep #13 as security, operations, NixOS, and split production packaging.
3. Keep #14 as qualification, CI, release, and final documentation.
4. Leave cumulative acceptance tests in #14, but ensure fixes discovered by those
   tests live in the earliest feature-owning PR: base service behavior in #19,
   BuildKit behavior in #8, secret behavior in #7, and so on.

## Phase 5 — Correct records and PR claims

1. Update `.agents/rebase-07-14-ledger.md` with the actual corrected Plan 06C base,
   published branch heads, both generations of backup refs, and current PR bases.
2. Remove stale statements that the frozen base is `e6df882`, that rebuild heads are
   the unpublished `rebuild/*` heads, or that GitHub still blocks PR #7's base.
3. Update #7–#9 descriptions so CLI claims match their final incremental code.
4. Update #14's description and ledger so it describes base/feature corrections as
   qualified inherited behavior rather than behavior first fixed by #14.
5. Preserve old review intent and note where code moved.

## Phase 6 — Validate and publish the corrected stack

1. Build and validate every corrected branch locally before rewriting any remote.
2. Review per-PR range-diffs and a capability matrix against both the pre-
   stabilization stack and the original old-head backups.
3. Update #19, then #20, then #21 with `--force-with-lease`. Verify each PR base and
   head on GitHub before proceeding.
4. Update #7–#14 from the bottom upward, again using only `--force-with-lease`.
5. Stop and reinventory on any lease failure or concurrent remote update.
6. Run or confirm CI at each available gate. The final #14 CI must pass Rust/contracts,
   production WASM/browser, disposable Docker, and Nix/NixOS jobs.
7. Do not delete either generation of backup refs.

## Done when

- The exact Plan 06C head is runnable and useful without any commit from #7–#14.
- Every advertised Plan 06C manifest field works in disposable Docker, including
  health checks.
- A new operator can follow one documented path from package to apply/dashboard/
  delete without guessing configuration or asset paths.
- #7–#10 add usable future features progressively; #11 and #12 deepen the existing
  CLI and UI rather than making earlier features usable for the first time.
- Core fixes no longer live only in qualification.
- The ledger and PR descriptions describe the actual published stack.
- All remote branches retain their existing PRs, all leases were respected, and all
  backup refs remain available.

## Final handoff

Report:

- old, pre-stabilization, and corrected head mappings;
- all backup refs;
- per-PR moved behavior and commits;
- Plan 06C quickstart and package contents;
- standalone Plan 06C validation evidence;
- per-PR range-diff/capability results;
- final GitHub PR bases, heads, and CI jobs;
- unresolved review comments, unavailable checks, or deliberate deviations.
