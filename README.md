# piqueld

`piqueld` is a small Rust control plane for one Docker Engine running a
single-node Swarm. Submit an application
manifest naming prebuilt images or Git build sources; the server resolves them
to digests and reconciles a private network, named volumes, and replicated services.

Applications are identified by name or stable ID. Each application owns the
saved manifest and its environments (`production` unless the manifest
[declares others](docs/application-manifest.md#declared-environments)) that deploy
it with their own history, status, volumes, secrets, routes, and network;
`piquelctl env` manages them. Services reach a service of another application
only through an explicit, two-sided [link](docs/application-manifest.md#links).
`piquelctl app service`, `app file`, `app volume`, `app route`, `app job`,
`app export`, `app import`, `app variable`, and `app repository` edit
individual saved settings. Each edit
validates and updates the internal manifest atomically; add `--deploy` to deploy
with the change. The dashboard uses the same editing endpoints. Apply remains
available for importing a complete manifest without deploying; `piquelctl app apply --deploy` explicitly saves and deploys. Every deployment
captures its configuration in SQLite and refreshes image tags; each successful
preparation in a tracking environment records an immutable release that
[promoted environments](docs/application-manifest.md#promoted-environments)
receive without rebuilding. [Previews](docs/application-manifest.md#previews)
deploy branches, or a local worktree, as disposable environments. Reconciliation and
retries use deployment snapshots, never pending configuration edits. Preconditions
prevent stale saves and deployments. Idempotency receipts survive restarts for
24 hours, and deployment history remains until environment deletion.

One async controller overlaps image pulls, observations, and timers while allowing
one resource mutation request at a time. The active deployment remains maintained
while a candidate prepares; once prepared, it starts rollout without automatic rollback.
`piquelctl env restore` deploys an earlier deployment again exactly as it ran, as
a new deployment, without running its jobs or reverting data.
Retries reuse prepared digests. Rename changes metadata without
redeployment. Durable
operations, attempt outcomes, and informational events remain in SQLite. Deletion
completes only after services and networks are verified absent; volumes remain,
except a preview's, which its deletion removes.

The daemon exposes a polling HTTP API, with SSE event streams and WebSockets
for one-off commands, over a Unix socket and explicitly enabled localhost or
Tailscale TCP listeners. It can also join the tailnet as its own
node and serve the website over HTTPS with a tailnet-issued certificate. The CLI and optional dashboard share domain
records and HTTP contracts.
See [module boundaries](docs/architecture/dependency-flow.md) for the code layout.

The private `data_dir` holds the embedded database (`piqueld.db`) and is created
with mode `0700`. The Unix API socket lives separately at
`<runtime_dir>/piqueld.sock`, defaulting to `/run/piqueld/piqueld.sock`, with
mode `0660` for the daemon's effective group. The service manager prepares the
runtime directory; the daemon validates both paths without changing existing
directory permissions. Group membership permits socket connections; account
authentication is required. See [authentication](docs/authentication.md) for setup,
passkeys, invitations, and CLI login.

The supported manifest and runtime model are documented in:

- [`docs/application-manifest.md`](docs/application-manifest.md)
- [`docs/authentication.md`](docs/authentication.md)
- [`docs/authorization.md`](docs/authorization.md)
- [`docs/ingress.md`](docs/ingress.md)
- [`docs/api.md`](docs/api.md)
- [`docs/web-ui.md`](docs/web-ui.md)
- [`docs/piquelctl.md`](docs/piquelctl.md)
- [`docs/configuration.md`](docs/configuration.md)
- [`docs/nixos.md`](docs/nixos.md)
- [`docs/observability.md`](docs/observability.md)
- [`docs/quickstart.md`](docs/quickstart.md)
- [`docs/resource-planning.md`](docs/resource-planning.md)
- [`docs/docker-reconciliation.md`](docs/docker-reconciliation.md)
- [`docs/migrations.md`](docs/migrations.md)

| Supported | Deferred until later releases |
| --- | --- |
| Prebuilt images, Git/Docker builds, repository-backed manifests, declared environments, variables, generated and stored secrets, read-only files, replicas, environment, command/args, health checks (including HTTP probes), startup dependencies, rollout order, resource limits, named volumes, mounts, one-shot before-rollout jobs, and private links between applications | Registry management and registry credentials |
| Single-node Swarm reconciliation, deployment on push, releases and promotion, branch previews, managed Caddy HTTPS routes (public, private on the tailnet, wildcard, redirects, Cloudflare Tunnel, managed DNS records), drift repair, durable operations, volume retention, application log snapshots, one-off commands in running tasks, and the `piquelctl` workflow | Arbitrary TCP/UDP ports, state transfer, and multi-node operation |
| Unix-socket, localhost, and Tailscale API transports, SSE event streams, scoped accounts and API tokens, a tamper-evident audit trail, and a Leptos/WASM dashboard for editing configuration, deploying, promoting, previews, secrets, history, and recent logs | |

## Development

Use the Rust toolchain pinned in `rust-toolchain.toml`, with its
`wasm32-unknown-unknown` and `x86_64-unknown-linux-musl` targets: daemon
builds with `embedded-ui` embed a dashboard built for the first, and every
daemon build embeds a
[health probe](docs/docker-reconciliation.md#http-health-probe) built for the
second. Nix is optional; `nix develop`
provides a reproducible development shell and the flake contains deployable
packages. macOS supports the CLI on Apple Silicon: use
`just validate-cli` for CLI development and `nix build .#cli` for the Apple
Silicon Nix package.
See [macOS support](docs/piquelctl.md#macos-support) for development and
connecting to a Linux daemon. The full Linux validation command is:

```console
just
```

`just` formats sources, then checks lints, compilation, tests, documentation
tests, dependency licenses, dependency boundaries, browser compilation, the
Playwright browser tests (run `just setup-playwright` first), and the freshness
of the checked-in OpenAPI specification, generated client, and manifest JSON
Schema. It does not modify generated files. Regenerate all three artifacts in
dependency order with:

```console
just generate
```

See [automated validation](docs/validation.md) for the CI jobs and
[contract coverage](docs/contract-coverage.md) for where tests live.

Client generation is implemented in Rust with Progenitor. Ordinary Cargo builds
use the checked-in client without compiling the generator. See
[client generation](tools/client-codegen/README.md) for the shared contract
mappings and the handwritten requests.

The optional privileged Docker qualification uses an isolated Docker-in-Docker
daemon:

```console
just docker-test
```

Both `just test` and `just docker-test` use nextest and emit JUnit XML at
`target/nextest/<profile>/junit.xml` (`default` locally). CI selects separate
`ci` and `docker-ci` profiles, runs all selected tests even after failures,
and preserves reports as workflow artifacts when tests fail. Doc and browser checks use
separate runners and are not included in these nextest reports.

The reproducible Nix package can be built with `nix build`; use `nix develop`
for the development shell.

The daemon reads `/etc/piqueld/config.toml` by default; `--config PATH` selects
another host configuration. Configuration covers local paths, listeners,
authentication, the tailnet node, Docker, ingress and DNS providers,
reconciliation timing, retention, images, previews, and observability (telemetry
export and notifications); see [configuration](docs/configuration.md) and the
[NixOS module](docs/nixos.md). The complete non-root development
example is [`examples/piqueld.toml`](examples/piqueld.toml).
[Development instances](docs/development.md) (`just dev`) run an isolated
daemon per worktree.
See [`docs/web-ui.md`](docs/web-ui.md) for development and release dashboard
asset commands.

Applications can also build Git sources locally or fetch their manifests from a
repository on Deploy or [on push](docs/application-manifest.md#deploying-on-push).
These are independent features; see
[the manifest reference](docs/application-manifest.md) for both configurations.

See [observability](docs/observability.md) for diagnostic history, daemon statistics, analytics, telemetry
export and webhook delivery configuration.
