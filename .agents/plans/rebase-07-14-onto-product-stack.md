# Rebase Plans 07–14 onto the usable-product stack

## Goal

Reuse the existing pull requests #7–14 while rebuilding their branches on top of
Plan 06C. Preserve their implemented features and review history without restoring
the speculative structure removed in Plan 06A or overwriting the new CLI/dashboard.

This is a semantic transplant, not one mechanical `git rebase --onto`. Resolve each
feature against the current architecture and verify capability parity deliberately.

## Inputs and target chain

The new lower stack is:

```text
Plan 06
  -> Plan 06A simplification and consistency
  -> Plan 06B essential CLI
  -> Plan 06C basic read-only web UI
```

Rebuild the existing stack in order:

```text
PR #7  secret lifecycle
  -> PR #8  build and registry
  -> PR #9  Traefik and runtime visibility
  -> PR #10 state import/export
  -> PR #11 advanced CLI
  -> PR #12 advanced web UI
  -> PR #13 NixOS, security, and operations
  -> PR #14 end-to-end qualification
```

Keep the existing remote branches and PRs. Work locally on temporary rebuild
branches first, then update each existing PR branch from the bottom upward using
`--force-with-lease`, never an unconditional force push.

The existing branch mapping at the time this plan was written is:

| PR | Existing branch |
| --- | --- |
| #7 | `plan/07-secret-lifecycle` |
| #8 | `plan/08-build-and-registry` |
| #9 | `plan/09-traefik-status-and-logs` |
| #10 | `plan/10-import-export` |
| #11 | `plan/11-cli` |
| #12 | `plan/12-web-ui` |
| #13 | `plan/13-nixos-security-and-operations` |
| #14 | `plan/14-qualification-ci-and-docs` |

Verify this mapping against GitHub before rewriting. Branch names are identifiers,
not proof of ancestry or completeness.

## Critical history warning

Do not assume every higher branch contains the current head of the branch below it.
In particular, the current Plan 07 branch includes follow-up fixes after the point
from which Plan 08 was originally stacked. Preserve all of those fixes, including
deployed-state barriers, recovery behavior, lookup corrections, pruning, and
pagination. Inventory actual commits and capabilities rather than trusting ancestry.

## Phase 1 — Freeze and inventory

1. Fetch current remote refs and require a clean checkout. Avoid a checkout another
   agent is actively mutating; use isolated worktrees or serialize branch work.
2. Record the exact Plan 06C target and every local/remote PR head and merge base.
3. Create clearly named backup refs for the old heads before rewriting anything.
   Keep them until all rebuilt PRs merge.
4. If GitHub access is available, save each PR description, review threads, requested
   changes, and CI state. Do not lose unresolved review intent when commits change.
5. For every PR, inventory its incremental capabilities by area: manifest/core,
   migrations, API/OpenAPI, client, daemon/runtime, CLI, UI, tests, docs, Nix/CI.
6. Compare adjacent branches and PR patches, not only commit messages. Note commits
   that landed on a lower branch after a higher branch forked.
7. Publish a short migration ledger mapping every old capability/fix to its target
   rebuilt PR or an explicit deliberate deferral. No item may disappear silently.

## Phase 2 — Semantic transplant rules

For each PR, create a fresh rebuild branch from the completed rebuilt predecessor:

1. Identify feature and follow-up-fix commits; ignore old merge commits and obsolete
   conflict-resolution commits.
2. Cherry-pick without committing, or manually port cohesive pieces, so conflicts
   are resolved by current design rather than by choosing an old side wholesale.
3. Preserve Plan 06A's boundaries: no duplicate DTO/domain models, placeholder
   modules, unnecessary repository traits, parallel state hierarchies, or duplicate
   public errors.
4. Add a feature back only when that PR makes it real. Use a clean migration in the
   rebuilt sequence; do not restore abandoned baseline scaffolding just to reduce a
   diff.
5. There must be one owner for concurrency, retries, durable operations, and stream
   reconnection behavior. Consolidate overlapping implementations.
6. Update API, transport-neutral client, CLI, dashboard, OpenAPI, docs, and focused
   tests in the same PR when the new feature is user-visible there.
7. Regenerate checked-in artifacts through documented generation commands. Keep
   ordinary validation non-mutating.
8. Run a range-diff against the old increment and complete its capability checklist
   before proceeding to the next PR. A textual difference is expected; an
   unexplained behavioral loss is not.

## Phase 3 — PR-specific integration

### PR #7 — Secret lifecycle

- Reintroduce the secret manifest/API/client model, encryption configuration,
  persistence, Docker secret rotation, and reconciliation only now.
- Add the minimal CLI secret workflow necessary to make the feature usable; do not
  wait for the rebuilt advanced CLI PR. The read-only dashboard may show safe secret
  metadata only if useful, never values.
- Preserve every later Plan 07 fix, especially deployed-state barriers, restart
  recovery, secret lookup, pruning, and pagination behavior.
- Retain all plaintext-safety guarantees across logs, errors, argv, serialization,
  DOM, and durable state.

### PR #8 — Build and registry

- Reintroduce Git source/build/registry configuration, persistence, BuildKit work,
  registry push, and digest deployment as one operational path.
- Create fresh migrations for the new feature rather than restoring removed tables.
- Keep one build concurrency owner and one durable source/build state machine.
- Start from the rebuilt latest Plan 07 so none of its follow-up fixes are lost.
- Extend CLI visibility/commands needed to operate builds; keep the dashboard
  read-only unless this PR's accepted scope clearly requires more.

### PR #9 — Traefik and runtime visibility

- Reintroduce ports, routes, Traefik lifecycle, richer runtime status, logs, and
  events as working capabilities rather than placeholders.
- If streaming is now justified, design one shared server/client reconnection and
  cursor model. Do not independently restore every deleted SSE implementation.
- Extend CLI and dashboard status/log views consistently and keep rendered/logged
  data bounded.

### PR #10 — State import and export

- Port application export and control-plane archive logic onto the clean current
  schema with explicit format/version validation and transactional replacement.
- Add the usable CLI commands in this PR, including binary-output and replacement
  confirmation safeguards.
- Add dashboard transfer features only when they remain part of the accepted PR
  scope; do not force advanced UI architecture into the read-only dashboard early.

### PR #11 — Advanced CLI

- Extend the essential Plan 06B CLI; do not replace it with the old CLI tree.
- Add the still-missing accepted Plan 11 capabilities such as profiles/auth,
  advanced stable JSON/exits, streaming logs/progress, and state/feature commands.
- Remove duplicate commands already delivered alongside Plans 07–10 and preserve
  the simpler output, confirmation, idempotency, and polling foundations where they
  remain appropriate.

### PR #12 — Advanced web UI

- Expand the Plan 06C Leptos application; do not overwrite it with the old UI.
- Add structured mutations, conflict handling, secrets, builds, logs, routes, and
  state transfer against the rebuilt API/client.
- Reuse the transport-neutral contract and established visual/accessibility system.
  Avoid a duplicate DTO layer or a state framework whose need has disappeared.

### PR #13 — NixOS, security, and operations

- Port packaging/module work, authentication, limits, CORS policy, health/readiness,
  metrics, credential handling, restart behavior, and operational documentation.
- Adapt paths, binaries, assets, and security policy to the rebuilt CLI/UI layout.
- Keep exposure secure by default and validate secret/file permissions.

### PR #14 — End-to-end qualification

- Port and consolidate CI, release packaging, VM/integration/security tests,
  acceptance runbooks, and final documentation.
- Update acceptance criteria for the rebuilt architecture while preserving complete
  product behavior.
- Remove duplicate validation jobs introduced by intermediate PRs and ensure local
  `just` commands and CI use the same authoritative checks.

## Migration policy

- Plan 06A may rewrite the unreleased baseline migration directly.
- Each rebuilt feature PR adds the next clean, sequential migration for the feature
  it actually introduces.
- Test fresh installs and current transactional invariants. Do not add upgrade paths
  from old branches that no user ran.
- At the final PR, inspect the full schema as a coherent whole and consolidate only
  if doing so materially simplifies the still-unreleased product.

## Verification for every rebuilt PR

- The migration ledger has no unexplained omissions.
- The old PR's accepted behavior and follow-up fixes are present or explicitly
  deferred with user approval.
- Plan 06A's simplifications are not accidentally undone.
- Manifest, migration, API/OpenAPI, client, CLI, UI, docs, and tests agree.
- A fresh database works and ownership/volume-retention guarantees remain intact.
- Canonical `just` checks pass; feature-specific Docker, WASM/browser, Nix, VM, or
  security checks run when relevant.
- Review the range-diff and cumulative capability matrix before updating the remote
  PR branch.

## Publishing the rebuilt stack

1. Validate all local rebuilt branches before rewriting any remote branch.
2. Update PR #7 first with `git push --force-with-lease`, confirm its PR base and CI,
   then continue upward one branch at a time.
3. Update each PR base branch and description to explain the semantic transplant,
   moved functionality, migrations, validation, and any deliberate deviation.
4. Preserve existing PR numbers, discussions, and review context. Reply to review
   threads whose code moved so reviewers can find the new location.
5. Never use unconditional `--force`, delete backup refs early, or rewrite a remote
   whose lease no longer matches. Stop and reinventory if another agent/user updates
   a branch during the operation.
6. Keep old-head backup refs until the entire rebuilt stack is merged.

## Done when

PRs #7–14 form a clean stack on top of Plan 06C, all accepted old capabilities and
follow-up fixes are accounted for, the simplification is preserved, and each PR is
reviewable as one coherent feature increment. The cumulative rebuilt Plan 14 product
matches or deliberately improves the old stack without carrying its avoidable
complexity forward.

## Final handoff

Provide the old/new head mapping, backup refs, migration ledger, per-PR validation,
range-diff notes, unresolved review items, deliberate deviations, and any checks that
could not run. Do not delete backups as part of the handoff.
