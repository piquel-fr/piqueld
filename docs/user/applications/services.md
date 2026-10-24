# Configuring services

A service is one container image that piqueld runs as one or more replicas.
This page sets up the services of a small application, `notes`: a PostgreSQL
database, and an nginx web server that starts once the database is ready.

## Before you start

- You've [deployed your first application](../getting-started/first-app.md),
  so you know how to apply a manifest.
- Run the commands on this page on the host, or on any machine where
  `piquelctl` is [signed in](../getting-started/first-account.md#sign-piquelctl-in).
- Editing an application needs the `apps:write`
  [permission](../accounts/authorization.md) on it, and deploying it
  `apps:deploy`.

## Describe the services

1. Save this manifest as `notes.toml`:

   ```toml
   api_version = "piqueld.dev/v1alpha1"
   kind = "Application"

   [metadata]
   name = "notes"

   [[spec.services]]
   name = "db"

   [spec.services.source]
   type = "image"
   image = "postgres:17-alpine"

   [spec.services.environment]
   POSTGRES_USER = "notes"
   POSTGRES_DB = "notes"
   POSTGRES_PASSWORD_FILE = "/run/secrets/db-password"

   [[spec.services.secrets]]
   name = "db-password"
   target = "/run/secrets/db-password"

   [spec.services.healthcheck]
   type = "command"
   command = ["pg_isready", "-U", "notes"]
   interval_seconds = 5
   start_period_seconds = 30

   [[spec.services]]
   name = "web"
   replicas = 2
   depends_on = ["db"]

   [spec.services.source]
   type = "image"
   image = "nginx:alpine"

   [spec.services.healthcheck]
   type = "http"
   port = 80
   path = "/"

   [[spec.secrets]]
   name = "db-password"
   generate = { type = "random", bytes = 32 }
   ```

   - `db` runs PostgreSQL. Its password is a secret piqueld generates; see
     [secrets and variables](secrets-and-variables.md).
   - `web` runs two replicas of nginx, and starts once `db` is healthy.
   - Each health check tells piqueld when its service is ready.

2. Deploy it:

   ```console
   piquelctl app apply --file notes.toml --deploy
   ```

   It deploys `db` first, waits until it passes its health check, then deploys
   `web`.

The services of an application reach each other by name: `web` reaches the
database at `db:5432`. Services of other applications can't, unless you
[connect the applications](links.md).

The sections below explain each setting. You can also change each one with
`piquelctl app service`, or in the dashboard: open the application's
**Services** tab, then the service. Its page has a tab for each group of
settings. Once you change a field, select **Save changes** below it.

![The web service's page, on its Health check tab: an HTTP check of port 80 and path /, every 10 seconds, with a 3-second timeout, no start period and 3 retries](../../images/applications/service-health-check.png)

Saving changes the application's manifest, not what's running. To apply it,
run `piquelctl app deploy notes`, or select **Deploy to production** on the
application's page. An application with several environments deploys one at a
time: `piquelctl env deploy notes <environment>`.

## Replicas

`replicas` sets how many copies of the service run, from 1 to 100. It's 1 by
default.

```console
piquelctl app service replicas notes web 3
```

In the dashboard, it's **Replicas**, on the service's **Source & scaling** tab.

## Environment variables

```toml
[spec.services.environment]
POSTGRES_USER = "notes"
```

Every replica gets these variables.

```console
piquelctl app service env set notes web GREETING hello
piquelctl app service env remove notes web GREETING
```

In the dashboard, use the service's **Environment** tab. Don't put passwords
here: [mount them as secrets](secrets-and-variables.md) instead.

## Command and arguments

`command` replaces the image's entrypoint, and `arguments` its default
arguments. Each element is one argument, passed as written, without a shell.
To raise PostgreSQL's connection limit, for example, add to the existing `db`
entry:

```diff
 [[spec.services]]
 name = "db"
+arguments = ["-c", "max_connections=200"]
```

With `piquelctl`, give the elements after `--`, and give none to go back to the
image's own:

```console
piquelctl app service arguments notes db -- -c max_connections=200
piquelctl app service arguments notes db --
```

`piquelctl app service command` sets `command` the same way. In the dashboard,
use the **Command & arguments** tab, one element per row.

## Health checks

A health check tells piqueld when a replica is ready. Deployments wait for
it, and Docker restarts a replica that keeps failing it. A service without a
health check counts as ready as soon as it runs.

An HTTP check sends a `GET` request to a port of the container, which must
answer with a 2xx or 3xx status:

```toml
[spec.services.healthcheck]
type = "http"
port = 80
path = "/"
```

It needs nothing in the image, not even a shell, so it also works in minimal
images. `path` is `/health` if you leave it out.

A command check runs a program in the container, which must exit with 0:

```toml
[spec.services.healthcheck]
type = "command"
command = ["pg_isready", "-U", "notes"]
```

The command runs without a shell. To use one, write
`command = ["sh", "-c", "…"]`.

Both kinds take these settings:

| Setting | Default | Meaning |
| --- | --- | --- |
| `interval_seconds` | 10 | Seconds between checks. |
| `timeout_seconds` | 3 | Seconds before a check fails. At most the interval. |
| `retries` | 3 | Failed checks in a row before the replica is unhealthy. |
| `start_period_seconds` | 0 | Seconds after the replica starts during which failed checks don't count. |

Give slow starters, such as databases, a start period.

With `piquelctl`, `health http` and `health command` replace the check. They
take the settings above as `--interval`, `--check-timeout`, `--retries` and
`--start-period`:

```console
piquelctl app service health http notes web 80 --path /
piquelctl app service health command notes db --interval 5 --start-period 30 -- pg_isready -U notes
```

Other commands change one setting, or remove the check:

```console
piquelctl app service health start-period notes db 60
piquelctl app service health clear notes web
```

In the dashboard, use the **Health check** tab. Pick **None** under
**Check type** to remove the check.

## Startup dependencies

`depends_on` lists services of the same application that must be ready before
this one is deployed:

```toml
[[spec.services]]
name = "web"
depends_on = ["db"]
```

A deployment waits until every replica of `db` passes its health check before
it deploys `web`. A dependency without a health check counts as ready once its
replicas run. If `db` fails, `web` keeps running its previous version.

Dependencies only order deployments: once deployed, `web` keeps running even if
`db` stops.

```console
piquelctl app service depends-on notes web -- db
```

In the dashboard, check the services on the **Startup dependencies** tab.

## Rollout order

When a deployment changes a service, Docker replaces its replicas one at a
time, in one of two orders:

- **`start-first`** starts the new replica, then stops the old one. There is no
  downtime, but both run for a moment.
- **`stop-first`** stops the old replica, then starts the new one. They never
  overlap, but the service is briefly unavailable.

By default, a service that mounts a [volume](volumes-and-files.md) it can
write to stops first, so that two copies of a database never write the same
files. Others start first. To choose:

```toml
[spec.services.rollout]
order = "stop-first"
monitor_seconds = 30
```

`monitor_seconds`, 30 by default, is how long Docker watches each new replica.
If one fails within that time, Docker pauses the rollout: the replicas it
hasn't replaced yet keep running the previous version, and the deployment
fails.

```console
piquelctl app service rollout notes web --order stop-first --monitor-seconds 30
```

Without flags, the command restores the defaults. In the dashboard, use the
**Rollout** tab. `piquelctl app plan` lists each service's order:

```text
Rollout:
  db: stop-first (derived) · monitor 30s
  web: start-first (derived) · monitor 30s
```

## Resource limits

Limit how much CPU and memory each replica may use:

```toml
[spec.services.resources]
cpu_millis = 500          # half a CPU core
memory_bytes = 268435456  # 256 MiB
```

Without limits, a replica may use all of the host's. Memory is in bytes.

```console
piquelctl app service cpu notes web 500
piquelctl app service memory notes web 268435456
piquelctl app service cpu notes web --clear
```

In the dashboard, use the **Resource limits** tab, and leave a field empty for
no limit.

## Check that it works

```console
$ piquelctl env show notes
production of notes (app-01a16d95420677219cf7b65149b4f7e7)

Intent: ready
Runtime: ready (observed 6 s ago)
...
```

`Runtime: ready` means every service runs all its replicas, and each passes its
health check. In the dashboard, open the environment: **Observed services**
lists each service with its healthy replicas.

## Troubleshooting

- **The deployment fails with `service_update_failed`.** A new replica failed
  its health check, or exited, and Docker paused the rollout. Read the
  service's logs with `piquelctl app logs notes --service web`, fix the cause,
  and deploy again.
- **A service never becomes healthy.** Check that the health check's port and
  path match what the container serves. An HTTP check connects to `127.0.0.1`
  inside the container, so the service must listen there too, such as on
  `0.0.0.0`. It doesn't follow redirects.
- **The deployment fails with `convergence_timeout`.** A service didn't become
  ready in time. A deployment gives up when no service has become ready for
  120 seconds, start periods included. Give a slow service a longer
  `start_period_seconds` so it isn't restarted while it starts, and raise the
  [daemon setting](../reference/configuration.md)
  `reconciliation.convergence_timeout_seconds` if it needs more than that.
- **Validation fails with `service_dependency_cycle`.** Two services depend on
  each other, directly or through others. Remove one of the dependencies.

The [manifest reference](../reference/manifest.md) and the
[`piquelctl` reference](../reference/cli.md#field-editing) list every setting.
