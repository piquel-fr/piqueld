# Docker reconciliation

The daemon controls one local Docker Engine running a single-node Swarm. It
manages private overlay networks, named volumes, and replicated services,
verifies ownership before mutations, and retains volumes on deletion. Service
updates are start-first, one task at a time, and pause on failure.

Apply validates and persists the entire normalized manifest before returning an
operation ID. Execution resolves all images to immutable digests, checks a fresh
plan, then deploys the complete target. Previous resolved state remains available
while replacement preparation is pending or blocked; old services keep running,
but corrective mutations toward obsolete intent stop.

An identical normalized manifest reuses the current operation without pulling
images or scheduling work. A failed operation is requested again under its
existing ID. Explicit `reconcile`, periodic repair, and restart recovery use the
same execution path. They reuse the latest intent's prepared digests, or retry
preparation if it never completed. They never fall back to an older intent.

Explicit `refresh` resolves the current manifest again without advancing its
generation. Active refreshes are reused; failed refreshes retry their prepared
target when available. A refresh after success starts a new operation. Refresh
supersedes apply/reconcile and is rejected while deletion is intended.

One async controller polls pending application futures and discovery together.
SQLite calls, Docker observations, image pulls, and convergence timers yield to
other ready work. Shared limits allow two image resolutions, eight application
observations, and one resource mutation request globally. Timers consume no I/O
slot. New intent cancels obsolete local preparation; dispatched Docker requests
may finish, but obsolete results cannot authorize subsequent actions.

Every action is followed by observation and fresh planning. No action cursor or
execution plan is stored. Interrupted attempts are recorded on startup, then
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
supersession, deletion completion, and meaningful health transitions. State
changes and their events share a transaction. Events never drive execution or
reconstruct state. Their independent retention defaults to 30 days; zero disables
pruning. Unchanged observations and raw Docker errors are not logged as events.

Docker requests have deadlines, image resolution checks tag stability, and
service observation inspects complete specifications. Raw engine error sources
remain in daemon logs; durable diagnostics are sanitized. Focused fake-runtime
tests cover execution; `just docker-test` is the separate privileged Docker
qualification against an isolated Docker-in-Docker daemon.
