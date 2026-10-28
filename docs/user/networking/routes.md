# Serving an application over HTTPS

This page puts an application's service on a public HTTPS address, such as
`https://hello.example.com`. piqueld runs a gateway in front of your
applications: it receives requests on ports 80 and 443, gets a certificate for
each hostname, and forwards each request to its service.

A route can also be private, so that only devices on your tailnet reach it;
see [private routes](private-routes.md).

## Before you start

- A domain name, and access to its DNS records.
- A server that the internet reaches on ports 80 and 443, with nothing else
  listening on them. Without inbound ports, such as behind NAT, serve your
  routes through a [Cloudflare Tunnel](cloudflare.md#serve-public-routes-through-a-cloudflare-tunnel)
  instead.
- Docker Engine 28 or later on the server.
- An application whose service serves plain HTTP, such as `hello` from
  [Deploying your first application](../getting-started/first-app.md).
- Run `piquelctl` on the host, or on any machine where it is signed in.

piqueld doesn't put a sign-in in front of your routes: anyone can open a
public route, so your application handles its own authentication.

## Enable the gateway

1. Enable it in your configuration, with
   [`ingress.enabled`](../reference/configuration-settings.md#ingressenabled):

   ```nix
   services.piqueld.settings.ingress.enabled = true;
   ```

   Or, in `/etc/piqueld/config.toml`:

   ```toml
   [ingress]
   enabled = true
   ```

2. Open TCP ports 80 and 443 in the server's firewall. The NixOS module
   doesn't open them:

   ```nix
   networking.firewall.allowedTCPPorts = [ 80 443 ];
   ```

3. Rebuild the system, or restart the daemon with
   `sudo systemctl restart piqueld`. piqueld reads its `[ingress]` settings
   only when it starts.

Check that the gateway runs. In the dashboard, **System status** on the
**Overview** shows **HTTPS ingress** as **Ready**. On the host:

```console
$ sudo piquelctl status
...
Public listener: healthy: Caddy is running and routing configuration is applied
Public ingress: ports 80/443
...
```

## Add a route

1. Create a DNS record for the hostname: an A record to your server's public
   IPv4 address. Add an AAAA record to its IPv6 address only if public IPv6
   traffic actually reaches your server. piqueld can also
   [create these records](dns-records.md) for you.

2. Add the route, in the dashboard, with `piquelctl`, or in the manifest, as
   below. A route sends the hostname's requests to a service, on the port it
   serves HTTP on inside its container; the port doesn't need to be
   published. Make it public so that anyone on the internet can connect.
   Without that, the route is [private](private-routes.md).

Saving a route doesn't change what's served; deploying does. piqueld starts
serving the route once its service is ready. The gateway then gets the
hostname's certificate from Let's Encrypt, and renews it by itself. It
redirects plain HTTP requests to HTTPS.

### In the dashboard

1. Open **Applications**, then **hello**, then its **Routes** tab.
2. Select **Add route**.
3. Fill in the **Hostname**, and set **Visibility** to **Public**.
4. Set **Destination** to **Service**, then choose the **Service** and its
   **HTTP port**.
5. Select **Save changes**.
6. Select **Deploy to production**.

![The Routes tab of hello: a notice that ingress is disabled in the daemon configuration, then one route, hello.example.com, public, to the web service on HTTP port 80](../../images/networking/routes-tab.png)

On a daemon whose gateway is off, as in this image, the tab says so above the
routes. You can still save and deploy them. piqueld serves them once you
[enable the gateway](#enable-the-gateway).

### With `piquelctl`

Add the route and deploy it with
[`app route add`](../reference/cli-commands.md#piquelctl-app-route-add):

```console
piquelctl app route add hello hello.example.com web 80 --visibility public --deploy
```

### In the manifest

1. Add the route to the manifest, `hello.toml`:

   ```toml
   [[spec.routes]]
   hostname = "hello.example.com"
   service = "web"
   port = 80
   visibility = "public"
   ```

2. Deploy it:

   ```console
   piquelctl app apply --file hello.toml --deploy
   ```

## Check that it works

In the dashboard, the environment's **Overview** lists the route's URL in its
**URLs** card. With `piquelctl`, use
[`env url`](../reference/cli-commands.md#piquelctl-env-url):

```console
$ piquelctl env url hello production
URL: https://hello.example.com ready
```

The URL is `ready` once all of these hold:

- the gateway serves the route;
- piqueld itself fetched `https://hello.example.com`, with a trusted
  certificate, and reached this server's gateway;
- its [DNS records](dns-records.md), if piqueld manages them, are in place.

`ready` doesn't include the service's health, which the environment reports
separately.

Until then, it is `pending`, followed by what it waits for.

To see every route of the application, with what each one's hostname needs
from DNS, open its **Routes** tab: **Deployed routes** lists them. With
`piquelctl`:

```console
$ piquelctl app route list hello
HOSTNAME  NAME  ENVIRONMENT  VISIBILITY  STATE  DESTINATION  DNS  RECORDS
hello.example.com  -  production  public  ready  web:80  manual  A/AAAA -> this server's public addresses
```

Each route is in one of these states:

| State | Meaning |
| --- | --- |
| `ready` | piqueld reached the route over trusted HTTPS. |
| `pending` | It waits for the gateway, DNS or its certificate. The next line says what to check. |
| `failed` | It can't be served, for example without a certificate; the next line says why. |
| `disabled` | The gateway, or [private routes](private-routes.md), are turned off. |

## Redirect a hostname

A route can answer with a redirect instead, for example from `www` to your
main hostname.

In the dashboard, add the route on the **Routes** tab, set its
**Visibility** to **Public** and its **Destination** to **Redirect**, fill in
**Redirect to**, then select **Save changes** and **Deploy to production**.

With `piquelctl`, use
[`app route redirect`](../reference/cli-commands.md#piquelctl-app-route-redirect):

```console
piquelctl app route redirect hello www.hello.example.com https://hello.example.com --visibility public --deploy
```

In the manifest, then apply it with `--deploy`:

```toml
[[spec.routes]]
hostname = "www.hello.example.com"
redirect = { to = "https://hello.example.com" }
visibility = "public"
```

The gateway answers redirects itself, so they need no service. It sends a 308
redirect and keeps the request's path and query: `www.hello.example.com/about`
goes to `hello.example.com/about`. To change that, set **Status** and **Keep
path and query** in the dashboard, `--status` and `--no-preserve-path` with
`piquelctl`, or
[`status`](../reference/manifest-settings.md#specroutesredirectstatus) and
[`preserve_path`](../reference/manifest-settings.md#specroutesredirectpreserve_path)
in the manifest. Redirect hostnames get certificates like any other.

## Give each environment its own hostname

A hostname belongs to one environment. Several hostnames can lead to one
service, but two environments can't share a hostname: saving or deploying a
manifest that renders the same hostname in two environments fails with
`hostname_conflict`.

Give each environment its own hostname with a
[variable](../reference/manifest.md#variables): set the route's hostname to
`${{ vars.domain }}`, and give `domain` a value in each environment. For a
`staging` environment beside `production`, the dashboard and `piquelctl`
remove the route to `hello.example.com` first: `staging` would render it too,
so piqueld refuses to create `staging` beside it. The manifest changes
everything in one save.

- **In the dashboard**, open the application:

  1. On the **Routes** tab, select **Remove** on the `hello.example.com`
     route, then **Save changes**.
  2. On the **Environments** tab, select **New environment**, name it
     `staging`, then select **Create environment**. This opens `staging`'s
     page: return to the application through the **hello** breadcrumb.
  3. On the **Variables** tab, select **Add variable**, and name it `domain`.
     Set its **Default** to `hello.example.com`, and its value in the
     **staging** column to `staging.hello.example.com`, then select **Save
     changes**.
  4. On the **Routes** tab, select **Add route**. Set its **Hostname** to
     `${{ vars.domain }}`, its **Service** to **web**, its **HTTP port** to `80`
     and its **Visibility** to **Public**, then select **Save changes**.
  5. Select **Deploy**, then deploy each environment.

- **With `piquelctl`**, create the environment with
  [`env create`](../reference/cli-commands.md#piquelctl-env-create) and set
  the variable with
  [`app variable set`](../reference/cli-commands.md#piquelctl-app-variable-set).
  Single quotes keep your shell from expanding `$`. The last command deploys
  both environments:

  ```console
  piquelctl app route remove hello hello.example.com
  piquelctl env create hello staging
  piquelctl app variable set hello domain hello.example.com
  piquelctl app variable set hello domain staging.hello.example.com --env staging
  piquelctl app route add hello '${{ vars.domain }}' web 80 --visibility public --deploy
  ```

- **In the manifest**:

  ```toml
  [spec.variables]
  domain = "hello.example.com"

  [spec.environments.production]

  [spec.environments.staging.variables]
  domain = "staging.hello.example.com"

  [[spec.routes]]
  hostname = "${{ vars.domain }}"
  service = "web"
  port = 80
  visibility = "public"
  ```

An environment keeps its hostnames until a deployment withdraws its routes,
so no other environment or application can take them in the meantime.

To keep an environment, such as `staging`, off the internet, make its routes
[private](private-routes.md#make-an-environments-routes-private).

## Read the client's address

Your service receives requests from the gateway, not from the client. The
gateway adds the client's address in `X-Forwarded-For`, and sets
`X-Forwarded-Proto` and `X-Forwarded-Host`. It replaces any `X-Forwarded-For`
the client sent.

Trust those headers only when they come from the gateway. While the gateway is
enabled, each service a route leads to gets the variable
`PIQUELD_INGRESS_PROXIES`: the comma-separated address
ranges the gateway connects from, such as `10.0.3.0/24`. In Express, for
example:

```js
app.set("trust proxy", process.env.PIQUELD_INGRESS_PROXIES.split(","));
```

The environment's other routed services connect from that range too, so
trusting it also trusts them.

## Turn the gateway off

If you [serve the dashboard on your own domain](private-routes.md#serve-the-dashboard-on-your-own-domain),
follow [Move the dashboard off the gateway](#move-the-dashboard-off-the-gateway)
instead: the daemon doesn't start with the gateway off while
`ingress.private.dashboard_hostname` is set.

1. Turn the gateway off in your configuration:

   ```nix
   services.piqueld.settings.ingress.enabled = false;
   ```

   Or, in `/etc/piqueld/config.toml`:

   ```toml
   [ingress]
   enabled = false
   ```

2. Rebuild the system, or restart the daemon.

In the dashboard, **System status** shows **HTTPS ingress** as **Disabled**,
and `sudo piquelctl status` on the host no longer prints a `Public listener`
line. Ports 80 and 443 close, and every route reports `disabled` once piqueld has
confirmed the gateway stopped. Until then, routes report `failed`, because
routing may still be active.

piqueld keeps your routes and certificates, and still accepts and deploys
manifests with routes. When you enable the gateway again, it serves the routes
of your deployments, including those deployed while it was off.

While the gateway is off, piqueld deletes the DNS records it
[manages](dns-records.md), and deletes [DNS-01 certificates](wildcards.md#dns-01-certificates)
once they expire.

Stopping the daemon, on the other hand, leaves the gateway running, so your
routes stay up. The gateway keeps renewing the certificates it got itself.
[DNS-01 certificates](wildcards.md#dns-01-certificates), for wildcards,
private routes and the dashboard, wait for the daemon to renew them. The
webhook and dashboard hostnames stop answering, since the daemon serves them.

### Move the dashboard off the gateway

The gateway serves the dashboard's hostname, so the dashboard needs another
address before you turn the gateway off.

**This breaks your passkeys.** The dashboard only works at the address
[`auth.public_url`](../reference/configuration-settings.md#authpublic_url)
names, and a passkey only works at the address it was created on. The
passkeys you have stop working, and the steps below add a new one.

1. [Sign `piquelctl` in](../getting-started/first-account.md#sign-piquelctl-in)
   with your account, on the host or through piqueld's own tailnet node,
   which keep working after the change. The host operator can't create
   enrollment links.

2. [Choose the dashboard's new address](../getting-started/install.md#choose-how-to-reach-the-dashboard),
   and add its settings to your configuration. Your own reverse proxy can
   now use ports 80 and 443.

3. Remove `ingress.private.dashboard_hostname`, and turn the gateway off:

   ```nix
   services.piqueld.settings.ingress.enabled = false;
   ```

   Or, in `/etc/piqueld/config.toml`, remove `dashboard_hostname` from
   `[ingress.private]`, and set:

   ```toml
   [ingress]
   enabled = false
   ```

   If you set `auth.public_url` to the dashboard's hostname, set it to the new
   address. Left unset, it becomes piqueld's tailnet node's address if
   `tailscale.enabled` is set, and `http://localhost:7845` otherwise.

4. Rebuild the system, or restart the daemon.

5. Create a link that adds a passkey to your account:

   ```console
   piquelctl account enroll alice
   ```

6. Open the link at the new address, and save the new passkey.

If no CLI login works any more,
[recover administrator access](../accounts/recovery.md#no-administrator-can-sign-in).
Then check that the gateway stopped, as [above](#turn-the-gateway-off).

## Limits

- The service must serve plain HTTP, over HTTP/1.1. WebSockets and streaming
  responses work.
- A route serves a whole hostname: there is no routing by path, and no TCP or
  UDP port.
- An application has up to 64 routes.
- The hostnames of piqueld's website (`auth.public_url`),
  [dashboard](private-routes.md#serve-the-dashboard-on-your-own-domain) and
  [webhooks](webhooks.md), and the names below them, can't be routed. A route
  that already had one still deploys, but isn't served.
- Removing a service, with **Remove service** on its page or
  [`app service remove`](../reference/cli-commands.md#piquelctl-app-service-remove),
  also removes its routes from the saved configuration.

## Troubleshooting

- **The route stays `pending` with `Public HTTPS is not verified yet`.**
  Check that the hostname's A and AAAA records point at your server and no
  other, and that ports 80 and 443 are open to the internet. piqueld checks
  the route from the server itself, so a router that can't loop the server's
  public address back to it keeps the route `pending`, even when it works from
  elsewhere.
- **`Waiting for the gateway configuration to be applied`.** Check **HTTPS
  ingress** in **System status**. `Ingress unavailable: could not prepare the
  Caddy gateway (requires free ports 80/443 and Docker 28+)` means another
  program uses port 80 or 443, or Docker is too old. If you run your own
  reverse proxy for the dashboard, move it to other ports or another server.
- **Saving fails with `hostname_conflict`.** Another environment or
  application already has the hostname, or piqueld uses it itself. See
  [the previous section](#give-each-environment-its-own-hostname).
- **A route deploys but isn't served, and the daemon logs `route hostname is
  reserved for the piqueld website or its webhooks; the gateway will not
  publish it`.** The route had the hostname before piqueld used it itself.
  Give the route, or piqueld's website, dashboard or webhooks, another
  hostname; see [Limits](#limits).
- **Your service sees the same client address for every IPv6 request.** Docker
  forwards IPv6 to an IPv4-only network through a proxy, which hides the
  client. Serve the route over IPv4 only, by removing its AAAA record.
