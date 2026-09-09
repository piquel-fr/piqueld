# Module boundaries

`piqueld-core` owns manifest and resource types, pure planning, application status,
operation records, and shared API requests and responses. It has no HTTP server,
database, Docker, or UI dependency. The daemon uses these contracts directly.
`piqueld-client` adds HTTP transport and reexports the shared contracts for the
CLI and dashboard.

Inside the daemon:

| Module | Responsibility |
| --- | --- |
| `api` | HTTP decoding, response composition, and error mapping |
| `application` | Compare normalized intent, check generations, and accept changes |
| `store` | SQLite records, transactions, and guarded writes |
| `reconcile` | One async controller; resolve, observe, plan, execute, and verify |
| `docker` | Docker requests, runtime specifications, ownership, and observation |
| `config` | Local configuration and private state-directory preparation |

The application service decides whether an apply changes the manifest, is a no-op,
or requests a retry. Slow image preparation runs during operation execution. Store transactions preserve that decision atomically.
The controller checks the latest operation before continuing runtime work and
computes each plan from a fresh observation. Shared records avoid conversion
between separate store, API, and client operation models.

`scripts/check-dependency-boundaries.sh` checks crate dependency boundaries.
Older plans under `.agents` are historical design material, not the current
architecture contract.
