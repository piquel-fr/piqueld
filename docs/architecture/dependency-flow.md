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
| `application` | Validate commands and wake reconciliation after acceptance |
| `store` | Atomic intent comparison, receipts, records, and guarded writes |
| `reconcile` | One async controller; resolve, observe, plan, execute, and verify |
| `docker` | Docker requests, runtime specifications, ownership, and observation |
| `config` | Local configuration and private state-directory preparation |

The application service validates commands. One store transaction compares current
intent and revision, accepts a change or no-op, and writes its request receipt.
Explicit reconciliation requests retries. Slow image preparation runs during
operation execution while the controller maintains the active deployment.
The controller checks the latest operation before continuing runtime work and
computes each plan from a fresh observation. Shared records avoid conversion
between separate store, API, and client operation models.

`scripts/check-dependency-boundaries.sh` checks crate dependency boundaries.
Older plans under `.agents` are historical design material, not the current
architecture contract.
