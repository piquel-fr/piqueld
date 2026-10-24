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

- The `telemetry:grant` permission, to grant the stack telemetry. Accounts
  with `admin` on every application have it.
- Grafana is served on a private route, so piqueld needs
  [managed ingress](../reference/configuration.md#managed-application-ingress)
  with its tailnet node, and a [DNS provider](../reference/configuration.md#dns-providers)
  that manages Grafana's domain, for its certificate.
- A Git repository of your own for the stack. Its configuration files are
  mounted from the repository, so the application
  [deploys from it](../reference/manifest.md#repository-backed-manifests). If
  it's private, the daemon's user needs credentials to clone it.
- A checkout of that repository on the machine where you run `piquelctl`.

## Deploy it

1. Fork piqueld, or copy `examples/observability` into your repository.

2. Edit `examples/observability/application.toml`:
   - `grafana_hostname`, under `[spec.variables]`: Grafana's hostname.
   - `[spec.manifest.repository]`: your repository's URL, and the `branch` to
     follow or the `commit` to pin.
   - `[[spec.exports]]`: replace the placeholder `my-app` with the
     applications that may send their own telemetry to the collector, or
     remove it. An importing application can send any telemetry, including
     records that look like the daemon's, so list only applications you trust.
     A name that no application holds yet goes to whichever application
     [takes it first](../reference/manifest.md#links).
   - each `path`, if you copied the directory elsewhere.

3. Commit and push your changes to that branch. The application deploys what
   the repository holds, not your local file.

4. Create the application:

   ```console
   piquelctl app apply --file examples/observability/application.toml
   ```

5. Grant it telemetry:

   ```console
   piquelctl env telemetry observability production on
   ```

6. Deploy it:

   ```console
   piquelctl app deploy observability --yes
   ```

   To deploy it again whenever you push to its branch, run
   `piquelctl env sync observability production on`. An environment pinned to
   a `commit` never does.

7. Read Grafana's generated `admin` password:

   ```console
   piquelctl app exec observability grafana -- cat /run/secrets/grafana-admin-password
   ```

8. Open the address `piquelctl env url observability` prints, from a device on
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

An import reaches the environment named like the importing one, so from an
environment other than `production`, add `environment = "production"` to the
`[[spec.imports]]` entry. The collector also accepts OTLP over HTTP on port
4318.

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
