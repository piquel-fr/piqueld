# Deploying an observability stack

Deploy an application that stores piqueld's telemetry and shows it in Grafana,
with dashboards for the daemon, deployments, services and logs.

The [example application](https://github.com/piquel-fr/piqueld/tree/main/examples/observability)
runs:

- an OpenTelemetry Collector, which [receives piqueld's
  telemetry](telemetry.md#receive-telemetry-in-an-application);
- Prometheus, keeping metrics for 15 days;
- Tempo and Loki, keeping traces and logs for 7 days;
- Grafana, whose data sources link logs, traces and metrics to each other.

## Before you start

- You're an administrator: granting telemetry needs the `telemetry:grant`
  permission.
- Grafana is served on a [private route](../reference/manifest.md#routes), so
  managed ingress and its tailnet node must be set up, with a DNS record for
  Grafana's hostname.
- A Git repository of your own for the stack. Its configuration files are
  mounted from the repository, so the application
  [deploys from it](../reference/manifest.md#repository-backed-manifests).

## Deploy it

1. Fork piqueld, or copy `examples/observability` into your repository.
2. Edit `examples/observability/application.toml`:
   - `grafana_hostname`, under `[spec.variables]`: Grafana's hostname;
   - `[spec.manifest]`: your repository, and the branch to follow or the
     `commit` to pin;
   - `[[spec.exports]]`: replace the placeholder `my-app` with the
     applications that may send their own telemetry to the collector, or
     remove it.
   If you copied the directory elsewhere, also fix each `path`.
3. Create the application, grant it telemetry, and deploy it:

   ```console
   piquelctl app apply --file examples/observability/application.toml
   piquelctl env telemetry observability production on
   piquelctl app deploy observability --yes
   ```

   To deploy it again whenever you push to its branch, run
   `piquelctl env sync observability production on`.

4. Read Grafana's generated `admin` password:

   ```console
   piquelctl app exec observability grafana -- cat /run/secrets/grafana-admin-password
   ```

5. Open the address `piquelctl env url observability` prints, from a device on
   your tailnet, and sign in as `admin`.

Grafana sets this password the first time it starts. To change it later,
change it in Grafana: regenerating the secret has no effect.

## Check that it works

`piquelctl status` shows the receiving collector as connected. In Grafana, the
piqueld dashboards fill in within a minute.

## Send your applications' telemetry too

Applications listed in the stack's `[[spec.exports]]` can send their own
telemetry to the collector. In each of them, import the collector and point
its OpenTelemetry SDK at it:

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

The collector also accepts OTLP over HTTP on port 4318.

> **Warning:** an importing application can send any telemetry, including
> records that look like the daemon's. Export the collector only to
> applications you trust. An export naming an application that doesn't exist
> yet goes to whichever application [takes that name
> first](../reference/manifest.md#links).

## Find your data in Grafana

Container metrics are labelled with the application, environment, preview,
workload and container, such as `piqueld_application`, and with the service as
`job`. Loki indexes logs by `service_name` and the same `piqueld_*` labels;
everything else, such as a record's trace ID, is structured metadata. See
[what piqueld sends](telemetry.md#what-piqueld-sends).

## Troubleshooting

- **The collector keeps restarting.** The environment isn't granted telemetry
  yet. Run `piquelctl env telemetry observability production on`.
- **Grafana's address doesn't open.** Check its DNS record, and that you're on
  the tailnet.
