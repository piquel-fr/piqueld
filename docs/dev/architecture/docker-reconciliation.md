# Docker reconciliation

The daemon controls one local Docker Engine running a single-node Swarm. It
manages private overlay networks, named volumes, Swarm secrets and configs, and
replicated services and jobs, verifies ownership before mutations, and retains
environments' volumes on deletion (deleting a preview removes its own). Service
updates run one task at a time and pause on failure. By default they are
start-first, except for services that mount any volume read-write: those stop the
old task before starting its replacement, so two tasks never write the same data
directory, at the cost of a short downtime per rollout. A service's `rollout` block
can set the order and the monitor window explicitly; observed services whose
order or monitor differs from the effective setting are updated with `rollout`
drift. Tasks are pinned
to the local manager's immutable node ID so local images and volumes cannot move
to a subsequently joined node. Preparation, job starts, and resource creation or
updates recheck the supported single-node topology; an unsupported topology blocks
that work and is retried with backoff after the operator restores it. Observation
and removals only need the local manager, so status, health, and deletion keep
working if another node joins. Existing unpinned services acquire the constraint
during their next reconciliation.

The deployable unit is an environment of an application. Every environment
deploys its application's saved manifest into its own network, services, and
volumes, named and labelled after the environment ID (the ownership label key
remains `io.piqueld.application`). Environments are reconciled independently;
an operation, its status, and its prepared target belong to one environment.

Apply validates and persists the entire normalized manifest. Save-only Apply
returns the saved configuration without an operation ID or scheduling work.
Apply with deployment and explicit Deploy prepare all sources, check a fresh
plan, then deploy the complete target. Previous resolved state remains available
while replacement preparation is pending or blocked; old services keep running,
and the controller continues correcting drift toward that active target. Once
preparation, a fresh concrete plan, and before-rollout jobs succeed, promotion
switches the maintained target before the remaining services roll out. Job
prerequisites may roll out before promotion. Promotion and active repair share the mutation
lock, so old-target repair cannot continue after promotion. There is no automatic
rollback after rollout starts.

Promoting a release into a
[promoted environment](../../user/reference/manifest.md#promoted-environments) (`env
promote`, not to be confused with the target promotion above) has no
preparation phase. The daemon checks the source and the release's images,
renders and compiles the release when it accepts the request, and saves the
prepared target with the deployment; the controller then reuses that target as
a retry would, running jobs and rolling out without building, fetching, or
resolving tags. An environment can't become promoted while a deployment is
requested or running (`application_busy`), and retrying an earlier deployment
built from its own source fails with `environment_promoted` instead of fetching,
building, or rolling out a target it prepared before, unless that target is
already the one the environment runs: repairing its drift rolls nothing new in.

Saving an identical normalized manifest does not schedule work. Apply with
deployment creates a new deployment even when the manifest is unchanged.
Explicit reconcile requests
a failed operation again under its existing ID. Explicit `reconcile`, periodic repair, and restart recovery use the
same execution path. They reuse the latest intent's prepared digests, or retry
preparation if it never completed. New deployments resolve all sources again.
Maintaining the active target during preparation does not replace the requested
candidate or report it as successfully deployed.

Before promotion, each deployment runs its `before-rollout` jobs as Swarm
replicated jobs pinned to the local node, without restarts, health checks, or
network aliases. A job's container is its service's, plus the environment
variables and secrets the job declares. A blocked plan runs no jobs; otherwise the private network and
volumes they need are ensured first. Before each job, the referenced service's
`depends_on` and transitive dependencies roll out to their prepared configuration
and converge using normal service health checks and convergence deadlines. The
controller replans after each action and resets the deadline after each service
converges. Jobs mount volumes read-only unless they declare exclusive access;
an exclusive job then removes every service that mounts one of its writable
volumes, and the adapter starts the run only once no other service mounts
those volumes and their containers have stopped. Conversely, the adapter
refuses to create or update a service while a job service of the application
mounts one of its volumes writable, or a container of a removed run still does.
Both checks run under the controller's mutation lock, so a service never shares
a volume with an exclusive run, even one left by a superseded operation. Waiting
for removed services and job runs to stop happens outside the lock, so other
environments' changes carry on meanwhile; the run then starts under the lock
only if the check still passes. Other services wait for all jobs to succeed, and the job's timeout
starts only after its dependencies are ready. The run's deadline is recorded
on its job service when it starts, so a resumed run keeps it. Jobs of a prerequisite service
must precede jobs that need it; manifest validation rejects the opposite order.
While the deployment may still roll out, active-target repair cannot revert or
remove prepared job prerequisites or restart services stopped for an exclusive
job, but continues maintaining other active services. Job services
and their containers are labelled with the operation that started them. Every
deployment not yet promoted, even one without jobs, first removes job services of earlier
operations and waits until their containers have stopped, and a new run is only
created after the replaced run's container has stopped, so runs never overlap. A
retried operation skips jobs it already ran successfully and resumes a run that
is still running or succeeded, which also covers a start whose response was
lost; a failed run is replaced. The controller polls each job until it exits or
its timeout passes, records its outcome and output as a build record, and only
then removes the job service. Cleanup intent is persisted independently of the
operation's outcome. If removal fails, periodic application scans retry stopping
the run, including after a daemon restart, without rerunning the job or changing
its recorded outcome. A later job cannot start until pending cleanup completes;
a failure to record a timeout or cancellation does not skip the stop attempt.
Terminal job failures and their cleanup requests are committed atomically, and
history retention keeps operations whose cleanup is still pending.
On shutdown, or when Docker could not report the
status before the timeout, the run is left for the retried operation to resume;
a superseded or cancelled operation stops its own run. A failed job fails the
operation as soon as its run finishes, before its output is read and its run
cleaned up, so a shutdown meanwhile never leaves the operation to be retried and
the job run again. Once its run has stopped, the deployment has ended, as it has after
a cancellation or a failure whose own plan an ownership or immutable
configuration conflict blocks, so repair restores the active target:
prerequisites roll back and stopped services start again. Once the conflict is
resolved, the blocked deployment rolls out again. A conflict does not end a
deployment one of whose exclusive jobs succeeded: the services it stopped would
run against data in the new format, so they stay stopped, and the prerequisites
stay, until the conflict is resolved and the deployment continues. Failures that are retried,
including conflicts planning can't see and failed service updates, keep the
prerequisites. Observation never
reports job services, so planning, repair, and health ignore them; deletion
removes leftovers before the network.

Explicit `deploy` captures the latest saved configuration and resolves its sources
again without advancing its generation. Each deployment supersedes pending work;
retries with the same idempotency key return the original operation. Deployment
is rejected while deletion is intended.

One async controller polls pending environment futures and discovery together.
SQLite calls, Docker observations, image pulls, and convergence timers yield to
other ready work. Shared limits allow two image resolutions and eight application
observations; resource mutations hold one global lock, and image builds run one at
a time, environments before previews. Timers consume no I/O
slot. New intent cancels obsolete local preparation; dispatched Docker requests
may finish, but obsolete results cannot authorize subsequent actions. Git and Docker
CLI commands run in private process groups; cancellation kills their local helper
processes too. Git ignores the host-wide `/etc/gitconfig` but reads the daemon
user's own configuration and credentials. Preparation timeouts fail with
`preparation_timeout` and are retried with the same backoff as other transient
failures.

New intent marks pending/running older operations superseded. CLI commands
waiting on an operation they started stop waiting successfully with that explicit
outcome rather than following the replacement; `env wait` and `preview wait` fail
with `deployment_superseded`.

Every action is followed by observation and fresh planning. No action cursor or
execution plan is stored. Plans place a service's create or update after the
convergence waits of its `depends_on` services, so dependents roll out only once
their dependencies are healthy; independent services still start together. On shutdown,
running attempts are cancelled and get 10 seconds to record their outcome. Interrupted attempts are recorded on startup, then
requested again. Each started attempt increments its operation's attempt number.

Transient failures retry indefinitely with exponential delays from 5 to 60
seconds. Preparation and convergence retain their configured deadlines. Success
resets the consecutive-failure backoff. Ownership/configuration conflicts and
paused rollouts remain observed without forced mutation. A fresh unblocked plan
can resume corrective work. Registry request rejections, Git fetch and build
failures, and secret storage failures require explicit retry or changed intent;
Docker, Swarm, ingress, journal, image resolution, convergence and preparation
timeout failures retry automatically, and their diagnostics are the ones marked
`retryable`.

Deletion remains running until a fresh observation confirms services and
networks are absent. Failed deletion attempts record diagnostics and follow the
same retry policy. Deletion removes the environment's Swarm secrets. Named volumes
remain available after deletion, except a preview's, which deletion removes and
waits for.

Operation state describes progress toward intent. Runtime health is recorded
separately, so a failed replacement can coexist with healthy existing services.
It is `ready` only when every service the environment expects is running and
converged: those of the deployment in progress, else of the active target. A missing
service makes it `degraded`, and no running service `absent`. A
[stopped](api.md#stopped-environments) environment is `stopped` instead of
`ready` once its services have scaled down. Status reports
when health was last observed (`health_observed_at_ms`); health scans refresh
it, so an old time means it is stale.
The intent generation and last resolved target generation are exposed separately;
a resolved generation alone does not assert container convergence.

Informational events record accepted changes, attempt starts/outcomes,
supersession, promotion, deletion completion, significant resource mutations,
active-target repairs, and meaningful health transitions. Operations expose current
phase/resource; failure events retain those fields and a structured error code. State
changes and their events share a transaction. Events never drive execution or
reconstruct state. Their independent retention defaults to 90 days. Application and daemon
scopes have separate policies. Action intent is journaled before runtime mutation;
retries and outcomes retain sanitized diagnostics. Unchanged successful
observations and raw Docker response bodies are not logged as events. See
[observability](../../user/guides/observability.md) for crash recovery and notification semantics.

Docker requests have deadlines, image resolution checks tag stability, and
service observation inspects complete specifications. Raw engine error sources
remain in daemon logs; durable diagnostics are sanitized. Focused fake-runtime
tests cover execution; `just docker-test` is the separate privileged Docker
qualification against an isolated Docker-in-Docker daemon.

Operation logs carry environment ID, operation ID, generation, and operation kind.
At `info`, the daemon reports operation start and completion with outcome and
duration. Enable `RUST_LOG=piqueld=debug` for preparation/convergence phases,
Docker actions, observations, and retry timing. Manifest values and runtime
configuration are not recorded as span fields. Persisted failures and API error
messages retain their sanitized codes and messages.

Docker requests use one shared 30-second policy. It covers observation queueing,
all inspection phases, and complete response bodies. Image resolution has a
separate ten-minute budget including its queue wait and pulls; the configured
preparation deadline still bounds the entire checkout/build/resolve phase.
Raw service update retries share one absolute request deadline. Cancelling a
request aborts its connection driver, and timeout errors retain their cause.

## HTTP health probe

HTTP health checks run `piqueld-probe`, a small static binary embedded in the
daemon, so images need no `wget`, shell, or C library:

```text
["CMD","/run/piqueld/probe","http","--port","8080","--path","/health","--timeout","3"]
```

The daemon publishes the probe as a Swarm config named by its content,
`piqueld-health-probe-<hash>`, the first time it deploys a service with an HTTP
check. Services mount it at `/run/piqueld/probe`, root-owned with mode `0555`,
so Swarm itself stores and distributes it, and Docker still runs the check, so
health still gates tasks and rollouts. A service whose check runs the probe
without mounting this daemon's config at `/run/piqueld/probe`, that mounts any
other config there, or that mounts the probe without its check running it,
registers as out-of-policy drift.

The probe's command line is a stable contract that newer probes keep
accepting. A daemon upgrade that changes the probe publishes a new config, and
services move to it through a normal health-gated rollout, as do services
whose HTTP checks earlier daemons pointed at `wget`. Image cleanup removes
probe configs of other versions that no service mounts any longer, in its
current spec or in the previous one a rollback would restore; the current
version's config is always kept.

Daemon builds compile the probe for the daemon architecture's
`*-unknown-linux-musl` target with the size-optimised `probe` profile, since
Swarm caps config size. `rust-toolchain.toml`, which rustup, `nix develop`, and
the Nix packages all use, installs only `x86_64-unknown-linux-musl`; building on
another architecture needs its musl target added.

## Service files

Each [file](../../user/reference/manifest.md#files) a service mounts is an immutable
Swarm config holding its content, named after the environment, the content's
SHA-256 digest, and the file's mode (`piqueld-<environment>-file-…`), since
Docker materializes a config once per container, and labelled
`io.piqueld.managed`, `io.piqueld.instance`, `io.piqueld.application`, and
`io.piqueld.file`. Publishing is idempotent: a config with the same name and
content is reused across deployments, while one holding other content, or
another installation's labels, is a configuration or ownership conflict.
Services mount configs by ID, root-owned with the file's mode, and Docker always
mounts configs read-only.

New content is a new config, so only the services mounting a changed file get
a new task specification and roll out. Observed config references that differ
from the desired ones in name, target, or mode, other than the probe at
`/run/piqueld/probe`, are drift, which repair restores; references that are not
root-owned or not mounted by ID are out-of-policy drift.

Once an operation of an environment ends, succeeded or failed, piqueld removes
the environment's file configs no service mounts any longer, in its current
spec or in the previous one a rollback would restore. It holds the lock repair
takes to change services, so no config is removed between being published and
mounted. A failed removal request is logged and retried after the next
operation, except that it fails an environment's deletion, which is retried.
Configs Docker refuses to remove because a container still uses them are left
for the next pass.

## Image retention

Git builds are labelled `io.piqueld.managed=true` and
`io.piqueld.instance=<instance ID>`, the installation's identity. Image cleanup
removes only images with both labels for its own installation, so development
instances and other installations sharing the Docker Engine never lose images,
and images built by hand or before these labels existed are left alone, as are
pulled registry images. An image ID covers its labels, so they can't change
under it; removal still rechecks them, then removes the image by ID, without
force and without its untagged parents. Cleanup never runs a broad prune.

An image is kept while a retention root uses it:

- what each environment and preview currently runs;
- each environment's and preview's latest deployment, in progress or retried
  with its prepared target until a newer one replaces it;
- each environment's last `images.keep_deployments` successful deployments
  (default 3), so their releases can be promoted again; previews keep none;
- what every environment that a
  [promoted environment](../../user/reference/manifest.md#promoted-environments)
  promotes from currently runs: the release `env promote` takes by default.

Images a container uses, even a stopped one, are kept too. Cleanup runs when the
daemon starts, after each operation finishes, and every
`images.cleanup_interval_seconds` (default 3600). Each removal is a journaled `remove_image` daemon action whose resource
is the image ID; a failed removal keeps the image and cleanup moves on.
`piquelctl status` and the dashboard report how many built images cleanup kept
and the total size of those it removed since the daemon started. Removing an
image frees only layers no other image shares, so less space may be reclaimed.

Preparations and cleanup share a lock. Each preparation holds it shared from
before it chooses, pulls, or builds images until its prepared target is saved,
when they are retention roots. Cleanup takes it exclusively while it reads the
roots and removes images, and skips its turn while any preparation holds it,
so it never removes an image a preparation is about to record, including one
Docker's build cache returned unchanged, and never delays a deployment for
more than one cleanup pass. Deployments that use a retained image again, such
as release promotion, check that it is still present first: a missing
registry image is pulled again by its digest, and a missing build, or a pull
that fails, fails with `image_unavailable` naming the service and image. A
release promotion holds the lock shared from that check until the promotion
and its prepared target are saved, so cleanup cannot remove the images in
between.
`app releases` shows whether each release's images are present.

## Link networks

A [link](../../user/reference/manifest.md#links) is an overlay network the importing
environment owns, named after it and the exporting environment
(`piqueld-<importer>-network-link-<exporter>-…`), labelled with the importer's
ownership labels plus `io.piqueld.link=<exporter environment ID>`. Every service
of the importer joins it without an alias; the exporter's imported services
join it with their link alias, `<service>.<application>`, and no other service
does, so a link never joins either environment's private network. The
importer's job containers join its link networks too, so jobs reach its
imports.

Links are resolved from both environments' active targets whenever either is
planned, like ingress, and are never part of a prepared target. The importer
creates the network in its own plan. The exporter observes the link networks
labelled for it, checks their owner and name, and attaches its services only
to those that exist; an attachment to any other network is drift. The
controller runs a full scan right away when the importer creates a link
network, when an operation changes the exports or imports an environment
deploys, and when the deletion of an environment with links is first
attempted, so the other side attaches or detaches within seconds rather than
at the next scan interval. The exporter thus joins while the importer's
services are still starting.

Revoking a link never waits for a rollout. Every full scan, each environment
detaches its services from the link networks its links no longer grant,
removing only that attachment from each service's current specification, even
while a failing service blocks its plan or a repair fails. What a deployment
that may still roll out grants counts too, so the services it starts for its
jobs keep their links. Removing a link, or deleting either environment, thus
detaches both sides. The importer
removes an obsolete link network once no container is attached to it any
longer (Docker refuses otherwise). Its deletion does not wait for that: it
leaves link networks still in use behind, and the exporter removes them on a
later full scan, once its services have left them, or when it is deleted
itself.

An exporting environment links to at most 64 importing environments. The
oldest keep their links; imports of newer ones stay unconnected until a slot
frees up.

## Stopped environments

A [stopped](api.md#stopped-environments) environment's services are planned
with no replicas. Like links, this is projected each time a target is planned,
from the environment's state as it is now, and is never part of a prepared
target, so its services keep every other setting and starting restores their
replicas without preparing anything. Stopping reruns a deployment that rolled
out, which then converges to zero replicas; a deployment in progress plans the
same and rolls out with none. Stopping or starting ends a convergence wait in
progress, and an operation whose plan predates either plans again before
succeeding. When the latest deployment never rolled out,
active-target repair applies every change that needs no convergence wait in
one pass, rather than one per scan, so the services of the target the
environment runs all scale down at once. Jobs are not projected: a deployment
started before the stop may still run its `before-rollout` jobs, whose startup
dependencies then have no replicas, and a job already running keeps the
dependencies it started with until it ends.

## Telemetry sockets

In an environment granted telemetry, the service that sets
[`receives_telemetry`](../../user/reference/manifest.md#telemetry-receivers) gets the
socket directory of its own, `<data_dir>/telemetry/<environment-id>/<service>/`,
as a writable bind mount at `/run/piqueld/telemetry`, and runs as the daemon's
`uid:gid` instead of its image's user, so the daemon can connect to the socket
its collector creates there. The runtime policy accepts that one bind mount and
user override, and the mount's source and the user are compared like any other
field: a bind mount or user that differs from them is `telemetry` drift.

Like links, the mount is projected each time the target is planned, from the
grant as it is now, and is never part of a prepared target. Granting or
revoking telemetry runs a full scan right away, so the controller adds or
removes the mount within seconds by updating the service, which restarts it,
as part of ordinary repair. The controller creates the directory, with mode
`0700`, before it mounts it. Export doesn't wait for any of this: a revoked
or deleted environment's receiver stops getting telemetry as the request is
accepted. On full scans, and as an environment's deletion ends, the controller
removes every directory no service mounts, except the receiving service's
while the environment is granted, observed under the lock that service
updates take. That covers revoked and deleted environments, and a renamed
receiver's old directory once its service is removed. At startup, it removes
those of environments that no longer exist.

## Ingress coordination

Runtime targets retain portable route intent regardless of global ingress enablement.
The controller projects ingress networks only when enabled. Exposed services join
their application's dedicated attachable overlay; other services stay private.
The standalone gateway joins overlays live and pins its stable egress network's
Docker gateway priority, avoiding gateway replacement during application additions.

Route additions/repoints happen after desired-resource convergence and before
obsolete backend cleanup. Existing destinations retain their network attachments
until Caddy accepts the replacement. Removals are journaled and withdrawn at rollout
promotion (or deletion execution), independently of replacement readiness. Failed
proxy configuration blocks cleanup and retries; it never silently declares success.
Saving alone does not alter public traffic. Swarm services still update in place;
retaining a route is not a blue-green deployment or an automatic application rollback.

Gateway startup/configuration drift repair runs independently from application jobs.
Piqueld shutdown leaves Caddy running with its persistent configuration and restart
policy. Disabling via TOML requires restart and explicitly removes the gateway, the
apps tailnet node, and `cloudflared` with its credentials.
Public HTTPS probes do not block deployment convergence and are shown separately.
