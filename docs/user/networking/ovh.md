# OVH

This page connects piqueld to the OVHcloud API, so that it can prove control
of your domains hosted at OVH. piqueld then gets
[DNS-01 certificates](wildcards.md#dns-01-certificates) for your private and
wildcard routes, and can [manage their DNS records](dns-records.md).

## Before you start

- Your domain's DNS zone is hosted at OVHcloud.
- You can create API keys for the OVHcloud account that holds the zone.

## Create the API keys

piqueld signs its requests with three values: an application key, an
application secret, and a consumer key. You create all three at once on
OVHcloud's token creation page; OVHcloud's
[first steps with the API](https://docs.ovhcloud.com/en/guides/manage-and-operate/api/first-steps)
explains how.

When the page asks for the rights, add these:

| Method | Path | piqueld uses it to |
| --- | --- | --- |
| `GET` | `/domain/zone` | list your zones |
| `POST` | `/domain/zone/*` | create records, and apply the zone's changes |
| `DELETE` | `/domain/zone/*` | delete records |

If piqueld [manages your routes' DNS records](dns-records.md), add these too:

| Method | Path | piqueld uses it to |
| --- | --- | --- |
| `GET` | `/domain/zone/*` | read records |
| `PUT` | `/domain/zone/*` | update records |

piqueld uses the keys for as long as the daemon runs. Choose a validity that
doesn't expire, or replace the keys before they do.

## Set it up

1. Save each value in its own file that only root can read, such as agenix or
   sops-nix secrets.

2. Add the provider to your configuration. `endpoint` is the region of your
   OVHcloud account:

   ```nix
   services.piqueld.settings.dns.providers = [
     {
       kind = "ovh";
       endpoint = "ovh-eu";
       application_key_file = "/run/secrets/ovh-application-key";
       application_secret_file = "/run/secrets/ovh-application-secret";
       consumer_key_file = "/run/secrets/ovh-consumer-key";
       # Only to let piqueld manage routes' DNS records.
       manage_records = true;
     }
   ];
   ```

   | `endpoint` | Account region | API |
   | --- | --- | --- |
   | `ovh-eu` | Europe | `https://eu.api.ovh.com/1.0` |
   | `ovh-ca` | Canada | `https://ca.api.ovh.com/1.0` |
   | `ovh-us` | United States | `https://api.us.ovhcloud.com/1.0` |

   In `/etc/piqueld/config.toml`, pass each file to the service with
   `LoadCredential=`, as for the
   [tailnet auth key](../getting-started/install.md#other-linux-distributions),
   and refer to it by name:

   ```toml
   [[dns.providers]]
   kind = "ovh"
   endpoint = "ovh-eu"
   application_key_file = "ovh-application-key"
   application_secret_file = "ovh-application-secret"
   consumer_key_file = "ovh-consumer-key"
   manage_records = true
   ```

3. Restart the daemon, which reads the files only when it starts. On NixOS,
   rebuild the system. Elsewhere, reload the unit you edited first:

   ```console
   sudo systemctl daemon-reload
   sudo systemctl restart piqueld
   ```

## Check that it works

On the host, ask piqueld to list your zones now:

```console
$ sudo piquelctl dns refresh
DNS provider: ovh (healthy, manages route records): example.com
```

It lists every zone the keys can see. piqueld also lists them when it starts,
and every hour, so a zone you add later is found within the hour. In the
dashboard, the **DNS provider** card in **System status** shows the same, and
**Check DNS providers** checks now.

## How piqueld uses OVH

- OVHcloud applies record changes only when the zone is refreshed. piqueld
  refreshes the zone after each change.
- Changes can take several minutes to reach OVHcloud's nameservers. Before it
  asks for a certificate, piqueld waits up to 10 minutes for its
  `_acme-challenge` record to appear on each of them.
- OVHcloud can't proxy records like Cloudflare does, so a
  [Cloudflare Tunnel](cloudflare.md#serve-public-routes-through-a-cloudflare-tunnel)
  can't serve routes in an OVH zone.

## Troubleshooting

- **`dns refresh` reports `unhealthy` with `Zone discovery failed`.** The
  message ends with OVHcloud's answer. An `HTTP 403` means the keys are wrong,
  expired or missing a right from [the tables above](#create-the-api-keys).
  piqueld keeps using the zones it found before, and tries again within a
  minute.
- **You replaced a key file, and nothing changed.** piqueld only reads the
  files when it starts. Restart the daemon.
- **A certificate fails with `was not visible on every authoritative
  nameserver`.** The zone's change didn't propagate within 10 minutes. piqueld
  tries again later; see [renewal](wildcards.md#renewal-and-failures).
- **A tunnel route's DNS records stay `pending`.** Tunnel routes need a
  proxied CNAME record, which only a Cloudflare zone can hold. Move the zone to
  Cloudflare, or serve the route on ports 80 and 443.
