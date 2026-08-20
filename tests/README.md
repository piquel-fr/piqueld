# Cross-cutting qualification

Crate-specific tests remain beside their owning crate so the normal workspace
commands find them. Cross-layer operator fixtures live in
`tests/fixtures/applications`:

- `qualified-prebuilt.toml` covers replicas, a persistent volume, a secret file,
  a health check, and an HTTP route.
- `qualified-git.toml` is the shape used for the Git/build/registry path; replace
  its documentation-only repository with an operator-controlled fixture.
- `adversarial-host-mount.toml` must fail strict decoding. Unknown fields cannot
  smuggle host mounts into the Docker resource model.

`apps/piqueld/tests/docker_integration.rs` is feature-gated, ignored, and refuses
to run without both `PIQUELD_DOCKER_ISOLATED=1` and
`PIQUELD_DOCKER_DISPOSABLE=1`. Use the CI job or
`scripts/run-docker-integration-test.sh`; never point it at a working host
daemon. See `docs/acceptance-runbook.md` for the evidence map.
