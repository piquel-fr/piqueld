# Module boundaries

`piqueld-core` owns manifest and resource types, application and environment
identities (`ApplicationId` for the product owning the manifest, `EnvironmentId`
for the deployable unit that runtime names derive from), pure planning and
[link](../../user/reference/manifest.md#links) resolution, environment status,
operation records, and shared API requests and responses. It has no HTTP server,
database, Docker, or UI dependency. The daemon uses these contracts directly.
`piqueld-client` adds HTTP transport and reexports the shared contracts for the
CLI and dashboard. `piqueld-probe` is the static HTTP health probe, depending
only on `thiserror`: the daemon build script embeds its binary, and
`piqueld-core` uses its container path and arguments to validate and compile
HTTP health checks. `piqueld-generated` writes and checks the files
`just generate` produces, for the daemon's `generate_openapi` example and
`piquelctl`'s `cli_reference` example.

Endpoint bindings are generated from the daemon's OpenAPI document by
Progenitor and checked into `piqueld-client`. They reuse the core wire contracts
and share bounded TCP/Unix/browser response handling. Handwritten convenience
methods unwrap envelopes, provide local validation, and cover the requests
Progenitor cannot generate: the two TOML operations, the local snapshot upload,
and the exec WebSocket. The generator is a
daemon example that only development builds compile; the client uses Progenitor's
small runtime support crate. See
[client generation](https://github.com/sourceorigin-dev/piqueld/blob/main/tools/client-codegen/README.md).

The daemon is split into crates that build in parallel, so a change
recompiles only the crates it touches and those above them:

| Crate | Modules | Depends on |
| --- | --- | --- |
| `piqueld-config` (`crates/piqueld-config`) | `config` (the crate root), `dns`, `directories`, `runtime_dir` | `piqueld-core` |
| `piqueld` (`apps/piqueld`) | every other module, the binary, and the embedded health probe and dashboard | all of the above |

`piqueld-config` holds the configuration file, the DNS provider accounts it
configures, which are also their API clients, and the private state and
runtime directories. Its `testing` feature adds the in-memory and
`challtestsrv` DNS providers the daemon's ingress tests use.

Inside the daemon:

| Module | Responsibility |
| --- | --- |
| `api` | HTTP decoding, authentication and per-route authorization middleware, the transport-independent service that validates commands, response composition, and error mapping |
| `auth` | Passkey ceremonies, account management with grant checks, invitations, and durable credentials |
| `application` | Runtime boundary: resolve accepted inputs during execution, observe runtime for reads, and wake reconciliation |
| `store` | Atomic intent comparison, receipts, records, and guarded writes |
| `reconcile` | One async controller; resolve, observe, plan, execute, and verify |
| `docker` | Docker requests, runtime specifications, ownership, and observation |
| `git` | Isolated, bounded checkouts and local worktree snapshots |
| `secrets` | Authenticated encryption and generation of application secrets |
| `ingress` | The installation-owned Caddy gateway, certificates, and tunnel and tailnet listeners |
| `tailnet` | The daemon's own tailnet node serving the website |

The API service validates commands. One store transaction compares current
intent and revision, accepts a change or no-op, and writes its request receipt.
Explicit reconciliation requests retries. Slow image preparation runs during
operation execution while the controller maintains the active deployment.
The controller checks the latest operation before continuing runtime work and
computes each plan from a fresh observation. Shared records avoid conversion
between separate store, API, and client operation models.

`just boundary` (`cargo xtask boundary`) checks that `piqueld-core` depends on no
server, Docker, database, or browser framework (`axum`, `bollard`, `sqlx`,
`leptos`), including through build and development dependencies. It also
checks that the daemon's crates don't depend on those they build alongside:
`piqueld-config` depends on no Docker, database, or browser framework, nor on
the daemon.
