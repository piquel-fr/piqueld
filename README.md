# piqueld

`piqueld` is a small Rust control plane for one Docker Engine running a
single-node Swarm. Submit an application
manifest naming prebuilt images; the server resolves those images to digests
and reconciles a private network, named volumes, and replicated services.

Applications are identified by name. Apply durably accepts the full normalized
manifest and returns an operation ID before resolving images. Identical applies
are no-ops, including after failure. Explicit reconcile repairs or retries stored targets;
refresh explicitly resolves images again. The CLI automatically protects mutations
with the inspected identity and generation, and reuses a request ID across transport
retries. SQLite retains acceptance receipts for 24 hours.

One async controller overlaps image pulls, observations, and timers while allowing
one resource mutation request at a time. The active deployment remains maintained
while a candidate prepares; promotion starts rollout without automatic rollback.
Unchanged image references reuse active digests. Rename changes metadata without
redeployment. Durable
operations, attempt outcomes, and informational events remain in SQLite. Deletion
completes only after services and networks are verified absent; volumes remain.

The daemon exposes a polling HTTP API over loopback TCP and a Unix socket. The
CLI and optional read-only dashboard share domain records and HTTP contracts.
See [module boundaries](docs/architecture/dependency-flow.md) for the code layout.

A single configured `data_dir` is the only state location and holds the Unix
API socket (`piqueld.sock`) and the embedded database (`piqueld.db`). On a clean
install the daemon creates it with mode `0700`, never modifies an existing
directory, and refuses symlinked path components anywhere in the path.

The supported manifest and runtime model are documented in:

- [`docs/application-manifest.md`](docs/application-manifest.md)
- [`docs/api.md`](docs/api.md)
- [`docs/web-ui.md`](docs/web-ui.md)
- [`docs/piquelctl.md`](docs/piquelctl.md)
- [`docs/configuration.md`](docs/configuration.md)
- [`docs/quickstart.md`](docs/quickstart.md)
- [`docs/resource-planning.md`](docs/resource-planning.md)
- [`docs/docker-reconciliation.md`](docs/docker-reconciliation.md)
- [`docs/migrations.md`](docs/migrations.md)

| Supported | Deferred until later releases |
| --- | --- |
| Prebuilt images, replicas, environment, command/args, health checks, resource limits, named volumes, and mounts | Git sources, builds, registry management, credentials, and secrets |
| Single-node Swarm reconciliation, drift repair, durable operations, polling, volume retention, and the essential `piquelctl` workflow | Published ports, routes, Traefik, logs, state transfer, authentication, and remote or multi-node operation |
| Unix-socket and loopback-TCP API transports, plus a read-only Leptos/WASM dashboard on the TCP listener | Mutating web controls, secrets, streams, and the advanced web UI |

## Development

Use a Rust 1.96-or-newer toolchain directly. Nix is optional; `nix develop`
provides a reproducible development shell and the flake contains deployment
checks. The ordinary command is:

```console
just
```

`just` regenerates the checked-in OpenAPI snapshot and then checks formatting,
lints, compiles, tests, checks documentation tests, audits dependencies and
licenses, verifies the snapshot, and checks dependency boundaries. Use
`just validate` for read-only validation. Regenerate OpenAPI explicitly with:

```console
just generate-openapi
```

The optional privileged Docker qualification uses an isolated Docker-in-Docker
daemon:

```console
just docker-test
```

The reproducible Nix package and checks can be evaluated explicitly with
`just nix-check`.

The daemon reads `/etc/piqueld/config.toml` by default; `--config PATH` selects
another host configuration. Configuration only covers local paths, listeners,
SQLite, Docker, and reconciliation timing. The complete non-root development
example is [`config/piqueld.example.toml`](config/piqueld.example.toml).
See [`docs/web-ui.md`](docs/web-ui.md) for development and release dashboard
asset commands.
