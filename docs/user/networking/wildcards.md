# Wildcard routes and DNS-01 certificates

A wildcard route serves every name one level below a domain, such as
`alice.tenants.example.com` and `bob.tenants.example.com`. One route then
gives each of your users or tenants their own subdomain, without a manifest
change for each.

This page also explains DNS-01 certificates, which piqueld gets through your
DNS provider for wildcard and [private](private-routes.md) routes.

## Before you start

- The gateway is enabled; see
  [Serving an application over HTTPS](routes.md#enable-the-gateway).
- A DNS provider covers the domain: [Cloudflare](cloudflare.md) or
  [OVH](ovh.md). piqueld needs it to get the wildcard's certificate. Through a
  [Cloudflare Tunnel](cloudflare.md#serve-public-routes-through-a-cloudflare-tunnel),
  a public wildcard route needs no DNS provider.
- Run `piquelctl` on the host, or on any machine where it is signed in.

## Add a wildcard route

1. Add a route whose hostname starts with `*.`:

   ```toml
   [[spec.routes]]
   hostname = "*.tenants.example.com"
   service = "web"
   port = 3000
   visibility = "public"
   ```

   Or with `piquelctl`, quoting the hostname so that your shell doesn't
   expand the `*`:

   ```console
   piquelctl app route add notes '*.tenants.example.com' web 3000 --visibility public
   ```

   In the dashboard, add it on the application's **Routes** tab like any
   other route, with `*.tenants.example.com` as its **Hostname**.

2. Create the DNS records for `*.tenants.example.com`: the same records an
   exact route of its visibility needs, such as an A record to your server for
   a public route. If piqueld [manages your DNS records](dns-records.md), it
   creates them.

3. Deploy the application.

The route matches `alice.tenants.example.com`, but neither
`tenants.example.com` nor `a.b.tenants.example.com`. Your service receives the
name the client asked for in the `Host` and `X-Forwarded-Host` headers, and
tells tenants apart with it.

## Check that it works

```console
piquelctl app route list notes
```

Once the route's state is `ready`, piqueld has reached a name below it over
trusted HTTPS: the first one-character name that no exact route has, such as
`a.tenants.example.com`. Its DNS records and certificate work, like an exact
route's.

## Exact routes win

- **An exact hostname always wins over a wildcard covering it.** Add
  `admin.tenants.example.com` as its own route, and the wildcard no longer
  serves it. This holds whichever application owns it, and whether it is
  public or private, so a wildcard never captures a hostname that exists
  already. The [webhook](webhooks.md) and
  [dashboard](private-routes.md#serve-the-dashboard-on-your-own-domain)
  hostnames win too.
- **Names below a wildcard belong to its application.** Only that application
  can add an exact route below it. An exact name that another application
  routed before the wildcard existed stays that application's.
- **A wildcard can't lie inside a hostname piqueld uses itself.** With
  `hooks.example.com` as the webhook hostname, `*.hooks.example.com` is
  refused. `*.example.com` is accepted, and doesn't serve `hooks.example.com`.

## Wildcards in previews

A preview's hostnames can start with `*.` too, for example
`*.${{ env.slug }}.dev.example.com`. Each preview then has a subdomain per
tenant, with a certificate of its own. See
[previews](../reference/manifest.md#previews).

## DNS-01 certificates

The gateway gets most certificates the usual way: the certificate authority
connects to it on port 80 or 443 to check that it serves the hostname. That
can't work for:

- private routes and the [dashboard hostname](private-routes.md#serve-the-dashboard-on-your-own-domain),
  which the certificate authority can't reach;
- public wildcard routes, since only DNS can prove control of every name
  below a domain.

For those, piqueld gets the certificate itself, from Let's Encrypt unless you
[choose another authority](#choose-the-certificate-authority), through your
DNS provider:

1. It creates an `_acme-challenge` TXT record in the zone.
2. It waits until every nameserver of the zone serves the record, for up to
   10 minutes.
3. The certificate authority checks the record and issues the certificate.
4. piqueld deletes the record, even when issuing fails.

Your DNS credentials never leave the daemon: the gateway gets the
certificates, not the credentials. With a
[Cloudflare Tunnel](cloudflare.md#serve-public-routes-through-a-cloudflare-tunnel), Cloudflare serves public
wildcards' HTTPS, so they don't need DNS-01.

### Which certificate covers a hostname

A private hostname, or the dashboard's, shares the wildcard certificate of its
parent domain, when that parent is the provider's zone or inside it. Otherwise
it gets a certificate of its own. A wildcard route uses its own wildcard
certificate. An exact public hostname gets its own certificate from the
gateway. With the zone `piquel.fr`:

| Hostname | Certificate |
| --- | --- |
| `admin.piquel.fr`, `staging.piquel.fr` | `*.piquel.fr` |
| `admin.staging.piquel.fr`, `*.staging.piquel.fr` | `*.staging.piquel.fr` |
| `piquel.fr` | `piquel.fr`, since its parent `fr` is outside the zone |

All the private previews below `dev.piquel.fr` therefore share one
certificate, `*.dev.piquel.fr`. Exact public routes below a public wildcard
keep their own certificates from the gateway. Exact private routes below it
share the wildcard's.

A hostname belongs to the provider with the longest zone containing it. A zone
that two providers both list is a conflict, and neither uses it.

### Choose the certificate authority

piqueld uses Let's Encrypt by default. It registers an account the first time
it needs a certificate, and accepts the authority's terms of service.

`email` is the account's contact address. piqueld sends it only when it first
registers, so set it before your first DNS-01 certificate:

```nix
services.piqueld.settings.ingress.acme.email = "admin@example.com";
```

Or, in `config.toml`:

```toml
[ingress.acme]
email = "admin@example.com"
```

`directory` sets another ACME certificate authority, by the HTTPS URL of its
directory. The default is
`https://acme-v02.api.letsencrypt.org/directory`. piqueld registers without
external account binding, so the authority must not require it: ZeroSSL, for
example, can't be the `directory`.

### Renewal and failures

piqueld checks its certificates every minute. It renews one when less than a
third of its lifetime remains: about 30 days before a 90-day certificate
expires.

- A failed attempt is retried after 5 minutes, then twice as long after each
  new failure, up to every 6 hours.
- A certificate that no route needs any more isn't renewed, and is deleted
  once it expires.
- piqueld renews these certificates only while the daemon runs. Its
  certificates live in the data directory, under `ingress/`, so
  [back it up](../guides/backups-and-upgrades.md#backing-up) whole.

On the host, `piquelctl status` lists each certificate, the hostnames using it,
and its expiry, in milliseconds since 1970:

```text
Certificate: *.piquel.fr for admin.piquel.fr, staging.piquel.fr (expires at Unix ms 1767225600000)
```

A failing one adds a `Certificate problem:` line with the error. In the
dashboard, **System status** shows a **Certificate** card for each, marked
**Issued**, **Pending**, **Failed**, or **Renewal failing** when the latest
attempt to renew a stored certificate failed.

Each issue, renewal and deletion appears in the daemon's
[events](../guides/observability.md#history-and-diagnostics). A destination
that receives [`daemon_failures` notifications](../guides/observability.md#notifications)
is notified when a certificate that keeps failing to renew is less than 14
days from expiry. If it also receives `recovery` notifications, it is notified
again once the certificate recovers.

## Troubleshooting

- **A route is `failed` with `No DNS-01 certificate: no configured DNS
  provider zone contains <hostname>`.** No provider lists the hostname's zone.
  Check the provider's zones with `piquelctl dns refresh`; see
  [Cloudflare](cloudflare.md) or [OVH](ovh.md).
- **A route or certificate reports `zone <zone> is claimed by more than one
  DNS provider`.** `piquelctl dns refresh` prints the same problem as
  `DNS problem: Zones also claimed by another provider are not used: <zones>`.
  Two providers list the same zone. Remove the zone from one of the accounts,
  or one provider from the configuration.
- **A certificate fails with `was not visible on every authoritative
  nameserver`.** The TXT record didn't reach every nameserver within 10
  minutes. piqueld tries again later.
- **A private route below a public wildcard answers 404 on the internet.**
  That's expected: the public listener completes the connection with the
  wildcard's certificate, but the wildcard doesn't serve names that have their
  own route.
- **Your hostname is refused with `hostname_conflict`.** Another application
  has a wildcard above it, or piqueld uses the hostname itself.
