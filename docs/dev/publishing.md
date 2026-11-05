# Publishing piqueld.dev

piqueld.dev is served by a piqueld instance, as two applications:

- `piqueld-dev` serves the marketing site at <https://piqueld.dev> and this
  book at <https://docs.piqueld.dev>. Each [preview](#preview-a-branch)
  serves a branch's site and book on the tailnet.
- `piqueld-observability` receives the instance's own telemetry, and shows it
  in Grafana at `grafana.piqueld.dev`, on the instance's tailnet.

Every push to `main` deploys both. This page sets them up once, on the
instance that serves them, then shows how to preview a change.

[`deploy/piqueld-dev.toml`](https://github.com/piquel-fr/piqueld/blob/main/deploy/piqueld-dev.toml)
and
[`deploy/piqueld-observability.toml`](https://github.com/piquel-fr/piqueld/blob/main/deploy/piqueld-observability.toml)
are their manifests. piqueld reads each from `main` on each deployment, with
the files its services build and mount, from the same commit:

- [`deploy/site`](https://github.com/piquel-fr/piqueld/tree/main/deploy/site)
  holds the `Dockerfile` of the `site` service. It serves
  [`apps/site`](https://github.com/piquel-fr/piqueld/tree/main/apps/site),
  static pages with no build step, with
  [static-web-server](https://static-web-server.net/) on port 8080, as an
  unprivileged user.
- [`deploy/docs`](https://github.com/piquel-fr/piqueld/tree/main/deploy/docs)
  holds the `Dockerfile` of the `docs` service. It builds the book with the
  mdBook that `flake.lock` pins, through `nix develop .#docs`, the shell
  `just docs` and CI use. It serves the pages with static-web-server
  too.
- [`deploy/observability`](https://github.com/piquel-fr/piqueld/tree/main/deploy/observability)
  holds the configuration of the telemetry stack: an OpenTelemetry Collector
  receiving the daemon's telemetry, Prometheus keeping metrics for 15 days,
  Tempo and Loki keeping traces and logs for 7, and Grafana. Grafana's data
  sources link logs and traces both ways, and metric exemplars to traces. Its
  dashboards cover the daemon's health, deployments and builds, services, and
  logs.

The telemetry stack is an application of its own because
[previews are never granted telemetry](../user/operations/telemetry.md#receive-telemetry-in-an-application):
in a preview of one application with both, the collector couldn't start. The
user guide's
[observability stack](../user/operations/observability-stack.md) is
a smaller version of this stack, for any instance.

To add another site, add its service, route and hostname variables to
`deploy/piqueld-dev.toml` in a pull request.

## Before you start

These steps set up an instance that doesn't serve piqueld.dev yet. If the
instance already serves it, such as from other applications, migrate that
deployment to these two applications instead: these steps would create new
ones, which can't deploy while the others hold their hostnames.

- The instance serves public routes through
  [HTTP ingress](../user/networking/routes.md), and has a
  [webhook hostname](../user/networking/webhooks.md) whose DNS is public, so
  GitHub can report pushes. It also serves
  [private routes](../user/networking/private-routes.md) on its tailnet, with
  a [DNS-01 provider](../user/networking/wildcards.md#dns-01-certificates) for
  `piqueld.dev`.
- The host is `x86_64-linux`, the only Linux system the flake's `docs` shell
  supports, and can reach GitHub and `cache.nixos.org` while it builds.
- You can create applications on the instance, change and deploy them, and
  run commands in them: `apps:create`, `apps:write`, `apps:deploy` and
  `apps:exec`. Granting `piqueld-observability` the daemon's telemetry also
  needs `telemetry:grant`, which only `admin` holds by default; see
  [permissions](../user/accounts/permissions.md#grants).
- You can change the DNS of `piqueld.dev` and the settings of the
  `piquel-fr/piqueld` repository on GitHub.

Run the `piquelctl` commands on this page from a checkout of `main`, on the
host or on any machine where `piquelctl` is signed in to the instance.

## Set it up

1. Point the hostnames at the instance:
   - `piqueld.dev`, `www.piqueld.dev` and `docs.piqueld.dev`, like any public
     route: an `A` record to the host's public address, a CNAME to the tunnel
     (flattened at the zone's apex) with a
     [Cloudflare Tunnel](../user/networking/cloudflare.md#serve-public-routes-through-a-cloudflare-tunnel),
     or nothing when piqueld
     [manages the zone's records](../user/networking/dns-records.md).
   - `grafana.piqueld.dev` and `*.preview.piqueld.dev`, like any
     [private route](../user/networking/private-routes.md#dns-for-private-routes):
     `A` and `AAAA` records to the apps node's tailnet addresses, or nothing
     when piqueld manages the zone's records. The wildcard covers every
     preview's hostnames.

2. Check what saving the manifests would do:

   ```console
   piquelctl app plan --file deploy/piqueld-dev.toml
   piquelctl app plan --file deploy/piqueld-observability.toml
   ```

   The first plan creates the `piqueld-dev` application, with its `site` and
   `docs` services, its routes on `piqueld.dev` and `docs.piqueld.dev`, and
   the redirect from `www.piqueld.dev`. The second creates
   `piqueld-observability`, with its `collector`, `prometheus`, `tempo`,
   `loki` and `grafana` services, and its route on `grafana.piqueld.dev`.

3. Save the websites' manifest and deploy it:

   ```console
   piquelctl app apply --file deploy/piqueld-dev.toml
   piquelctl --timeout 10m app deploy piqueld-dev
   ```

   `app apply` and `app deploy` ask you to confirm. `app deploy` then waits
   until the deployment finishes. The first build downloads the Nix image and
   the documentation toolchain, and can take several minutes. Later builds
   reuse them, and only rebuild the book.

4. Save the telemetry stack's manifest, grant its environment the daemon's
   telemetry, and deploy it:

   ```console
   piquelctl app apply --file deploy/piqueld-observability.toml
   piquelctl env telemetry piqueld-observability production on
   piquelctl app deploy piqueld-observability
   ```

   Grant the telemetry first: without it, the collector can't start, and the
   deployment fails.

5. Generate the secret of each application's webhook:

   ```console
   piquelctl app repository webhook rotate piqueld-dev
   piquelctl app repository webhook rotate piqueld-observability
   ```

   Each prints its application's payload URL and secret. piqueld never shows
   a secret again.

6. In GitHub, open the repository's **Settings**, then **Webhooks**, and
   select **Add webhook**. Set **Payload URL** and **Secret** to the values
   `piqueld-dev` printed, **Content type** to `application/json`, and the
   events to **Just the push event**. Then select **Add webhook**. Add a
   second webhook the same way, with the values `piqueld-observability`
   printed.

From now on, change the sites and the stack through pull requests to `main`,
including their manifests: each push to `main` deploys the manifests, the
book and the stack's configuration of that commit. Services whose
configuration didn't change keep running. Applying a file again is safe,
since it is the same manifest the repository holds.
[Repository-backed manifests](../user/reference/manifest.md#repository-backed-manifests)
and [deploying on push](../user/reference/manifest.md#deploying-on-push)
describe how this works.

## Check that it works

The site serves the marketing pages and the book:

```console
curl --fail --head https://piqueld.dev/
curl --fail --head https://docs.piqueld.dev/dev/publishing.html
```

`piquelctl status` shows the telemetry receiver connected. Open
<https://grafana.piqueld.dev> from the tailnet, and sign in as `admin` with the
generated password:

```console
piquelctl app exec piqueld-observability grafana -- cat /run/secrets/grafana-admin-password
```

Grafana sets it when it first starts, so regenerating the secret doesn't
change it; change it in Grafana instead. Grafana's **piqueld** folder holds the
dashboards.

After the next push to `main`, check that both deployed:

```console
piquelctl env show piqueld-dev production
piquelctl env show piqueld-observability production
```

Each `Sync` line says `following`, with the commit of that push. In GitHub,
each webhook's **Recent Deliveries** shows each push, with piqueld's response
`204`.

## Preview a branch

A [preview](../user/reference/manifest.md#previews) of `piqueld-dev`
serves a branch's site and book, so you can read a change before it reaches
`main`. Its routes are private, on the tailnet. With the slug piqueld gives
the preview, such as `piqueld-dev-feat-x-3fa2c1`, the site is on
`https://<slug>.preview.piqueld.dev` and the book on
`https://docs-<slug>.preview.piqueld.dev`.

To preview your worktree without pushing it, uncommitted changes included,
run this from the worktree:

```console
piquelctl --timeout 10m preview deploy piqueld-dev --local
```

It deploys to the preview of the current branch, creating it if needed. Add
`--slot <name>` to give each agent working on one branch a preview of its
own. To preview a pushed branch instead, run
`piquelctl preview create piqueld-dev --branch <branch>`, or select
**New preview** on the application's **Previews** tab in the dashboard.

Then list the preview's URLs:

```console
piquelctl preview show piqueld-dev <branch>
```

The preview's site still links to the book on `docs.piqueld.dev`, since the
site's links are absolute.

Delete the preview when you are done:

```console
piquelctl preview delete piqueld-dev <branch>
```

The [`piquelctl` reference](../user/reference/cli-commands.md#piquelctl-preview)
covers the other preview commands, such as pruning the previews of deleted
branches.

## Send applications' telemetry to it

Other applications on the instance can send their own telemetry to the
collector. Uncomment the `[[spec.exports]]` of
`deploy/piqueld-observability.toml`, with their names in place of `my-app`.
Then import the collector in each of them, and point its OpenTelemetry SDK at
it:

```toml
[[spec.imports]]
application = "piqueld-observability"
service = "collector"

[[spec.services]]
name = "web"
[spec.services.environment]
OTEL_EXPORTER_OTLP_ENDPOINT = "http://collector.piqueld-observability:4317"
OTEL_EXPORTER_OTLP_PROTOCOL = "grpc"
```

## Build the images locally

From the root of the repository:

```console
docker build --file deploy/docs/Dockerfile --tag piqueld-docs .
docker run --rm --publish 8080:8080 piqueld-docs
```

The book is then on <http://localhost:8080>. The marketing site builds the same
way from `deploy/site/Dockerfile`; to look at it without Docker, see
[`apps/site`](https://github.com/piquel-fr/piqueld/tree/main/apps/site). To build it in a
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
  holds `piqueld.dev`, `www.piqueld.dev`, `docs.piqueld.dev` or
  `grafana.piqueld.dev`. This instance already serves piqueld.dev: migrate
  that deployment instead.
- **A preview's URLs don't open.** Check that the device you open them from
  is on the tailnet, and that `*.preview.piqueld.dev` resolves to the apps
  node's tailnet addresses. The first preview's certificate,
  `*.preview.piqueld.dev`, can take up to 10 minutes to issue.
- **A push deploys nothing.** `piquelctl env show <application> production`
  says why on its `Sync` line, and GitHub's **Recent Deliveries** shows whether
  the delivery failed. A `401` means the secret doesn't match: generate a new
  one and copy it into GitHub again.
