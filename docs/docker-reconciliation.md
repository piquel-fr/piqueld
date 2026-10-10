# Docker reconciliation

The daemon controls one local Docker Engine running a single-node Swarm. It
manages private overlay networks, named volumes, and replicated services,
verifies ownership before mutations, and retains volumes on deletion. Service
updates run one task at a time and pause on failure. By default they are
start-first, except for services that mount any volume read-write: those stop the
old task before starting its replacement, so two tasks never write the same data
directory, at the cost of a short downtime per rollout. A service's `rollout` block
can set the order and the monitor window explicitly; observed services whose
order or monitor differs from the effective setting are updated with `rollout`
drift. Tasks are pinned
to the local manager's immutable node ID so local images and volumes cannot move
to a subsequently joined node. Preparation, promotion, and resource creation or
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
network aliases. A blocked plan runs no jobs; otherwise the private network and
volumes they need are ensured first. Before each job, the referenced service's
`depends_on` and transitive dependencies roll out to their prepared configuration
and converge using normal service health checks and convergence deadlines. The
controller replans after each action and resets the deadline after each service
converges. Other services wait for all jobs to succeed, and the job's timeout
starts only after its dependencies are ready. Jobs of a prerequisite service
must precede jobs that need it; manifest validation rejects the opposite order.
Active-target repair cannot revert or remove prepared job prerequisites, but
continues maintaining other active services. Job services
and their containers are labelled with the operation that started them. Every
deployment, even one without jobs, first removes job services of earlier
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
operation; prerequisite changes remain, and other active services keep being
maintained. Observation never
reports job services, so planning, repair, and health ignore them; deletion
removes leftovers before the network.

Explicit `deploy` captures the latest saved configuration and resolves its sources
again without advancing its generation. Each deployment supersedes pending work;
retries with the same idempotency key return the original operation. Deployment
is rejected while deletion is intended.

One async controller polls pending environment futures and discovery together.
SQLite calls, Docker observations, image pulls, and convergence timers yield to
other ready work. Shared limits allow two image resolutions, eight application
observations, and one resource mutation request globally. Timers consume no I/O
slot. New intent cancels obsolete local preparation; dispatched Docker requests
may finish, but obsolete results cannot authorize subsequent actions. Git and Docker
CLI commands run in private process groups; cancellation kills their local helper
processes too. Preparation timeouts fail with `preparation_timeout` and are retried
with the same backoff as other transient failures.

New intent marks pending/running older operations superseded; the CLI stops waiting
successfully with that explicit outcome rather than following the replacement.

Every action is followed by observation and fresh planning. No action cursor or
execution plan is stored. Plans place a service's create or update after the
convergence waits of its `depends_on` services, so dependents roll out only once
their dependencies are healthy; independent services still start together. Interrupted attempts are recorded on startup, then
requested again. Each started attempt increments its operation's attempt number.

Transient failures retry indefinitely with exponential delays from 5 to 60
seconds. Preparation and convergence retain their configured deadlines. Success
resets the consecutive-failure backoff. Ownership/configuration conflicts and
paused rollouts remain observed without forced mutation. A fresh unblocked plan
can resume corrective work. Registry request rejections require explicit retry
or changed intent; temporary availability failures retry automatically.

Deletion remains running until a fresh observation confirms services and
networks are absent. Failed deletion attempts record diagnostics and follow the
same retry policy. Named volumes remain available after deletion.

Operation state describes progress toward intent. Runtime health is recorded
separately, so a failed replacement can coexist with healthy existing services.
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
[observability](observability.md) for crash recovery and notification semantics.

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
  (default 3), for restores; previews keep none;
- the current release of every environment a promoted environment promotes
  from, once promotion exists.

Images a container uses, even a stopped one, are kept too. Cleanup runs after
each operation finishes and every `images.cleanup_interval_seconds` (default
3600). Each removal is a journaled `remove_image` daemon action whose resource
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
more than its removals. Deployments that use a retained image again, such as
restores and promotion, check that it is still present first: a missing
registry image is pulled again by its digest, and a missing build, or a pull
that fails, fails with `image_unavailable` naming the service and image.
`app releases` shows whether each release's images are present.

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
policy. Disabling via TOML requires restart and explicitly removes the gateway.
Public HTTPS probes do not block deployment convergence and are shown separately.
