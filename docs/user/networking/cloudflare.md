# Cloudflare

piqueld works with Cloudflare in two independent ways:

- As a **DNS provider**, it proves control of your domains for
  [DNS-01 certificates](wildcards.md#dns-01-certificates), and can
  [manage your routes' DNS records](dns-records.md).
- Through a **Cloudflare Tunnel**, it serves your public routes without any
  inbound port on your server.

## Before you start

- Your domain's DNS zone is hosted on Cloudflare. Its registrar can be
  another company.
- The gateway is enabled; see
  [Serving an application over HTTPS](routes.md#enable-the-gateway).

## Use Cloudflare as a DNS provider

### Create an API token

piqueld calls Cloudflare's API with an API token that has two permissions:

| Permission | piqueld uses it to |
| --- | --- |
| **Zone › Zone › Read** (Zone Read in the permissions reference) | list your zones |
| **Zone › DNS › Edit** (DNS Write in the permissions reference) | create, read, update and delete records |

Limit the token's zone resources to the zones piqueld should use, or allow
them all. Cloudflare's [Create API token](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/)
explains how to create it, and its
[API token permissions](https://developers.cloudflare.com/fundamentals/api/reference/permissions/)
list each permission.

### Set it up

1. Save the token in a file that only root can read, such as an agenix or
   sops-nix secret.

2. Add the provider to your configuration:

   ```nix
   services.piqueld.settings.dns.providers = [
     {
       kind = "cloudflare";
       api_token_file = "/run/secrets/cloudflare-dns-token";
       # Only to let piqueld manage routes' DNS records.
       manage_records = true;
     }
   ];
   ```

   In `/etc/piqueld/config.toml`, pass the file to the service with
   `LoadCredential=`, as for the
   [tailnet auth key](../getting-started/install.md#other-linux-distributions),
   and refer to it by name:

   ```toml
   [[dns.providers]]
   kind = "cloudflare"
   api_token_file = "cloudflare-dns-token"
   manage_records = true
   ```

3. Restart the daemon, which reads the token only when it starts. On NixOS,
   rebuild the system. Elsewhere, reload the unit you edited first:

   ```console
   sudo systemctl daemon-reload
   sudo systemctl restart piqueld
   ```

### Check that it works

On the host, ask piqueld to list your zones now:

```console
$ sudo piquelctl dns refresh
DNS provider: cloudflare (healthy, manages route records): example.com
```

It lists every zone the token can read. piqueld also lists them when it
starts, and every hour. After you change the token's permissions or add a
zone, run `piquelctl dns refresh` again, or select **Check DNS providers** in
the dashboard's **System status**. Checking needs the `system:operate`
[permission](../accounts/authorization.md#grants), and seeing the result
`system:read`.

## Serve public routes through a Cloudflare Tunnel

Without a tunnel, the internet reaches your public routes on your server's
ports 80 and 443. With one, piqueld runs Cloudflare's `cloudflared` beside the
gateway. It connects out to Cloudflare, and Cloudflare sends your public
routes' requests back through that connection. Your server needs no inbound
port at all, which also works behind NAT. Private routes still arrive over
your tailnet.

Consider the trade-offs first:

- **Cloudflare sees all your public traffic unencrypted.** It serves HTTPS to
  your visitors, and forwards their requests through the tunnel. On your
  server, the tunnel's traffic stays on a network that only piqueld's own
  containers join, so your applications can't observe it.
- **Ports 80 and 443 close.** Every public route goes through the tunnel; you
  can't serve some on ports 80 and 443.
- **Cloudflare's free Universal SSL certificate covers one level of
  subdomain.** It covers
  `example.com` and `notes.example.com`, but not deeper names such as
  `x.staging.example.com`; see Cloudflare's
  [Universal SSL limitations](https://developers.cloudflare.com/ssl/edge-certificates/universal-ssl/limitations/).
- **The gateway no longer redirects HTTP to HTTPS.** Turn on Cloudflare's
  **Always Use HTTPS** to do it instead.

### Create the tunnel

The tunnel is locally managed: piqueld configures it, and you never add
hostnames to it in Cloudflare.

1. On any machine, install `cloudflared` and log in to your Cloudflare
   account:

   ```console
   cloudflared tunnel login
   ```

2. Create the tunnel:

   ```console
   cloudflared tunnel create piqueld
   ```

   It prints the tunnel's ID, and writes its credentials to
   `~/.cloudflared/<tunnel-id>.json`. Cloudflare's
   [Create a locally-managed tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/do-more-with-tunnels/local-management/create-local-tunnel/)
   describes both commands. You don't need a `config.yml` or
   `cloudflared tunnel run`: piqueld does that part.

3. Copy the credentials file to your server, readable only by root, such as
   an agenix or sops-nix secret.

4. Turn on [Always Use HTTPS](https://developers.cloudflare.com/ssl/edge-certificates/additional-options/always-use-https/)
   for the zone in Cloudflare.

### Set it up

1. Point piqueld at the credentials file:

   ```nix
   services.piqueld.settings.ingress = {
     enabled = true;
     tunnel = {
       enabled = true;
       credentials_file = "/run/secrets/cloudflared-tunnel.json";
     };
   };
   ```

   Or in `config.toml`, with the file passed by `LoadCredential=`:

   ```toml
   [ingress.tunnel]
   enabled = true
   credentials_file = "cloudflared-tunnel"
   ```

2. Restart the daemon. It replaces the gateway to switch to the tunnel, which
   briefly interrupts your routes. On NixOS, rebuild the system. Elsewhere,
   reload the unit you edited first:

   ```console
   sudo systemctl daemon-reload
   sudo systemctl restart piqueld
   ```

3. Point each public hostname at the tunnel with a proxied CNAME record to
   `<tunnel-id>.cfargotunnel.com`:

   ```text
   notes.example.com.   CNAME   6ff42ae2-765d-4adf-8112-31c55c1551ef.cfargotunnel.com.   ; proxied
   ```

   If piqueld [manages your DNS records](dns-records.md), it creates them.
   Otherwise, create them in Cloudflare, or, on the machine where you ran
   `cloudflared tunnel login`, with
   `cloudflared tunnel route dns piqueld notes.example.com`.
   `piquelctl app route list` shows the record each route needs.

### Check that the tunnel works

On the host:

```console
$ sudo piquelctl status
...
Public listener: healthy: Caddy is running and routing configuration is applied
Public ingress: Cloudflare Tunnel 6ff42ae2-765d-4adf-8112-31c55c1551ef (connected)
...
```

In the dashboard, **System status** shows a **Cloudflare Tunnel** card,
**Connected** once the tunnel reaches Cloudflare. A public route is `ready`
once piqueld reached it through the tunnel over trusted HTTPS.

### Leave the tunnel

To serve public routes on ports 80 and 443 again:

1. Set `tunnel.enabled = false`, and rebuild the system or restart the
   daemon. piqueld replaces the gateway again, stops `cloudflared`, and
   deletes its copy of the credentials.
2. Open TCP ports 80 and 443 in the server's firewall, as in
   [Enable the gateway](routes.md#enable-the-gateway).
3. Replace each proxied CNAME record to the tunnel with A and AAAA records to
   your server. If piqueld [manages your DNS records](dns-records.md) and you
   listed your `public_addresses`, it does that itself.

## Troubleshooting

- **`dns refresh` reports `Zone discovery failed` with `HTTP 403`.** The token
  is wrong or expired, or may lack **Zone › Zone › Read**.
- **`dns refresh` lists `no zones`.** The token's zone resources don't include
  your zone. Edit the token in Cloudflare, then run `dns refresh` again.
- **Certificates fail with `write record` and `HTTP 403`.** The token lacks
  **Zone › DNS › Edit** on that zone.
- **A route reports `dns_conflict` after `cloudflared tunnel route dns`.** With
  `manage_records`, piqueld doesn't own records it didn't create. Delete the
  CNAME record, and let piqueld create it.
- **The tunnel stays `not connected`.** The daemon logs include
  `cloudflared`'s own logs. While it is disconnected, public routes are
  unreachable. In the dashboard's **System status**, the **Cloudflare Tunnel**
  card shows **Disconnected**, and **HTTPS ingress** shows **Unhealthy**.
- **Startup fails with `is not a cloudflared tunnel credentials file`.** The
  file must be the JSON file `cloudflared tunnel create` wrote, holding
  `AccountTag`, `TunnelSecret` and `TunnelID`, not `cert.pem` or a tunnel
  token.
- **A deeper name, such as `x.staging.example.com`, has no valid certificate
  through the tunnel.** Cloudflare's Universal SSL certificate doesn't cover
  it; see the
  [trade-offs](#serve-public-routes-through-a-cloudflare-tunnel). Use
  Cloudflare's Advanced Certificate Manager, or keep tunnel hostnames one
  level below the zone.
- **Switching to or from the tunnel doesn't happen.** If an application's
  network is broken, piqueld keeps the gateway in its current mode until the
  network is repaired or that application's routes are removed. Until then,
  the gateway applies no route changes, so deployments of environments with
  routes fail with `ingress_unavailable` and are retried. The daemon logs name
  the application.
