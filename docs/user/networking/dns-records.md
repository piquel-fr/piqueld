# Managing DNS records

piqueld can create your routes' DNS records itself, keep them pointing at the
right place, and delete them when you remove a route. This page turns that on.
Without it, you create each route's records by hand, as the other networking
pages describe.

## Before you start

- A [Cloudflare](cloudflare.md) or [OVH](ovh.md) account hosting the zone,
  with the credentials those pages describe.
- For public routes on ports 80 and 443, your server's public IP addresses.
- Run `piquelctl` on the host, or on any machine where it is signed in.

## Set it up

1. Set [`manage_records`](../reference/configuration-settings.md#dnsprovidersmanage_records)
   on the provider, and list your server's public addresses in
   [`public_addresses`](../reference/configuration-settings.md#ingresspublic_addresses):

   ```nix
   services.piqueld.settings = {
     ingress.enabled = true;
     ingress.public_addresses = [ "203.0.113.10" "2001:db8::10" ];
     dns.providers = [
       {
         kind = "cloudflare";
         api_token_file = "/run/secrets/cloudflare-dns-token";
         manage_records = true;
       }
     ];
   };
   ```

   Or, in `config.toml`:

   ```toml
   [ingress]
   enabled = true
   public_addresses = ["203.0.113.10", "2001:db8::10"]

   [[dns.providers]]
   kind = "cloudflare"
   api_token_file = "cloudflare-dns-token"
   manage_records = true
   ```

   List an IPv6 address only if public IPv6 traffic actually reaches your
   server. With a [Cloudflare Tunnel](cloudflare.md#serve-public-routes-through-a-cloudflare-tunnel),
   `public_addresses` isn't used.

2. Rebuild the system, or restart the daemon.

piqueld only manages records in the zones of providers that set
`manage_records`. Other providers' zones stay yours to manage.

## What piqueld creates

| Route | Records |
| --- | --- |
| Public, on ports 80 and 443 | A and AAAA records to `public_addresses`. Without any, you create them. |
| Public, through a Cloudflare Tunnel | A proxied CNAME record to `<tunnel-id>.cfargotunnel.com`. |
| Private | A and AAAA records to the [apps node's](private-routes.md) tailnet addresses. |

- The [webhook hostname](webhooks.md) gets a public route's records while
  the gateway serves it, and the
  [dashboard hostname](private-routes.md#serve-the-dashboard-on-your-own-domain)
  a private route's.
- A [wildcard route](wildcards.md) gets its records on its wildcard name,
  `*.<parent>`. Exact routes' records below it are more specific, so they
  override it.
- A and AAAA records have a 5-minute TTL. On Cloudflare, records to your
  server's addresses aren't proxied: piqueld turns proxying back off if you
  turn it on.

piqueld writes a record once the gateway serves the route, and deletes it once
the gateway no longer does. It checks every 10 seconds whether records need to
change, and reads them back every 5 minutes to repair any you changed by hand.
So when the apps node's tailnet addresses change, its records follow within
about 20 seconds, plus the time the provider takes to publish them.

## Records piqueld owns

Before it writes a hostname's records, piqueld creates a TXT record next to
them that marks them as its own: `_piqueld.<hostname>`, holding this
installation's ID. For a wildcard, it is `_piqueld._wildcard.<parent>`.
`piquelctl status` prints the installation's ID after `instance`.

piqueld never overwrites records it doesn't own. If a hostname already has an
A, AAAA or CNAME record without that TXT record, or another installation also
claims it, its route reports `dns_conflict` and lists the records in the way.
Once you've checked that they're stale, and not serving another installation,
remove them, and piqueld writes its own. Otherwise, use another hostname.
piqueld leaves other record types at the hostname, such as MX or TXT records,
alone.

Don't delete the `_piqueld` records: without them, piqueld no longer
recognizes its own records, and the hostname reports `dns_conflict`.

## Check that it works

In the dashboard, the application's **Routes** tab shows each route's DNS in
the **DNS** and **DNS records** columns of **Deployed routes**. With
`piquelctl`, the `DNS` and `RECORDS` columns show the same:

```console
piquelctl app route list hello
```

The DNS state is one of:

| State | Meaning |
| --- | --- |
| `managed` | piqueld created the records and keeps them current. |
| `pending` | piqueld will write them, but hasn't yet: the gateway doesn't serve the route yet, the apps node has no tailnet addresses yet, private routes are turned off, or the provider failed and piqueld retries within a minute. |
| `dns_conflict` | Records piqueld doesn't own are in the way. |
| `manual` | No provider manages the zone, a public route has no `public_addresses`, the provider couldn't list its zones, or two providers list the zone. You create the records. |

**DNS records** and `RECORDS` show what the hostname needs, whoever creates
it. A route's URL stays pending while its DNS state is `pending` or
`dns_conflict`.

To see which providers manage records, open **System status** in the
dashboard: each provider's **DNS provider** card says **Manages route DNS
records**, or **Route DNS records are manual**. On the host, `piquelctl
status` says the same:

```text
DNS provider: cloudflare (healthy, manages route records): example.com
```

Each change appears in the daemon's
[events](../guides/observability.md#history-and-diagnostics). A failed write
also records a `dns_records_failed` diagnostic.

## Turn it off

Removing `manage_records` or `public_addresses` leaves the existing records in
place. Disabling the gateway with `ingress.enabled = false` deletes the
records piqueld manages, once the gateway has stopped.

## Troubleshooting

- **A route reports `dns_conflict`.** Its hostname has records piqueld
  doesn't own, such as one you created by hand or with
  `cloudflared tunnel route dns`. Until the route is `ready`, its message
  lists them. Delete them once you've checked that nothing else uses them;
  see [records piqueld owns](#records-piqueld-owns).
- **A route stays `pending` with `The DNS provider could not update the
  records`.** Check the provider's credentials and permissions on its page,
  and the daemon logs. piqueld retries within a minute.
- **A tunnel route stays `pending` in an OVH zone.** OVH can't hold the
  proxied CNAME a tunnel route needs. Move the zone to Cloudflare.
- **After a restart, every route's DNS state is `pending` for a while.**
  piqueld waits for the gateway to apply its configuration before it changes
  any record.
- **After reinstalling piqueld with a new database, every route reports
  `dns_conflict`.** The new installation has a new ID, so it doesn't own the
  records the old one created. Delete them, with their `_piqueld` TXT records,
  and piqueld writes its own.
