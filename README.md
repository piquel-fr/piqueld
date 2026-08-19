# piqueld

`piqueld` is a small Rust control plane for one Docker Engine running a
single-node Swarm. Plan 06B supports one honest workflow: submit an application
manifest that names prebuilt container images, resolve those images to digests,
and reconcile the resulting private network, named volumes, and replicated
services.

The daemon stores normalized intent, resolved runtime state, application status,
and durable operations in SQLite. It exposes a versioned HTTP API over a
loopback TCP listener and a Unix socket. Clients poll application and operation
resources; there is no event-stream endpoint.

The configured database path is the only state location. On a clean install the
daemon creates missing parent directories without changing existing directory
permissions, and refuses symlinked database path components.

The supported manifest and runtime model are documented in:

- [`docs/application-manifest.md`](docs/application-manifest.md)
- [`docs/api.md`](docs/api.md)
- [`docs/piquelctl.md`](docs/piquelctl.md)
- [`docs/configuration.md`](docs/configuration.md)
- [`docs/quickstart.md`](docs/quickstart.md)
- [`docs/resource-planning.md`](docs/resource-planning.md)
- [`docs/docker-reconciliation.md`](docs/docker-reconciliation.md)
- [`docs/migrations.md`](docs/migrations.md)

| Supported in Plan 06B | Deferred until later plans |
| --- | --- |
| Prebuilt images, replicas, environment, command/args, health checks, resource limits, named volumes, and mounts | Git sources, builds, registry management, credentials, and secrets |
| Single-node Swarm reconciliation, drift repair, durable operations, polling, volume retention, and the essential `piquelctl` workflow | Published ports, routes, Traefik, logs, state transfer, authentication, packaging, and UI |
| Unix-socket and loopback-TCP API transports | Remote or multi-node operation |

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
SQLite, Docker, and reconciliation limits. The complete non-root development
example is [`config/piqueld.example.toml`](config/piqueld.example.toml).
