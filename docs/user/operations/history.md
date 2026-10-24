# History and diagnostics

Find out what piqueld did, when, and why something failed.

## See what happened

piqueld records every deployment step, configuration change, secret change
and failure as an event. Secret values, environment values and webhook URLs
are never part of them.

- In the dashboard, the **Events** page lists every event you can read, and
  each environment's **Events** tab lists that environment's.
- With `piquelctl`, give an application's ID, which `piquelctl app show`
  prints:

  ```console
  $ piquelctl events --application app-01a16b1282cf755287920502b5793cd1 --limit 3
  11  1792797737680  environment_created  -  attempt -     created environment production  by host operator (uid 1000)
  12  1792797737682  refresh_requested  operation-01a16b1282d27232a446f3dbc4a19808  attempt 0       by host operator (uid 1000)
  13  1792797737713  operation_started  operation-01a16b1282d27232a446f3dbc4a19808  attempt 1       by host operator (uid 1000)
  Next cursor: v1:13
  ```

  Events come oldest first, 50 at a time. Pass the printed cursor with
  `--cursor` to read the next page.

Each line has the event's number, its time in Unix milliseconds, its kind, the
operation it belongs to, and who caused it. To wait for one operation to
finish, or see where it is, run `piquelctl operation <operation-id>`. In the
dashboard, the environment's **Deployments** tab expands each deployment into
its steps.

## Find out why something failed

The **Errors** page lists failures, newest first. Filter them by period,
scope, event kind or error code.

![The Errors page, listing an operation and an action that failed with image_resolution_rejected for broken/production, and two dependency health changes](../../images/operations/errors.png)

Each failure has a diagnostic. Select **Diagnostic details** to read it:

- a **summary** of what went wrong, and a stable **code**, such as
  `image_resolution_rejected`;
- its **causes**, such as the Docker request that failed and its HTTP status;
- the **next step**, and whether retrying can help.

The **Errors** page links each failure to its environment and to the events of
its deployment. An API error that has a diagnostic names it in
`details.diagnostic_id`.

## Read logs and build output

- A service's output: `piquelctl app logs <app>`, or the environment's
  **Logs** tab. These are recent lines read from Docker; for a lasting record,
  [export them](telemetry.md).
- A build's output: `piquelctl builds`, or the **Builds** page and each
  application's **Builds** tab.

## Watch the daemon itself

The dashboard's **Daemon status** page shows:

- whether the database, the Docker engine, the Swarm manager and HTTPS ingress
  are ready, and how previews, built images and telemetry export stand;
- the daemon's uptime, memory, CPU, database size and free disk space;
- how many events and diagnostics are kept, and how much build output;
- running and queued operations, and pending and failed notifications.

These are current values. For their history, [export telemetry](telemetry.md).

The **Analytics** page sums up deployments over the last 24 hours, 7, 30 or 365
days: how many succeeded or failed, how long attempts took, how often they were
retried, how long each kind of step takes, and the most common failure codes.

## How long history is kept

| Setting | Default | What it keeps |
| --- | --- | --- |
| `retention.event_days` | `90` | Application and environment events |
| `retention.daemon_event_days` | `90` | Daemon events |
| `retention.audit_days` | `365` | The [audit trail](audit.md) |
| `retention.finished_operation_days` | `10` | Finished operations other than deployments |

`0` keeps everything. Deployment history is kept until its environment is
deleted, and build output has its own size and age limits; see
[the configuration reference](../reference/configuration.md).

Deleting an environment keeps its events in the application's history.
Deleting an application deletes its history.

## Troubleshooting

- **An event you expected is missing.** You only see the applications you have
  the `events:read` permission on, and daemon events need `system:read`; see
  [permissions](../accounts/authorization.md). It may also be older than the
  retention period.
- **`piquelctl events` rejects the application.** It takes the application's
  ID, not its name.
