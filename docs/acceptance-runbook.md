# Acceptance runbook

Run this against one exact commit on disposable infrastructure. Record the
commit, Rust/nixpkgs/Docker versions, runner, UTC time, and artifact checksums.
The commands below distinguish repository evidence from observations that require
an operator's external Cloudflare/Tailscale/browser environment.

| # | Criterion | Evidence |
|---:|---|---|
| 1 | Install through NixOS | `checks.<system>.nixos-vm`; module evaluation |
| 2 | Start beside Docker | VM service start and readiness check |
| 3 | Initialize one Swarm manager | VM `docker info`/node check; Docker qualification |
| 4 | Create and list a secret safely | secret repository/API/CLI tests; no-value metadata contract |
| 5 | Submit a TOML manifest | core manifest tests and `qualified-prebuilt.toml` |
| 6 | Reject adversarial host mounts | strict decode of `adversarial-host-mount.toml` |
| 7 | Plan before mutation | API/client/CLI plan tests |
| 8 | Reconcile an image by digest | Docker qualification and resource-planning tests |
| 9 | Build Git source and push registry digest | `buildkit_push_registry_digest_and_swarm_deploy` |
| 10 | Rotate a deployed secret safely | secret lifecycle tests and Docker service qualification |
| 11 | Preserve volume data on delete | `swarm_init_create_replica_drift_restart_delete_and_volume_retention` |
| 12 | Repair replica drift | the same Docker qualification test |
| 13 | Report failed health | the same Docker qualification test and status API tests |
| 14 | Serve a managed route | `traefik_route_status_scale_update_and_multiplexed_logs` |
| 15 | Bound and multiplex logs/events | runtime log/client/API tests and the route qualification |
| 16 | Export and transactionally import state | transfer persistence/API/CLI contract tests |
| 17 | Operate through CLI transports | `piquelctl` and transport-neutral client tests |
| 18 | Operate through the dashboard | UI state tests, Trunk build, Chromium smoke |
| 19 | Enforce authentication and limits | auth/API contract tests, NixOS credential checks |
| 20 | Produce a checked release | `nix build .#release`, package reproducibility, `SHA256SUMS` |

## Commands

Run the portable and contract layers first:

```console
just
./scripts/qualification.sh contracts
```

Build and inspect the production UI:

```console
(cd apps/piqueld-ui && trunk build index.html --release --locked --public-url /)
./scripts/browser-smoke.sh
```

Run the privileged layer only on a disposable daemon. CI starts a loopback
registry and sets the required isolation attestations:

```console
PIQUELD_DOCKER_ISOLATED=1 PIQUELD_DOCKER_DISPOSABLE=1 \
PIQUELD_DOCKER_SOCKET=/var/run/docker.sock \
PIQUELD_TEST_REGISTRY=127.0.0.1:5000 \
PIQUELD_TEST_ORIGIN_PORT=18080 \
cargo test -p piqueld --features docker-integration \
  --test docker_integration -- --ignored --test-threads=1
```

Run `nix flake check -L --no-update-lock-file` for the fast CI package/module/VM
evidence. The production release package is checked separately by the release
workflow with `nix build .#release` and the reproducibility checks below.
Build a release with `nix build .#release` and verify it twice under different
umasks with `scripts/package-release.sh`.

## External checks

The daemon intentionally does not create Cloudflare tunnels, DNS records,
Tailscale ACLs, or public firewall openings. For a manual tunnel check, publish
one explicit Traefik origin port on a disposable host, configure an existing
private tunnel to that origin, send a request with the manifest's Host header,
and record the observed status/body and tunnel logs. Verify the same request
fails when the tunnel or origin is stopped.

For browser evidence, use the built-in Chromium smoke plus a current browser to
confirm keyboard navigation, focus visibility, responsive layout, no secret
plaintext in DOM/storage, and the create/plan/apply/status/log/transfer flows.
These observations are not claimed by headless static smoke alone.

State transfer is control-plane-only. Before deleting an application, export its
manifest/state, copy volume data separately, and record the dependency report;
after import, re-supply secrets and registry/Git assets before reconciliation.
