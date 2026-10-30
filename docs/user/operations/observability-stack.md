# Deploying an observability stack

Deploy an application that stores piqueld's telemetry and lets you explore it
in Grafana.

[`examples/observability/application.toml`](https://github.com/piquel-fr/piqueld/blob/main/examples/observability/application.toml)
is a small starting point, with no dashboards, that you can extend. It runs:

- an OpenTelemetry Collector, which
  [receives piqueld's telemetry](telemetry.md#receive-telemetry-in-an-application);
- Prometheus, Tempo and Loki, which store its metrics, traces and logs;
- Grafana, on a private route, with a data source for each.

The manifest holds every configuration file inline, so it deploys from the
file alone, without a repository. Edits you make to the application, such as
its hostname, stay until you apply the file again.

## Before you start

- Your instance serves
  [private routes](../guides/ingress.md#public-and-private-routes) on its
  tailnet, with a [DNS-01 provider](../guides/ingress.md#dns-01-certificates)
  for the zone of Grafana's hostname.
- You can create applications, change and deploy them, and run commands in
  them: `apps:create`, `apps:write`, `apps:deploy` and `apps:exec`. Reading
  the collector's logs needs `logs:read`. Granting the stack the daemon's
  telemetry also needs `telemetry:grant`, an installation-wide permission
  that no preset includes: `admin` holds it, or it can be granted on its own.
  See
  [permissions](../accounts/permissions.md#grants).
- `piquelctl` is signed in to your instance, on a machine with a checkout of
  piqueld's repository. Run the commands on this page from the checkout's
  root. Without a checkout, download `application.toml` and pass its path
  instead.

## Deploy it

1. Choose Grafana's hostname, such as `grafana.example.com`, and point it at
   the instance. Like any
   [private route](../guides/ingress.md#dns-for-private-routes), it needs `A`
   and `AAAA` records to the apps node's tailnet addresses, unless piqueld
   [manages the zone's records](../guides/ingress.md#managed-dns-records).

2. Save the manifest:

   ```console
   piquelctl app apply --file examples/observability/application.toml
   ```

   After you confirm, this creates the `observability` application, with its
   `collector`, `prometheus`, `tempo`, `loki` and `grafana` services. If you
   already have an application named `observability`, this replaces its
   configuration instead: to keep it, change `name` in a copy of the file,
   and use that name in the commands below.

3. Set your hostname in place of `grafana.example.com`, the manifest's
   placeholder. In the dashboard, change the **Default** of `grafana_hostname`
   on the application's **Variables** tab, and select **Save changes**. Or
   run:

   ```console
   piquelctl app variable set observability grafana_hostname grafana.example.com
   ```

4. Grant the application's `production` environment the daemon's telemetry:
   select **Grant telemetry** on its **Overview**, or run:

   ```console
   piquelctl env telemetry observability production on
   ```

   Grant it before deploying: without it, the collector can't start, and the
   deployment fails.

5. Deploy it: select **Deploy** on the environment's page, or run:

   ```console
   piquelctl app deploy observability
   ```

   `app deploy` asks you to confirm, then waits until the deployment finishes.
   The stack now receives the telemetry of every application.

6. Read Grafana's generated `admin` password. In the dashboard, open the
   environment's **Terminal** tab, choose the `grafana` **Service**, enter
   `cat /run/secrets/grafana-admin-password` as the **Command**, and select
   **Run**. Or run:

   ```console
   piquelctl app exec observability grafana -- cat /run/secrets/grafana-admin-password
   ```

7. From a device on your tailnet, open Grafana's address, which the
   environment's **URLs** card and `piquelctl env url observability` show.
   Sign in as `admin` with that password.

## Check that it works

The **Telemetry export** card on the dashboard's **Daemon status** page, and
`piquelctl status`, show the telemetry receiver connected. In Grafana, open
**Explore** and pick a data source:

- **Prometheus** has the daemon's metrics, with `job="piqueld"`, and the
  resource usage of each container.
- **Loki** has the daemon's logs and every container's output, except the
  collector's own: piqueld never sends a receiving service its own output.
  Query them by `service_name`, such as `{service_name="piqueld"}`. Read the
  collector's output on the `production` environment's **Logs** tab, with
  **Service** set to `collector`, or with
  `piquelctl app logs observability --service collector`.
- **Tempo** has the daemon's traces. Search them by service or by name.

## Send your applications' telemetry too

Other applications can send their own telemetry to the collector. Export it to
them, here `my-app`:

- in the dashboard, select **Add export** on the `observability`
  application's **Links** tab, choose the `collector` service, list `my-app`
  under **Applications**, save, and deploy;
- with `piquelctl`:

  ```console
  piquelctl app export set observability collector --applications my-app --deploy
  ```

- or add an `[[spec.exports]]` entry to your copy of the manifest, and apply
  it with `--deploy`; see [links](../reference/manifest.md#links). Applying
  the file replaces the saved manifest, so first set `grafana_hostname` in it
  to your hostname.

Then, in each of them, import the collector, and point its OpenTelemetry SDK
at it with `OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.observability:4317`
and `OTEL_EXPORTER_OTLP_PROTOCOL=grpc`. In the dashboard, add the import on
the application's **Links** tab, and the variables on the service's
**Environment** tab. With `piquelctl`, use
[`app import add`](../reference/cli-commands.md#piquelctl-app-import-add) and
[`app service env set`](../reference/cli-commands.md#piquelctl-app-service-env-set).
In the manifest:

```toml
[[spec.imports]]
application = "observability"
service = "collector"

[[spec.services]]
name = "web"

[spec.services.environment]
OTEL_EXPORTER_OTLP_ENDPOINT = "http://collector.observability:4317"
OTEL_EXPORTER_OTLP_PROTOCOL = "grpc"
```

Then deploy each of them. Until then, they can't reach the collector.

- In the dashboard, return to the application through the **my-app**
  breadcrumb, then select **Deploy**.
- With `piquelctl`, add `--deploy` to the last command, or run
  `piquelctl app deploy my-app`.
- With the manifest, apply your file with
  `piquelctl app apply --file my-app.toml --deploy`.

An import reaches the environment named like the importing one, so from an
environment other than `production`, choose `production` as the import's
**Environment**, pass `--environment production`, or add
`environment = "production"` to the `[[spec.imports]]` entry. The collector
also accepts OTLP over HTTP on port 4318.

An importing application can send any telemetry, including records that look
like the daemon's, so export the collector only to applications you trust. A
name that no application holds yet goes to whichever application
[takes it first](../reference/manifest.md#links).

## Find your data in Grafana

Prometheus labels each container's metrics with its application,
environment, preview, workload and container, such as `piqueld_application`,
and with the service as `job`. Loki indexes logs by `service_name`, and keeps
the other attributes, such as `piqueld_application` or a record's trace ID, as
structured metadata. See [what piqueld sends](telemetry.md#what-piqueld-sends).

Prometheus keeps metrics for 15 days and Tempo traces for 14, their images'
defaults. Loki keeps logs until you delete its volume. Grafana keeps nothing
between restarts, so the password stays the generated one. Add a volume at
`/var/lib/grafana` to keep the dashboards you create.

## Troubleshooting

- **The deployment fails because the collector can't start.** The environment
  isn't granted telemetry yet. Select **Grant telemetry** on its **Overview**,
  or run `piquelctl env telemetry observability production on`, then deploy
  again.
- **Grafana's address doesn't open.** Check its DNS records, and that you're on
  the tailnet.
