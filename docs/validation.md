# Automated validation

CI runs on GitHub-hosted runners. Jobs group the recipes that compile the same
code, so each job builds the workspace once, and run in parallel. Pull requests
run only the jobs whose inputs changed; pushes to main run every job. The `ci`
job summarizes them: it fails if any job failed. Failed checks do not cancel
unrelated jobs.

Rust build caches are scoped to jobs and saved from main only; pull requests
restore them. The Nix job caches the store, including crane's dependency builds
and the dashboard. Pull requests build the combined package; main builds every
package. Dashboard tools are cached by version.

| Capability | Local recipe | CI job |
| --- | --- | --- |
| Rust formatting | `fmt-check` | `checks` |
| Dependency policy and advisories | `deny` | `checks` |
| All-target, all-feature compilation and Clippy | `lint` | `rust` |
| Native workspace tests, against the real embedded dashboard | `test` | `rust` |
| Documentation tests | `doc-test` | `rust` |
| Generated API specification and client freshness | `openapi-check` | `rust` |
| Core dependency boundaries | `boundary` | `rust` |
| Browser compilation | `ui-check`, `check-wasm` | `wasm-ui` |
| Real browser transport tests | `test-wasm` | `wasm-ui` |
| Isolated Engine integration tests | `docker-test` | `docker-integration` |
| Browser end-to-end tests | `test-playwright` | `browser-playwright` |
| CLI on macOS | `validate-cli` | `cli (macOS)` |
| Embedded daemon and CLI release builds | `nix build` | `nix packages` |

`just` runs the default validation set, including a single check of the generated
OpenAPI specification and client. Both local validation and CI compare the
committed artifacts without rewriting them. `check` and `build` are covered by
the stronger all-target Clippy and the Nix package build. `fmt` and
`generate` are editing commands whose results are checked above.
`run`, `daemon`, `daemon-embedded`, and `dev` launch interactive processes and are
not finite validation commands.
