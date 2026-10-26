# Automated validation

CI runs on GitHub-hosted runners. Jobs group the recipes that compile the same
code, so each job builds the workspace once, and run in parallel. Pull requests
run only the jobs whose inputs changed, and drafts only the quick ones: Docker,
Playwright, macOS, and Nix jobs wait until a pull request is marked ready.
The `docs` job runs on every pull request. Pushes to main run every job. The
`ci` job summarizes them: it fails if any job failed. Failed checks do not
cancel unrelated jobs.

Rust build caches are scoped to jobs and saved from main only; pull requests
restore them. The Nix job caches the store, including crane's dependency builds
and the dashboard. Pull requests build the combined package; main builds every
package. Dashboard tools are cached by version.

| Capability | Local recipe | CI job |
| --- | --- | --- |
| Rust formatting | `fmt-check` | `checks` |
| Dependency policy and advisories | `deny` | `checks` |
| All-target, all-feature compilation and Clippy, plus wasm32 Clippy of the dashboard, client, and core | `lint` | `rust` |
| Native workspace tests, against the real embedded dashboard | `test` | `rust` |
| Documentation tests | `doc-test` | `rust` |
| Generated API specification, client, and manifest JSON Schema freshness | `openapi-check` | `rust` |
| Core dependency boundaries | `boundary` | `rust` |
| Browser compilation | `ui-check`, `check-wasm` | `wasm-ui` |
| Real browser transport tests | `test-wasm` | `wasm-ui` |
| Isolated Engine integration tests | `docker-test` | `docker-integration` |
| Browser end-to-end tests | `test-playwright` | `browser-playwright` |
| CLI on macOS | `validate-cli` | `cli (macOS)` |
| Embedded daemon and CLI release builds | `nix build` | `nix packages` |
| Documentation book build and link check | `docs` | `docs` |

`just` runs the default validation set (`validate`), including a single check of
the generated OpenAPI specification, client, and manifest JSON Schema. Both local
validation and CI compare the committed artifacts without rewriting them. CI
skips `check` and `build`: the stronger all-target Clippy and the Nix package
build cover them. `fmt` and `generate` are editing commands whose results are
checked above, and `setup-playwright` installs the browser test dependencies.
`run`, `ctl`, `daemon`, `daemon-embedded`, `dev`, and `docs-serve` launch
interactive processes and are not finite validation commands.

## Docker test suites

Controller tests run against `FakeDocker`, an in-memory Docker engine in
`apps/piqueld/src/docker/fake.rs`. `just docker-test` runs
`apps/piqueld/tests/docker_integration.rs` against a real engine: a throwaway
Docker-in-Docker daemon, never the host's.

The scenarios in `apps/piqueld/tests/support/docker_contract.rs` check that the
fake behaves like the real engine. Both suites run each of them:
`fake_docker.rs` against the fake, in the `rust` job, and
`docker_integration.rs` against the engine, in the `docker-integration` job.
When the controller starts relying on another engine behavior, add a scenario
for it. When a scenario passes against the fake but fails against the engine,
fix the fake.
