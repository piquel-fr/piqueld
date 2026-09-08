# piqueld

`piqueld` is a small Rust control plane for one Docker Engine running a
single-node Swarm. Submit an application
manifest naming prebuilt images; the server resolves those images to digests
and reconciles a private network, named volumes, and replicated services.

Applications are identified by name. One apply endpoint creates or updates the
desired state. The server resolves images on every apply and reuses the current
operation when the resulting target is unchanged. Failed or cancelled operations
can be requested again under the same ID. Clients need no keys or generations.

One sequential controller observes Docker and computes fresh plans until each
operation converges. A new target supersedes earlier active work; operation
history remains in SQLite. Deletion stays running until services and networks
are verified absent, while named volumes are retained.

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
