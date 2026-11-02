# Configuration

`piqueld` reads `/etc/piqueld/config.toml` when no option is supplied. An
explicit file is selected with:

```console
piqueld --config /path/to/config.toml
```

[Daemon configuration settings](configuration-settings.md) lists every
setting with its type and default.

Check a file without starting the daemon:

```console
piqueld check-config --config /path/to/config.toml
```

It applies the same parsing and validation as startup and exits nonzero with a
diagnostic on the first problem. It touches nothing on the host: no state,
sockets, Docker, or network. Credential files are only located, not read, so it
runs where they are absent, such as a Nix build: relative `_file` paths are
accepted without `$CREDENTIALS_DIRECTORY`, and their contents (readable regular files,
non-empty, a valid tunnel credentials file or webhook URL) are checked at
startup.

`--log-file <path>` also appends the daemon's logs to a file as JSON lines,
created with mode `0600`. Standard output keeps its format: plain text on a
terminal or the systemd journal, JSON elsewhere.

The daemon serves its API and dashboard even while Docker is down, not yet
started (its socket missing), or not a Swarm manager. Reconciliation, image
cleanup and ingress start once Docker answers
as a single-node Swarm manager, retried with backoff up to 10 seconds apart and
journaled as an `ensure_swarm` action. Until then requested deployments wait.
Readiness probes Docker and Swarm directly, so once they recover it can report
ready up to 10 seconds before deployments resume.

SIGINT or SIGTERM stops the daemon gracefully: open connections get 10
seconds, and every task one minute, before it exits anyway. A second signal
exits at once.

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
[host operator](../accounts/recovery.md#the-host-operator).

The service manager or installer must create the runtime directory before
startup. Use daemon ownership and mode `0750` for group access, or `0700` for
private development. Group-readable/traversable runtime directories must use
the daemon's effective group. Group write and all access by others are rejected.
Both paths reject symlinks and ancestors vulnerable to replacement by untrusted
users. Both final directories must grant their owner read, write, and execute
access. Existing directory permissions are never changed.

For the development example, run `mkdir -p -m 0700 /tmp/piqueld-dev-run` first;
`just dev` handles this automatically.

Reconciliation intervals and timeouts are bounded to `1..=86400` seconds.
The convergence timeout bounds how long a deployment waits without progress:
it restarts each time a service converges, so each link of a
[startup dependency](manifest.md#startup-dependencies) chain gets
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

`[previews]` bounds what [previews](manifest.md#previews), often
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
being deleted count until they are gone, since their resources are not freed
before then, and repeating the creation of an existing preview always
succeeds. The check runs in the creating transaction, so
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
next deployment; until then, previews deployed before the limits existed run
without them, which status reports as unlimited replicas. Previews are never deleted automatically.


`[build_history]` bounds persisted build output: `log_max_bytes` defaults to
4194304 (maximum 64 MiB), and `log_retention_days` to 30 (1–3650). Build metadata
remains until the application is deleted. Expiration removes output chunks while
retaining the attempt and an explicit expired indicator.

`[images]` controls cleanup of the images this installation builds. It keeps
what every environment and preview runs or is deploying, plus each
environment's last `keep_deployments` successful deployments, so their releases
can be promoted again, and removes the rest one by one, by ID. Images other installations
built on the same Docker Engine, images built by hand, and pulled registry
images are never removed. See
[image retention](../../dev/architecture/docker-reconciliation.md#image-retention).

```toml
[images]
keep_deployments = 3
cleanup_interval_seconds = 3600
```

## Local snapshots

`[snapshots]` bounds the snapshots of local worktrees that
`piquelctl preview deploy --local` uploads:

```toml
[snapshots]
max_upload_bytes = 268435456
```

`max_upload_bytes`, at least 1, bounds the Git bundle `piquelctl preview deploy --local`
uploads, which is streamed to `<data_dir>/snapshots` and refused with 413
`snapshot_too_large` once it exceeds the limit. It is separate from the 2 MiB
limit of other request bodies. Bundles are usually thin, carrying only what
the application's repository lacks, so the limit mostly matters for branches
whose history the repository does not have. `<data_dir>/snapshots` keeps only
the snapshots previews' latest deployments use; the others are removed on the
next full reconciliation scan. It need not be backed up. It is a section of its
own, rather than part of `[previews]`, because the API reports the
`[previews]` limits and older clients reject fields they don't know.

A bundle is kept as uploaded, so receiving one takes about as much disk as the
upload, and every allocation Git makes for it is capped at 1 GiB, so a small
bundle of huge objects cannot exhaust memory. Its files must also fit
[`checkouts.max_bytes`](#git-checkouts), or no deployment could check it out:
a larger snapshot is refused with 413 `snapshot_too_large`, as is one whose
base history, fetched from the application's repository, takes more than
`max_bytes`. An upload holds
one of two receiving slots until its deployment is accepted or refused, so at
most two unaccepted uploads are on disk at once.

## Git checkouts

Deployments check Git repositories out to read their manifest and build
`git` sources, in `<data_dir>/checkouts`, which is emptied at startup:

```toml
[checkouts]
max_bytes = 4294967296
```

`max_bytes`, at least 1, bounds the bytes all checkouts, history and
working tree, take at once. Clones are shallow, so a checkout's history is
about one compressed copy of its tree. At most four clones run at once, and
one is stopped once it has downloaded more than a quarter of `max_bytes`.
Each tree is then measured before its files are written, each file and
directory charged its size in whole 4 KiB blocks, at least one, plus 256
bytes for Git's index; the history counts as the disk it takes. A checkout
waits until it fits beside the others, and one larger than `max_bytes`, or with
more than 500,000 files and directories, fails. Once checked out, the disk a
checkout really takes is compared with what it was charged: a checkout that
takes more than the budget has room for fails. Clones still downloading are not counted, so
checkouts take at most twice `max_bytes`. Files are checked out as
committed: the repository's attributes cannot run filters such as LFS's, or
convert encodings, line endings or `$Id$`. Git runs with the daemon user's
configuration and credentials but ignores the host-wide `/etc/gitconfig`, so an
operator setting such as rewriting GitHub URLs to SSH does not reach checkouts. A pinned commit that the server
won't serve by itself is found in the history of every branch. `GET /api/v1/system/status` reports the limit, so `piquelctl
preview deploy --local` can refuse a snapshot that would not fit before
packing it.

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

See [observability](../guides/observability.md) for telemetry export, diagnostic
ownership and retention semantics. [Notification destinations](../guides/observability.md#notifications)
are managed through the API, CLI and dashboard rather than this file; a
`[notifications]` table is rejected as unknown.

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
- piqueld owns the node's preferences and its Serve configuration for
  `https_port`, and rewrites both at every start.
- `auth_key_file` is only needed for the first login. Without it, startup logs a
  login URL and waits until the node is approved.
- An unset `auth.public_url` becomes `https://<hostname>.<tailnet>.ts.net`,
  unless `ingress.private.dashboard_hostname` is set. A value that differs is
  allowed, but is reported in `piquelctl status`
  and the dashboard, because passkeys only work on `auth.public_url`.
- The node name is trusted automatically; it does not need to be listed in
  `server.allowed_hosts`.
- The node has its own tailnet IP, so it never competes with managed ingress
  for port 443.
- piqueld asks its `tailscaled` who each peer is (`tailscale whois`, one
  lookup per address at a time). The [audit trail](../guides/observability.md#audit-trail) records
  that tailnet user or tag and node, cached for a minute per address. API
  tokens can be [bound to a tailnet identity](../accounts/cli-and-tokens.md#bind-a-token-to-tailnet-devices),
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
[development instances](../../dev/development.md#tailnet) do:

```toml
[tailscale]
enabled = true
socket = "/tmp/piqueld-dev-1000/tailnet/tailscaled.sock"
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

`auth.public_url` is the canonical HTTPS website origin. It defaults to
`https://<ingress.private.dashboard_hostname>` when that is set, then to the
tailnet node's URL when `[tailscale]` is enabled, and to `http://localhost:7845`
otherwise. Remote passkey login requires HTTPS even over Tailscale. See
[your own reverse proxy](../getting-started/install.md#your-own-reverse-proxy),
[creating your account](../getting-started/first-account.md),
[invitations](../accounts/accounts.md#invite-someone), and
[credential lifetimes](../accounts/cli-and-tokens.md#how-long-credentials-last).

`auth.max_token_days` limits the lifetime of new API tokens. Unset (the default)
allows any lifetime, including tokens that never expire; with a limit, tokens must
expire within it. Existing tokens are unaffected. See
[API tokens](../accounts/cli-and-tokens.md#create-an-api-token).

```toml
[auth]
max_token_days = 90
```

## Credential files

Settings that carry credentials are only accepted as `_file` settings, which
read the value from a file at startup. Surrounding whitespace, such as a
trailing newline, is removed, and an empty file is rejected, as is anything but
a regular file (symlinks to one are followed).
Relative paths resolve against `$CREDENTIALS_DIRECTORY` and may not contain
`.` or `..` components, so systemd
`LoadCredential=`, agenix and sops-nix work unchanged:

```toml
[telemetry]
endpoint = "https://otel.example.com:4317"
headers_file = "otlp-headers" # $CREDENTIALS_DIRECTORY/otlp-headers
```

`tailscale.auth_key_file` follows the same path rules, but piqueld hands the path to Tailscale instead of reading the key, so the
file only has to exist for the node's first login.
piqueld reads `ingress.private.auth_key_file` at
startup and writes it, mode 0600, to `<data_dir>/ingress/tailscale/config/auth-key`
for the apps node's container, which only uses it for its first login.
piqueld reads `ingress.tunnel.credentials_file` at
startup, checks that it is a tunnel credentials file, and writes it, mode 0600, to
`<data_dir>/ingress/tunnel/credentials.json` for the `cloudflared` container.
piqueld reads `telemetry.headers_file` at
startup and sends its `name: value` lines as gRPC headers with every
[telemetry export](../guides/observability.md#telemetry-export).

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
`secret_storage_unavailable` diagnostics; see [observability](../guides/observability.md).

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
versions are marked unavailable. [Notification destinations](../guides/observability.md#notifications),
whose URLs are encrypted with the same key, are deleted; create them again. Docker secrets and running services are left in
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
Caddy serving independently and renewing the certificates it obtains itself.
Restarting piqueld with ingress disabled removes the managed gateway container and its restart policy, closing public listeners while
retaining route intent, reservations, and Caddy's certificates. While disabled,
DNS-01 certificates are not renewed and are deleted once expired, and managed DNS
records are deleted. Re-enabling restores deployed routes, including deployments
made while disabled, without activating saved-but-undeployed changes.

`public_addresses` lists the server's public IPv4 and IPv6 addresses. Where a
DNS provider manages records, direct public routes get A/AAAA records to them;
without any, those records stay manual. Tunnel mode does not use them. See
[managed DNS records](../networking/dns-records.md).

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
See [private routes](../networking/private-routes.md).

`ingress.private.dashboard_hostname` serves the website (dashboard and API) to the
tailnet on your own domain, through the private listener like a private route.
An unset `auth.public_url` becomes its HTTPS origin, and applications may not
route it or its subdomains. It requires `ingress.enabled` and
`ingress.private.enabled`, and is read once at startup. See
[dashboard hostname](../networking/private-routes.md#serve-the-dashboard-on-your-own-domain).

`[ingress.tunnel]` serves public routes through a locally managed Cloudflare
Tunnel instead of ports 80/443, which are then closed:

```toml
[ingress.tunnel]
enabled = true
credentials_file = "cloudflared-tunnel.json"   # from `cloudflared tunnel create piqueld`
```

It is read once at startup. Each public hostname then needs a proxied CNAME to
`<tunnel-id>.cfargotunnel.com`, and Cloudflare sees public traffic in plaintext.
See [Cloudflare Tunnel](../networking/cloudflare.md#serve-public-routes-through-a-cloudflare-tunnel).

`ingress.webhook_hostname` serves GitHub push webhooks, and nothing else, on a
dedicated public hostname, for applications whose repository syncs with
`webhook`. The daemon then always serves the webhook socket; the gateway
forwards to it while ingress is enabled. It is read once at startup. See
[push webhooks](../networking/webhooks.md).

Gateway certificates, accepted configuration, and the private administration socket
live below `<data_dir>/ingress`. Only dedicated subdirectories are mounted into
Caddy; it receives neither the Docker socket nor the daemon API socket/database.
The control mount also holds the webhook socket, which serves only webhook
deliveries.
See [Serving an application over HTTPS](../networking/routes.md) for route
status, and [managing DNS records](../networking/dns-records.md) for DNS.

## DNS providers

`[[dns.providers]]` lists DNS provider accounts. piqueld uses them to obtain
certificates through ACME DNS-01 for hostnames a public CA cannot reach, such as
private routes, and, with `manage_records = true`, to create and maintain routes'
A/AAAA/CNAME records in their zones (see
[managed DNS records](../networking/dns-records.md)). Credentials are accepted only as `_file` settings; relative
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
[DNS-01 certificates](../networking/wildcards.md#dns-01-certificates) for issuance and renewal.
