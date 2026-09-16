# piqueld

`piqueld` is a small Rust control plane for one Docker Engine running a
single-node Swarm. Submit an application
manifest naming prebuilt images; the server resolves those images to digests
and reconciles a private network, named volumes, and replicated services.

Applications are identified by name. Apply saves the full configuration without
deploying; `piquelctl apply --deploy` explicitly saves and deploys. Every deployment
captures its configuration in SQLite and refreshes image tags. Reconciliation and
retries use deployment snapshots, never pending configuration edits. Preconditions
prevent stale saves and deployments. Idempotency receipts survive restarts for
24 hours, and deployment history remains until application deletion.

One async controller overlaps image pulls, observations, and timers while allowing
one resource mutation request at a time. The active deployment remains maintained
while a candidate prepares; promotion starts rollout without automatic rollback.
Retries reuse prepared digests. Rename changes metadata without
redeployment. Durable
operations, attempt outcomes, and informational events remain in SQLite. Deletion
completes only after services and networks are verified absent; volumes remain.

The daemon exposes a polling HTTP API over a Unix socket and explicitly enabled
localhost or Tailscale TCP listeners. The CLI and optional dashboard share domain
records and HTTP contracts.
See [module boundaries](docs/architecture/dependency-flow.md) for the code layout.

The private `data_dir` holds the embedded database (`piqueld.db`) and is created
with mode `0700`. The Unix API socket lives separately at
`<runtime_dir>/piqueld.sock`, defaulting to `/run/piqueld/piqueld.sock`, with
mode `0660` for the daemon's effective group. The service manager prepares the
runtime directory; the daemon validates both paths without changing existing
directory permissions. Group membership grants full operator access.

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
| Prebuilt images, Git/Docker builds, repository-backed manifests, replicas, environment, command/args, health checks, resource limits, named volumes, and mounts | Automatic deployment, registry management, credentials, and secrets |
| Single-node Swarm reconciliation, drift repair, durable operations, polling, volume retention, application log snapshots, and the essential `piquelctl` workflow | Published ports, routes, Traefik, state transfer, authentication, and multi-node operation |
| Unix-socket, localhost, and Tailscale API transports, plus a Leptos/WASM dashboard for saving configuration, deploying, inspecting history, and reading recent logs | Secrets, streams, and the advanced web UI |

## Development

Use a Rust 1.96-or-newer toolchain directly. Nix is optional; `nix develop`
provides a reproducible development shell and the flake contains deployable
packages. macOS supports the CLI on Apple Silicon: use
`just validate-cli` for CLI development and `nix build .#cli` for the Apple
Silicon Nix package.
See [macOS support](docs/piquelctl.md#macos-support) for development and
connecting to a Linux daemon. The full Linux validation command is:

```console
just
```

`just` checks formatting, lints, compilation, tests, documentation tests,
dependency licenses, dependency boundaries, and the freshness of the checked-in
OpenAPI specification and generated client. It does not modify generated files.
Regenerate both artifacts in dependency order with:

```console
just generate
```

Client generation requires Java 11 or newer and curl (included in `nix develop`);
ordinary Cargo builds use checked-in Rust. See
[client generation](tools/client-codegen/README.md) for the pinned tooling,
shared contract mappings, and template.

The optional privileged Docker qualification uses an isolated Docker-in-Docker
daemon:

```console
just docker-test
```

Both `just test` and `just docker-test` use nextest and emit JUnit XML at
`target/nextest/<profile>/junit.xml` (`default` locally). CI selects separate
`ci` and `docker-ci` profiles, runs all selected tests even after failures,
and preserves reports as workflow artifacts. Blacksmith automatically discovers
these XML files for test analytics. Doc and browser checks use separate runners
and are not included in these nextest reports.

The reproducible Nix package can be built with `nix build`; use `nix develop`
for the development shell.

The daemon reads `/etc/piqueld/config.toml` by default; `--config PATH` selects
another host configuration. Configuration only covers local paths, listeners,
SQLite, Docker, and reconciliation timing. The complete non-root development
example is [`examples/piqueld.toml`](examples/piqueld.toml).
See [`docs/web-ui.md`](docs/web-ui.md) for development and release dashboard
asset commands.

Applications can also build Git sources locally or fetch their manifests from a
repository on manual Deploy. These are independent features; see
[the manifest reference](docs/application-manifest.md) for both configurations.
