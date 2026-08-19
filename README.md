# piqueld

`piqueld` is a small Rust control plane for one Docker Engine running a
single-node Swarm. The usable product stack accepts application manifests,
resolves images or Git sources into verified artifacts, manages encrypted
logical secrets, and reconciles private networks, named volumes, routes, and
replicated services.

The daemon stores normalized intent, resolved runtime state, application status,
and durable operations in SQLite. It exposes a versioned HTTP API over a
loopback TCP listener and a Unix socket, an authenticated browser dashboard,
resumable operation/runtime streams, and bounded polling fallbacks.

The configured database path is the only state location. On a clean install the
daemon creates missing parent directories without changing existing directory
permissions, and refuses symlinked database path components.

The supported manifest and runtime model are documented in:

- [`docs/application-manifest.md`](docs/application-manifest.md)
- [`docs/api.md`](docs/api.md)
- [`docs/web-ui.md`](docs/web-ui.md)
- [`docs/piquelctl.md`](docs/piquelctl.md)
- [`docs/configuration.md`](docs/configuration.md)
- [`docs/quickstart.md`](docs/quickstart.md)
- [`docs/resource-planning.md`](docs/resource-planning.md)
- [`docs/docker-reconciliation.md`](docs/docker-reconciliation.md)
- [`docs/ingress.md`](docs/ingress.md)
- [`docs/migrations.md`](docs/migrations.md)
- [`docs/state-archive-v1.md`](docs/state-archive-v1.md)
- [`docs/operations.md`](docs/operations.md)
- [`docs/operator-guide.md`](docs/operator-guide.md)
- [`docs/acceptance-runbook.md`](docs/acceptance-runbook.md)
- [`docs/security.md`](docs/security.md)
- [`docs/release.md`](docs/release.md)
- [`docs/troubleshooting.md`](docs/troubleshooting.md)

| Supported product behavior | Deliberate boundary |
| --- | --- |
| Prebuilt/Git sources, digest verification, builds, encrypted logical secrets, routes, logs, state transfer, durable operations, and the full CLI/dashboard workflows | One host, one Docker Engine, one Swarm manager, and no multi-node orchestration |
| Unix-socket access, loopback bearer authentication, tightly constrained trusted-proxy identity mode, health/readiness, low-cardinality metrics, and bounded requests | No automatic firewall openings, public registry/API exposure, account system, or time-series database |
| NixOS daemon/CLI/UI packages with external systemd credentials and hardened service defaults | NixOS VM/Docker qualification is an owning release check, not a host-Docker prerequisite |

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

The cross-layer qualification entry points are `./scripts/qualification.sh
contracts` and `nix flake check -L`; the acceptance matrix is in
[`docs/acceptance-runbook.md`](docs/acceptance-runbook.md).

The reproducible Nix package and checks can be evaluated explicitly with
`just nix-check`.

The daemon reads `/etc/piqueld/config.toml` by default; `--config PATH` selects
another host configuration. Configuration only covers local paths, listeners,
SQLite, Docker, reconciliation limits, security policy, external credentials,
metrics, and the production dashboard asset directory. The complete non-root
development example is [`config/piqueld.example.toml`](config/piqueld.example.toml).
See [`docs/web-ui.md`](docs/web-ui.md) for development and release asset
commands.
