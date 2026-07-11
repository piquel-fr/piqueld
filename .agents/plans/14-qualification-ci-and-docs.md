# Plan 14 — End-to-end qualification, CI, and prototype handoff

## Goal

Prove every prototype acceptance criterion in realistic environments, close gaps
found by testing, and leave reproducible CI and operator/developer documentation.
This plan may fix defects but must not add deferred product features.

## Deliverables

- Layered test harnesses: pure/unit/property, database, API/client/CLI/UI, privileged
  Docker integration, and NixOS VM end-to-end.
- End-to-end fixtures for prebuilt and Git-built applications, secrets, volumes,
  replicas, health, routes, logs, drift, conflicts, restart, deletion, and import.
- CI jobs for all commands in section 25, with Docker tests isolated in a privileged
  job, Nix caching where appropriate, migration/round-trip checks, and artifact
  production for `piqueld`, `piquelctl`, UI assets, Nix packages, and checksums.
- `cargo-deny` advisories/licenses/sources policy, dependency audit, and reproducible
  release metadata.
- Acceptance runbook mapping each of the 20 design criteria to an automated test or
  an explicit manual Cloudflare/Tailscale verification step.
- Final architecture, API/manifest, CLI, NixOS install, security/threat-boundary,
  troubleshooting, state-export limitation, and contributor-test documentation.

## Work

1. Run the full suite first and inventory gaps against every design section. Prefer
   targeted fixes and regression tests; do not paper over flakes with retries unless
   the underlying asynchronous condition is asserted correctly.
2. Exercise daemon death/restart during resolve, build, secret rotation, service
   update, delete, and state import. Verify durable truth and convergence after each.
3. Manually modify owned Swarm services and prove repair; create similarly named
   unowned resources and prove refusal. Verify deletion always retains volumes.
4. Test adversarial manifests, oversized bodies/archives/build contexts, archive
   traversal, log-stream lag, malformed Docker responses, credential leakage, and
   forbidden host mounts/arbitrary labels.
5. Validate mutable image tags/Git branches resolve to and deploy immutable values.
   Verify export/import reports resources not included in control-plane archives.
6. Keep external Cloudflare account automation out of CI. Use a local origin test for
   Traefik and document the small manual tunnel reachability check.
7. Produce release artifacts from a clean checkout and verify checksums/startup.

## Required final checks

```text
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace
cargo test --doc --workspace
cargo deny check
nix flake check
```

Also run the privileged Docker integration job, NixOS module/VM tests, migrations on
a fresh database, manifest round trips, UI browser smoke tests, and the acceptance
runbook.

## Done when

All 20 acceptance criteria have traceable evidence, CI is green from a clean clone,
release artifacts are reproducible and checksummed, known limitations match the
explicitly deferred scope, and a new operator can install and exercise the prototype
from the documentation alone.

