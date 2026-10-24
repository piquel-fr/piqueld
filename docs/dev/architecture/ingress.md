# Ingress

This page describes how piqueld's managed ingress works, for people changing
`apps/piqueld/src/ingress`, `apps/piqueld/src/dns` or the route handling around
them. The user guide's [networking pages](../../user/networking/routes.md)
describe what users see and do.

Ingress is designed for a single-node installation used by one trusted person
or team. Applications own routes by hostname, exact or wildcard; the
installation owns the listeners, the certificate storage, and the containers
below.

## Components

`Ingress` (`ingress/mod.rs`) serializes every gateway change behind one writer
lock, and runs four loops until shutdown:

| Loop | Period | Does |
| --- | --- | --- |
| `run_gateway` | 10 s | Converges the gateway, the apps node and `cloudflared` with the routing table, updates health, relays container logs. |
| `run_probes` | 15 s | Probes each route the gateway acknowledged, up to four at once. |
| `run_certificates` | 1 min | Issues and renews [DNS-01 certificates](#dns-01-certificates). |
| `run_records` | 10 s | Reconciles [managed DNS records](#managed-dns-records), when a provider sets `manage_records`. |

Deployments call `Ingress::apply` under the same lock; see
[ingress coordination](docker-reconciliation.md#ingress-coordination) for how
route changes fit into a rollout.

piqueld runs up to three containers, each pinned to a version and digest in
`ingress/mod.rs` (`CADDY_IMAGE`, `TAILSCALE_IMAGE`, `CLOUDFLARED_IMAGE`) and
replaced when a piqueld release changes the pin:

- the Caddy **gateway**, `piqueld-ingress-<16 hex digits of the instance ID's SHA-256>`;
- the **apps tailnet node**, `<gateway>-tailscale`, while `[ingress.private]`
  is enabled;
- **`cloudflared`**, `<gateway>-cloudflared`, while `[ingress.tunnel]` is
  enabled.

All three run as the daemon's UID and GID, with every capability dropped,
`no-new-privileges`, a read-only root filesystem, `local` logs capped at 10 MB
× 3, and the `unless-stopped` restart policy. They keep running across daemon
restarts. Caddy alone adds `NET_BIND_SERVICE` and sets
`net.ipv4.ip_unprivileged_port_start=0`, to bind ports 80 and 443. Caddy and
the apps node get a 16 MiB `noexec,nosuid` tmpfs on `/tmp`. No container gets
the Docker socket or the daemon's API socket or database.

Each container's spec hash is stored in the `io.piqueld.ingress-configuration`
label; a container whose hash differs is replaced. A matching hash is not
trusted alone: the image, user, command, binds, capabilities, sysctls, tmpfs,
network, ports and environment are checked again, and privileged containers
are refused.

## Listeners

Caddy runs one server per listener, each with its own routes
(`ingress/configuration.rs`). Routes only target application services; the
daemon's website and API reach the gateway only through the
[webhook and dashboard sockets](#webhook-and-dashboard-sockets). Only the
gateway publishes ports, and only the public listener's; in tunnel mode,
nothing does.

| Servers | Listen on | Serve |
| --- | --- | --- |
| `public` (HTTPS), `public_http` | `:443` and `:80`, published on every host address | public routes, the webhook hostname |
| `tunnel` (plain HTTP), instead of both | `:8080`, not published | the same, from `cloudflared` |
| `private` (HTTPS), `private_http` | `:8443` and `:8081`, not published | private routes, the dashboard hostname |

- A hostname appears only on its own listener, and each HTTPS server's TLS
  connection policy matches only its own hostnames as SNI, with
  `strict_sni_host`. A forged SNI for a private route on the public listener
  gets no certificate. A forged Host for one gets a 404 on port 80; over HTTPS
  with another hostname's SNI, Caddy's strict SNI check refuses it.
- Each route serves the probe endpoint, `/.well-known/piqueld-ingress`, which
  returns the instance ID, then its handler. Every server ends with a 404.
- Known hosts redirect HTTP to HTTPS with a 308. Unknown HTTP hosts get a 404,
  and unknown TLS names no certificate.
- Services are proxied over HTTP/1.1, with a five-minute
  `stream_close_delay`, so WebSockets survive a reload for up to five
  minutes. Redirect routes are a `static_response`.
- Exact public hostnames use Caddy's automatic HTTPS, without on-demand
  issuance. Caddy skips wildcards, which use the DNS-01 certificates piqueld
  loads with `tls.certificates.load_pem`, and still manages an exact public
  hostname that a loaded wildcard certificate covers
  (`ignore_loaded_certificates`), so it never depends on it. Automatic HTTPS
  is off on the private servers.
- A wildcard host matches every name one label below its parent, except the
  exact hostnames of any listener: exact routes, and the webhook and
  dashboard hostnames. A private route's name therefore stays out of a public
  wildcard even while private ingress is off.

The private servers exist only while private ingress is enabled, and private
routes never fall back to the public listener. Application networks are
attached to the gateway, so applications can connect to the private listener
too. It therefore completes TLS, and answers HTTP, only for tailnet client
addresses (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`). Only peers on the edge
network may set the client address with a PROXY protocol header
(`proxy_protocol` with `fallback_policy: ignore`); anyone else's header is
ignored and their own address is used.

Application networks come from Swarm's default address pools. A pool
overlapping the tailnet ranges would let application containers pass for
tailnet clients, so the private listener stays off until the pools are read
from `docker info` and verified outside them. Its health then names the pool:
`Swarm address pool <pool> overlaps the tailnet ranges the private listener
trusts; …`. A successful check is cached for the process's lifetime.

## Networks

- The **edge network** is a non-internal bridge named like the gateway, with
  Docker gateway priority 1. It gives the gateway outbound DNS and ACME, and
  is the only network of the apps node and `cloudflared`. The host reaches it
  too, through the bridge's gateway address, which the private probe uses.
- Each environment with a proxied route has a dedicated attachable **ingress
  overlay**, shared by Caddy and the environment's routed services, separate
  from other environments' and from private backend networks. Environments
  with only redirect routes have none. Caddy joins overlays live with gateway
  priority 0, so attaching one doesn't replace the container. Docker Engine 28
  and API 1.48 are required for that priority; `ensure_gateway` checks the
  version, so only an enabled gateway needs it.

The gateway is trusted across every exposed environment.

## Client addresses

Backends receive connections from the gateway, not from clients. Caddy
replaces any client-supplied `X-Forwarded-For` with the address it accepted
the connection from, and sets `X-Forwarded-Proto` and `X-Forwarded-Host`.

Every service a proxied route targets receives `PIQUELD_INGRESS_PROXIES`, while
ingress is enabled: the comma-separated CIDRs of its environment's ingress
network, such as `10.0.3.0/24`. Manifests can't set the variable, and it is
hidden from the observed environment. It follows the network: piqueld updates
services if the network is recreated, and treats a modified value as drift.

Only the gateway, Swarm's load balancer for that network, and the
environment's routed services are attached to the ingress network. A forwarded
header from that range was therefore set by Caddy or by the application
itself: trusting the range also trusts every routed service and replica, and a
compromised one could forge client addresses. Private networks use different
ranges, so services outside the ingress network can't. Requests reaching Caddy
through Docker's userland proxy, such as IPv6 to an IPv4-only bridge, appear to
come from the bridge's gateway.

On the private listener, the client address is the tailnet address from the
apps node's PROXY header. On the tunnel listener, it is `Cf-Connecting-IP`,
trusted only from the edge network; tunnel routes set `X-Forwarded-For` to it
and `X-Forwarded-Proto` to `https`.

## Configuration changes and replacement

Gateway updates apply as one installation-wide configuration, loaded through
Caddy's private admin socket, `unix//control/admin.sock`, and persisted as its
autosave configuration, so the container restarts on its own configuration.
Ordinary configuration changes preserve listener availability. HTTP clients
must handle idle connections closing during a reload; active HTTP/2 streams and
WebSockets are covered by tests. This is not blue-green application deployment.

A spec change replaces the container:

1. Pull the pinned image if it is missing, in its own `ingress_pull_image`
   action, within 3 minutes.
2. Validate the Caddy configuration with that image (`caddy validate`, in a
   throwaway container).
3. Create `<gateway>-next` and attach its networks while the old gateway still
   serves.
4. Save `rollback.json`, stop the old gateway and rename it
   `<gateway>-previous`, then start the new one.

A failed start restores the old gateway; an interrupted replacement is
recovered on the next pass. Replacement still needs a brief stop and start on
the shared ports, and Docker outages can delay recovery.

A missing or conflicting application network retains that application's
accepted destinations and keeps its new destinations from being applied.
Explicit withdrawals still proceed, and healthy applications update
independently. The gateway never attaches an unverified network. Health
reports the degraded state, and logs name the application and network to
repair. If a gateway upgrade is also pending, replacement waits until those
networks are repaired or their routes withdrawn; the running gateway keeps its
attachments and keeps applying other route changes meanwhile.

Switching between direct and tunnel mode changes the spec, but is not deferred
like an upgrade, since the old gateway can't serve the other mode's listeners.
While an application's retained routes lack a verified network, the switch
fails with `cannot switch between direct and tunnel ingress: retained routes
for application <id> lack a verified network; repair it or withdraw its
routes`, and the old gateway keeps serving in the old mode. Until then the
gateway applies no route changes for any application: every deployment or
deletion of an environment with routes fails at `Ingress::apply` with the
retryable `ingress_unavailable`, and private routes report `pending`.

Gateway requests don't hold the controller's global Docker mutation lock, so
deployments without routes proceed while routing waits.

## The apps tailnet node

Private traffic arrives through a second tailnet node, separate from the
daemon's own `[tailscale]` node (`tailnet/`): that one terminates TLS itself
and must not depend on Docker, while this one passes TLS through to Caddy
intact. A separate node also gets its own ACLs and tags.

The node (`ingress/node.rs`) runs `containerboot` on the edge network only,
with userspace networking (`TS_USERSPACE=true`), `TS_ACCEPT_DNS=false`,
`TS_AUTH_ONCE=true` and `TS_BOOT_TIMEOUT=24h`. Its serve configuration, owned
by piqueld, forwards tailnet TCP 443 to `<gateway>:8443` and TCP 80 to
`<gateway>:8081`, with a PROXY v2 header:

```text
{"TCP":{"443":{"TCPForward":"<gateway>:8443","ProxyProtocol":2},"80":{"TCPForward":"<gateway>:8081","ProxyProtocol":2}}}
```

Its files live in `<data_dir>/ingress/tailscale`:

```text
state/                  node identity and preferences
run/tailscaled.sock     LocalAPI, read for status
config/serve.json       serve configuration
config/auth-key         first-login auth key (0600), when configured
```

The daemon reads `ingress.private.auth_key_file` at startup and writes it to
`config/auth-key`, which is removed when no key is configured. A fingerprint
of the key, in the `io.piqueld.ingress-auth-key` label, replaces the container
when the key changes. Later starts reuse the node state. Without a key, the
node's login URL is logged once at warning level, and status reports that it
needs login.

The node starts after the gateway has converged and the pool check has passed.
Its status comes from the LocalAPI's `/localapi/v0/status`: the backend state,
MagicDNS name and tailnet addresses. Changes are `ingress_start_tailnet`
journal actions, and the node is removed when ingress or `[ingress.private]`
is disabled. While its removal is unconfirmed, private routes report `failed`
rather than `disabled`. Its logs are relayed at debug level.

## Cloudflare Tunnel

The tunnel (`ingress/tunnel.rs`, `config/tunnel.rs`) is locally managed:
piqueld generates `cloudflared`'s configuration with a single catch-all rule
to `http://<gateway>:8080`, so adding or removing routes never touches
Cloudflare, and hostnames the gateway doesn't know get Caddy's 404.

The credentials file is read at startup only when `enabled = true`. It must be
JSON with `AccountTag` and `TunnelSecret` present and `TunnelID` a UUID.
piqueld writes it, mode 0600, to `<data_dir>/ingress/tunnel/credentials.json`
beside the generated `config.json`, both mounted read-only at
`/etc/cloudflared`. The credentials copy is deleted whenever `cloudflared`
isn't meant to run: once ingress is disabled, even when Docker fails, or once
the tunnel is disabled and the gateway has converged in direct mode again.

`cloudflared` runs on the edge network only, with the shared hardening above.
A credentials fingerprint is part of its spec hash, so new credentials replace
it. Changes are `ingress_start_tunnel` journal actions, and its logs are
relayed at info level. Its metrics listen on the container's loopback,
`127.0.0.1:2000`. A Docker healthcheck runs
`cloudflared tunnel --metrics 127.0.0.1:2000 ready` every 10 seconds (every 2
seconds during a 30-second start period, with a 5-second timeout and 1 retry),
and piqueld reads the outcome through the Docker API, so it needs no route to
the container.

The `tunnel` server listens for plain HTTP on `:8080`, unpublished, and serves
public routes, the webhook hostname and the probe endpoint, nothing else. The
`public` servers don't run. It has no certificate and no HTTP → HTTPS
redirect, since Cloudflare terminates TLS. It refuses every peer outside the
edge network. Only containers piqueld manages join that network: `cloudflared`,
and the apps node, which forwards only to the private listener. The host can
reach it too, as it can the private listener. `PIQUELD_INGRESS_PROXIES` is
unchanged in tunnel mode: backends still receive requests from the gateway.

While the tunnel is disconnected, ingress is unhealthy, since public routes are
unreachable. The public probe is unchanged: it reaches the probe endpoint
through Cloudflare, and the instance ID proves that the tunnel reaches this
gateway.

## Webhook and dashboard sockets

The gateway's control mount, `<data_dir>/ingress/control`, holds Caddy's admin
socket and two daemon sockets, bound with a umask that leaves them to the
daemon's user, which the gateway runs as:

- `webhooks.sock` serves only `POST /hooks/github/{id}`
  (`api/http/webhooks.rs`), with its own router and none of the API's
  middleware. The public or tunnel listener forwards
  `https://<webhook_hostname>/hooks/github/*` to it, with Caddy's
  `request_body` limit of 1 MiB. The daemon binds it whenever
  `ingress.webhook_hostname` is set, even with ingress disabled, without
  waiting for Swarm.
- `dashboard.sock` serves the same website as the daemon's TCP listeners. The
  private listener forwards every path of `ingress.private.dashboard_hostname`
  to it, setting `X-Forwarded-For` to the tailnet client. The daemon's gateway
  router trusts that header on this socket only, for login throttling and the
  audit trail; a request without exactly one valid IP gets 403
  `untrusted_client`. The daemon identifies each client with `tailscale whois`
  through the apps node's socket, `<data_dir>/ingress/tailscale/run/tailscaled.sock`,
  so the `tailscale` CLI must be on its `PATH`. The NixOS module adds it when
  `dashboard_hostname` is set. The hostname is served only while the private
  listener is.

The webhook endpoint verifies `X-Hub-Signature-256` (`sha256=<64 hex digits>`,
HMAC-SHA256 of the body) against the application's secret with OpenSSL's
constant-time comparison (`api/service/webhooks.rs`). Secrets are 32 random
bytes, hex-encoded, encrypted with the secret master key, and deleted by
lost-key recovery. Responses:

| Status | When |
| --- | --- |
| 204 | Verified. A `push` hints sync to list the repository soon, at most once per 30 seconds per repository URL; a hint inside the window is kept. Other events, or a missing `X-GitHub-Event`, change nothing. |
| 401 | Missing, malformed or wrong signature, unknown application, an ID that isn't one, or no secret. |
| 405 | Another method than `POST`. |
| 408 | The request, body included, took more than 10 seconds. |
| 413 | The body is over 1 MiB, in the daemon; Caddy's `request_body` limit has the same size, untested. |
| 503 | The secret can't be decrypted, or 16 deliveries are already running. |

The endpoint never reads the payload: sync resolves branch heads with
`git ls-remote` itself, and it doesn't check that the application syncs with
`webhook`. Signature failures log a warning and 503s an error; nothing is
audited.

## Hostname reservations

Hostnames are reserved per environment in `hostname_reservations`
(`store/ingress.rs`): the routes of its manifest rendered for that
environment, plus those its resolved spec, latest operation's target and
deployment manifest, and desired and applied gateway routes still hold. They
are recomputed in the transaction that changes their source, so they survive
pending deployments and incomplete withdrawals. Visibility doesn't affect
them. A unique violation means another environment owns the name; one of the
same application reports `hostname {hostname} is reserved by environment
<name> of this application`.

The installation reserves the effective `auth.public_url` hostname, the
dashboard hostname and the webhook hostname, with their subdomains, in
`installation_hostnames`, at every start (`api/service/startup.rs`). Only
names an environment doesn't hold already are checked against them, and
against other applications' wildcards (`under_foreign_wildcard`). An
environment that held a name before the installation claimed it keeps saving
and deploying, but `routing_table` withholds the route, and startup logs
`route hostname is reserved for the piqueld website or its webhooks; the
gateway will not publish it`. A `public_url` host that isn't a public DNS name,
such as `localhost`, isn't reserved. `webhook_hostname` must differ from
`dashboard_hostname`; nothing checks it against `auth.public_url`.

A wildcard reserves its pattern. An exact name below another application's
wildcard is refused, unless that application reserved it first.

## Route status and probes

Each route's state (`Ingress::route_state`) is, in order:

1. `disabled` while ingress is disabled, or `failed` while the gateway's
   shutdown is unconfirmed.
2. For a private route without private ingress, `disabled`, or `failed` while
   the node's removal is unconfirmed.
3. `failed` when it needs a DNS-01 certificate that has a problem.
4. `pending` until the gateway has acknowledged it (`applied_routes`), and,
   for a private route, while the node is unhealthy.
5. `ready` once its probe passes, else `pending`.

While a route isn't ready, a DNS records problem replaces its message with
`DNS records (<state>): <problem>`.

A `ready` public route means an HTTPS request from the daemon validated a
publicly trusted certificate and reached this gateway's probe endpoint. A
`ready` private route means the host's resolver answers exactly the apps
node's tailnet addresses, the node is logged in, and the private listener,
reached over the edge network through a local relay sending a PROXY header for
`100.100.100.100` and the hostname as SNI, served a trusted certificate and the
endpoint. The tailnet hop itself is not probed. A wildcard is probed on the
first name in `a`–`z`, `0`–`9` below its parent that no exact route claims
(`RouteHostname::served`).

The endpoint never invokes the application: a ready route is neither a backend
health check nor proof of reachability from every network. Applications should
configure container health checks to establish backend readiness before route
cutover. Public DNS or certificate delays don't fail a deployment once its
runtime and route configuration are applied; failure to apply required gateway
configuration does, retains the required old backends, and retries. Explicit
route removals withdraw exposure as the deployment starts, before obsolete
service and network cleanup.

A route's effective visibility, the stricter of its own and its environment's
or previews' ceiling, is computed when a deployment renders the manifest, and
stored with its deployed routes. A visibility change therefore counts as a
removal and an addition (`stage_routes`): the route leaves its old listener as
the deployment starts, and joins the new one once backends are ready.

## DNS-01 certificates

Private routes and the dashboard hostname, which a public CA can't reach, and
public wildcards outside tunnel mode get certificates through ACME DNS-01
instead of Caddy (`ingress/certificates.rs`, `dns/`). piqueld obtains them
with `instant-acme` and loads them into stock Caddy; DNS credentials stay in
the daemon, so a compromised gateway can't take over a domain.

- A hostname is covered by the wildcard of its parent when the parent is
  within the zone of the provider owning it, and by its exact name otherwise;
  a wildcard route by its own wildcard (`CertificateName::covering`). A
  hostname belongs to the provider with the longest matching zone; a zone two
  providers list is a conflict (`dns/zones.rs`).
- Issuing creates the `_acme-challenge` TXT record (TTL 60 s), publishes the
  zone, and polls each authoritative nameserver from the zone's NS records,
  over TCP without recursion or caching, every 5 seconds for up to 10 minutes,
  because OVH propagation is slow. It then answers the challenge, finalizes
  the order and deletes the record, also when issuance fails or shutdown
  interrupts the order. A deletion that fails is kept as the name's leftover,
  deleted before the next attempt, since too many TXT values at one name make
  the CA reject validation. Each ACME step is bounded by 2 minutes.
- Files, all 0600: the account key in
  `<data_dir>/ingress/acme/account-<16 hex digits of the directory URL's SHA-256>.json`,
  and each certificate with its key in one file,
  `<data_dir>/ingress/certificates/<name>.pem` (`_wildcard.<parent>.pem` for
  wildcards), so a renewal replaces the pair atomically. The account accepts
  the CA's terms of service.
- Each pass refreshes stale zones, renews certificates with less than a third
  of their lifetime left, and retries failures with backoff from 5 minutes,
  doubling, up to 6 hours. Certificates no route needs aren't renewed, and are
  deleted once expired. A hostname no zone owns, such as while discovery fails
  after a restart, keeps a stored certificate covering it.
- Issuance, renewal and deletion are daemon-scoped journal actions with the
  `ingress_certificate_issue`, `ingress_certificate_renew` and
  `ingress_certificate_delete` phases. A failed attempt records a
  `certificate_renewal_failed` diagnostic, and the condition of the same name
  is raised while a needed certificate that failed to renew expires within 14
  days.

Zones are discovered at the first pass and hourly, which also checks
credentials. A failed discovery clears the refresh time, so the next caller
retries: the records loop within 10 seconds when a provider manages records,
otherwise the certificates loop within a minute. `POST /api/v1/system/dns/refresh`
discovers at once. Provider requests time out after 30 seconds and never
follow redirects or proxies. OVH stages changes until `POST
/domain/zone/<zone>/refresh`, and every OVH call first reads the unauthenticated
`/auth/time`, to sign with the API's clock.

## Managed DNS records

`ingress/records.rs` maintains each route's records through the providers that
set `manage_records`:

| Route | Records |
| --- | --- |
| private | A and AAAA to the apps node's tailnet addresses |
| public, tunnel | a proxied CNAME to `<tunnel-id>.cfargotunnel.com`; a provider that can't proxy, such as OVH, leaves the route `pending` and writes nothing |
| public, direct | A and AAAA to `public_addresses`, unproxied; without any, `manual` |

The webhook hostname gets a public route's records and the dashboard hostname
a private route's, though neither has a status to show it. A wildcard's
records go on `*.<parent>`.

- **Ownership.** Before writing a hostname's records, piqueld creates a
  `_piqueld.<hostname>` TXT record holding the instance ID
  (`_piqueld._wildcard.<parent>` for a wildcard, since providers accept `*`
  only as a name's first label). A hostname is owned when that record is
  present and no other TXT record is at its name. An unowned hostname with A,
  AAAA or CNAME records, or another claim, is never written and reports
  `dns_conflict` with the records found, until they are removed; a foreign
  claim appearing later turns an owned name into a conflict too. Other record
  types at the hostname are left alone. Claimed hostnames are kept in the
  `dns_records` table from before the first write until their records are
  deleted, so a route removed while the daemon was down is still cleaned up.
- **Lifecycle.** Records follow the routes the gateway applied
  (`applied_table`). A new route's records are written once the gateway serves
  it; a removed route's records, then its ownership record, are deleted once
  the gateway withdrew it. A visibility change therefore deletes and rewrites
  them. After a restart, every name stays `pending` until the gateway has
  converged with this daemon's configuration (`gateway_converged`), which is
  set before `cloudflared` starts, so a switch to tunnel mode repoints records
  once Caddy runs in that mode. Disabling ingress deletes the managed records
  once the gateway has stopped. A route waiting for the gateway, a private
  route while the node has no addresses, and a private route while private
  ingress is disabled keep their current records. Removing `public_addresses`
  or `manage_records` leaves existing records in place, with their ownership
  record and claim.
- **Reconciliation.** Every 10 seconds, the loop refreshes stale zones and
  computes the desired records from the applied routes, the mode and the
  node's addresses, without reading any. When they change, 5 minutes after a
  complete pass, or 1 minute after an incomplete one, a pass reads each
  managed hostname's records and repairs drift, one hostname at a time.
  Same-type changes are updated in place; leftovers are deleted, then missing
  records created, so a CNAME replacing addresses, or the reverse, is written
  after the old records are gone. A failed provider call leaves the hostname
  `pending`. A route the gateway changed during the pass waits for the next.
  An unpublished OVH zone is published on the next pass even when nothing else
  changed.
- **History.** Each hostname's change is a daemon-scoped journal action with
  the `ingress_dns_create` (not owned yet), `ingress_dns_update` or
  `ingress_dns_delete` phase. Failed writes record a `dns_records_failed`
  diagnostic, which doesn't notify; failed reads are only logged. Passes that
  change nothing record no history.

## Status and recovery

Core readiness (`ready`) describes the database, Docker and the Swarm.
Ingress has its own fields under `ingress` in `GET /api/v1/system/readiness`
(`IngressStatus` in `crates/piqueld-core/src/api.rs`):

- `healthy`: the gateway and its public listener, including the tunnel's
  connection in tunnel mode. An application network failure clears it.
- `gateway`: only whether the gateway accepted its configuration, which
  another application's broken network doesn't clear.
- `message`: which stage failed, without the error, which stays in the logs.
- `public`: `direct`, or `tunnel` with its `id`, `connected` and `message`.
- `private`: the apps node's `enabled`, `healthy`, `state`, `dns_name`,
  `addresses` and `message`. A broken node degrades only private routes.
- `routes`: each route's effective `visibility`, the `dns` records its
  hostname needs, `dns_state`, `state` and `message`, only for applications the
  caller can read.

Every gateway change (network, image pull, start, replacement, recovery, route
reload, stop) is a daemon-scoped journal action with an `ingress_*` phase.
Methods decide from reads before opening an action, so steady-state passes
record no history. Health changes of the gateway, the apps node or the tunnel
feed the `ingress_unavailable` condition and diagnostic, which notifies under
`daemon_failures` when sustained. The containers' logs are relayed to the
daemon's every 10 seconds, at most 100 lines per container, each cut to 4096
characters. Gateway startup,
port and configuration failures leave the daemon available.

Disabling ingress and restarting removes the containers and the tunnel
credentials copy, but keeps route state, Caddy's data and configuration, and
node state on disk, and keeps accepting route-bearing manifests. Re-enabling
exposes only deployed route intent. A normal shutdown leaves Caddy serving and
renewing the certificates it obtains itself; DNS-01 certificates renew only
while the daemon runs.

The data directory holds, below `ingress/`:

```text
data/            Caddy's data home: its certificates
config/caddy/    autosave.json, rollback.json, candidate.json (0600; they can hold DNS-01 keys)
control/         admin.sock, webhooks.sock, dashboard.sock
acme/            the ACME account key
certificates/    DNS-01 certificates
tailscale/       the apps node's state, socket and configuration
tunnel/          config.json, credentials.json
```

`control` and `tunnel` are regenerated; back up the rest with the database.

## Validation

`just docker-test` runs Caddy in the isolated Docker-in-Docker harness, with
private test certificates and randomly allocated loopback host ports
(`ingress/tests.rs`, `ingress/tests/`). It covers routing, redirects,
unknown-host rejection, network separation, gateway replacement failure and
recovery, unrelated deployments during a stalled gateway update, withdrawals
with a broken application network, a backend cutover held behind a failing
health check, and forwarded client addresses arriving from the injected
ingress range.

With Pebble-issued DNS-01 certificates (`tests/dns01.rs`), it checks that a
public wildcard route serves two names below it with their own `Host`, that an
exact route below it still wins and that a name two labels deep is not
matched, that each listener serves only its own routes (a forged Host on port
80, or SNI, for a private route on the public listener gets a 404 or no
certificate), that a public → private change withdraws the route from the
public listener, that the PROXY v2 client address reaches backends in
`X-Forwarded-For`, that a container on an application's ingress network
completes TLS for a private route neither directly nor with a forged PROXY
header, and that the apps node container runs hardened and reports its state.

In tunnel mode (`tests/tunnel.rs`), with a container on the edge network
standing in for `cloudflared`, it checks that neither the gateway nor
`cloudflared` publishes a port, that the tunnel listener serves public routes
with `Cf-Connecting-IP` as the client address but not private ones, that a
container on an application's network can't reach it, and that returning to
direct mode removes `cloudflared` and its credentials. Distinct backend
responses establish that requests actually switch destinations.

Persistent HTTP/1, HTTP/2, WebSocket and SSE connections are exercised across
reloads (`tests/traffic.rs`). Caddy's bundled ACME server issues short-lived
certificates in the harness (`tests/acme.rs`): separate orders require HTTP-01
and TLS-ALPN-01, and fresh trusted TLS handshakes must observe renewed
certificates without daemon intervention. The CA is private and test DNS is
local to the disposable gateway container. This tests the ACME protocol and
renewal, not a public CA's policies or internet reachability. DNS record
reconciliation runs against an in-memory provider (`records/tests.rs`), and
the Cloudflare and OVH clients against recorded responses.

### Before a production rollout

Public CA validation needs a real domain and reachable ports, which the
isolated suite can't establish. Deploy a disposable application on a domain
whose A/AAAA records point at the installation. From a separate internet
connection, verify the HTTP redirect and the trusted HTTPS response, inspect
the issuer and expiry, and confirm that `/.well-known/piqueld-ingress` returns
this installation's ID. Check both IPv4 and IPv6 when publishing both records,
and keep certificate issuance and renewal diagnostics under observation.

The apps node needs a real tailnet, so CI doesn't run it. Before relying on
private routes, enable `[ingress.private]`, deploy a disposable private route,
and point its A/AAAA records at the node's addresses. From a tailnet device,
the route must answer over trusted HTTPS, `http://` must redirect, and the
backend must see the device's tailnet address in `X-Forwarded-For`. From a
device off the tailnet, connecting must fail, and the same hostname on the
server's public address must answer 404 without a certificate. The same
applies to the dashboard hostname: from a tailnet device, the website must
load over trusted HTTPS, passkey login must work, and the audit trail must name
the device's tailnet user.

A real tunnel needs Cloudflare, so CI doesn't run one either. Before relying on
it, enable `[ingress.tunnel]`, deploy a disposable public route, and create its
proxied CNAME. From the internet, the route must answer over HTTPS with the
backend seeing the client's address in `X-Forwarded-For`, and status must show
the tunnel connected. From outside, a port scan of the server must find ports
80 and 443 closed.
