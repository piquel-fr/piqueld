# Docker reconciliation

Plan 06A connects the daemon directly to Docker Engine through Bollard. Startup
requires an active single-node Swarm manager; configuration may opt into
initializing an inactive local engine. The supported workload is a prebuilt image
resolved to a digest before persistence.

The adapter manages private overlay networks, local named volumes, and replicated
services. It rechecks deterministic names and ownership labels before every
mutation. Foreign resources block a plan. Service updates use conservative
start-first, one-at-a-time rolling settings and pause on failure. Existing
runtime settings outside the supported model are not silently adopted.

Application deletion removes services and the private network, waits for
convergence, and retains named volumes. Raw Docker messages and task text are
kept in internal error sources for logs only; durable operation and status
diagnostics contain stable codes and sanitized messages.

The coordinator wakes after API mutations and performs authoritative periodic
polling scans. No Docker event listener or event-stream API is required. Durable
operation steps resume after interruption, while each step re-observes and
re-plans before executing.

The Docker boundary remains a real test seam. Focused fake-Docker tests exercise
the scheduler and handler without an Engine. The privileged lifecycle test is
ignored by ordinary runs; `just docker-test` starts an isolated privileged
Docker-in-Docker daemon, runs it against a private Unix socket, and cleans up the
temporary daemon and resources afterward.
