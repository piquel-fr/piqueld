# Automated validation

CI runs independent checks on Blacksmith runners in parallel on every pull request.
Rust build caches are scoped to jobs, dashboard tools are cached by version, and
Nix caches are separate for each native architecture. Failed checks do not cancel
unrelated jobs.

| Capability | Local recipe | CI job |
| --- | --- | --- |
| Rust formatting | `fmt-check` | `fmt` |
| All-target, all-feature compilation and Clippy | `lint` | `lint` |
| Native workspace tests | `test` | `test` |
| Real embedded assets and inline-script CSP hashes | `test-embedded` | `embedded-test` |
| Documentation tests | `doc-test` | `test` |
| Dependency policy and advisories | `deny` | `deny` |
| Generated API contract freshness | `openapi-check` | `openapi` |
| Core dependency boundaries | `boundary` | `boundary` |
| Browser compilation | `ui-check`, `check-wasm` | `wasm-ui` |
| Real browser transport tests | `test-wasm` | `wasm-ui` |
| Isolated Engine integration tests | `docker-test` | `docker-integration` |
| Embedded daemon and CLI release builds | `build-embedded` | `embedded-release` |
| Nix package builds, tests, formatting, boundaries | `nix-check` | `nix` (native x86_64 and aarch64) |
| NixOS service VM test (requires KVM) | `nix-check` | `nix` (x86_64 only) |

`just` regenerates OpenAPI and runs the default native validation set. CI checks
the committed document without rewriting it. `check` and `build` are covered by
the stronger all-target Clippy and package/release builds. `fmt` and
`generate-openapi` are editing commands whose results are checked above.
`run`, `daemon`, `daemon-embedded`, and `dev` launch interactive processes and are
not finite validation commands.
