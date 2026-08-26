# Docker reconciliation

The daemon connects directly to Docker Engine through Bollard. Startup
requires an active single-node Swarm manager; configuration may opt into
initializing an inactive local engine. The supported workload is a prebuilt image
resolved to a digest before persistence.

The adapter manages private overlay networks, local named volumes, and replicated
services. It rechecks deterministic names and ownership labels before every
mutation. Foreign resources block a plan. Service updates use conservative
start-first, one-at-a-time rolling settings and pause on failure. The runtime
policy verifies exactly the fields piqueld authors — replication, update
settings, the restart condition and delay, mounts, environment, network targets,
health checks, and resource limits. Fields the specification builder never sets
are ignored, so engine-defaulted echo-back (which varies between daemon
versions) no longer factors into drift.

Every Docker interaction is bounded by a request-timeout deadline at the adapter
boundary; Bollard only bounds a request up to the response headers, so the
adapter applies its own deadline and reports elapsed deadlines as engine
unavailability. The hand-rolled service wire path shares that classification:
connect, handshake, and request deadlines all surface as unavailability with
distinguishing context.

Image resolution verifies tag stability across the pull. The repository digests
recorded for the tag are captured before and after the pull and must still
overlap; a concurrently re-pointed tag restarts the resolution a bounded number
of times before failing with the sanitized image-resolution error.

Docker's compact service-list response is never used as the final semantic source:
the adapter performs a complete service inspection before ordinary observation or
deciding whether an update is needed. Observations inspect the listed services
concurrently and tolerate services deleted mid-observation; the task list is
skipped entirely when no services remain. Service create, update, and inspection
pass through a narrow wire adapter that normalizes Docker's `Healthcheck`
spelling to the typed `HealthCheck` model. An exact transient
`update out of sequence` response gets a bounded retry with a refreshed service
version; other errors fail without retry.

Application deletion removes services and the private network, waits for
convergence, and retains named volumes. Raw Docker messages and task text are
kept in internal error sources for logs only; durable operation and status
diagnostics contain stable codes and sanitized messages.

The coordinator wakes after API mutations and performs authoritative periodic
polling scans. No Docker event listener or event-stream API is required. Durable
operation steps resume after interruption, while each step re-observes and
re-plans before executing.

The Docker boundary remains a real test seam. Focused fake-Docker tests exercise
the scheduler and handler without an Engine, including foreign-resource
refusals for services, networks, and volumes and the image tag-stability retry.
The privileged lifecycle test is ignored by ordinary runs; `just docker-test`
starts an isolated privileged Docker-in-Docker daemon, runs it against a private
Unix socket, and cleans up the temporary daemon and resources afterward.
