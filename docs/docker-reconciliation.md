# Docker reconciliation

The daemon connects to a local Docker Engine through Bollard. It requires a
single-node Swarm manager and can initialize an inactive Swarm when configured.
Images are resolved to immutable digests before desired state is accepted.

The Docker adapter owns resource names, ownership checks, wire specifications,
image resolution, and runtime observation. It manages private overlay networks,
local named volumes, and replicated services. It verifies ownership before
mutating a resource. Service updates use start-first rolling updates, one task at
a time, and pause on failure.

One controller scans applications sequentially, waking after accepted mutations
and at the configured interval. For each active operation it observes Docker,
builds a fresh plan, executes the next action, and repeats. A stored action
cursor is unnecessary: Docker state determines what remains to do. Interrupted
work resumes through the same observation and planning loop after restart.
The latest operation ID determines whether work is still current; superseded
work stops before continuing with further actions.

Periodic scans also detect drift after a successful apply. Drift reconciliation
uses the stored immutable image references. It does not refresh mutable tags;
applying the manifest again does that.

Apply operations succeed when the desired resources converge and fail when
execution cannot complete within its limits. Applying the same resolved target
after failure or cancellation requests another attempt under the same operation
ID. A newer target receives a new operation and cancels earlier active work.

Deletion removes services and networks and retains named volumes. A deletion
operation remains `running` until a fresh observation verifies resource absence.
An error or convergence timeout is recorded on the running operation, and the
next scan tries again. A successful Docker removal response alone does not
complete deletion.

Docker requests and convergence attempts have deadlines. Image resolution checks
that the pulled tag still identifies a stable repository digest. Service
observations inspect full specifications, and service updates retry an exact
transient version conflict with a refreshed Docker version. Raw Docker errors
remain available to daemon logs; public diagnostics contain safe messages.

The Docker trait supports fake-runtime tests. `just docker-test` runs the
optional privileged lifecycle check against an isolated Docker-in-Docker daemon.
Running that command is separate from ordinary compile and test checks.
