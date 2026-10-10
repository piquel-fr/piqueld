# Managed HTTP ingress

Piqueld manages one Caddy gateway for a single-node installation used by one trusted
person or team. Applications own exact-host routes; the installation owns the
listeners and certificate storage. Routes are public (the internet) or private (the
tailnet only, the default). Public routes arrive on ports 80 and 443, or through a
[Cloudflare Tunnel](#cloudflare-tunnel) with no inbound port at all. Apps must
implement their own authentication.

## Enable and expose an application

Set the read-only global daemon TOML, then restart piqueld:

```toml
[ingress]
enabled = true
```

Docker Engine 28+ with API 1.48+ is required. Ports 80 and 443 must be free and
reachable from the internet, unless public routes go through a
[Cloudflare Tunnel](#cloudflare-tunnel). Create an A record for the server's public
IPv4 address; add AAAA only if public IPv6 actually reaches the gateway, or let
piqueld [manage DNS records](#managed-dns-records). Caddy obtains and renews certificates for public routes without
DNS-provider credentials; see [DNS-01 certificates](#dns-01-certificates) for
the rest.

Add a route to an application manifest and deploy it:

```toml
[[spec.routes]]
hostname = "notes.example.com"
service = "web"
port = 3000
visibility = "public"
```

Routes without `visibility` are private; see
[public and private routes](#public-and-private-routes).

A route can instead redirect its hostname, for example `www` to the apex domain.
Caddy answers redirects itself; they need no service and get certificates the
same way:

```toml
[[spec.routes]]
hostname = "www.notes.example.com"
redirect = { to = "https://notes.example.com" }
visibility = "public"
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

## Public and private routes

`visibility` says who may connect to a route, not how traffic arrives:

- `public`: anyone on the internet, through the published ports 80 and 443 or the
  Cloudflare Tunnel.
- `private` (default): only devices on the tailnet, on the same hostname. Off the
  tailnet, connections simply fail.

Environments and previews cap their routes' visibility. A route's effective
visibility is the stricter of its own and its ceiling, so a ceiling can make a
route private but never public:

```toml
[spec.variables]
domain = "piquel.fr"

[spec.environments.staging]
visibility = "private"              # environments default to "public"

[spec.environments.staging.variables]
domain = "staging.piquel.fr"

[spec.previews]
visibility = "private"              # the default for previews

[[spec.routes]]
hostname = "${{ vars.domain }}"
service = "web"
port = 3000
visibility = "public"

[[spec.routes]]
hostname = "admin.${{ vars.domain }}"
service = "admin"
port = 3000
```

| Environment | Hostname | Reachable from |
| --- | --- | --- |
| production | `piquel.fr` | internet |
| production | `admin.piquel.fr` | tailnet |
| staging | `staging.piquel.fr`, `admin.staging.piquel.fr` | tailnet |

The effective visibility is computed when a deployment renders the manifest and is
stored with its deployed routes. Changing it is a route change: public → private
withdraws the route from the public listener as the deployment starts, like a
removal, and private → public publishes it once backends are ready, like an
addition. Hostname reservations ignore visibility.

`piquelctl app route add|redirect ... --visibility public|private`,
`piquelctl app route visibility <app> <hostname> <public|private>` and
`piquelctl env visibility <app> <env> <public|private>` edit the saved manifest;
the dashboard has a visibility selector per route in the Routes tab and the
ceiling in each environment's Overview.

### Listeners

Caddy runs one server per listener, each with its own routes:

| Server | Listens on | Serves |
| --- | --- | --- |
| `public` | host `:80` and `:443` (published) | public routes |
| `tunnel`, instead of `public` | `:8080` HTTP (not published) | public routes, from `cloudflared` |
| `private` | `:8443` HTTPS and `:8081` HTTP → HTTPS (not published) | private routes |

A hostname appears only on its own listener, and each listener's TLS policy
completes handshakes only for its own hostnames. A forged Host header for a
private route on the public listener therefore receives a 404, and a forged SNI
no certificate. Application networks are attached to the gateway, so applications
can open connections to the private listener too; it therefore completes TLS (and
answers HTTP) only for tailnet client addresses (`100.64.0.0/10`,
`fd7a:115c:a1e0::/48`). Only peers on the gateway's edge network, such as the apps
node, may set the client address with a PROXY protocol header; anyone else's
header is ignored and their own address is used. Application networks come from
Swarm's default address pools; a custom pool overlapping those ranges would let
application containers pass for tailnet clients. The private listener therefore
stays off, and its health names the pool, until the pools are verified outside
the tailnet ranges. Both listeners serve the probe endpoint. If
private ingress fails, private routes are unavailable; they never fall back to the
public listener.

### The apps tailnet node

Private traffic arrives through a second tailnet node, separate from the daemon's
own `[tailscale]` node: that one terminates TLS itself and must not depend on
Docker, while application traffic passes through to Caddy with its TLS intact. A
separate node also gets its own ACLs and tags. Enable it in daemon TOML and
restart piqueld:

```toml
[ingress.private]
enabled = true
hostname = "piqueld-apps"            # piqueld-apps.<tailnet>.ts.net
auth_key_file = "ts-apps-auth-key"   # $CREDENTIALS_DIRECTORY/ts-apps-auth-key
```

While it is disabled, private routes report `disabled` and are not served.

The node is a pinned `tailscale/tailscale` container (version and digest, like
Caddy's) on the gateway's edge network only, with userspace networking. It runs as
the daemon's UID/GID with no capabilities, a read-only root and `unless-stopped`,
and keeps running across daemon restarts. Its serve configuration, owned by
piqueld, forwards tailnet TCP 443 to `<gateway>:8443` and TCP 80 to
`<gateway>:8081` with a PROXY v2 header, so backends see each tailnet client's
address in `X-Forwarded-For`. Its state lives in `<data_dir>/ingress/tailscale`;
include it in backups to keep the node's identity.

The auth key is only needed for the first login; later starts reuse the node
state. Without one, the daemon logs the node's login URL and status reports that
it needs login. Its lifecycle shares the gateway's: `ingress_start_tailnet`
journal actions, drift detection by spec hash, and removal when ingress is
disabled.

### DNS for private routes

Unless piqueld [manages them](#managed-dns-records), DNS records are created
manually. Each private hostname needs A and AAAA records pointing at the apps
node's tailnet addresses, which status and `piquelctl app
route list` show. A wildcard per environment (for example `staging.piquel.fr`
plus `*.staging.piquel.fr`) means new routes need no DNS change. Names are not
secret: they appear in public DNS and certificate transparency logs; only
reachability is restricted. Some routers' DNS-rebinding protection drops answers
in `100.64.0.0/10`; allowlist the domain on the router, or use Tailscale split DNS.

Private routes get their certificates through [DNS-01](#dns-01-certificates). A
private hostname outside every configured provider zone reports `failed` with that
cause.

## Cloudflare Tunnel

Direct ingress needs inbound ports 80 and 443, which is impossible behind NAT or
CGNAT and exposes the host. Private routes already arrive over the tailnet, which
connects outbound only. With a Cloudflare Tunnel, public routes connect outbound too,
so the host needs no inbound port at all.

Create a locally managed tunnel once, with `cloudflared` on any machine logged in
to the Cloudflare account:

```sh
cloudflared tunnel login
cloudflared tunnel create piqueld   # writes ~/.cloudflared/<tunnel-id>.json
```

Then point daemon TOML at the credentials file it wrote and restart piqueld:

```toml
[ingress.tunnel]
enabled = true
credentials_file = "cloudflared-tunnel.json"   # $CREDENTIALS_DIRECTORY/cloudflared-tunnel.json
```

The file is read at startup like other `_file` settings and must hold the tunnel's
`AccountTag`, `TunnelSecret` and `TunnelID`. piqueld writes it, mode 0600, to
`<data_dir>/ingress/tunnel/credentials.json` for the container, and deletes that
copy whenever `cloudflared` is not meant to run: once the tunnel or ingress is
disabled.

With the tunnel enabled, every public route is served through it, and the gateway
publishes no host port: ports 80 and 443 are closed. Switching modes changes the
gateway's container spec, so it is replaced like any other gateway upgrade, with a
brief interruption. Unlike other upgrades, a mode switch is not deferred while an
application's network is broken: the old gateway cannot serve the other mode's
listeners, so ingress reports the failure, keeps the old mode, and switches once
the network is repaired or its routes are withdrawn. Private routes stay on the
tailnet.

### `cloudflared`

piqueld runs a pinned `cloudflare/cloudflared` container (version and digest, like
Caddy's) on the gateway's edge network only. It runs as the daemon's UID/GID with no
capabilities, a read-only root and `unless-stopped`, and keeps running across daemon
restarts. Its configuration and credentials are bind-mounted read-only. Its
lifecycle shares the gateway's: `ingress_start_tunnel` journal actions, drift
detection by spec hash (including a fingerprint of the credentials, so new
credentials replace it), recovery, log relay, and removal when ingress or the tunnel
is disabled.

The tunnel is locally managed: piqueld generates `cloudflared`'s configuration
with a single catch-all rule to `http://<gateway>:8080`, so adding or removing
routes never touches Cloudflare. Hostnames the gateway does not know receive a 404
from Caddy.

### The tunnel listener

Caddy's `tunnel` server listens for plain HTTP on `:8080`, unpublished, and serves
public routes and their probe endpoint, nothing else; the `public` server does not
run. It has no certificate and no HTTP → HTTPS redirect, since Cloudflare
terminates TLS: enable "Always Use HTTPS" for the zone on Cloudflare.

Application networks are attached to the gateway, so the tunnel listener refuses
every peer outside the edge network. Only containers piqueld manages join that
network: `cloudflared`, and the apps tailnet node when private ingress is
enabled, which forwards only to the private listener. The host can also reach the
edge network, like the private listener. The client address comes from
`Cf-Connecting-IP`, which is therefore only trusted from the edge network. Backends receive it in `X-Forwarded-For`, and `X-Forwarded-Proto` is
`https`. `PIQUELD_INGRESS_PROXIES` is unchanged: backends still receive requests
from the gateway.

### DNS for tunnel routes

Unless piqueld [manages them](#managed-dns-records), DNS records are created
manually. Each public hostname needs a proxied (orange cloud) CNAME record to `<tunnel-id>.cfargotunnel.com`, which route status and
`piquelctl app route list` show:

```text
notes.example.com.   CNAME   6ff42ae2-765d-4adf-8112-31c55c1551ef.cfargotunnel.com.   ; proxied
```

`cloudflared tunnel route dns piqueld notes.example.com` creates the same record.

### Trade-offs

- **Cloudflare sees all public traffic in plaintext.** It terminates TLS at its
  edge. Inside the host, the `cloudflared` → Caddy hop runs only on the gateway's
  edge network, so other applications cannot observe it.
- **DNS hosting:** the zone must be hosted on Cloudflare; the registrar can stay
  elsewhere.
- **Certificate depth:** Universal SSL covers one label below the zone. Deeper
  public names, such as `x.staging.example.com`, need Advanced Certificate Manager.
- **HTTP → HTTPS** is handled by Cloudflare ("Always Use HTTPS"), not the gateway.

### Status

System status and `piquelctl status` show the ingress mode, the tunnel ID and
whether it is connected to Cloudflare. A Docker healthcheck runs
`cloudflared tunnel ready` inside the container every 10 seconds (every 2 seconds
while it starts), which checks its `/ready` metrics endpoint; piqueld reads the
outcome through the Docker API, so it needs no route to the container, and the
metrics endpoint listens only on the container's loopback. While the tunnel is not
connected, ingress is unhealthy, since public routes are unreachable. The public HTTPS probe is
unchanged: it reaches the probe endpoint through Cloudflare, and the installation ID
proves that the tunnel reaches this gateway.

## Push webhooks

Applications whose repository syncs with `webhook` (see
[deploying on push](application-manifest.md#deploying-on-push)) need GitHub to
reach piqueld, whose API is only on the tailnet. The gateway exposes exactly one
path for that, on a dedicated public hostname:

```toml
[ingress]
enabled = true
webhook_hostname = "hooks.example.com"
```

- **Routing.** The public listener (or, in tunnel mode, the tunnel listener)
  forwards `https://hooks.example.com/hooks/github/<application-id>` to a Unix
  socket in the gateway's control mount, `<data_dir>/ingress/control/webhooks.sock`.
  The daemon serves nothing else on that socket: no API, no dashboard, no
  authentication endpoints. Every other path of the hostname is a 404, and HTTP
  redirects to HTTPS. Only the daemon's user, which the gateway runs as, can
  connect to the socket.
- **DNS and certificates.** The hostname gets a public route's records: an
  A/AAAA record to `public_addresses`, or a proxied CNAME to the tunnel,
  managed like routes' when a DNS provider sets `manage_records` for its zone
  (see [managed DNS records](#managed-dns-records)), and created by hand
  otherwise. Caddy obtains its certificate. Applications may not route the
  hostname or its subdomains, like the website's, so pick a dedicated name such
  as `webhooks.example.com`: `example.com` and its other subdomains stay
  available.
- **Verification.** Each application has its own secret, generated by piqueld
  with `piquelctl app repository webhook rotate APP` (or in the dashboard) and
  shown only once; generating another replaces it at once. It is encrypted with
  the secret master key, and lost-key recovery discards it. A delivery must carry
  GitHub's `X-Hub-Signature-256`, the HMAC-SHA256 of its body under that secret,
  compared in constant time; anything else, including an unknown application,
  gets 401. Bodies are limited to 1 MiB (413 beyond, at the gateway and in the
  daemon), at most 16 deliveries run at once (503 beyond), and each gets 10
  seconds.
- **What a delivery does.** A verified `push` only hints sync to list the
  repository's branches soon, at most once every 30 seconds per repository;
  other events, such as `ping`, change nothing. The payload is never read: sync
  resolves each branch head with `git ls-remote` itself, so a delivery cannot
  choose what deploys. Refused deliveries are logged, not audited.

The daemon serves the socket whenever `webhook_hostname` is set, even without
managed ingress (`enabled = false`). The gateway then routes nothing to it, so
another reverse proxy may, terminating TLS for the hostname and forwarding
only `/hooks/github/` to the socket. It must run as the daemon's user, the only
one that can reach the data directory and connect, and should bound bodies like
the gateway does.

In GitHub, add a webhook to the manifest repository with the payload URL from
`piquelctl app repository webhook show APP`, content type `application/json`,
the secret, and the `push` event. GitHub's delivery log shows each response.

## DNS-01 certificates

Private routes, which a public CA cannot reach, get certificates through ACME
DNS-01 instead of Caddy's automatic HTTPS. piqueld obtains them itself through the
[DNS providers](configuration.md#dns-providers) in daemon TOML and loads them into
stock Caddy through its admin API (`tls.certificates.load_pem`). Automatic HTTPS
is off on the private listener, so Caddy never attempts HTTP-01 for them. DNS
credentials stay in the daemon, so a compromised gateway cannot take over a
domain. Public routes keep Caddy's automatic HTTPS.

A hostname is covered by the wildcard of its parent domain when that parent is
the provider's zone or lies inside it, and by its exact name otherwise:

| Hostname | Certificate |
| --- | --- |
| `admin.piquel.fr` | `*.piquel.fr` |
| `staging.piquel.fr` | `*.piquel.fr` (same certificate) |
| `auth.staging.piquel.fr`, `admin.staging.piquel.fr` | `*.staging.piquel.fr` |
| `piquel.fr` | `piquel.fr` (the parent `fr` is outside the zone) |

All previews under `*.dev.piquel.fr` therefore share one certificate.

To issue, piqueld creates the `_acme-challenge` TXT record and polls the zone's
authoritative nameservers until each serves it, for up to 10 minutes because
OVH propagation is slow. It then answers the challenge, finalizes the order, and
deletes the TXT record, which it also deletes when issuance fails or the daemon
shuts down mid-order. The ACME account key lives in `<data_dir>/ingress/acme/`
and each certificate with its private key in one file,
`<data_dir>/ingress/certificates/<name>.pem` (`_wildcard.<parent>.pem` for
wildcards), all mode 0600; include both in backups. `[ingress.acme] directory` defaults to
Let's Encrypt production.

Certificates are checked every minute and renewed when less than a third of their
lifetime remains. Failures retry with backoff, from 5 minutes up to 6 hours.
Certificates that no route needs any more are not renewed, and are deleted once
expired. A hostname outside every provider zone, or in a zone claimed by two
providers, reports that as its certificate's error.

Issuance, renewal and deletion are daemon-scoped journal actions with the
`ingress_certificate_issue`, `ingress_certificate_renew` and
`ingress_certificate_delete` phases, so they appear in Events. A failed attempt
records a `certificate_renewal_failed` diagnostic. With `daemon_failures`
notifications enabled, a certificate whose renewal keeps failing notifies once it
is within 14 days of expiry. `piquelctl status` and the dashboard's system status
show each provider with its kind, zones and health, and each certificate with its
hostnames, expiry and last error.

## Managed DNS records

piqueld can create and maintain each route's DNS records through the
[DNS providers](configuration.md#dns-providers) that set `manage_records`:

```toml
[ingress]
enabled = true
public_addresses = ["203.0.113.10", "2001:db8::10"]

[[dns.providers]]
kind = "cloudflare"
api_token_file = "cloudflare-dns-token"
manage_records = true
```

It is off by default: zones of other providers stay manual. A hostname belongs to
the provider with the longest matching zone, as for certificates.

| Route | Records |
| --- | --- |
| private | A and AAAA to the apps node's tailnet addresses |
| public, [tunnel](#cloudflare-tunnel) | a proxied CNAME to `<tunnel-id>.cfargotunnel.com` (the zone must be on Cloudflare: OVH cannot proxy, so such routes stay `pending`) |
| public, direct | A and AAAA to `[ingress] public_addresses`; without any, these records stay manual |

The [webhook hostname](#push-webhooks) gets a public route's records.

Private routes never get public addresses. Managed records are exact hostnames,
never wildcards: existing manual wildcards keep working, and the more specific
managed records override them.

**Ownership.** piqueld only changes records it owns. Before writing a hostname's
records, it creates a `_piqueld.<hostname>` TXT record holding the installation ID.
A hostname that already has A, AAAA or CNAME records without that ownership record,
or that another installation also claims, is never written: its route reports
`dns_conflict`, with the existing records, until the operator removes them.
Other record types at the hostname, such as MX or TXT, are left alone. Claimed
hostnames are also kept in SQLite until their records are deleted, so a route
removed while the daemon was down is still cleaned up.

**Lifecycle.** Records follow the routes the gateway has applied. A new route's
records are written once the gateway serves it, and a removed route's records and
ownership record are deleted only after the gateway has withdrawn it. A visibility
change withdraws the route first, so its records are deleted, then written again
with the new targets once the gateway serves it on its new listener. After a
restart, records only change once the gateway has applied the daemon's
configuration, so switching to tunnel mode repoints records only once the gateway
runs in that mode. Disabling ingress deletes the records piqueld manages once the
gateway has stopped. A route waiting for the gateway, or
a private route while the apps node has no tailnet addresses, keeps its current
records. Removing `public_addresses` or `manage_records` leaves existing records
in place, unmanaged.

**Reconciliation.** Every 10 seconds piqueld computes the desired records from
the applied routes, the ingress mode and the apps node's addresses, without
calling providers. When they change, and every 5 minutes otherwise, it reads each
managed hostname's records and repairs any drift. Address changes of the apps
node are therefore followed within seconds. A failed provider call, including a
rate limit, leaves that hostname `pending` and is retried after a minute. A route
the gateway changes during a pass waits for the next one. OVH changes take effect
once the zone is refreshed; a failed refresh is retried even when nothing else
changed. Changes
are written in place where the record type stays the same; a CNAME replacing
addresses (or the reverse) is written after the old records are deleted.

Each change is a daemon-scoped journal action with the `ingress_dns_create`,
`ingress_dns_update` or `ingress_dns_delete` phase, so it appears in Events;
failures record a `dns_records_failed` diagnostic. Passes that change nothing
record no history.

Each route reports its `dns_state`: `manual`, `managed`, `pending` or
`dns_conflict`, in the API, `piquelctl app route list` and the dashboard's Routes
tab. `piquelctl status` and the dashboard's system status show which providers
manage records.

## Traffic and isolation

The piqueld website and API are never served through ingress; routes can only
target application services. The hostname of `auth.public_url` and its subdomains
are reserved for the installation, so application code can never run on the
passkey origin or set cookies for it. Saving or deploying such a route fails with
`hostname_conflict`. Routes saved before that hostname was configured stay
unpublished and are logged at startup. Without a tunnel, ingress owns ports 80/443,
so the website's HTTPS reverse proxy must listen on a different address or host.

Only Caddy publishes ports, and only the public listener's; in tunnel mode, nothing
does. HTTP redirects to HTTPS for known hosts; unknown HTTP hosts receive 404 and
unknown TLS names receive no certificate.
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
`enabled`, `healthy`, `message`, `public`, `private`, and `routes` fields under system
readiness. `healthy` covers the gateway and its public listener, including the
tunnel's connection in tunnel mode; `public` reports the mode (`direct` or `tunnel`,
with the tunnel ID and whether it is connected); `private` reports
the private listener separately: the apps node's login state, `MagicDNS` name and
tailnet addresses. A broken node degrades only private routes. Each route reports
its effective `visibility`, the `dns` records its hostname needs, and whether
piqueld [manages them](#managed-dns-records) (`dns_state`). `routes` lists
only the routes of applications the caller can
[read](authorization.md#what-callers-see).

Every gateway change (network, image pull, start, replacement, recovery, route
reload, stop) is a daemon-scoped journal action with an `ingress_*` phase, so it
appears in Events with its outcome. Reconciliation passes that change nothing
record no history. Health changes are recorded as `ingress_unavailable`
diagnostics. With `daemon_failures` notifications enabled, a gateway that stays
unhealthy past the failure threshold notifies, and its recovery follows; see
[observability](observability.md).

Deployed route status is separate from application health. A `ready` public route
means an HTTPS request from the daemon validated a publicly trusted certificate and
reached this gateway at `/.well-known/piqueld-ingress`. A `ready` private route
means public DNS answers exactly the apps node's tailnet addresses, the node is
logged in, and the private listener, reached over the edge network with a PROXY
header naming a tailnet client (`100.100.100.100`) and the hostname as SNI,
served a trusted certificate and this endpoint. The tailnet hop
itself is not probed end to end. This small reserved endpoint returns
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
certificates independently. Back up the private daemon data directory, including
`ingress/data` and `ingress/config`, together with SQLite.

## Validation

`just docker-test` runs Caddy in the isolated Docker-in-Docker harness, with private
test certificates and randomly allocated loopback host ports. It covers routing,
redirects, unknown-host rejection, network separation, gateway replacement failure
and recovery, unrelated deployments during a stalled gateway update, withdrawals
with a broken app network, a backend cutover held behind a failing health check,
and forwarded client addresses arriving from the injected ingress range. With
Pebble-issued DNS-01 certificates, it checks that each listener serves only its
own routes (a forged Host or SNI for a private route on the public listener gets a
404 and no certificate), that a public → private change withdraws the route from
the public listener, that the PROXY v2 client address reaches backends in
`X-Forwarded-For`, that a container on an application's ingress network completes
TLS for a private route neither directly nor with a forged PROXY header, and that
the apps node container runs hardened and reports its state. In tunnel mode, with
a container on the edge network standing in for `cloudflared`, it checks that
neither the gateway nor `cloudflared` publishes a port, that the tunnel listener
serves public routes with `Cf-Connecting-IP` as the client address but not private
ones, that a container on an application's network cannot reach it, and that
returning to direct mode removes `cloudflared` and its credentials.
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

The apps node needs a real tailnet, so CI does not run it. Before relying on private
routes, enable `[ingress.private]`, deploy a disposable private route, and point its
A/AAAA records at the node's addresses. From a tailnet device, the route must answer
over trusted HTTPS, `http://` must redirect, and the backend must see the device's
tailnet address in `X-Forwarded-For`. From a device off the tailnet, connecting must
fail, and the same hostname on the server's public address must answer 404 without
a certificate.

A real tunnel needs Cloudflare, so CI does not run one either. Before relying on
it, enable `[ingress.tunnel]`, deploy a disposable public route, and create its
proxied CNAME. From the internet, the route must answer over HTTPS with the
backend seeing the client's address in `X-Forwarded-For`, and status must show
the tunnel connected. From outside, a port scan of the server must find ports 80
and 443 closed.
