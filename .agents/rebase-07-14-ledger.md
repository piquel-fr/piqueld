# Usable-product stack stabilization ledger

This is the contemporaneous record for the Plan 06A/06B/06C stabilization and
the semantic rebuild of PRs #7–#14. It records the exact pre-rewrite GitHub
state, the corrected local stack, preserved review context, moved behavior,
validation, and publication status.

Inventory date: 2026-08-20 (Europe/Paris)

Repository: `piquel-fr/piqueld`

Checkout: `/home/piquel/Projects/piqueld`

The checkout was kept serial and no worktrees were created. The remote was
fetched before the rebuild and again before publication. No unconditional force
push or hard reset was used.

## Corrected stack

The corrected product base is the exact Plan 06C head below. Plans 07–14 are
future increments on top of it; no Plan 06D is required for basic usability.

| Increment | Branch | Corrected local head |
|---|---|---|
| Plan 06 | `plan/06-docker-reconciliation` | `586b3abcf9b0df6f615ff2e3e335a83f6bb7e5e6` |
| PR #19 / Plan 06A | `plan/06a-simplification` | `d422de79a5cc30b1e7d367af348d5cb248864ca6` |
| PR #20 / Plan 06B | `plan/06b-cli` | `3488b68aab91bfa7c5d492182d52c4bd08fff425` |
| PR #21 / Plan 06C | `plan/06c-basic-web-ui` | `1a8e53be5982233cc9164dc4953bf9c2d9ddc8f1` |
| PR #7 | `plan/07-secret-lifecycle` | `c283f88dcbd4bfe4c01c8a830589741699a3b74d` |
| PR #8 | `plan/08-build-and-registry` | `9ec678ea019a8beb2dc640bb4e833c56b330aea4` |
| PR #9 | `plan/09-traefik-status-and-logs` | `20a9fbe451c159a44f785c35304a02dc40afb70d` |
| PR #10 | `plan/10-import-export` | `f06ee4ce334cbe1a3a7c85393aba3a63b283be79` |
| PR #11 | `plan/11-cli` | `62024c6c3bec8910e72ddc2c4c7cff4fd31a47be` |
| PR #12 | `plan/12-web-ui` | `95f1bdca9a3ac6877778196e1276c2a27f438121` |
| PR #13 | `plan/13-nixos-security-and-operations` | `8618511d44673edf6ae10f8035adf391d6c11f21` |
| PR #14 implementation before this ledger update | `plan/14-qualification-ci-and-docs` | `b520049` |

The Plan 06C head is independently qualified before the future stack. The
NixOS module correction now lives in its owning Plan 13 increment. Plan 14
contains cumulative qualification and documentation, not feature fixes.

## Preserved backup refs

The original-stack refs were retained. A new dated stabilization generation
was created before rewriting remote branches, with `pre` refs for every old PR
head and `corrected` refs for every rebuilt head.

### Review-correction generation `20260819-2330`

```text
refs/backup/review-corrections/20260819-2330/plan-06c-basic-web-ui -> 6652b1eed3449d0f7300fd9e15a41b7b38186050
refs/backup/review-corrections/20260819-2330/pr-07-secret-lifecycle -> 653feba0a96c3119d35b4a653c608c71a44b187f
refs/backup/review-corrections/20260819-2330/pr-08-build-and-registry -> 7a253057775fb2e5ac375ab591e7282e4a515f96
refs/backup/review-corrections/20260819-2330/pr-09-traefik-status-and-logs -> 406fe739e1277e08ef792c07bbda94344fe9e724
refs/backup/review-corrections/20260819-2330/pr-10-import-export -> cdbb35bd9d1f4678d2e4fb03c80f392a7fedf60d
refs/backup/review-corrections/20260819-2330/pr-11-cli -> 48bd4d0a5ad8944663780da261f2fe8e126330ca
refs/backup/review-corrections/20260819-2330/pr-12-web-ui -> ae1e97a1ba58b3632b811123cb32358d1242b964
refs/backup/review-corrections/20260819-2330/pr-13-nixos-security-and-operations -> b37478da71610745166cc425c2251a49509c0832
refs/backup/review-corrections/20260819-2330/pr-14-qualification-ci-and-docs -> abe80cc7576ff607ec0136cac93bceaf7f125309
```

### Original stack

```text
refs/backup/rebase-06-stack/old/plan-06-docker-reconciliation-20260819-715170b -> 715170bfef8467fe8f8376861b7705e08b8d7f3e
refs/backup/rebase-06-stack/old/plan-06a-simplification-20260819-84a2793 -> 84a279309ad2ab39b08f671ecfdf0be712ef7801
refs/backup/rebase-06-stack/old/plan-06b-cli-20260819-b89a993 -> b89a9938b7ea5e1415761be0b02e655618fec882
refs/backup/rebase-06-stack/old/plan-06c-basic-web-ui-20260819-e6df882 -> e6df8821d5a63b9b121dec6781602348879ddea2
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

### Stabilization generation `20260819-180607`

```text
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/plan-06a-simplification -> 808dbc4faeacfc027d9423ad3aab8a278b786279
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/plan-06b-cli -> c1f69449165868ce6478d328eb431ecc291f99e0
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/plan-06c-basic-web-ui -> bffabb307f9fe12bfbb49c91da050ddcd165c02c
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/pr-07-secret-lifecycle -> 004ced097d292b8dc66366c59cf822f05f913fd0
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/pr-08-build-and-registry -> 9b8d27ce099390e056be8cb72e64a8ed630c798e
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/pr-09-traefik-status-and-logs -> 91b6dba60f2474a08718b2fd3e6207d0bcc658c9
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/pr-10-import-export -> 76afc92a2a32111144734fbf8cbbe697b3542b60
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/pr-11-cli -> 7504afe6e8f8513548561e5c657590836713620e
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/pr-12-web-ui -> 56d25b4c65c97dd6e4832ea42a30b33c41ca2be7
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/pr-13-nixos-security-and-operations -> 35b26cebf1d4a2620b2d82378a9bee813b119506
refs/backup/stabilize-usable-product-stack/20260819-180607/pre/pr-14-qualification-ci-and-docs -> ccb911d6e8061f75eefb64b98b96ec43fd3331ab

refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/plan-06a-simplification -> d422de79a5cc30b1e7d367af348d5cb248864ca6
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/plan-06b-cli -> 3488b68aab91bfa7c5d492182d52c4bd08fff425
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/plan-06c-basic-web-ui -> 6652b1eed3449d0f7300fd9e15a41b7b38186050
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/pr-07-secret-lifecycle -> 653feba0a96c3119d35b4a653c608c71a44b187f
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/pr-08-build-and-registry -> 7a253057775fb2e5ac375ab591e7282e4a515f96
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/pr-09-traefik-status-and-logs -> 406fe739e1277e08ef792c07bbda94344fe9e724
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/pr-10-import-export -> cdbb35bd9d1f4678d2e4fb03c80f392a7fedf60d
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/pr-11-cli -> 48bd4d0a5ad8944663780da261f2fe8e126330ca
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/pr-12-web-ui -> ae1e97a1ba58b3632b811123cb32358d1242b964
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/pr-13-nixos-security-and-operations -> b37478da71610745166cc425c2251a49509c0832
refs/backup/stabilize-usable-product-stack/20260819-180607/corrected/pr-14-qualification-ci-and-docs -> ce4447c1e93f2b56c29310313bc554f82dc2eb59
```

## Exact pre-rewrite PR metadata

These were captured from GitHub after fetching and before publication. Every PR
was open, draft, and mergeable; no reviewers were requested.

| PR | Branch | Exact old base SHA | Exact old head SHA |
|---:|---|---|---|
| #19 | `plan/06a-simplification` | `586b3abcf9b0df6f615ff2e3e335a83f6bb7e5e6` | `808dbc4faeacfc027d9423ad3aab8a278b786279` |
| #20 | `plan/06b-cli` | `3edec14eaf3cff570ce9afaffdc12d5473decbbd` | `c1f69449165868ce6478d328eb431ecc291f99e0` |
| #21 | `plan/06c-basic-web-ui` | `c1f69449165868ce6478d328eb431ecc291f99e0` | `bffabb307f9fe12bfbb49c91da050ddcd165c02c` |
| #7 | `plan/07-secret-lifecycle` | `bffabb307f9fe12bfbb49c91da050ddcd165c02c` | `004ced097d292b8dc66366c59cf822f05f913fd0` |
| #8 | `plan/08-build-and-registry` | `004ced097d292b8dc66366c59cf822f05f913fd0` | `9b8d27ce099390e056be8cb72e64a8ed630c798e` |
| #9 | `plan/09-traefik-status-and-logs` | `9b8d27ce099390e056be8cb72e64a8ed630c798e` | `91b6dba60f2474a08718b2fd3e6207d0bcc658c9` |
| #10 | `plan/10-import-export` | `91b6dba60f2474a08718b2fd3e6207d0bcc658c9` | `76afc92a2a32111144734fbf8cbbe697b3542b60` |
| #11 | `plan/11-cli` | `76afc92a2a32111144734fbf8cbbe697b3542b60` | `7504afe6e8f8513548561e5c657590836713620e` |
| #12 | `plan/12-web-ui` | `7504afe6e8f8513548561e5c657590836713620e` | `56d25b4c65c97dd6e4832ea42a30b33c41ca2be7` |
| #13 | `plan/13-nixos-security-and-operations` | `56d25b4c65c97dd6e4832ea42a30b33c41ca2be7` | `35b26cebf1d4a2620b2d82378a9bee813b119506` |
| #14 | `plan/14-qualification-ci-and-docs` | `35b26cebf1d4a2620b2d82378a9bee813b119506` | `ccb911d6e8061f75eefb64b98b96ec43fd3331ab` |

The recorded old base is also the old increment merge base in each case. The
notable ancestry defect was #20's old base: it pointed at `3edec14`, not the
then-current #19 head. The corrected stack uses the actual immediately
preceding corrected head at every layer.

### Review state preserved

The review queries found no submitted reviews and no inline review threads on
#19–#21 or #7–#14. The discussion timelines contained only non-actionable
CodeRabbit “Review skipped — Draft detected” comments. Bodies were replaced
after publication; comments, reviews, and threads were not deleted.

The captured pre-stabilization #14 workflow was run `32262266266`, with these
successful jobs: Rust/contracts `96098087000`, production WASM/browser
`96098087027`, disposable Docker `96098087128`, and Nix packages/NixOS VM
`96098086815`.

## Moved behavior and capability ownership

### PR #19 / Plan 06A

- Ported the base Docker behavior stranded in old #14: bounded retry only for
  Docker’s exact “update out of sequence” response (`7725ee6`), complete typed
  service inspection before comparison/observation (`e6e8f11`), and the narrow
  `HealthCheck`/`Healthcheck` service-wire normalization (`f4f9128`).
- Kept typed desired/observed models, sanitized public errors, and focused
  Docker regressions beside the owning adapter.
- Extended the isolated lifecycle with health checks, repeated idempotent
  reconciliation, updates, drift repair, recovery, ownership refusal,
  deletion, and retained-volume assertions.
- Removed `DaemonConfig::data_dir`; `database.path` is authoritative. Missing
  database parents are created safely without changing existing parent modes.
  The obsolete `migrations/.gitkeep` and future-feature baseline schema were
  not restored.

### PR #20 / Plan 06B

- Added real clap `--help`, `--version`, and `--config PATH` behavior with
  contextual errors for explicitly missing files.
- Kept validated built-in defaults when `/etc/piqueld/config.toml` is absent
  and explains the shipped example path for non-root development.
- Added `daemon_version` through API/OpenAPI/native DTO/WASM DTO/status JSON,
  and completed the essential status/list/show/plan/apply/operation/delete
  workflow over Unix and TCP.
- Added the complete package/build-to-delete quickstart without introducing a
  dynamic capability framework.

### PR #21 / Plan 06C

- Rebuilt the basic read-only Leptos/WASM dashboard with daemon version,
  same-origin API access, package-local UI discovery, explicit `server.ui_dir`,
  bounded single-flight polling, hidden-tab pausing, stale-data retention,
  accessibility, and no browser persistence or telemetry.
- Detects repeated pagination cursors and visibly marks results incomplete when
  the 20-page bound truncates them.
- Uses a request generation when loading application details, so a late A
  response cannot replace the currently selected B detail.
- The normal package installs only `piqueld`, `piquelctl`, the example config,
  and fingerprinted UI assets. `generate_openapi` and the native UI placeholder
  are excluded.

### PR #7

- Rebuilt the complete secret lifecycle: encrypted storage, protected key/file
  inputs, metadata-only APIs, Docker secret reconciliation, lookup/recovery,
  deployed-state barriers, pruning, and pagination.
- Added only the minimal CLI commands required here: metadata list, protected
  stdin/file set-or-replace, and safe delete. Plaintext is never a command
  argument or browser value.

### PR #8

- Preserved the one-owner durable Git/BuildKit/registry pipeline, fresh build
  migration, verified artifact publication, digest-only deployment, and
  credential scope.
- Moved the required BuildKit correction into this owner: Bollard’s supported
  BuildKit session/provider path is supplied rather than leaving builds without
  a session.
- Added only minimal build visibility for the application/operation association;
  the advanced CLI remains in #11.

### PR #9

- Preserved owned Traefik, routes, runtime status, bounded logs, and one shared
  cursor/reconnect stream model.
- Added a minimal bounded historical `piquelctl logs` command. Follow/watch and
  profiling remain in #11.

### PR #10

- Preserved the feature-owned transfer implementation: bounded binary archives,
  confirmation, transactionality, dependency reporting, and API/client support.

### PR #11

- Extends the established flat CLI with profiles/authentication, one stable
  `--json` envelope, one exit-code mapping, and `--watch`/`--follow` options.
- Removed the duplicate grouped application/state tree and the split
  legacy/advanced output behavior; every operation has one command form.
- Secret, build, log, and transfer implementations remain owned by #7–#10;
  no duplicate command or DTO trees were introduced.

### PR #12

- Extended the corrected basic dashboard with Plan 12 structured forms,
  conflict recovery, operation/build/runtime views, write-only secret controls,
  state transfer, and accessible responsive states without replacing the Plan
  06C DTO/state architecture.
- Retired Plan 06C's read-only browser smoke at this boundary because its
  deliberate assertion that mutation controls are absent is no longer valid.

### PR #13

- Preserved security policy, fail-closed TCP authentication, listener limits,
  health/readiness/metrics, NixOS credentials and hardening, split packaging,
  recovery policy, and operational documentation.
- Corrected the NixOS module here to stop emitting the removed top-level
  `data_dir` field and added a focused check over the generated TOML. The valid
  `[registry].data_dir` remains supported.

### PR #14

- Contains qualification, CI, release packaging, fixtures, acceptance
  scenarios, and final documentation only.
- Retains the cumulative NixOS VM that proves the corrected Plan 13 module can
  start the packaged daemon; the module fix itself is not owned here.
- The Docker retry/inspection/health-wire behavior is inherited from #19;
  BuildKit session behavior from #8; secret logical-name matching and lifecycle
  behavior from #7. The old #14 fix commits were intentionally not reintroduced
  as first implementations here.

## Range-diff record

The tables in this section record the original semantic rebuild. The later
review-correction restack is intentionally smaller: Plan 06C adds the detail
request race fix and exact browser qualification; Plans 07–10 are ancestry-only
rebases; Plan 11 removes duplicate CLI forms; Plan 12 retires the obsolete
read-only smoke; Plan 13 owns the NixOS fix/check; and Plan 14 retains only
cumulative qualification plus aligned operator docs.

Each comparison was run with:

```text
git range-diff --no-dual-color <old-base>..<old-head> <new-base>..<new-head>
```

| PR | Old range | Corrected range | Result |
|---:|---|---|---|
| #19 | `586b3ab..808dbc4` | `586b3ab..d422de7` | Old simplification commits match; corrected Docker/storage fixes are additional. |
| #20 | `3edec14..c1f6944` | `d422de7..3488b68` | Old CLI increment is replaced by the two coherent Plan 06B commits on #19. |
| #21 | `c1f6944..bffabb3` | `3488b68..6652b1e` | Basic UI increment is replaced by the packaged/read-only Plan 06C implementation. |
| #7 | `bffabb3..004ced0` | `6652b1e..653feba` | Secret increment is semantically rebuilt in three focused commits. |
| #8 | `004ced0..9b8d27c` | `653feba..7a25305` | Build increment is semantically rebuilt; BuildKit correction and minimal visibility are explicit. |
| #9 | `9b8d27c..91b6dba` | `7a25305..406fe73` | Ingress/status/log behavior is rebuilt with the bounded historical CLI increment. |
| #10 | `91b6dba..76afc92` | `406fe73..cdbb35b` | Transfer behavior is rebuilt as a feature-owned increment. |
| #11 | `76afc92..7504afe` | `cdbb35b..48bd4d0` | Advanced CLI behavior is rebuilt without duplicating lower feature commands. |
| #12 | `7504afe..56d25b4` | `48bd4d0..ae1e97a` | UI feature commits are rebuilt on the single Plan 06C client/state architecture. |
| #13 | `56d25b4..35b26ce` | `ae1e97a..b37478d` | Security/NixOS operations are rebuilt against the corrected API and package. |
| #14 | `35b26ce..ccb911d` | `b37478d..ce4447c` | Qualification/docs are rebuilt; lower fixes are inherited rather than duplicated. |

The abbreviated ranges above are unambiguous in this repository; the exact
full SHAs are in the metadata tables and backup refs.

## Plan 06C standalone qualification

The exact Plan 06C head was gated before rebuilding #7:

- `just` passed, including formatting, strict native Clippy, workspace checks
  and tests, docs, cargo-deny, OpenAPI, and dependency boundaries.
- `just ui-check` passed.
- Strict WASM Clippy passed with
  `cargo clippy --target wasm32-unknown-unknown -p piqueld-client -p piqueld-ui -- -D warnings`.
- The exact production bundle passed the Chromium desktop (1280×800) and
  narrow (390×844) smoke. It covered empty/populated states, rapid A→B detail
  selection, keyboard focus, stale-data retention, disconnect/recovery,
  responsive overflow, and the absence of mutation controls.
- The isolated Docker lifecycle passed health checks, repeated idempotent
  reconcile, update, drift repair, recovery, ownership refusal, deletion, and
  retained-volume assertions.
- The production package built and contained only operator-facing binaries,
  the example TOML, and packaged UI assets; no `generate_openapi` or native UI
  placeholder was present.
- Help/version, explicit config, missing explicit config, default-config
  fallback, clean state-directory startup, Unix status, TCP status, API route,
  and static dashboard route smoke passed on the Plan 06C package.
- The essential Unix/TCP `piquelctl` workflow and the documented quickstart
  were exercised.

## Review-correction validation

- Plan 06C: `just`, `just ui-check`, strict WASM Clippy, release UI build,
  and the desktop/narrow Chromium matrix passed.
- Plan 11: `just` passed after the command/output consolidation; the Unix and
  TCP black-box suites exercise the single stable JSON contract.
- Plan 12: `just` passed after retiring the obsolete read-only smoke.
- Plan 13: `just`, the focused `module-config` Nix derivation, and
  `just nix-check` passed. The generated TOML contains no removed top-level
  `data_dir` and retains the valid registry key.
- Plan 14: `just`, strict WASM Clippy, the exact locked production Trunk build,
  Plan 14's Chromium smoke, the Docker feature compile, and the isolated
  Docker lifecycle passed. The lifecycle completed in 72.54 seconds.
- The final cumulative Nix retry was stopped at the user's request because the
  package/VM build was taking too long. It had completed evaluation without an
  error; no success is claimed for that interrupted run. The completed Plan 13
  Nix check and the post-publication Plan 14 CI remain the relevant evidence.

## Previous cumulative validation

Passed on the corrected #14 checkout:

- `just`
- `just ui-check`
- strict WASM Clippy as above
- `cargo test --workspace` through the `just` workspace test step
- `cargo check -p piqueld --test docker_integration --features docker-integration`
- `just docker-test`: the local foundational Plan 06C lifecycle passed in
  69.97 seconds with the feature gate and disposable flag enabled. The local
  engine has no disposable registry/origin, so the full future three-test
  registry/Traefik suite is intentionally exercised by CI instead.
- `just nix-check`: all six x86_64 checks passed, including the NixOS VM;
  incompatible `aarch64-linux` was omitted by the host check.
- Production package build:
  `nix build --no-update-lock-file --out-link /tmp/piqueld-package-final-20260819-v2 --print-out-paths .#default`
  produced `/nix/store/5yym0nhjg625kmvmd4gppbkaj0zpwpwy-piqueld-0.1.0`.

The final package contents were:

```text
bin/.piqueld-wrapped
bin/piquelctl
bin/piqueld
share/piqueld/piqueld.example.toml
share/piqueld/ui/index.html
share/piqueld/ui/piqueld-ui-7974bac830cef0b.js
share/piqueld/ui/piqueld-ui-7974bac830cef0b_bg.wasm
share/piqueld/ui/style-ae7cd8f6b5ab68c2.css
```

The package wrapper supplies its own Nix-output UI directory. The final
package help/version checks passed. A fresh future-stack daemon smoke was
blocked by an existing host-managed `piqueld-traefik` service owned by a
different instance; the adapter correctly refused adoption. That service and
its application were inventoried by label and left untouched. The lower Plan
06C package daemon smoke and the disposable NixOS VM remain successful startup
evidence; the final remote CI is the authoritative future-stack package/VM
gate.

## Previous publication status

This section records the publication before the `20260819-2330` review
corrections. It is retained as history and is superseded by the corrected-head
table at the start of this document and the final handoff for this restack.

The eleven branches were published bottom-up with
`--force-with-lease=<ref>:<captured-old-head>`. Every lease matched; no lease
failure or concurrent branch update occurred. GitHub now reports this exact
open/draft/mergeable stack:

| PR | Final base | Final base SHA | Final head | Final head SHA |
|---:|---|---|---|---|
| #19 | `plan/06-docker-reconciliation` | `586b3abcf9b0df6f615ff2e3e335a83f6bb7e5e6` | `plan/06a-simplification` | `d422de79a5cc30b1e7d367af348d5cb248864ca6` |
| #20 | `plan/06a-simplification` | `d422de79a5cc30b1e7d367af348d5cb248864ca6` | `plan/06b-cli` | `3488b68aab91bfa7c5d492182d52c4bd08fff425` |
| #21 | `plan/06b-cli` | `3488b68aab91bfa7c5d492182d52c4bd08fff425` | `plan/06c-basic-web-ui` | `6652b1eed3449d0f7300fd9e15a41b7b38186050` |
| #7 | `plan/06c-basic-web-ui` | `6652b1eed3449d0f7300fd9e15a41b7b38186050` | `plan/07-secret-lifecycle` | `653feba0a96c3119d35b4a653c608c71a44b187f` |
| #8 | `plan/07-secret-lifecycle` | `653feba0a96c3119d35b4a653c608c71a44b187f` | `plan/08-build-and-registry` | `7a253057775fb2e5ac375ab591e7282e4a515f96` |
| #9 | `plan/08-build-and-registry` | `7a253057775fb2e5ac375ab591e7282e4a515f96` | `plan/09-traefik-status-and-logs` | `406fe739e1277e08ef792c07bbda94344fe9e724` |
| #10 | `plan/09-traefik-status-and-logs` | `406fe739e1277e08ef792c07bbda94344fe9e724` | `plan/10-import-export` | `cdbb35bd9d1f4678d2e4fb03c80f392a7fedf60d` |
| #11 | `plan/10-import-export` | `cdbb35bd9d1f4678d2e4fb03c80f392a7fedf60d` | `plan/11-cli` | `48bd4d0a5ad8944663780da261f2fe8e126330ca` |
| #12 | `plan/11-cli` | `48bd4d0a5ad8944663780da261f2fe8e126330ca` | `plan/12-web-ui` | `ae1e97a1ba58b3632b811123cb32358d1242b964` |
| #13 | `plan/12-web-ui` | `ae1e97a1ba58b3632b811123cb32358d1242b964` | `plan/13-nixos-security-and-operations` | `b37478da71610745166cc425c2251a49509c0832` |
| #14 | `plan/13-nixos-security-and-operations` | `b37478da71610745166cc425c2251a49509c0832` | `plan/14-qualification-ci-and-docs` | `ce4447c1e93f2b56c29310313bc554f82dc2eb59` |

All PRs remain open and draft. The connector's PR-body update action returned
403 for this integration; the authenticated `gh api` fallback successfully
updated all eleven descriptions, including the corrected #7–#9 CLI ownership
claims and the #14 inherited-fix wording. The connector then verified the
updated bodies, bases, and heads.

The final #14 workflow for the published implementation head
`ce4447c1e93f2b56c29310313bc554f82dc2eb59` was run `32287810891` and completed
successfully. All four required jobs passed:

| Job | Job ID | Result |
|---|---:|---|
| Rust and contract qualification | `96181503845` | success |
| Production WASM and Chromium smoke | `96181504318` | success; production bundle and browser smoke passed |
| Privileged disposable Docker qualification | `96181504376` | success; registry, Swarm, routing, logs, and retention passed |
| Nix packages and NixOS VM | `96181504188` | success; flake, module, package, and VM checks passed |

The final ledger publication is documentation-only; the CI result above is the
authoritative qualification for the corrected implementation tree.
