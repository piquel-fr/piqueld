# Configuration

`piqueld` reads `/etc/piqueld/config.toml` when no option is supplied. An
explicit file is selected with:

```console
piqueld --config /path/to/config.toml
```

`--log-file <path>` also appends the daemon's logs to a file as JSON lines,
created with mode `0600`. Standard output keeps its format: plain text on a
terminal or the systemd journal, JSON elsewhere.

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
connections; account authentication is also required, except for root and the
daemon's own user, who act as the
[host operator](authentication.md#the-host-operator).

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
| `ingress.public_addresses` | `[]` (direct public routes' records stay manual) |
| `ingress.acme.directory` | `https://acme-v02.api.letsencrypt.org/directory` |
| `ingress.acme.email` | none |
| `ingress.private.enabled` | `false` (restart required) |
| `ingress.private.hostname` | `"piqueld-apps"` |
| `ingress.private.auth_key_file` | none |
| `ingress.tunnel.enabled` | `false` (restart required) |
| `ingress.tunnel.credentials_file` | none; required while enabled |
| `dns.providers` | `[]` |
| `dns.providers[].manage_records` | `false` |
| `reconciliation.scan_interval_seconds` | `60` |
| `reconciliation.prepare_timeout_seconds` | `300` |
| `reconciliation.convergence_timeout_seconds` | `120` |
| `retention.event_days` | `90` (`0` disables application event pruning) |
| `retention.daemon_event_days` | `90` (`0` disables daemon event pruning) |
| `retention.audit_days` | `365` (`0` disables audit trail pruning) |
| `metrics.listen` | `[]` (metrics-only listener disabled) |
| `notifications.enabled` | `false` |
| `notifications.security` | `true` ([security notifications](observability.md#security-notifications); like every category, sent only when `notifications.enabled`) |
| `retention.finished_operation_days` | `10` (`0` disables pruning; terminal operations older than the cutoff are pruned during each reconciliation cycle) |
| `previews.max_per_application` | `10` |
| `previews.max_total` | `30` |
| `previews.default_cpu_millis` | `500` |
| `previews.default_memory_bytes` | `536870912` (512 MiB) |
| `previews.max_replicas` | `1` |

Reconciliation intervals and timeouts are bounded to `1..=86400` seconds.
The convergence timeout bounds how long a deployment waits without progress:
it restarts each time a service converges, so each link of a
[startup dependency](application-manifest.md#startup-dependencies) chain gets
the full timeout.
One async controller overlaps pending work. Internal global limits allow two
image resolutions, one image build, eight observations, and one resource
mutation request. Timers consume no I/O slot. These limits are not
configurable. Waiting environment builds go before waiting preview builds,
except that a preview build goes next once three environment builds went ahead
of it, so a steady stream of environment builds cannot starve previews. A
running build is never interrupted. Waiting for the build counts against the
preparation timeout.

The data directory is the only persistent daemon state. Back up the database,
`secrets.key` (see below), and `<data_dir>/ingress/acme` and
`<data_dir>/ingress/certificates`, which hold the ACME account key and the
DNS-01 certificates with their private keys (all mode 0600), and
`<data_dir>/ingress/tailscale`, the apps tailnet node's identity. Lost certificates
are reissued, but each reissue counts against the CA's rate limits. The daemon holds
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

## Previews

`[previews]` bounds what [previews](application-manifest.md#previews), often
created by agents, may use on the node that also runs production:

```toml
[previews]
max_per_application = 10
max_total = 30
default_cpu_millis = 500
default_memory_bytes = 536870912
max_replicas = 1
```

Creating a preview that would take its application's previews past
`max_per_application`, or the installation's past `max_total`, fails with
`preview_limit_reached`. The error lists the previews counted, with their
branch, slot, slug and last deployment, so a caller can delete one. Previews
being deleted no longer count, and repeating the creation of an existing
preview always succeeds. The check runs in the creating transaction, so
concurrent creations cannot pass a limit together. Zero refuses every new
preview.

When a preview's manifest is rendered, each CPU or memory limit a service
leaves unset gets `default_cpu_millis` or `default_memory_bytes`, and replicas
above `max_replicas` are capped. Explicit limits are kept, even above the
defaults. Each change is a warning on the plan and the deployment, and is shown
on the preview. Environments are never bounded. The defaults must be values a
manifest could set: 1–1048576 millicores, a nonzero memory limit, and 1–100
replicas.

Lowering a limit never deletes or redeploys anything. Previews over a lowered
count keep running and only new ones are refused; `piquelctl status` and the
dashboard show the overage. Previews pick up changed resource bounds on their
next deployment. Previews are never deleted automatically.

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
between tailnet nodes is encrypted by Tailscale; these listeners serve plain
HTTP. For HTTPS, use a [tailnet node](#tailnet-node) instead. Set `listen_mode = "localhost"`
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

## Tailnet node

With `[tailscale]` enabled, piqueld joins the tailnet as its own node and serves
the website and API over HTTPS on port 443 (`https_port`) of that node. piqueld starts and
supervises a dedicated `tailscaled` in userspace-networking mode, separate from
any host Tailscale daemon, so `tailscaled` and `tailscale` must be on `PATH`.
That `tailscaled` terminates TLS with the tailnet-issued certificate and
forwards connections to a loopback listener with a PROXY protocol header that
carries the client's tailnet address. The listener accepts connections only
from processes running as the daemon's own user that still hold their end of
the connection, checked against the kernel's socket table, so other local
users cannot pose as a tailnet peer.

```toml
[tailscale]
enabled = true
hostname = "piqueld"      # piqueld.<tailnet>.ts.net
auth_key_file = "ts-auth-key" # $CREDENTIALS_DIRECTORY/ts-auth-key
```

- The tailnet needs MagicDNS and HTTPS certificates enabled.
- Node state and the `tailscaled` socket live in `<data_dir>/tailscale`, so the
  node identity, its name and the passkeys bound to it follow the data directory
  rather than the host.
- piqueld owns the node's preferences and its Serve configuration for port 443,
  and rewrites both at every start.
- `auth_key_file` is only needed for the first login. Without it, startup logs a
  login URL and waits until the node is approved.
- An unset `auth.public_url` becomes `https://<hostname>.<tailnet>.ts.net`. An
  explicit value that differs is allowed, but is reported in `piquelctl status`
  and the dashboard, because passkeys only work on `auth.public_url`.
- The node name is trusted automatically; it does not need to be listed in
  `server.allowed_hosts`.
- The node has its own tailnet IP, so it never competes with managed ingress
  for port 443.
- piqueld asks its `tailscaled` who each peer is (`tailscale whois`, one
  lookup at a time). The [audit trail](observability.md#audit-trail) records
  that tailnet user or tag and node, cached for a minute per address. API
  tokens can be [bound to a tailnet identity](authorization.md#tailnet-bound-tokens),
  which is always checked with a fresh lookup.

Startup fails if the node cannot start or obtain a certificate. While running,
piqueld refreshes the login state and certificate every minute; Tailscale renews
the certificate before it expires. `piquelctl status` and the dashboard report
the login state, certificate expiry, and whether `auth.public_url` matches. If
`tailscaled` exits, piqueld stops so the service manager can restart both.
`tailscaled` logs are forwarded at debug level.

`listen_mode` is independent and remains available for installations without a
dedicated node.

### Sharing a tailscaled

`socket` makes piqueld use a `tailscaled` that is already logged in instead of
starting its own, and `https_port` serves the website on another port of the
node. Several daemons can then share one node, each on its own port, as the
[development instances](development.md#tailnet) do:

```toml
[tailscale]
enabled = true
socket = "/tmp/piqueld-dev/tailnet/tailscaled.sock"
https_port = 7846         # https://<node>.<tailnet>.ts.net:7846
```

- piqueld only adds its port to the node's Serve configuration, and removes it
  when it stops. It neither logs the node in nor changes its preferences, so
  `hostname` is unused and `auth_key_file` is rejected; startup fails unless
  the node is logged in.
- The `tailscaled` must run as the daemon's user, because the forwarding
  listener only accepts that user's connections.
- An unset `auth.public_url` becomes `https://<node>:<https_port>`.
- piqueld keeps running if the shared `tailscaled` exits; `piquelctl status`
  and the dashboard report the node as unhealthy until it is back.

## Authentication origin

`auth.public_url` is the canonical HTTPS website origin. It defaults to the
tailnet node's URL when `[tailscale]` is enabled, and to `http://localhost:7845`
otherwise. Remote passkey login requires HTTPS even over Tailscale. See
[authentication](authentication.md) for reverse-proxy setup, the private
first-account link, invitations, and credential lifetimes.

`auth.max_token_days` limits the lifetime of new API tokens. Unset (the default)
allows any lifetime, including tokens that never expire; with a limit, tokens must
expire within it. Existing tokens are unaffected. See
[authorization](authorization.md#api-tokens).

```toml
[auth]
max_token_days = 90
```

## Credential files

Settings that carry credentials, such as webhook `url`, also accept a `_file`
variant (`url_file`) that reads the value from a file at startup. Setting both
variants is an error. Surrounding whitespace, such as a trailing newline, is
removed, and an empty file is rejected.
Relative paths resolve against `$CREDENTIALS_DIRECTORY`, so systemd
`LoadCredential=`, agenix and sops-nix work unchanged:

```toml
[[notifications.destinations]]
name = "discord"
kind = "discord"
url_file = "discord-webhook" # $CREDENTIALS_DIRECTORY/discord-webhook
```

`tailscale.auth_key_file` has no inline variant and follows the same path
rules, but piqueld hands the path to Tailscale instead of reading the key, so the
file only has to exist for the node's first login.
`ingress.private.auth_key_file` has no inline variant either. piqueld reads it at
startup and writes it, mode 0600, to `<data_dir>/ingress/tailscale/config/auth-key`
for the apps node's container, which only uses it for its first login.
`ingress.tunnel.credentials_file` has no inline variant: piqueld reads it at
startup, checks that it is a tunnel credentials file, and writes it, mode 0600, to
`<data_dir>/ingress/tunnel/credentials.json` for the `cloudflared` container.

Errors and the read-only settings view name the file a value came from, never
the value. Restart the daemon after changing a credential file.

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
ports 80/443, or behind a Cloudflare Tunnel. This setting is read once at startup and is read-only in the UI.
Enabling requires Docker Engine 28+ and API 1.48+; the daemon reports gateway
startup, port conflicts, and version failures through ingress health without
stopping application management. Application images' own ports remain private.

Restart after changing the TOML. With ingress enabled, stopping piqueld leaves
Caddy serving independently. Restarting piqueld with ingress disabled removes the
managed gateway container and its restart policy, closing public listeners while
retaining route intent, reservations, and certificates. No DNS or certificate work
runs while disabled. Re-enabling restores deployed routes, including deployments
made while disabled, without activating saved-but-undeployed changes.

`public_addresses` lists the server's public IPv4 and IPv6 addresses. Where a
DNS provider manages records, direct public routes get A/AAAA records to them;
without any, those records stay manual. Tunnel mode does not use them. See
[managed DNS records](ingress.md#managed-dns-records).

```toml
[ingress]
enabled = true
public_addresses = ["203.0.113.10", "2001:db8::10"]
```

Routes cannot use the `auth.public_url` hostname or its subdomains, and the
website's reverse proxy cannot share the gateway's ports on the same address.

`[ingress.private]` serves private routes to the tailnet through a second tailnet
node, the apps node, run as a container beside the gateway:

```toml
[ingress.private]
enabled = true
hostname = "piqueld-apps"            # piqueld-apps.<tailnet>.ts.net
auth_key_file = "ts-apps-auth-key"   # $CREDENTIALS_DIRECTORY/ts-apps-auth-key
```

`hostname` must be a single DNS label, different from `tailscale.hostname` when
both nodes run. Without `auth_key_file`, the daemon logs the node's login URL.
It is read once at startup; while disabled, private routes report `disabled`.
See [public and private routes](ingress.md#public-and-private-routes).

`[ingress.tunnel]` serves public routes through a locally managed Cloudflare
Tunnel instead of ports 80/443, which are then closed:

```toml
[ingress.tunnel]
enabled = true
credentials_file = "cloudflared-tunnel.json"   # from `cloudflared tunnel create piqueld`
```

It is read once at startup. Each public hostname then needs a proxied CNAME to
`<tunnel-id>.cfargotunnel.com`, and Cloudflare sees public traffic in plaintext.
See [Cloudflare Tunnel](ingress.md#cloudflare-tunnel).

Gateway certificates, accepted configuration, and the private administration socket
live below `<data_dir>/ingress`. Only dedicated subdirectories are mounted into
Caddy; it receives neither the Docker socket nor the daemon API socket/database.
See [ingress](ingress.md) for DNS, networking, lifecycle, and status details.

## DNS providers

`[[dns.providers]]` lists DNS provider accounts. piqueld uses them to obtain
certificates through ACME DNS-01 for hostnames a public CA cannot reach, such as
private routes, and, with `manage_records = true`, to create and maintain routes'
A/AAAA/CNAME records in their zones (see
[managed DNS records](ingress.md#managed-dns-records)). Credentials are accepted only as `_file` settings; relative
paths resolve against `$CREDENTIALS_DIRECTORY`. They are read once at startup and
never enter a container. Settings show only their file paths.

```toml
[ingress.acme]
# directory = "https://acme-v02.api.letsencrypt.org/directory"
email = "admin@example.com" # optional

[[dns.providers]]
kind = "cloudflare"
api_token_file = "cloudflare-dns-token"   # Zone:Read and DNS:Edit
manage_records = true                     # optional, off by default

[[dns.providers]]
kind = "ovh"
endpoint = "ovh-eu"                       # or "ovh-ca", "ovh-us"
application_key_file = "ovh-application-key"
application_secret_file = "ovh-application-secret"
consumer_key_file = "ovh-consumer-key"
```

The OVH consumer key needs `GET /domain/zone`, `GET /auth/time`, and `POST`,
`DELETE` on `/domain/zone/*`; with `manage_records`, also `GET` and `PUT` on
`/domain/zone/*`. Zones are discovered through each provider's API at
startup and hourly, which also checks the credentials; a failed discovery is
retried every minute. To check now, for example after changing a token's
permissions or adding a zone, run `piquelctl dns refresh`, press **Check DNS
providers** in the dashboard's System status panel, or call
`POST /api/v1/system/dns/refresh`. Replacing a credential file needs a restart.
A hostname belongs to the provider with the longest matching
zone; a zone claimed by two providers is a conflict and is not used. See
[ingress](ingress.md#dns-01-certificates) for issuance and renewal.
