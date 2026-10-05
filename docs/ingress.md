# Managed HTTP ingress

Piqueld manages one Caddy gateway for a single-node installation used by one trusted
person or team. Applications own exact-host routes; the installation owns public
listeners and certificate storage. Apps must implement their own authentication.

## Enable and expose an application

Set the read-only global daemon TOML, then restart piqueld:

```toml
[ingress]
enabled = true
```

Docker Engine 28+ with API 1.48+ is required. Ports 80 and 443 must be free and
reachable from the internet. Create an A record for the server's public IPv4
address; add AAAA only if public IPv6 actually reaches the gateway. DNS changes
are manual. Caddy obtains and renews certificates without DNS-provider credentials.

Add a route to an application manifest and deploy it:

```toml
[[spec.routes]]
hostname = "notes.example.com"
service = "web"
port = 3000
```

A route can instead redirect its hostname, for example `www` to the apex domain.
Caddy answers redirects itself; they need no service and get certificates the
same way:

```toml
[[spec.routes]]
hostname = "www.notes.example.com"
redirect = { to = "https://notes.example.com" }
```

The dashboard's Routes tab supports the same save/deploy lifecycle. Removing a
service in the UI also removes its routes from saved configuration. A domain can
point at only one environment's service; multiple domains may point at one service.
Hostname reservations are held per environment and include the application's saved
routes, so two environments of one application cannot share hostnames: creating a
second environment of an application with routes fails with `hostname_conflict`
until environments can override hostnames.
The backend serves plain HTTP on its internal port. WebSockets and streaming are
supported. Wildcards, path rewriting/routing, tunnels, arbitrary TCP/UDP, and
HTTPS backends are outside this release.

## Traffic and isolation

The piqueld website and API are never served through ingress; routes can only
target application services. The hostname of `auth.public_url` and its subdomains
are reserved for the installation, so application code can never run on the
passkey origin or set cookies for it. Saving or deploying such a route fails with
`hostname_conflict`. Routes saved before that hostname was configured stay
unpublished and are logged at startup. Ingress owns ports 80/443, so the website's
HTTPS reverse proxy must listen on a different address or host.

Only Caddy publishes ports. HTTP redirects to HTTPS for known hosts; unknown HTTP
hosts receive 404 and unknown TLS names receive no automatically issued certificate.
Each environment's exposed services share a dedicated ingress overlay with Caddy;
environments with only redirect routes have none.
Environment ingress networks are separate from each other and from private backend
networks. The gateway is trusted across all exposed environments.

Caddy runs as the daemon's UID/GID in a standalone Docker container, with only the
`NET_BIND_SERVICE` capability (required by the official binary), a read-only root filesystem, and a private Unix administration socket.
The gateway does not receive Docker API access. Its image version and multi-platform
manifest digest are pinned together and follow piqueld releases. The stable bridge network provides outbound DNS/ACME connectivity;
application overlay attachment does not replace the gateway container.

Ordinary configuration changes preserve listener availability. HTTP clients must
handle idle connections closing during reload; active HTTP/2 streams and WebSockets
are covered by integration tests. WebSocket connections may reconnect after a
five-minute close delay. Gateway replacement or host failure can interrupt traffic.
This does not add blue-green application deployment.

Before replacement, piqueld downloads the pinned image, validates the Caddy
configuration with that image, and prepares the new container and its network
attachments while the old gateway still serves. The old container
and its configuration remain available until the replacement starts successfully.
Failed startup restores the old gateway; interrupted replacements are recovered on
the next reconciliation. Replacement still requires a brief stop/start on the shared
ports. Docker outages can delay recovery; inspect ingress health and daemon logs.

### Client addresses

Backends receive connections from the gateway, not from clients. Caddy replaces
any client-supplied `X-Forwarded-For` with the address it accepted the connection
from, and sets `X-Forwarded-Proto` and `X-Forwarded-Host`. Every routed service
receives `PIQUELD_INGRESS_PROXIES`: the comma-separated CIDRs of its application's
ingress network (for example `10.0.3.0/24`). Configure the application to trust
forwarding headers only from peers in that range, e.g. Express
`app.set("trust proxy", process.env.PIQUELD_INGRESS_PROXIES.split(","))`.

Only the gateway, Swarm's load balancer for that network, and the application's
own routed services are attached to the ingress network, so a forwarded header
from that range was set by Caddy or by the application itself. Trusting the range
therefore also trusts every routed service and replica of the application; a
compromised one could forge client addresses. Private networks use different
ranges, so services outside the ingress network cannot forge it. The value
follows the network: piqueld updates services if the network is recreated, and
treats a modified value as drift. Requests reaching Caddy through Docker's userland proxy
(for example IPv6 to an IPv4-only bridge) appear to come from the bridge gateway.

## Status and recovery

System status displays ingress alongside Docker health; effective Settings remain
read-only. Caddy startup/port/configuration failures leave the daemon available.
Detailed causes and Caddy certificate diagnostics are logged by piqueld. Core
readiness (`ready`) continues to describe database/Docker/Swarm; ingress has its own
`enabled`, `healthy`, `message`, and `routes` fields under system readiness.

Every gateway change (network, image pull, start, replacement, recovery, route
reload, stop) is a daemon-scoped journal action with an `ingress_*` phase, so it
appears in Events with its outcome. Reconciliation passes that change nothing
record no history. Health changes are recorded as `ingress_unavailable`
diagnostics. With `daemon_failures` notifications enabled, a gateway that stays
unhealthy past the failure threshold notifies, and its recovery follows; see
[observability](observability.md).

Deployed route status is separate from application health. A `ready` route means
an HTTPS request from the daemon validated a publicly trusted certificate and reached
this gateway at `/.well-known/piqueld-ingress`. This small reserved endpoint returns
the installation ID and never invokes the application. It is not a backend health
check or proof of reachability from every external network. DNS, firewall, NAT
loopback, or pending certificate issuance can keep a route `pending`; inspect A/AAAA
records and daemon logs. Gateway failure leaves routes pending, and disabled ingress
is reported explicitly. Applications should configure their own container health
checks to establish backend readiness before route cutover.

Public DNS or certificate delays do not fail a deployment once its runtime and route
configuration are applied. Failure to apply required gateway configuration does fail
it, retains required old backends, and retries. Explicit route removals withdraw
public exposure as deployment execution starts, before obsolete service/network
cleanup. Hostname reservations survive pending deployments and incomplete withdrawals.

Gateway updates apply as one installation-wide configuration. A missing or conflicting
application network retains that application's accepted destinations and prevents its
new destinations from being applied. Explicit withdrawals still proceed, and healthy
applications can update independently. The gateway never attaches an unverified
network. Global health reports the degraded state, and logs identify the application
and network to repair. Each route's public HTTPS readiness is checked independently.
If a gateway upgrade is also pending, replacement waits until those networks are
repaired or their routes are withdrawn. The running gateway keeps its existing
attachments and continues applying other route changes in the meantime.
Gateway requests do not hold the controller's global Docker mutation lock, so private
application deployments can proceed while routing is waiting.

Disabling ingress in TOML and restarting stops public routing, retains certificate
and route state, and continues accepting route-bearing manifests. Re-enabling exposes
only deployed route intent. A normal daemon shutdown leaves Caddy serving and renewing
certificates independently. `piqueld backup` archives `ingress/data` and
`ingress/config` together with SQLite; see [backup and restore](backups.md).

## Validation

`just docker-test` runs Caddy in the isolated Docker-in-Docker harness, with private
test certificates and randomly allocated loopback host ports. It covers routing,
redirects, unknown-host rejection, network separation, gateway replacement failure
and recovery, unrelated deployments during a stalled gateway update, withdrawals
with a broken app network, a backend cutover held behind a failing health check,
and forwarded client addresses arriving from the injected ingress range.
Distinct backend responses establish that requests actually switch destinations.

Persistent HTTP/1, HTTP/2, WebSocket and SSE connections are exercised across reloads.
Caddy's bundled ACME server issues short-lived certificates in the harness: separate
orders require HTTP-01 and TLS-ALPN-01, and fresh trusted TLS handshakes must observe
renewed certificates without daemon intervention. The CA is private and test DNS is
local to the disposable gateway container. This tests the ACME protocol and renewal,
not a public CA's policies or internet reachability.

Before a production rollout, deploy a disposable app on a domain whose A/AAAA records
point at the installation. From a separate internet connection, verify the HTTP
redirect and trusted HTTPS response, inspect the issuer/expiry, and confirm the
reserved `/.well-known/piqueld-ingress` endpoint returns this installation's ID.
Check both IPv4 and IPv6 when publishing both records. Keep certificate issuance and
renewal diagnostics under observation. Public CA validation requires a real domain
and reachable ports; the isolated suite cannot establish those deployment conditions.
