# Introduction

`piqueld` is a small Rust control plane for one Docker Engine running a
single-node Swarm. Submit an application
manifest naming prebuilt images or Git build sources; the server resolves them
to digests and reconciles a private network, named volumes, and replicated services.

Applications are identified by name or stable ID. Each application owns the
saved manifest and its environments (`production` unless the manifest
[declares others](reference/manifest.md#declared-environments)) that deploy
it with their own history, status, volumes, secrets, routes, and network;
`piquelctl env` manages them. Services reach a service of another application
only through an explicit, two-sided [link](reference/manifest.md#links).
`piquelctl app service`, `app file`, `app volume`, `app route`, `app job`,
`app export`, `app import`, `app variable`, and `app repository` edit
individual saved settings. Each edit
validates and updates the internal manifest atomically; add `--deploy` to deploy
with the change. The dashboard uses the same editing endpoints. Apply remains
available for importing a complete manifest without deploying; `piquelctl app apply --deploy` explicitly saves and deploys. Every deployment
captures its configuration in SQLite and refreshes image tags; each successful
preparation in a tracking environment records an immutable release that
[promoted environments](reference/manifest.md#promoted-environments)
receive without rebuilding. [Previews](reference/manifest.md#previews)
deploy branches, or a local worktree, as disposable environments. Reconciliation and
retries use deployment snapshots, never pending configuration edits. Preconditions
prevent stale saves and deployments. Idempotency receipts survive restarts for
24 hours, and deployment history remains until environment deletion.

One async controller overlaps image pulls, observations, and timers while allowing
one resource mutation request at a time. The active deployment remains maintained
while a candidate prepares; once prepared, it starts rollout without automatic rollback.
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
See [module boundaries](../dev/architecture/dependency-flow.md) for the code layout.

The private `data_dir` holds the embedded database (`piqueld.db`) and is created
with mode `0700`. The Unix API socket lives separately at
`<runtime_dir>/piqueld.sock`, defaulting to `/run/piqueld/piqueld.sock`, with
mode `0660` for the daemon's effective group. The service manager prepares the
runtime directory; the daemon validates both paths without changing existing
directory permissions. Group membership permits socket connections; account
authentication is required. See [authentication](accounts/authentication.md) for setup,
passkeys, invitations, and CLI login.

| Supported | Deferred until later releases |
| --- | --- |
| Prebuilt images, Git/Docker builds, repository-backed manifests, declared environments, variables, generated and stored secrets, read-only files, replicas, environment, command/args, health checks (including HTTP probes), startup dependencies, rollout order, resource limits, named volumes, mounts, one-shot before-rollout jobs, and private links between applications | Registry management and registry credentials |
| Single-node Swarm reconciliation, deployment on push, releases and promotion, branch previews, managed Caddy HTTPS routes (public, private on the tailnet, wildcard, redirects, Cloudflare Tunnel, managed DNS records), drift repair, durable operations, volume retention, application log snapshots, one-off commands in running tasks, and the `piquelctl` workflow | Arbitrary TCP/UDP ports, state transfer, and multi-node operation |
| Unix-socket, localhost, and Tailscale API transports, SSE event streams, scoped accounts and API tokens, a tamper-evident audit trail, and a Leptos/WASM dashboard for editing configuration, deploying, promoting, previews, secrets, history, and recent logs | |

The daemon reads `/etc/piqueld/config.toml` by default; `--config PATH` selects
another host configuration. Configuration covers local paths, listeners,
authentication, the tailnet node, Docker, ingress and DNS providers,
reconciliation timing, retention, images, previews, and telemetry export;
[notification destinations](guides/observability.md#notifications) are managed
through the API, CLI and dashboard instead. See [configuration](reference/configuration.md) and the
[NixOS module](getting-started/nixos.md). The complete non-root development
example is [`examples/piqueld.toml`](https://github.com/piquel-fr/piqueld/blob/main/examples/piqueld.toml).

Applications can also build Git sources locally or fetch their manifests from a
repository on Deploy or [on push](reference/manifest.md#deploying-on-push).
These are independent features; see
[the manifest reference](reference/manifest.md) for both configurations.

See [observability](guides/observability.md) for diagnostic history, daemon statistics, analytics, telemetry
export and webhook delivery configuration.
