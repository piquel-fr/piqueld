# Configuration

`piqueld` reads `/etc/piqueld/config.toml` when no option is supplied. An
explicit file is selected with:

```console
piqueld --config /path/to/config.toml
```

An explicitly supplied file must exist and pass validation; a missing or
invalid file is an error with its path included in the diagnostic. If the
production default file is absent, the daemon uses its validated built-in
defaults and explains how to select the repository's complete development
example with `--config examples/piqueld.toml`. The development example
keeps its state in `/tmp/piqueld-dev`, with its Unix API socket at
`/tmp/piqueld-dev-run/piqueld.sock`.

The daemon keeps persistent state in a private data directory: the embedded
database (`piqueld.db`) and future user data. Missing data-directory components
are created with mode `0700`; existing components are never chmodded.

The Unix API socket is separate, at `<runtime_dir>/piqueld.sock`. It is always
`0660`, owned by the daemon's user and effective group. Group membership allows
connections; account authentication is also required.

The service manager or installer must create the runtime directory before
startup. Use daemon ownership and mode `0750` for group access, or `0700` for
private development. Group-readable/traversable runtime directories must use
the daemon's effective group. Group write and all access by others are rejected.
Both paths reject symlinks and ancestors vulnerable to replacement by untrusted
users. Both final directories must grant their owner read, write, and execute
access. Existing directory permissions are never changed.

For the development example, run `mkdir -p -m 0700 /tmp/piqueld-dev-run` first;
`just dev` handles this automatically. The production defaults are:

| Setting | Default |
| --- | --- |
| `server.data_dir` | `/var/lib/piqueld` |
| `server.runtime_dir` | `/run/piqueld` (must already exist) |
| `server.listen_mode` | `"off"` |
| `server.port` | `7845` |
| `server.allowed_hosts` | `[]` (additional trusted DNS hostnames) |
| derived socket path | `<runtime_dir>/piqueld.sock` |
| derived database path | `<data_dir>/piqueld.db` |
| `docker.socket` | `/var/run/docker.sock` |
| `docker.auto_initialize_swarm` | `true` |
| `ingress.enabled` | `false` (restart required) |
| `reconciliation.scan_interval_seconds` | `60` |
| `reconciliation.prepare_timeout_seconds` | `300` |
| `reconciliation.convergence_timeout_seconds` | `120` |
| `retention.event_days` | `0` (application event pruning disabled) |
| `retention.daemon_event_days` | `0` (daemon event pruning disabled) |
| `metrics.listen` | `[]` (metrics-only listener disabled) |
| `notifications.enabled` | `false` |
| `retention.finished_operation_days` | `10` (`0` disables pruning; terminal operations older than the cutoff are pruned during each reconciliation cycle) |

Reconciliation intervals and timeouts are bounded to `1..=86400` seconds.
One async controller overlaps pending work. Internal global limits allow two
image resolutions, eight observations, and one resource mutation request. Timers
consume no I/O slot. These limits are not configurable.

The data directory is the only persistent daemon state. The daemon holds
exclusive OS locks on both directories for its lifetime. Separate instances
require separate data and runtime directories. A competing process fails before
opening the database or replacing a socket. Process exit (including a crash)
releases the locks; there are no lock files to remove.

The runtime directory is dedicated to piqueld. Its lock coordinates cooperating
daemon instances, and its permissions prevent operator-group members from
replacing entries. Processes running as the daemon user or root must also honor
the lock; an unrelated process with that identity can otherwise replace the
socket during recovery. The lock is not an isolation boundary between processes
sharing the daemon's identity.

Under the runtime lock, startup probes an existing socket. An active listener is
left untouched; a connection-refused socket is removed and rebound. Unexpected
files, symlinks, timeouts, and other probe errors stop startup without replacing
the path. Both listeners are bound before reconciliation starts.

The CLI now defaults to `/run/piqueld/piqueld.sock`; it does not fall back to the
old state-directory socket. Existing custom installations must prepare a runtime
directory and update their configuration. CLI socket overrides and profiles
remain available for custom locations.

The dashboard is not configurable at runtime: it is embedded when the daemon
is built with the `embedded-ui` cargo feature and absent otherwise. It is
served on the TCP listener only, so a TCP listen mode must be enabled to reach
it; the Unix API socket serves the API alone.

`[build_history]` bounds persisted build output: `log_max_bytes` defaults to
4194304 (maximum 64 MiB), and `log_retention_days` to 30 (1–3650). Build metadata
remains until the application is deleted. Expiration removes output chunks while
retaining the attempt and an explicit expired indicator.

## TCP listen modes and Tailscale

`server.listen_mode` selects `off` (Unix socket only), `localhost` (127.0.0.1
and ::1), `tailscale` (the host's Tailscale IPv4 and IPv6 addresses), or `both`.
Every selected TCP address uses `server.port`, which must be 1–65535. The Unix
socket remains available in every mode. The old `server.http_listen` setting is
rejected; replace it with `listen_mode` and `port`.

For remote access, install and connect Tailscale on the daemon host and client:

```toml
[server]
listen_mode = "tailscale"
port = 7845
```

At startup, piqueld runs `tailscale status --json --peers=false` with a five-second
timeout. If the command is missing, fails, or reports Tailscale is not running
or has no addresses, piqueld logs a warning and continues with the remaining
listeners. It never retries discovery: restart piqueld after bringing Tailscale
up or changing its addresses. Any actual bind failure aborts startup, including
a failure on just one address. Direct binding requires Tailscale's normal network
interface; userspace-only networking is not supported.

Every API caller must authenticate, including over Tailscale. Use tailnet policy
to control who can connect. HTTP traffic
between tailnet nodes is encrypted by Tailscale; piqueld does not manage HTTPS,
Tailscale Serve, enrollment, or certificates. Set `listen_mode = "localhost"`
to remove the Tailscale listener, or `"off"` for Unix-socket-only access, and
restart the daemon. Independently configured proxies can still expose localhost.

TCP requests must use `localhost`, a literal IP address, or a DNS hostname listed
in `server.allowed_hosts`. To use a Tailscale DNS name, for example, set
`allowed_hosts = ["my-host.my-tailnet.ts.net"]` in `[server]`. Entries are exact
hostnames without ports, schemes, or wildcards. Only add names controlled by
trusted operators; this allowlist prevents an unrelated domain from rebinding
to the daemon's address. DNS names are not discovered or trusted automatically.

Authentication validates API mutations against `auth.public_url`: any supplied
`Origin` must match it, and browser login, registration, and cookie-authenticated
mutations require it. A TLS-terminating proxy must preserve the browser's HTTPS
`Origin`, even when forwarding over HTTP; piqueld does not trust forwarded headers.
Keep both the proxy and daemon private, reachable only over a trusted LAN or
tailnet, never the internet.

TCP additionally rejects mutations with cross-site or same-site Fetch Metadata.
Native clients without browser headers continue to work. The Unix socket skips
the TCP hostname and Fetch Metadata checks; authentication and its origin checks
still apply on every API transport.

See [observability](observability.md) for notification category switches, webhook
destinations, metrics exposure, diagnostic ownership and retention semantics.

## Authentication origin

`auth.public_url` is the canonical HTTPS website origin (default
`http://localhost:7845` for development). Remote passkey login requires HTTPS
even over Tailscale. See [authentication](authentication.md) for reverse-proxy
setup, the private first-account link, invitations, and credential lifetimes.

## Secret storage

The first secret write creates `secrets.key` in `server.data_dir`, atomically and
with mode 0600. It holds a 32-byte master key. Back up this key together with the
database; losing it makes encrypted values unrecoverable. Once the database has
accepted a secret, a missing key is only regenerated after explicit lost-key
recovery, even when all secrets are deleted. New writes authenticate a persistent
key verifier; replacing the key with a different valid 32-byte file fails closed.
Restore the original key, owned by the daemon user with private permissions.
Secret metadata remains readable without it. Key failures are recorded as
`secret_storage_unavailable` diagnostics; see [observability](observability.md).

Secret values use authenticated XChaCha20-Poly1305 encryption, binding ciphertext
to application, logical name and version. The implementation uses
[RustCrypto's existing AEAD implementation](https://docs.rs/chacha20poly1305/0.10.1/chacha20poly1305/).
Docker receives values only when provisioning a service's immutable secret file
versions. File mounts use Docker's read-only 0444 permissions inside the container.

### Recovering from a lost key

Restore `secrets.key` from backup whenever possible. If it cannot be recovered,
`piquelctl secrets recover-key --yes` discards **all stored values for every
application on the daemon**. It refuses while the current key still works.
Names, generations, file references and deployment history remain; discarded
versions are marked unavailable. Docker secrets and running services are left in
place. An unusable key file is renamed to `secrets.key.retired-<ms>` rather than
deleted, since it may match another database backup.

Supply new values under the existing names, then explicitly Deploy. The first new
value generates a fresh key. Deployments pinned to discarded values fail with
`secret_unavailable` before changing running services. Recovery replaces the
storage key, not the passwords or API tokens themselves, and is not a guarantee of
secure erasure from existing backups. Back up the new key with the database.

## Managed application ingress

`[ingress] enabled = true` enables the installation-owned Caddy gateway on TCP
ports 80/443. This setting is read once at startup and is read-only in the UI.
Enabling requires Docker Engine 28+ and API 1.48+; the daemon reports gateway
startup, port conflicts, and version failures through ingress health without
stopping application management. Application images' own ports remain private.

Restart after changing the TOML. With ingress enabled, stopping piqueld leaves
Caddy serving independently. Restarting piqueld with ingress disabled removes the
managed gateway container and its restart policy, closing public listeners while
retaining route intent, reservations, and certificates. No DNS or certificate work
runs while disabled. Re-enabling restores deployed routes, including deployments
made while disabled, without activating saved-but-undeployed changes.

Routes cannot use the `auth.public_url` hostname or its subdomains, and the
website's reverse proxy cannot share the gateway's ports on the same address.

Gateway certificates, accepted configuration, and the private administration socket
live below `<data_dir>/ingress`. Only dedicated subdirectories are mounted into
Caddy; it receives neither the Docker socket nor the daemon API socket/database.
See [ingress](ingress.md) for DNS, networking, lifecycle, and status details.
