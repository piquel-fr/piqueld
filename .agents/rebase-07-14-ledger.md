# PR 07–14 semantic rebuild ledger

This is the contemporaneous migration record for rebuilding the existing PR
stack on the completed Plan 06C product stack. It is intentionally kept with
the repository so the old branch heads, review intent, and capability mapping
remain available after the remote branches are rewritten.

Inventory date: 2026-08-19 (Europe/Paris)
Repository: `piquel-fr/piqueld`
Checkout: `/home/piquel/Projects/piqueld`

## Frozen base

The exact Plan 06C base is:

```text
e6df8821d5a63b9b121dec6781602348879ddea2
```

Backup ref:
`refs/backup/rebase-07-14/base/plan-06c-basic-web-ui-20260819-e6df882`

There was no remote tracking ref for `plan/06c-basic-web-ui` at inventory time.
The rebuild therefore starts from this exact local commit. Publishing a base
branch is a separate decision and is not implied by rewriting the PR heads.

## Old head and ancestry inventory

At the initial fetch, every old PR head below matched its remote branch. The
merge-base column is against the immediately recorded old predecessor (and for
#7, against the old Plan 06 base). PR #8 is the important ancestry warning: its
recorded base was `f8b6abe…`, not the then-current #7 head `d5cb30a…`.

| PR | Branch | Old local/remote head | Recorded base | Merge base used for old increment | Old commits/files | State |
|---:|---|---|---|---|---:|---|
| 7 | `plan/07-secret-lifecycle` | `d5cb30ad292e94bca29c857ee88756710c6b1d29` | `plan/06-docker-reconciliation` (`715170bfef8467fe8f8376861b7705e08b8d7f3e`) | `715170bfef8467fe8f8376861b7705e08b8d7f3e` | 11 / 37 | open draft |
| 8 | `plan/08-build-and-registry` | `1a5bfdfe7a6dcae454c2606b9933a09187818d3a` | #7 at `f8b6abeacbb4a3e3c638f0f5c29c52dc83939711` | `f8b6abeacbb4a3e3c638f0f5c29c52dc83939711` | 5 / 54 | open draft |
| 9 | `plan/09-traefik-status-and-logs` | `09726980002f5f50b885d3ecfd2340b4f6b0ab3a` | #8 at `1a5bfdfe7a6dcae454c2606b9933a09187818d3a` | `1a5bfdfe7a6dcae454c2606b9933a09187818d3a` | 5 / 25 | open draft |
| 10 | `plan/10-import-export` | `311881d528d5621ef2597ce203c79848915e483c` | #9 at `09726980002f5f50b885d3ecfd2340b4f6b0ab3a` | `09726980002f5f50b885d3ecfd2340b4f6b0ab3a` | 5 / 41 | open draft |
| 11 | `plan/11-cli` | `99e85d694e0f35e3418f35dd27984957dc4425ba` | #10 at `311881d528d5621ef2597ce203c79848915e483c` | `311881d528d5621ef2597ce203c79848915e483c` | 5 / 20 | open draft |
| 12 | `plan/12-web-ui` | `3d3e46e9422794877a9eec6d3ad8a33af811d4f6` | #11 at `99e85d694e0f35e3418f35dd27984957dc4425ba` | `99e85d694e0f35e3418f35dd27984957dc4425ba` | 5 / 22 | open draft |
| 13 | `plan/13-nixos-security-and-operations` | `3f01d57de6be6c3b1ab8ef682b9edc1631b7e14f` | #12 at `3d3e46e9422794877a9eec6d3ad8a33af811d4f6` | `3d3e46e9422794877a9eec6d3ad8a33af811d4f6` | 5 / 24 | open draft |
| 14 | `plan/14-qualification-ci-and-docs` | `b129bafd102cf2fb461ed776f3808f110a451b5e` | #13 at `3f01d57de6be6c3b1ab8ef682b9edc1631b7e14f` | `3f01d57de6be6c3b1ab8ef682b9edc1631b7e14f` | 8 / 94 | open draft |

## Immutable old-head backups

These refs are never deleted by this operation:

```text
refs/backup/rebase-07-14/base/plan-06c-basic-web-ui-20260819-e6df882 -> e6df8821d5a63b9b121dec6781602348879ddea2
refs/backup/rebase-07-14/old/pr-07-secret-lifecycle-20260819-d5cb30a -> d5cb30ad292e94bca29c857ee88756710c6b1d29
refs/backup/rebase-07-14/old/pr-08-build-and-registry-20260819-1a5bfdf -> 1a5bfdfe7a6dcae454c2606b9933a09187818d3a
refs/backup/rebase-07-14/old/pr-09-traefik-status-and-logs-20260819-0972698 -> 09726980002f5f50b885d3ecfd2340b4f6b0ab3a
refs/backup/rebase-07-14/old/pr-10-import-export-20260819-311881d -> 311881d528d5621ef2597ce203c79848915e483c
refs/backup/rebase-07-14/old/pr-11-cli-20260819-99e85d6 -> 99e85d694e0f35e3418f35dd27984957dc4425ba
refs/backup/rebase-07-14/old/pr-12-web-ui-20260819-3d3e46e -> 3d3e46e9422794877a9eec6d3ad8a33af811d4f6
refs/backup/rebase-07-14/old/pr-13-nixos-security-and-operations-20260819-3f01d57 -> 3f01d57de6be6c3b1ab8ef682b9edc1631b7e14f
refs/backup/rebase-07-14/old/pr-14-qualification-ci-and-docs-20260819-b129ba -> b129bafd102cf2fb461ed776f3808f110a451b5e
```

## Captured PR descriptions and review intent

The following bodies were captured from GitHub before any remote rewrite.

### #7 — Plan 07: secret lifecycle

> Implements Plan 07 secret lifecycle as a stacked change on Plan 06.
>
> Includes encrypted-at-rest storage, protected key/file handling, metadata-only APIs, Docker secret reconciliation, rotation and historical generation handling, and two independent review passes.
>
> Validation: workspace tests and clippy, docs, no-default-features, cargo-deny, dependency boundaries, OpenAPI snapshot, and nix flake check. The privileged Docker integration test remains intentionally ignored because it requires an isolated engine.

Review intent: one CodeRabbit review-skipped comment only; it says auto reviews
are disabled for non-default base branches. No actionable review thread.

### #8 — Plan 08: build and registry pipeline

> Implements Plan 08 as a stacked change on Plan 07.
>
> Includes reproducible HTTPS Git resolution, hardened deterministic build contexts, scheduler-owned durable BuildKit execution, verified OCI registry digests, exact cache identity, bounded redacted logs/SSE, restart recovery, and digest-only deployment. Two independent review/fix passes hardened credential scope, context TOCTOU, OCI verification, and redesigned the real path so build rows exist before external I/O and resolved state is published atomically only from verified artifacts.
>
> Validation: workspace all-feature tests, no-default feature check, strict clippy, docs, cargo-deny, OpenAPI snapshot, and nix flake check. Two privileged isolated Docker/registry/Swarm tests compile but remain gated because they require a disposable engine and registry.

No review comments or requested changes.

### #9 — Plan 09: Traefik, status, and logs

> Implements Plan 09 as a stacked change on Plan 08.
>
> Includes owned digest-pinned Traefik/ingress infrastructure, secure route generation and global host collision checks, readiness-aware runtime status, bounded historical and SSE log streaming, typed-client/OpenAPI support, and cloudflared topology documentation. Two review passes hardened exact ownership/drift, IDNA/internal-host handling, task diagnostics, socket/manager placement, concurrency races, cursor/backpressure behavior, and client bounds.
>
> Validation: full workspace tests, strict clippy, all-feature Docker integration compilation, docs/OpenAPI, and exact final-tree nix flake check. Privileged ingress/scale/update/log qualification remains gated behind an isolated disposable Docker engine.

No review comments or requested changes.

### #10 — Plan 10: state import and export

> Implements Plan 10 as a stacked change on Plan 09.
>
> Includes canonical application export, deterministic versioned checksummed state archives, portable/encrypted modes, strict bounded pre-mutation validation, expiring digest-bound replacement confirmation, maintenance gating, transactional replacement/audit, instance-safe ownership semantics, dependency reporting, API/OpenAPI/client support, and documentation. Two review passes hardened raw ustar parsing, envelope authentication, confirmation concurrency, FK ordering, schema/deletion safety, restore reconciliation bootstrap, and disclosure controls.
>
> Validation: all-feature and no-default workspace tests, strict clippy, cargo-deny, focused archive/encryption/audit/fault tests, and exact final-tree nix flake check. Shared Docker was not touched.

No review comments or requested changes.

### #11 — Plan 11: piquelctl operator CLI

> Implements Plan 11 as a stacked change on Plan 10.
>
> Includes the complete public-client-only piquelctl command surface, Unix/HTTP profiles and protected bearer tokens, safe secret inputs, generation-aware mutations, human/stable JSON output, SSE watch and polling fallback, binary archive safeguards, stable exit codes, and operator docs. Two review passes hardened profile/credential precedence, protected-file TOCTOU, plaintext/body zeroization, reflected errors, retry/timeout behavior, atomic outputs, and command-by-command contracts.
>
> Validation: every command exercised over TCP and Unix in black-box tests, stable JSON/human/exits/canaries/Ctrl-C coverage, all-feature and no-default workspace tests, strict clippy, dependency-boundary confirmation, and exact final-tree nix flake check. Three isolated-Docker tests remain intentionally ignored.

No review comments or requested changes.

### #12 — Plan 12: structured Leptos web UI

> Implements Plan 12 as a stacked change on Plan 11.
>
> Includes a pure Rust/Leptos CSR administrative UI with full structured manifest forms, plan/apply/delete previews, conflict recovery, live operation/build/runtime streams, write-only secret controls, state transfer, accessible responsive states, hardened daemon SPA serving, and fingerprinted production UI assets in the Nix package. Two review passes added the public delete-plan contract, corrected typed plans/form roundtrips, hardened CSP/assets/SSE/conflicts/secret memory, and expanded accessibility/security contracts.
>
> Validation: full workspace tests, native and wasm32 strict clippy, production Trunk release with fingerprinted asset/canary scan, and full nix flake/package checks. Real browser automation could not run because both product-native preview status/open returned an external Auth-required transport error; no browser evidence is claimed, and the documented desktop/390px interactive matrix remains for an authenticated preview environment.

No review comments or requested changes.

### #13 — Plan 13: NixOS security and operations

The captured body states that it adds production authentication, listener limits,
health/readiness, metrics, distinct daemon/CLI/UI packaging, a hardened NixOS
module with systemd credentials, safe Swarm/registry wiring, durable recovery,
and operator/security/recovery/NixOS documentation. It records full Cargo and
NixOS VM validation, with three privileged Docker tests isolated, and says no
host Docker daemon was accessed.

No review comments or requested changes.

### #14 — Plan 14: Qualification, CI, and prototype handoff

The captured body states that it adds layered Rust/WASM/Chromium/NixOS/Docker CI,
reproducible release packaging and checksums, expanded Docker acceptance tests,
fixtures and a 20-criterion runbook, and final operator/architecture/security/
release/troubleshooting/contributor documentation. It explicitly leaves external
Cloudflare, real operator HTTPS Git, full interactive browser, privileged runtime
CI, and operator-recorded failure-injection evidence unsigned until those real
environments execute them.

No review comments or requested changes.

## Captured old CI state

Workflow-run queries were against the exact old heads above.

| PR | Workflow runs | Commit statuses |
|---:|---|---|
| 7 | none | CodeRabbit success |
| 8 | none | none |
| 9 | none | none |
| 10 | none | none |
| 11 | none | none |
| 12 | none | none |
| 13 | none | none |
| 14 | run `31516093384` failed; run `31516092805` cancelled | none |

## Capability migration ledger

Old merge commits and obsolete conflict-resolution merges are intentionally not
ported. The feature/fix commits map as follows:

| Old capability or follow-up | Old evidence | Rebuilt target and treatment |
|---|---|---|
| Encrypted logical secrets, key permissions, metadata-only API/client, Docker secret delivery and rotation | #7 `cbb092f`, `cdded1f` | #7 rebuilt as `48a298c`; current schema/API/client/CLI paths only |
| Removed abandoned SQLx/scaffolding from the secret transplant | #7 `0183cd3` | #7 preserved Plan 06A simplification; no old scaffolding restored |
| Secret generation lookup correction | #7 `1115384` | #7 rebuilt lookup against current store |
| Restart/reconcile secret recovery | #7 `ed49a1d` | #7 rebuilt recovery scan and durable replacement |
| Deployed-state barrier before secret pruning/rotation | #7 `4a4df25` | #7 rebuilt deployed snapshot and barrier |
| Retired-generation pruning | #7 `7d5eb1c` | #7 rebuilt post-deploy pruning |
| Secret metadata pagination | #7 `d5cb30a` | #7 rebuilt bounded cursor pagination |
| Git resolution, deterministic context, BuildKit, registry digest verification, build persistence | #8 `50839c6`, `f0eae59` | #8 rebuilt from latest #7 as `0321107`; fresh migration 0003 and one build-concurrency owner |
| Build credential scope, context TOCTOU, atomic verified-artifact publication and recovery | #8 `f0eae59` | #8 rebuilt durable build service/planner/action and verified-only deployment |
| Routes/ports, owned Traefik, runtime status, bounded logs and events | #9 `6c72270`, `1316fa1` | #9 rebuilt as `f79994b`; shared cursor/reconnection SSE design |
| Canonical app export, deterministic portable/encrypted archives, strict validation, digest confirmation, maintenance gate, transactional import/audit and dependencies | #10 `1ec606a`, `4125d7d` | #10 rebuilt from `f79994b` as `4f92d0b072a04a081c0b706c5588e59c66633edd`; fresh migration 0004, API/client/CLI/docs/tests together |
| Complete advanced CLI surface | #11 `b45b426`, `ab1b68e` | #11 rebuilt from actual #10 as `6916091076de717d21860713d54d44701b63c90a`; grouped application/secret/operation/state commands extend Plan 06B and preserve #10 transfer commands |
| Operation watch endpoint required by the rebuilt CLI | #11 `b45b426` (old #9 did not carry the operation-events route) | Moved into #11 on top of #9's shared cursor/reconnection primitive; server OpenAPI, transport-neutral client, Last-Event-ID handling, deduplication, stream fallback, and focused API coverage move together |
| Protected profile/token/secret inputs, stable output and exit categories, binary output safety, retry/timeout/error redaction | #11 `ab1b68e` | Rebuilt against the current public client; uses protected `openat2` reads, zeroized token/body buffers, atomic private outputs, profile precedence, stable envelopes, and no plaintext CLI arguments |
| Dependency license policy needed by the existing rustls/gix graph | cumulative #8 dependency graph; discovered during #11 full validation | Added only `ISC`, `MIT-0`, and `CDLA-Permissive-2.0` to `deny.toml`; `cargo deny check` and full `just` now pass, with duplicate-version warnings retained |
| Structured UI and packaged assets | #12 `24a36dc`, `d845f04` | #12 rebuilt as `0e71c2d`; extended the Plan 06C dashboard with typed structured forms, reviewed mutations, observability, write-only secrets, state transfer, and hardened same-origin asset serving; did not restore the old dashboard/state tree; follow-up clarified action copy and removed nested main/duplicate footer markup |
| Public delete-plan and generation overflow guard | #12 follow-up review intent; current old branch did not have a usable current-stack contract | #12 added `POST /api/v1/applications/{id}/delete-plan`, typed client support, checked generation increment, OpenAPI, and API contract coverage |
| Conflict-preserving editor and user-visible validation | #12 `24a36dc` | #12 retains local manifests across optimistic-concurrency conflicts, offers explicit server reload, maps safe field errors, and invalidates stale previews before apply |
| Live operation/build/runtime views | #12 `24a36dc` | #12 uses the rebuilt operation SSE cursor/reconnect contract, bounded `LogBuffer`, typed operation/build/status/log DTOs, browser EventSource cleanup, bounded reconnect attempts, and typed polling fallback; build output intentionally polls because no build SSE route exists in the rebuilt API |
| Write-only browser secret controls | #12 `24a36dc` | #12 uses wasm typed binary secret methods, clears input/drafts after success and component cleanup, zeroizes Rust and JS body buffers, and never reveals or persists plaintext |
| Browser state transfer and application export | #12 `24a36dc` | #12 uses wasm typed transfer methods, 32 MiB bound, explicit phrase, SHA-256 digest-bound single-use confirmation, zeroized confirmation/archive buffers, dependency report, and in-memory downloads |
| Hardened SPA fallback and packaged UI documentation | #12 `d845f04` | #12 moved descriptor-relative symlink-safe serving into `api/ui.rs`, added CSP/security headers, HEAD/range/content-type/cache behavior, 16 MiB asset bound, and updated API/UI docs; Nix/Trunk packaging already existed in the frozen Plan 06C base |
| NixOS/security/operations and recovery | #13 `3254200`, `6a7d09c` | #13 pending; adapt to rebuilt binary/assets |
| Qualification CI, release, acceptance, docs and Rust compatibility fixes | #14 `22bbb1a`, `5d38829`, `a2990f8`, `38cf58d`, `a1d0f36` | #14 pending; consolidate against cumulative rebuilt stack |

## Rebuilt heads so far

| Rebuilt increment | Local branch | Head |
|---:|---|---|
| 7 | `rebuild/07-secret-lifecycle` | `48a298c` (`feat(secrets): rebuild logical secret lifecycle on product stack`) |
| 8 | `rebuild/08-build-and-registry` | `0321107` (`feat(build): rebuild Git source pipeline on product stack`) |
| 9 | `rebuild/09-traefik-status-and-logs` | `f79994b` (`feat(ingress): rebuild status and logs on product stack`) |
| 10 | `rebuild/10-import-export` | `4f92d0b072a04a081c0b706c5588e59c66633edd` (`feat(transfer): rebuild state import and export on product stack`) |
| 11 | `rebuild/11-cli` | `6916091076de717d21860713d54d44701b63c90a` (`feat(cli): rebuild advanced operator workflow on product stack`) |
| 12 | `rebuild/12-web-ui` | `0e71c2d` (`fix(web): clarify dashboard structure and actions`; feature base `ae45d9e3d4c7c5f704cf3978a77182639ceffad9`) |

## Local #10 validation at ledger checkpoint

Passed:

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo check --workspace --all-targets`
- `cargo test --workspace --no-run`
- focused transfer unit tests (4)
- in-process transfer API contract (binary content type, app export, exact digest confirmation, single-use replay rejection)
- fake Docker tests (2)
- persistence tests (3)
- SQLx fresh migration test
- generated OpenAPI `--check`
- `git diff --check`

Listener-based TCP/Unix tests need an elevated execution environment because the
managed sandbox denies local socket binds; they are not treated as product
failures. Full `just`, no-default-feature, cargo-deny, Nix, WASM/Trunk, browser,
and privileged Docker validation remain cumulative gates before remote rewrites.

## Local #11 validation at ledger checkpoint

Passed on `6916091076de717d21860713d54d44701b63c90a`:

- `just` (full workspace check, tests, doc-tests, cargo-deny, OpenAPI check, and dependency boundary script)
- strict workspace Clippy and formatting checks
- black-box CLI integration over Unix and TCP with stable JSON, auth canaries, grouped commands, secret input, state transfer safeguards, and refusal paths
- operation SSE API/client contract, including typed event decoding
- workspace API, persistence, migration, fake-runtime, manifest, and UI tests

The privileged Swarm/Docker acceptance test remains ignored by design. WASM,
Trunk release, NixOS VM, browser, and privileged Docker validation remain
cumulative gates for their owning rebuilt PRs.

## Local #12 validation at ledger checkpoint

Passed on `0e71c2d` (feature base `ae45d9e3d4c7c5f704cf3978a77182639ceffad9`):

- `just` (workspace format, strict native Clippy, workspace checks/tests,
  doc-tests, cargo-deny, generated OpenAPI check, and dependency boundaries)
- elevated TCP/Unix API contract coverage, including delete-plan, operation
  SSE, state transfer, SPA route precedence, CSP/security headers, byte ranges,
  and encoded-path rejection
- `cargo check --target wasm32-unknown-unknown -p piqueld-client -p piqueld-ui`
- `cargo clippy --target wasm32-unknown-unknown -p piqueld-client -p piqueld-ui
  --lib -- -D warnings`
- UI state unit tests and strict dependency-boundary checks

Unavailable in this checkout: `trunk` is not installed, so `just ui-build`
could not run; no release asset canary or Nix package/VM evidence is claimed.
The T3 collaborative preview reported no automation host after both status and
open attempts, so no browser interaction evidence is claimed. The privileged
Docker acceptance test remains intentionally ignored.
