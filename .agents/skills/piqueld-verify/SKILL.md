---
name: piqueld-verify
description: >
  Pick and run the smallest automated checks that cover a piqueld change, add
  focused tests in the right suite, and run the final `just` validation before
  committing. Use while iterating on Rust, API, manifest, CLI, or dashboard
  changes, when tests fail, or before opening a pull request.
---

# Verify a change

Run only what covers the code you changed while iterating; run the full `just`
once at the end (`AGENTS.md` requires it to pass). `just` runs `cargo fmt`,
so it can modify files.

## While iterating

| You changed | Run |
| --- | --- |
| `crates/piqueld-core` (manifest, planner, resources) | `cargo nextest run -p piqueld-core -E 'test(<name>)'` |
| `apps/piqueld` (API, store, reconcile, auth) | `cargo nextest run -p piqueld -E 'binary(<file>) & test(<name>)'` |
| `apps/piquelctl` or `crates/piqueld-client` | `just validate-cli` |
| `apps/piqueld-ui` | `just ui-check`, then the browser test below |
| API DTOs, endpoints, or manifest fields | `just generate` and commit the regenerated `docs/openapi-v1.json`, client, and `docs/application-manifest.schema.json` |
| Docker reconciliation (`apps/piqueld/src/docker`, `reconcile`) | `cargo nextest run -p piqueld -E 'binary(fake_docker)'`, then `just docker-test` (privileged, slow) |
| Dependencies | `just deny` and `just boundary` |
| Embedded assets or CSP | `just test-embedded` |

`docs/contract-coverage.md` maps each contract to the suite where new cases
belong. Add focused cases there rather than new smoke tests. Use
`cargo nextest list -p <crate>` to find test names.

Clippy runs with `-D warnings` on all targets and features:
`cargo clippy --locked -p <crate> --all-targets --all-features -- -D warnings`
checks one crate quickly.

## Browser tests

`tests/playwright` drives Chromium against a stubbed-Docker fixture daemon with a
virtual passkey, so it can do what agents cannot do in the preview: register,
sign in, and approve CLI logins. Use it to lock in any dashboard behavior you
verified by hand.

`node_modules` is per checkout, so run the setup once in each new worktree:

```sh
just setup-playwright
just test-playwright --grep '<test title>'
```

New tests go in `tests/playwright/tests/*.spec.ts` and use the fixtures in
`fixtures.ts`: `daemon` (fresh isolated daemon), `account` (a signed-in user),
`passkeys`, and `cli` (a `piquelctl` bound to the fixture socket). Use role
locators and retrying assertions, never fixed sleeps. Uncaught browser errors
fail the test automatically. On failure, traces and screenshots are in
`tests/playwright/test-results/`.

The fixture's Docker runtime is stubbed: deployments fail to prepare. Real
deployments are covered by `just docker-test` and the `piqueld-e2e` skill.

## Before committing

1. Update the docs that describe the changed behavior (`docs/`, `README.md`).
2. If the feature touches one surface, make sure the manifest, CLI, and
   dashboard all expose it (`AGENTS.md`).
3. Run `just`. Fix every failure; do not skip checks. If a check cannot run in
   this environment (for example Docker is unavailable), say so explicitly.
