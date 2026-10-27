# Publishing piqueld.dev

piqueld.dev is served by a piqueld instance, which runs everything on it as one
application, `piqueld-dev`: this book at <https://docs.piqueld.dev>, and
Grafana at `grafana.piqueld.dev`, on the instance's tailnet, showing the
instance's own telemetry. Every push to `main` deploys it. This page sets it
up once, on the instance that serves it.

[`deploy/piqueld-dev.toml`](https://github.com/piquel-fr/piqueld/blob/main/deploy/piqueld-dev.toml)
is the application's manifest. piqueld reads it from `main` on each
deployment, with the files its services build and mount, from the same commit:

- [`deploy/docs`](https://github.com/piquel-fr/piqueld/tree/main/deploy/docs)
  holds the `Dockerfile` of the `web` service. It builds the book with the
  mdBook that `flake.lock` pins, through `nix develop .#docs`, the shell
  `just docs` and CI use. It serves the pages with
  [static-web-server](https://static-web-server.net/) on port 8080, as an
  unprivileged user.
- [`deploy/observability`](https://github.com/piquel-fr/piqueld/tree/main/deploy/observability)
  holds the configuration of the telemetry stack: an OpenTelemetry Collector
  receiving the daemon's telemetry, Prometheus keeping metrics for 15 days,
  Tempo and Loki keeping traces and logs for 7, and Grafana. Grafana's data
  sources link logs and traces both ways, and metric exemplars to traces. Its
  dashboards cover the daemon's health, deployments and builds, services, and
  logs.

The user guide's
[observability stack](../user/guides/observability.md#observability-stack) is
a smaller version of this stack, for any instance.

To add a site, such as a marketing page, add its service and route to the
manifest in a pull request.

## Before you start

These steps set up an instance that doesn't serve piqueld.dev yet. If the
instance already serves it, such as from other applications, migrate that
deployment to `piqueld-dev` instead: these steps would create a second
application, which can't deploy while the others hold its hostnames.

- The instance serves public routes through
  [HTTP ingress](../user/guides/ingress.md#enable-and-expose-an-application),
  and has a [webhook hostname](../user/guides/ingress.md#push-webhooks) whose
  DNS is public, so GitHub can report pushes. It also serves
  [private routes](../user/guides/ingress.md#public-and-private-routes) on its
  tailnet, with a [DNS-01 provider](../user/guides/ingress.md#dns-01-certificates)
  for `piqueld.dev`.
- The host is `x86_64-linux`, the only Linux system the flake's `docs` shell
  supports, and can reach GitHub and `cache.nixos.org` while it builds.
- You can create applications on the instance, change and deploy them, and
  run commands in them: `apps:create`, `apps:write`, `apps:deploy` and
  `apps:exec`. Granting the application the daemon's telemetry also needs
  `telemetry:grant`, which only `admin` holds by default; see
  [permissions](../user/accounts/authorization.md#grants).
- You can change the DNS of `piqueld.dev` and the settings of the
  `piquel-fr/piqueld` repository on GitHub.

Run the `piquelctl` commands on this page from a checkout of `main`, on the
host or on any machine where `piquelctl` is signed in to the instance.

## Set it up

1. Point the hostnames at the instance:
   - `docs.piqueld.dev`, like any public route: an `A` record to the host's
     public address, a CNAME to the tunnel with a
     [Cloudflare Tunnel](../user/guides/ingress.md#cloudflare-tunnel), or
     nothing when piqueld
     [manages the zone's records](../user/guides/ingress.md#managed-dns-records).
   - `grafana.piqueld.dev`, like any
     [private route](../user/guides/ingress.md#dns-for-private-routes): `A`
     and `AAAA` records to the apps node's tailnet addresses, or nothing when
     piqueld manages the zone's records.

2. Check what saving the manifest would do:

   ```console
   piquelctl app plan --file deploy/piqueld-dev.toml
   ```

   The plan creates the `piqueld-dev` application, with its `web`,
   `collector`, `prometheus`, `tempo`, `loki` and `grafana` services, and its
   routes on `docs.piqueld.dev` and `grafana.piqueld.dev`.

3. Save the manifest, grant its environment the daemon's telemetry, and deploy
   it:

   ```console
   piquelctl app apply --file deploy/piqueld-dev.toml
   piquelctl env telemetry piqueld-dev production on
   piquelctl --timeout 10m app deploy piqueld-dev
   ```

   `app apply` and `app deploy` ask you to confirm. `app deploy` then waits
   until the deployment finishes. Grant the telemetry first: without it, the
   collector can't start, and the deployment fails. The first build downloads
   the Nix image and the documentation toolchain, and can take several
   minutes. Later builds reuse them, and only rebuild the book.

4. Generate the secret of the repository's webhook:

   ```console
   piquelctl app repository webhook rotate piqueld-dev
   ```

   It prints the webhook's payload URL and secret. piqueld never shows the
   secret again.

5. In GitHub, open the repository's **Settings**, then **Webhooks**, and
   select **Add webhook**. Set **Payload URL** and **Secret** to the values
   from the previous step, **Content type** to `application/json`, and the
   events to **Just the push event**. Then select **Add webhook**.

From now on, change the site through pull requests to `main`, including
`deploy/piqueld-dev.toml`: each push to `main` deploys the manifest, the book
and the stack's configuration of that commit. Services whose configuration
didn't change keep running. Applying the file again is safe, since it is the
same manifest the repository holds.
[Repository-backed manifests](../user/reference/manifest.md#repository-backed-manifests)
and [deploying on push](../user/reference/manifest.md#deploying-on-push)
describe how this works.

## Check that it works

The site serves the book:

```console
curl --fail --head https://docs.piqueld.dev/dev/publishing.html
```

`piquelctl status` shows the telemetry receiver connected. Open
<https://grafana.piqueld.dev> from the tailnet, and sign in as `admin` with the
generated password:

```console
piquelctl app exec piqueld-dev grafana -- cat /run/secrets/grafana-admin-password
```

Grafana sets it when it first starts, so regenerating the secret doesn't
change it; change it in Grafana instead. Grafana's **piqueld** folder holds the
dashboards.

After the next push to `main`, check that it deployed:

```console
piquelctl env show piqueld-dev production
```

Its `Sync` line says `following`, with the commit of that push. In GitHub,
the webhook's **Recent Deliveries** shows each push, with piqueld's response
`204`.

## Send applications' telemetry to it

Other applications on the instance can send their own telemetry to the
collector. Uncomment the manifest's `[[spec.exports]]`, with their names in
place of `my-app`. Then import the collector in each of them, and point its
OpenTelemetry SDK at it:

```toml
[[spec.imports]]
application = "piqueld-dev"
service = "collector"

[[spec.services]]
name = "web"
[spec.services.environment]
OTEL_EXPORTER_OTLP_ENDPOINT = "http://collector.piqueld-dev:4317"
OTEL_EXPORTER_OTLP_PROTOCOL = "grpc"
```

## Build the docs image locally

From the root of the repository:

```console
docker build --file deploy/docs/Dockerfile --tag piqueld-docs .
docker run --rm --publish 8080:8080 piqueld-docs
```

The book is then on <http://localhost:8080>. To build it in a
[development instance](development.md#docker)'s engine rather than the host's,
set `DOCKER_HOST` to that engine's socket first.

## Troubleshooting

- **The deployment fails with `git_build_failed`.** The build's output ends
  with the error. List the builds with
  `piquelctl builds list --application <id>`, where `<id>` is the ID
  `piquelctl app show piqueld-dev` prints, then read one with
  `piquelctl builds logs <number>`.
- **The first deployment fails with `preparation_timeout`.** The build took
  longer than
  [`reconciliation.prepare_timeout_seconds`](../user/reference/configuration-settings.md#reconciliationprepare_timeout_seconds),
  300 seconds by default. Deploy again with
  `piquelctl --timeout 10m app deploy piqueld-dev`: the steps that finished
  stay cached. If it keeps failing, raise the setting.
- **The deployment fails with `hostname_conflict`.** Another application
  holds `docs.piqueld.dev` or `grafana.piqueld.dev`. This instance already
  serves piqueld.dev: migrate that deployment instead.
- **A push deploys nothing.** `piquelctl env show piqueld-dev production`
  says why on its `Sync` line, and GitHub's **Recent Deliveries** shows whether
  the delivery failed. A `401` means the secret doesn't match: generate a new one
  and copy it into GitHub again.
