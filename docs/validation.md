# Automated validation

CI runs independent checks on Blacksmith runners in parallel on every pull request.
Rust build caches are scoped to jobs and dashboard tools are cached by version.
Failed checks do not cancel unrelated jobs.

| Capability | Local recipe | CI job |
| --- | --- | --- |
| Rust formatting | `fmt-check` | `fmt` |
| All-target, all-feature compilation and Clippy | `lint` | `lint` |
| Native workspace tests | `test` | `test` |
| Real embedded assets and inline-script CSP hashes | `test-embedded` | `embedded-test` |
| Documentation tests | `doc-test` | `test` |
| Dependency policy and advisories | `deny` | `deny` |
| Generated API specification and client freshness | `openapi-check` | `openapi` |
| Core dependency boundaries | `boundary` | `boundary` |
| Browser compilation | `ui-check`, `check-wasm` | `wasm-ui` |
| Real browser transport tests | `test-wasm` | `wasm-ui` |
| Isolated Engine integration tests | `docker-test` | `docker-integration` |
| Embedded daemon and CLI release builds | `build-embedded` | `embedded-release` |

`just` runs the default validation set, including a single check of the generated
OpenAPI specification and client. Both local validation and CI compare the
committed artifacts without rewriting them. `check` and `build` are covered by
the stronger all-target Clippy and package/release builds. `fmt` and
`generate` are editing commands whose results are checked above.
`run`, `daemon`, `daemon-embedded`, and `dev` launch interactive processes and are
not finite validation commands.
