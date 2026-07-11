# piqueld — First Prototype Design Specification
# 1. Purpose
`piqueld` is a declarative application deployment and infrastructure-management system written in Rust.
The first prototype will manage applications on a **single NixOS machine running Docker in single-node Swarm mode**.
Although the initial deployment contains only one machine, the runtime model should remain compatible with a future multi-node Swarm cluster. The first prototype must not implement multi-node management, node agents, distributed storage, control-plane replication, or high-availability management.
The purpose of this prototype is to validate the core product model:
1. Applications are described declaratively.
2. The desired configuration is stored in libSQL.
3. `piqueld` converts that configuration into Docker Swarm resources.
4. `piqueld` continuously reconciles Swarm with the desired state.
5. Applications can be managed through the API, CLI, UI, and external text manifests.
6. Locally built images are stored in a local container registry.
7. Public traffic reaches applications through Cloudflare Tunnel and Traefik.
# 2. Vision
`piqueld` should be a small, transparent, reproducible application control plane for self-hosted infrastructure.
It should provide the convenience of platforms such as Dokploy while placing more emphasis on:
- Declarative configuration.
- Deterministic resource generation.
- Explicit plans.
- Inspectable state.
- Portable configuration.
- Conservative destructive behaviour.
- A small operational footprint.
- A Rust-only application codebase.

The long-term product direction may include multi-node clusters, dedicated builders, specialized runner nodes, and restricted node agents. Those concerns must influence boundaries in the architecture, but they must not become prototype features.
## 2.1 Product statement
> `piqueld` is a declarative control plane that turns application specifications into Docker Swarm services and continuously keeps the runtime aligned with those specifications.
## 2.2 Architectural boundary
`piqueld` owns:
- Application intent.
- Manifest validation.
- Image builds.
- Image resolution.
- Swarm resource generation.
- Deployment planning.
- Reconciliation.
- Routing configuration.
- Secret management.
- User-facing status.
- State import and export.

Docker Swarm owns:
- Container execution.
- Service task lifecycle.
- Replica maintenance.
- Service discovery.
- Overlay networking.
- Rolling task updates.
- Runtime secret and configuration distribution.

Traefik owns:
- HTTP routing.
- Service discovery from Swarm.
- Load balancing between service replicas.

Cloudflare Tunnel owns:
- Public ingress into the private infrastructure.
- The outbound connection to Cloudflare’s edge.
# 3. Core principles
## 3.1 Declarative
Users describe what should exist, not the sequence of commands required to create it.
For example:
```toml
[[spec.services]]
name = "web"
replicas = 2
image = "ghcr.io/example/web:1.4.0"
```

The user does not instruct `piqueld` to:
1. Pull an image.
2. Create a network.
3. Create two containers.
4. Attach them to Traefik.
5. Restart them after failure.
Those actions are derived from the desired specification.
## 3.2 Convergent
Repeated reconciliation of an unchanged desired configuration should eventually produce no additional actions.
Conceptually:
```text
reconcile(desired, matching_observed_state) = no changes
```
Runtime drift must be detected and corrected for resources owned by `piqueld`.
## 3.3 Deterministic
The same normalized application specification should produce the same intended Swarm resource definitions.
Determinism applies to:
- Service names.
- Network names.
- Volume names.
- Traefik labels.
- Registry image names.
- Resource labels.
- Swarm service specifications.

It does not imply identical:
- Container IDs.
- Task IDs.
- Creation timestamps.
- Runtime IP addresses.
- Logs.
- External service responses.
## 3.4 Reproducible deployments
Mutable source references must be resolved before deployment.
Examples:
```text
Image tag:
ghcr.io/example/web:latest
    → ghcr.io/example/web@sha256:...

Git branch:
main
    → exact Git commit
```
The resolved image digest or source commit should be stored with the current application state.
The first prototype will not expose revision history or rollback, but the current deployed state must still record resolved inputs.
## 3.5 Transparent
Users should be able to inspect:
- The desired application configuration.
- The resolved image digest.
- The currently observed Swarm service.
- The actions in the current deployment.
- Validation errors.
- Build output.
- Runtime logs.
- Drift between desired and observed state.
## 3.6 Conservative
`piqueld` must not delete data implicitly.
In particular:
- Removing an application must not delete its volumes by default.
- Replacing imported state must require explicit confirmation.
- Unknown Docker resources must not be modified.
- Only resources carrying valid `piqueld` ownership labels may be changed or deleted.
# 4. First prototype scope
The first prototype will support:
- One Linux host.
- NixOS as the primary supported operating system.
- One Docker Engine.
- Docker Swarm initialized in single-node mode.
- One active `piqueld` daemon.
- One libSQL database in local embedded mode.
- Applications containing one or more services.
- Replicated Swarm services.
- Prebuilt container images.
- Git repositories built from Dockerfiles.
- A local OCI registry for build outputs.
- Environment variables.
- Managed secrets mounted as files.
- Named Docker volumes.
- Private application overlay networks.
- HTTP routes through Traefik.
- Public ingress through an externally configured Cloudflare Tunnel.
- Application create, read, update, delete, plan, and deploy operations.
- External TOML application manifests.
- Application export.
- Control-plane state export and import.
- Live deployment status.
- Build logs.
- Container logs.
- A CLI.
- A minimal web UI.
- A NixOS module.
## 4.1 Explicitly out of scope
The first prototype will not implement:
- Multi-machine orchestration.
- Multiple Swarm managers.
- Node placement interfaces.
- Node capability labels in the public application schema.
- `piqueld` agents.
- Leader election.
- High-availability `piqueld`.
- libSQL replication.
- Configuration revision history.
- User-facing rollback.
- GitOps reconciliation.
- Distributed volume replication.
- Stateful-service classification.
- Shared-storage management.
- Volume backups.
- Registry backup or replication.
- Multi-user accounts.
- Role-based access control.
- Kubernetes support.
- Docker Compose compatibility.
- Alternative proxy backends.
- Long-term metrics storage.
- Plugin systems.
- Arbitrary host command execution.
- Arbitrary Docker API passthrough.
These exclusions are deliberate. The prototype should prove the control model before expanding the feature surface.
# 5. High-level architecture
```text
                         MANAGEMENT ACCESS

                     Tailscale private network
                               │
                               ▼
                    Tailscale Serve or HTTPS
                               │
                  ┌────────────▼────────────┐
                  │        piqueld          │
                  │                         │
                  │ API                     │
                  │ Application service     │
                  │ Reconciliation engine   │
                  │ Build controller        │
                  │ libSQL state            │
                  └────────────┬────────────┘
                               │ Docker API
                               ▼
                  ┌─────────────────────────┐
                  │ Single-node Swarm       │
                  │                         │
                  │ Application services    │
                  │ Traefik                 │
                  │ Overlay networks        │
                  │ Secrets and configs     │
                  └────────────┬────────────┘
                               │
                               ▼
                         Local registry


                          PUBLIC TRAFFIC

Internet
   │
Cloudflare
   │
Cloudflare Tunnel
   │
cloudflared
   │
Traefik
   │
Swarm application service
```
## 5.1 One daemon
The first prototype must use one `piqueld` process.
Do not divide the control plane into separately deployed services.
Internal modules should have clear responsibilities, but they should share:
- One process.
- One database.
- One API.
- One operation scheduler.
- One Docker connection pool.
## 5.2 Single-node Swarm
All application workloads are created as Swarm services, even though the Swarm contains only one node.
This provides:
- Replica semantics.
- Service discovery.
- Overlay networks.
- Swarm secrets.
- Swarm configs.
- Rolling updates.
- A future path to multiple nodes.
`piqueld` should interact with the Docker Engine API directly. It should not execute `docker stack deploy`.
# 6. Domain model
The prototype should use a small domain model.
## 6.1 Application
An application is the top-level user-managed resource.
An application contains:
- Metadata.
- Services.
- Volumes.
- Routes.
- Secret references.
Example:
```text
Application: notes
  ├── Service: web
  ├── Service: worker
  ├── Volume: data
  └── Route: notes.example.com → web:3000
```
## 6.2 Service
A service becomes one Docker Swarm service.
A service contains:
- Name.
- Image source or build source.
- Replica count.
- Environment variables.
- Command and arguments.
- Exposed internal ports.
- Volume mounts.
- Secret mounts.
- Health check.
- Resource limits where supported.
The first prototype supports replicated services only.
A global-service mode is unnecessary for the single-node prototype.
## 6.3 Source
A service has exactly one source.
### Prebuilt image
```toml
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1.4.0"
```
Before deployment, `piqueld` resolves the image to a digest.
### Git build
```toml
[spec.services.source]
type = "git"
repository = "https://github.com/example/notes.git"
reference = "main"
context = "."
dockerfile = "Dockerfile"
```
Before deployment, `piqueld`:
1. Resolves the Git reference to a commit.
2. Checks out the commit.
3. Builds the image.
4. Pushes it to the local registry.
5. Resolves the registry image digest.
6. Deploys the digest.
## 6.4 Volume
A volume is a named Docker volume.
Example:
```toml
[[spec.volumes]]
name = "data"
```
A service can mount it:
```toml
[[spec.services.mounts]]
volume = "data"
target = "/var/lib/notes"
read_only = false
```
The prototype will not distinguish between stateful and stateless services in code.
All managed volumes use the same model.
Volumes are retained when an application is deleted unless the user explicitly requests deletion.
## 6.5 Route
A route maps a hostname to a service and internal port.
```toml
[[spec.routes]]
host = "notes.example.com"
service = "web"
port = 3000
```
`piqueld` converts routes into Traefik service labels.
The first prototype only needs host-based HTTP routing.
The following are deferred:
- Path-based routing.
- TCP routing.
- UDP routing.
- Middleware configuration.
- Rate limiting.
- Authentication middleware.
- User-defined TLS settings.
## 6.6 Secret management
Secrets are sensitive values such as passwords, API tokens, certificates, and connection strings.
`piqueld` uses two secret representations:
1. An authoritative encrypted value stored in libSQL.
2. An immutable Docker Swarm secret used to deliver the value to containers.
Docker Swarm is only the runtime delivery mechanism. `piqueld` keeps an encrypted copy so it can recreate missing Swarm secrets, restore imported state, and replace secret values.
### Creating secrets
Secrets are created separately from application manifests:
```bash
printf '%s' "$DATABASE_URL" |
    piqueldctl secret set database-url --stdin
```
Values should be accepted through standard input, protected files, or raw API request bodies. They should not normally be accepted as command-line arguments.
The API never returns the plaintext after creation. It exposes metadata only:
- Name.
- Whether a value is set.
- Last update time.
- Current generation.
- Referencing applications and services.
### Application references
Application manifests contain secret references, never values:
```toml
[[spec.services.secrets]]
source = "database-url"
target = "database-url"
mode = "0400"
```
The service receives the secret as a file:
```text
/run/secrets/database-url
```
Applications should read secrets from files:
```toml
[spec.services.environment]
DATABASE_URL_FILE = "/run/secrets/database-url"
```
The prototype will not inject secret values directly into environment variables.
### Encryption at rest
Each value is encrypted before being written to libSQL.
The master encryption key must be stored separately from the database, preferably through a systemd credential or protected root-owned file. It must not be stored in the daemon configuration, database, Nix store, logs, or state exports.
The database stores:
```text
id
name
generation
encryption_algorithm
encryption_key_id
nonce
ciphertext
swarm_secret_name
created_at
updated_at
```
Use a maintained authenticated-encryption implementation such as XChaCha20-Poly1305. Each encryption operation must use a new random nonce.
Plaintext secret types should not implement ordinary logging, display, or serialization traits and should be cleared from memory when dropped.
### Runtime delivery
When a service requires a secret, `piqueld`:
1. Decrypts the value.
2. Creates a Docker Swarm secret.
3. Grants it only to services that reference it.
4. Mounts it at the configured target path.
5. Discards the plaintext from memory.
The internal Swarm secret name includes a random generation:
```text
piqueld-secret-database-url-01JZ8R7B4W
```
The application continues to use the stable path:
```text
/run/secrets/database-url
```
### Replacement
Docker secrets cannot be modified in place.
Replacing a secret therefore:
1. Encrypts and stores the new value.
2. Creates a new Swarm secret generation.
3. Updates consuming services.
4. Waits for the services to converge.
5. Removes the previous generation once unused.
During a rolling update, old and new tasks may temporarily use different secret generations.
Replacing a secret only changes the value delivered to the container. It does not automatically update an external database, API provider, or other system that validates the credential.
### Reconciliation and failures
The reconciler verifies that:
- Every referenced logical secret exists.
- The expected Swarm secret exists.
- Services reference the current generation.
- Unused old generations are eventually removed.
If a Swarm secret is missing, `piqueld` recreates it from the encrypted database value.
If the value cannot be decrypted, affected applications are marked degraded and deployment is blocked. `piqueld` must never substitute an empty value.
If a service fails while adopting a replacement secret, the previous generation is retained until it is no longer used.
### Deletion
A secret cannot be deleted while referenced by an application.
The user must first remove all references and deploy the updated applications. `piqueld` can then remove the Swarm secret and encrypted database record.
Forced deletion is not supported in the prototype.
### Import and export
Application exports contain secret references only.
A normal control-plane export contains secret metadata but excludes:
- Plaintext.
- Ciphertext.
- The master encryption key.
After importing such an export, required secret values must be supplied again.
An optional encrypted export may include ciphertext, but restoration requires the same master key. The key must always be transferred separately.
### Security restrictions
Secret values must never appear in:
- API responses.
- Logs.
- Error messages.
- URLs.
- Application exports.
- Browser storage.
- Docker labels.
The UI displays metadata and replacement controls but provides no reveal operation.
This design does not protect secrets from host root, a compromised `piqueld` process, a Docker administrator, or an application that is authorized to receive the secret.
## 6.7 Desired state
The desired state is the normalized application specification stored in libSQL.
## 6.8 Resolved state
The resolved state contains deployment-specific immutable values:
- Git commit.
- Image digest.
- Local registry reference.
- Normalized Swarm resource names.
- Specification hash.
The first prototype only retains the latest desired and resolved state.
## 6.9 Observed state
The observed state is reconstructed from Docker Swarm.
It includes:
- Existing services.
- Current service specifications.
- Current task state.
- Existing networks.
- Existing volumes.
- Existing secrets.
- Current image references.
- Relevant Traefik labels.
Observed state is not authoritative. It is compared against the desired state.
# 7. Resource ownership and naming
All managed Docker resources must carry ownership labels.
Example:
```text
io.piqueld.managed=true
io.piqueld.instance=<instance-id>
io.piqueld.application=<application-id>
io.piqueld.service=<service-name>
io.piqueld.spec-hash=<hash>
```
`piqueld` must never modify or delete resources that do not contain valid ownership labels for the current instance.
## 7.1 Deterministic names
Suggested resource naming:
```text
Application network:
piqueld-<application-id>

Service:
piqueld-<application-id>-<service-name>

Volume:
piqueld-<application-id>-<volume-name>

Secret:
piqueld-<application-id>-<secret-name>-<content-prefix>
```
Names should use stable internal application IDs rather than relying only on user-editable names.
# 8. Application manifest
The operator-facing text format should be TOML.
The API should use JSON.
The manifest must include:
- API version.
- Resource kind.
- Metadata.
- Specification.
Example:
```toml
api_version = "piqueld.dev/v1alpha1"
kind = "Application"

[metadata]
name = "notes"

[spec]

[[spec.services]]
name = "web"
replicas = 1

[spec.services.source]
type = "git"
repository = "https://github.com/example/notes.git"
reference = "main"
context = "."
dockerfile = "Dockerfile"

[spec.services.environment]
RUST_LOG = "info"
DATABASE_URL_FILE = "/run/secrets/database-url"

[spec.services.healthcheck]
type = "http"
port = 3000
path = "/health"
interval_seconds = 10
timeout_seconds = 3

[[spec.services.mounts]]
volume = "data"
target = "/var/lib/notes"
read_only = false

[[spec.services.secrets]]
name = "database-url"
target = "/run/secrets/database-url"

[[spec.volumes]]
name = "data"

[[spec.routes]]
host = "notes.example.com"
service = "web"
port = 3000
```
## 8.1 Manifest rules
The parser must:
- Reject unknown fields.
- Reject duplicate service names.
- Reject duplicate volume names.
- Reject routes referencing missing services.
- Reject mounts referencing missing volumes.
- Reject secret references that do not exist.
- Validate names against a documented naming format.
- Apply defaults before hashing.
- Normalize collections before hashing.
- Produce useful field-level errors.
## 8.2 No internal text editor
The UI must not include an embedded TOML editor in the first prototype.
There are two supported update paths:
### Structured API updates
Used by:
- The UI.
- CLI subcommands.
- Other API clients.
### Full manifest application
Used by:
- `piquelctl application apply`.
- External scripts.
- Configuration import.
Both paths must call the same application service and produce the same normalized desired state.
# 9. Update and conflict model
The prototype does not need configuration history, but it still needs safe concurrent updates.
Each application should have a monotonically increasing generation:
```text
generation = 7
```
A client reads generation `7`, edits the application, and submits:
```json
{
  "expected_generation": 7,
  "spec": {}
}
```
If the current generation is already `8`, the update is rejected with a conflict.
This prevents the UI and text manifest workflow from silently overwriting each other.
## 9.1 Full replacement
The prototype should use full application-specification replacement.
Do not implement:
- Field ownership.
- Three-way merging.
- Strategic merge patches.
- Server-side apply.
- Automatic conflict resolution.
A future API may add JSON Patch for narrow updates.
# 10. Planning and deployment workflow
Configuration persistence and runtime deployment should be separate operations.
## 10.1 Plan
A plan compares a proposed application specification with the current desired and observed state.
A plan can contain actions such as:
```text
CREATE network piqueld-app-01
CREATE volume piqueld-app-01-data
BUILD service web
PUSH image to local registry
CREATE secret database-url
CREATE service piqueld-app-01-web
ADD route notes.example.com
```

For an update:
```text
BUILD new image for web
UPDATE service image
UPDATE service environment
WAIT for service convergence
```

For deletion:
```text
REMOVE service web
REMOVE application network
RETAIN volume data
```
## 10.2 Apply
An apply operation:
1. Validates the submitted specification.
2. Checks the expected generation.
3. Resolves mutable inputs.
4. Produces a plan.
5. Stores the desired and resolved state.
6. Creates a durable operation record.
7. Executes the plan.
8. Verifies the resulting Swarm state.
9. Updates application status.
## 10.3 Deployment status
Suggested statuses:
```text
pending
resolving
building
deploying
healthy
degraded
failed
deleting
```
The current status should be separate from the desired application document.
## 10.4 No rollback feature
The prototype will not expose:
- Previous configurations.
- Previous image revisions.
- Rollback commands.
- Rollback UI.
The architecture should avoid making rollback impossible, but no revision-history tables or rollback workflows are required yet.

Swarm updates should use a conservative failure policy:
- Start replacement tasks before stopping healthy tasks where possible.
- Pause a failed update.
- Preserve useful failure information.
- Do not automatically destroy the last healthy deployment.
# 11. Reconciliation engine
The reconciler is the central runtime component.
## 11.1 Reconciliation cycle
For each application:
1. Load desired and resolved state.
2. Query Docker for owned resources.
3. Convert Docker responses into an observed domain model.
4. Compare desired and observed state.
5. Generate an action plan.
6. Execute required actions.
7. Wait for Swarm convergence.
8. Update current status.
Conceptually:
```rust
fn plan(
    desired: &ResolvedApplication,
    observed: &ObservedApplication,
) -> Result<Plan, PlanningError>;
```
Planning should remain as close to a pure function as practical.
## 11.2 Reconciliation triggers
Reconciliation should run:
- After an application is applied.
- After an application is deleted.
- When relevant Docker events occur.
- When `piqueld` starts.
- During a periodic full scan.
The periodic scan recovers from:
- Missed Docker events.
- Daemon restarts.
- Manual resource modification.
- Partial operations.
- Unexpected task failures.
## 11.3 Concurrency
Use:
- At most one mutating operation per application.
- A bounded global number of concurrent deployments.
- A separate low concurrency limit for builds.
- Tokio cancellation tokens for shutdown.
- Durable operation state for restart recovery.
No external task queue is needed.
## 11.4 Idempotency
Every executor action should be safe to retry.
Examples:
```text
Ensure network exists
Ensure volume exists
Ensure secret exists
Ensure service has expected specification
Ensure obsolete owned service is absent
```
Avoid actions based purely on “create” or “delete” commands without checking current state.
# 12. Local registry
The prototype will include a local OCI registry for images built by `piqueld`.
## 12.1 Purpose
The registry provides:
- Stable storage for locally built images.
- Digest-addressable deployment.
- Separation between build and execution.
- Easier image cleanup.
- A future path to multi-node image distribution.
## 12.2 Prototype deployment
For the single-node prototype:
- Run the registry locally.
- Bind it only to loopback or another non-public interface.
- Store registry data in a dedicated persistent directory.
- Configure Docker to trust the local registry if TLS is not used.
- Never expose the registry through Cloudflare Tunnel.
- Never expose it to the public network.
Example image naming:
```text
127.0.0.1:5000/piqueld/<application-id>/<service-name>:<build-id>
```
The deployed Swarm service should ultimately reference the digest:
```text
127.0.0.1:5000/piqueld/app-01/web@sha256:...
```
## 12.3 Future compatibility
Registry addressing must be configurable.
A future multi-node deployment will require:
- A registry address reachable by all nodes.
- TLS.
- Registry authentication.
- Registry availability and backup planning.
Those features are not part of the prototype.
## 12.4 Image cleanup
The prototype may implement conservative cleanup of unreferenced build tags, but aggressive garbage collection is not required.
No image referenced by a current application may be removed.
# 13. Build workflow
The build pipeline is:
```text
Git repository
      ↓
Resolve reference to commit
      ↓
Checkout isolated working directory
      ↓
Create build context
      ↓
Build with Docker BuildKit
      ↓
Tag for local registry
      ↓
Push to registry
      ↓
Resolve digest
      ↓
Update Swarm service
```
## 13.1 Build isolation
Each build should use:
- A unique temporary directory.
- A bounded build timeout.
- A maximum context size.
- Redacted build arguments.
- Controlled environment variables.
- Cleanup after completion.
## 13.2 Build identity
The build identity should include at least:
- Git commit.
- Dockerfile path.
- Build context path.
- Build arguments excluding secret values.
- Target stage.
- Platform.
- Relevant builder configuration.
A content-derived build key can be used for caching and naming.
## 13.3 Build secrets
Build secrets must not be passed as ordinary Docker build arguments.
Secret build support can be limited or deferred if BuildKit secret mounts cannot be implemented safely in the first prototype.
# 14. Traefik integration
Traefik will be the only supported proxy backend in the prototype.
It should run as a Swarm service and use the Swarm provider.
## 14.1 Routing translation
A route:
```toml
[[spec.routes]]
host = "notes.example.com"
service = "web"
port = 3000
```
becomes service labels resembling:
```text
traefik.enable=true
traefik.http.routers.<router>.rule=Host(`notes.example.com`)
traefik.http.routers.<router>.entrypoints=web
traefik.http.services.<service>.loadbalancer.server.port=3000
```
Users should not provide arbitrary Traefik labels in the first prototype.
`piqueld` owns the translation from its stable route model to Traefik-specific configuration.
## 14.2 Ingress network
`piqueld` should ensure a shared ingress overlay network exists.
Traefik joins this network.
Any service with a public route also joins this network.
Each application additionally receives its own private overlay network for service-to-service communication.
## 14.3 Public ingress
Cloudflare Tunnel is external to `piqueld`.
The intended flow is:
```text
Cloudflare Tunnel
    → Traefik origin port
    → Swarm service
```
`piqueld` will not manage:
- Cloudflare accounts.
- Tunnel creation.
- Cloudflare DNS.
- Tunnel credentials.
- Cloudflare Access policies.
The NixOS module may document how to connect an existing `cloudflared` service to Traefik.
# 15. Persistence
Use the official libSQL Rust SDK in embedded local mode.
Use SQLx for compile-time type-safe queries.
Use explicit SQL migrations and a repository abstraction.
## 15.1 Proposed tables
### `applications`
```text
id
name
generation
desired_spec_json
resolved_spec_json
spec_hash
desired_state
created_at
updated_at
```

### `application_status`
```text
application_id
phase
message
observed_spec_hash
last_reconciled_at
updated_at
```

### `operations`
```text
id
application_id
kind
status
started_at
finished_at
error_code
error_message
```

### `operation_steps`
```text
id
operation_id
sequence
kind
status
message
started_at
finished_at
```

### `secrets`
```text
id
name
encrypted_value
content_hash
created_at
updated_at
```

### `builds`
```text
id
application_id
service_name
source_commit
image_reference
image_digest
status
started_at
finished_at
error_message
```

### `instance_metadata`
```text
instance_id
schema_version
created_at
```
## 15.2 Operational records are not configuration history
`operations` and `builds` exist for:
- Crash recovery.
- Current progress.
- Debugging.
- Failure reporting.
They are not a user-facing deployment-history or rollback system.
A retention policy may remove old completed operation records.
## 15.3 Transactions
Application updates must be transactional.
The desired specification, resolved specification, generation, and initial operation record should be written atomically.
# 16. Import and export
## 16.1 Application export
An application can be exported as TOML:
```text
piquelctl application export notes
```
The export includes:
- Desired application specification.
- Original image or source reference.
- Optionally, resolved image digest and Git commit.
It does not include secret values.
## 16.2 Application apply
```text
piquelctl application plan --file notes.toml
piquelctl application apply --file notes.toml
```
Applying a manifest replaces the full application specification.
## 16.3 Control-plane state export
The prototype should support an export containing:
- Applications.
- Current desired specifications.
- Current resolved specifications.
- Secret ciphertext.
- Relevant instance metadata.
- Checksums.
- Export schema version.

The export does not include:
- Docker volume contents.
- Local registry blobs.
- Container logs.
- Cloudflare configuration.
- Git repositories.
- External image registries.

Therefore, the prototype state export is a **control-plane export**, not a complete disaster-recovery backup.
## 16.4 State import
State import should:
1. Validate the archive.
2. Validate the schema version.
3. Verify checksums.
4. Validate all application specifications.
5. Require explicit replacement confirmation.
6. Stop ordinary reconciliation.
7. Import state transactionally.
8. Resume reconciliation.
9. Report missing images, secrets, or external dependencies.
Existing volumes should be retained unless explicitly removed by a separate destructive operation.
# 17. API design
Use versioned HTTP and JSON.
Use Server-Sent Events for one-way live streams.
Do not create a custom binary protocol.
## 17.1 Initial endpoints
```text
GET    /api/v1/system/status
GET    /api/v1/system/capabilities

GET    /api/v1/applications
POST   /api/v1/applications
GET    /api/v1/applications/{id}
PUT    /api/v1/applications/{id}
DELETE /api/v1/applications/{id}

POST   /api/v1/applications/plan
POST   /api/v1/applications/{id}/plan
POST   /api/v1/applications/{id}/reconcile

GET    /api/v1/applications/{id}/status
GET    /api/v1/applications/{id}/logs
GET    /api/v1/applications/{id}/events

GET    /api/v1/operations/{id}
GET    /api/v1/operations/{id}/events

GET    /api/v1/secrets
POST   /api/v1/secrets
PUT    /api/v1/secrets/{name}
DELETE /api/v1/secrets/{name}

GET    /api/v1/state/export
POST   /api/v1/state/import
```
Exact paths may change, but the resource boundaries should remain.
## 17.2 Structured errors
API errors should include:
```json
{
  "code": "application_generation_conflict",
  "message": "The application was modified by another client.",
  "details": {
    "expected_generation": 4,
    "current_generation": 5
  }
}
```
Stable machine-readable error codes are more important than preserving exact error messages.
## 17.3 OpenAPI
Generate and serve an OpenAPI description from the daemon.
The CLI and UI should use the same public API that external clients use.
# 18. Repository and crate structure
Avoid splitting the prototype into too many crates.
Recommended workspace:
```text
Cargo.toml

crates/
  piqueld-core/
  piqueld-client/

apps/
  piqueld/
  piquelctl/
  piqueld-ui/

migrations/
nix/
tests/
```
## 18.1 `piqueld-core`
Contains:
- Application domain types.
- Manifest types.
- Validation.
- Normalization.
- Hashing.
- Planning types.
- Public error codes.
It must not depend on:
- Axum.
- libSQL.
- Bollard.
- Leptos.
## 18.2 `piqueld`
The daemon crate and process.
Internal modules:
```text
api/
auth/
build/
config/
docker/
operations/
proxy/
reconcile/
registry/
secrets/
store/
```
These should remain modules rather than separate crates until independent reuse or compile-time boundaries justify extraction.
## 18.3 `piqueld-client`
Typed Rust HTTP client used by:
- The CLI.
- The UI where practical.
- Integration tests.
- External Rust consumers.
## 18.4 `piquelctl`
CLI binary for administering `piqueld`.
Suggested commands:
```text
piquelctl status

piquelctl application list
piquelctl application show <name>
piquelctl application plan --file <path>
piquelctl application apply --file <path>
piquelctl application export <name>
piquelctl application delete <name>
piquelctl application logs <name>

piquelctl secret list
piquelctl secret set <name>
piquelctl secret delete <name>

piquelctl operation watch <id>

piquelctl state export
piquelctl state import <archive>
```
## 18.5 `piqueld-ui`
A Rust/WASM web UI.
The prototype UI should support:
- Application list.
- Application status.
- Structured application creation and editing.
- Deployment plan preview.
- Apply.
- Delete.
- Build progress.
- Runtime logs.
- Secret metadata management.
- State export and import.
It should not contain a raw TOML editor.
# 19. Technology stack
## 19.1 Core
- Rust stable.
- Rust 2024 edition.
- Tokio.
- Serde.
- `serde_json`.
- `toml`.
- `thiserror`.
- `anyhow` only at binary boundaries.
- `tracing`.
- `tracing-subscriber`.
## 19.2 API
- Axum.
- Tower.
- `tower-http`.
- Utoipa for OpenAPI.
- Schemars for generated JSON Schema.
## 19.3 Database
- Official `libsql` crate.
- Embedded local database.
- SQLx.
- Explicit SQL migrations.
## 19.4 Docker and Swarm
- Bollard.
- Docker Engine API.
- Swarm service, network, secret, config, and task APIs.
- Docker event stream.
## 19.5 Git
- `gix`.
- HTTPS repositories first.
- Token-based credentials first.
- SSH support deferred if necessary.
## 19.6 Builds
- Docker BuildKit.
- Dockerfiles.
- Tar build contexts created in Rust.
- Local OCI registry.
## 19.7 CLI
- Clap.
- `piqueld-client`.
- Human-readable output by default.
- JSON output through an explicit flag.
- Stable non-zero exit codes.
## 19.8 UI
- Leptos in client-side rendering mode.
- No JavaScript or TypeScript application source.
- HTML and CSS remain normal web assets.
# 20. Daemon bootstrap configuration
The daemon has a small read-only TOML configuration file.
It contains host-specific settings only.
Example:
```toml
data_dir = "/var/lib/piqueld"

[server]
unix_socket = "/run/piqueld/piqueld.sock"
http_listen = "127.0.0.1:7845"

[database]
path = "/var/lib/piqueld/piqueld.db"

[docker]
socket = "/var/run/docker.sock"
auto_initialize_swarm = true

[registry]
address = "127.0.0.1:5000"
data_dir = "/var/lib/piqueld/registry"

[traefik]
enabled = true
origin_port = 8080

[reconciliation]
scan_interval_seconds = 60
max_parallel_operations = 4
max_parallel_builds = 1
```
The daemon must not rewrite this file.
Application configuration belongs in libSQL.
# 21. Authentication and network security
The control plane must not be publicly reachable.
## 21.1 Local access
Local clients may use a Unix socket protected by filesystem permissions.
## 21.2 Remote access
Remote access should occur through Tailscale.
Recommended deployment:
```text
Tailscale client
    → Tailscale Serve
    → piqueld bound to loopback
```
`piqueld` may trust Tailscale identity headers only when:
- The HTTP server is bound to loopback.
- Direct non-proxied remote access is impossible.
- The proxy strips incoming spoofed identity headers.
A static bearer token may be supported as a simpler fallback.
## 21.3 Prototype authorization
The prototype needs only one administrative role.
It does not need:
- Users.
- Teams.
- Sessions.
- Password recovery.
- OAuth providers.
- Fine-grained permissions.
Every authenticated control-plane caller should be considered an infrastructure administrator.
## 21.4 Docker socket risk
Access to the Docker socket is effectively host-administrative access.
Consequently:
- The daemon must be treated as privileged.
- The API must not expose arbitrary Docker calls.
- Application manifests must not allow arbitrary host paths by default.
- Bind mounts should be excluded or tightly restricted in the prototype.
- Docker errors must be sanitized before returning them to clients.
## 21.5 Public traffic separation
Cloudflare Tunnel may route public application traffic to Traefik.
It must never route traffic to:
- The `piqueld` API.
- The Docker API.
- The local registry.
- The Traefik administrative API.
# 22. NixOS module
The repository must include a Nix flake and NixOS module.
Suggested interface:
```nix
services.piqueld = {
  enable = true;
  package = pkgs.piqueld;

  dataDir = "/var/lib/piqueld";

  server = {
    unixSocket = "/run/piqueld/piqueld.sock";
    httpListen = "127.0.0.1:7845";
  };

  swarm.autoInitialize = true;

  registry = {
    enable = true;
    address = "127.0.0.1:5000";
  };
};
```

The module should:
- Install `piqueld`.
- Optionally install `piquelctl`.
- Generate the daemon configuration.
- Create state and runtime directories.
- Configure the systemd service.
- Configure access to the Docker socket.
- Configure the local registry.
- Configure Docker’s local registry trust where required.
- Avoid opening firewall ports automatically.
- Support credentials through systemd credentials or protected files.
The module should not manage Cloudflare account configuration.
# 23. Observability
Use structured tracing throughout the daemon.
Every relevant log should include available identifiers:
```text
application_id
service_name
operation_id
build_id
spec_hash
docker_service_id
```
## 23.1 Health endpoints
Expose:
```text
/api/v1/system/health
/api/v1/system/readiness
```
Health means the process is running.
Readiness should require:
- Database access.
- Docker access.
- Swarm manager availability.
- Required internal infrastructure status.
## 23.2 Metrics
The prototype may expose Prometheus-compatible control-plane metrics:
- Reconciliation count.
- Reconciliation failures.
- Active operations.
- Build duration.
- Deployment duration.
- Docker API failures.
- Number of applications.
- Number of unhealthy services.
Do not store time-series data in libSQL.
# 24. Testing strategy
## 24.1 Unit tests
Test:
- Manifest parsing.
- Unknown-field rejection.
- Validation.
- Default application.
- Normalization.
- Stable hashing.
- Resource naming.
- Traefik label generation.
- Swarm service generation.
- Desired-versus-observed planning.
- Error mapping.
- Secret redaction.
## 24.2 Property tests
Important properties:
```text
normalize(normalize(x)) == normalize(x)
```

```text
plan(desired, matching_observed) == empty
```

```text
parse(export(parse(manifest))) == normalized_manifest
```

```text
resource_names(application) are stable
```
## 24.3 Database tests
Test:
- Migrations.
- Transaction rollback.
- Generation conflicts.
- State export/import round trips.
- Corrupt import rejection.
- Operation restart recovery.
- Secret encryption and decryption.
## 24.4 Docker integration tests
Use an isolated Docker environment where practical.
Test:
- Swarm initialization.
- Network creation.
- Volume creation.
- Secret creation.
- Service deployment.
- Replica updates.
- Health-check failure.
- Build and registry push.
- Digest-based deployment.
- Drift correction.
- Application deletion.
- Volume retention.
- Traefik label generation.
- Daemon restart during deployment.
## 24.5 NixOS VM tests
Test:
- Module evaluation.
- Service startup.
- Swarm initialization.
- Registry availability.
- Directory permissions.
- Unix-socket permissions.
- Daemon restart.
- Persistent database state.
- Configuration changes.
## 24.6 Security tests
Test:
- Unauthenticated API rejection where applicable.
- Secret redaction.
- Invalid archive paths.
- Oversized request rejection.
- Invalid manifest rejection.
- Attempts to modify unowned Docker resources.
- Attempts to use forbidden host mounts.
# 25. CI requirements
The initial CI pipeline should include:
```text
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace
cargo test --doc --workspace
cargo deny check
nix flake check
NixOS module tests
migration tests
manifest round-trip tests
```
Docker integration tests may run in a separate privileged CI job.
Release artifacts should include:
- `piqueld`.
- `piquelctl`.
- UI assets embedded in or distributed with `piqueld`.
- Nix packages.
- Checksums.
# 26. Prototype implementation phases
## Phase 1: Domain and planning
Implement:
- Application types.
- TOML parsing.
- Validation.
- Normalization.
- Hashing.
- Swarm resource model.
- Desired-versus-observed planner.
- Unit and property tests.
No database, API, or UI is required to validate this phase.
## Phase 2: Persistence and API
Implement:
- libSQL.
- Migrations.
- Application repository.
- Generation conflicts.
- Operation journal.
- Axum API.
- OpenAPI.
- Typed client.
## Phase 3: Swarm deployment
Implement:
- Docker connection.
- Optional single-node Swarm initialization.
- Resource ownership labels.
- Networks.
- Volumes.
- Secrets.
- Service creation and updates.
- Task observation.
- Reconciliation.
- Docker events.
## Phase 4: Build and registry
Implement:
- Local registry.
- Git resolution.
- Git checkout.
- Dockerfile builds.
- Registry push.
- Digest resolution.
- Build logs.
- Build status.
## Phase 5: Traefik and public routing
Implement:
- Traefik infrastructure service.
- Shared ingress network.
- Route label generation.
- Route verification.
- Documentation for Cloudflare Tunnel integration.
## Phase 6: CLI and UI
Implement:
- `piquelctl`.
- Application management.
- Plan and apply.
- Logs.
- Secret management.
- State export and import.
- Minimal Leptos UI.
## Phase 7: Hardening
Implement:
- Restart recovery.
- Request limits.
- Authentication mode.
- Secret encryption.
- NixOS VM tests.
- Docker integration tests.
- Security review.
- Documentation.
# 27. Prototype acceptance criteria
The prototype is successful when an operator can:
1. Install `piqueld` through the NixOS module.
2. Start it on a machine with Docker.
3. Initialize or validate single-node Swarm mode.
4. Create a secret.
5. Submit a TOML application manifest.
6. Preview the deployment plan.
7. Build an image from a Git repository.
8. Store the image in the local registry.
9. Deploy the image by digest as a Swarm service.
10. Run multiple replicas on the single node.
11. Route a hostname through Traefik.
12. Reach the application through Cloudflare Tunnel.
13. Inspect deployment progress.
14. Read application logs.
15. Modify the application through the UI or CLI.
16. Detect a stale-generation update.
17. Restart `piqueld` without losing desired state.
18. Have `piqueld` repair a manually modified owned service.
19. Delete an application without deleting its volumes.
20. Export and re-import the control-plane state.
# 28. Deferred architecture
The following should remain possible without being implemented now.
## Multi-node Swarm
A future deployment may add:
- Additional worker nodes.
- Three Swarm managers.
- Node labels.
- Placement constraints.
- Placement preferences.
- Dedicated builder nodes.
- Dedicated ingress nodes.
The prototype must not assume that a service task always runs on the same machine as `piqueld`, except where explicitly required by the local registry configuration.
## Remote registry
The registry configuration should be replaceable with:
- A cluster-accessible private registry.
- An external hosted registry.
- Authenticated TLS access.
## Restricted node agent
A future agent may support host-level operations that Swarm cannot perform.
It must not be required for ordinary service execution.
## Configuration history and rollback
The domain model may later add immutable application revisions.
The first prototype stores only the latest desired and resolved state.
## Storage architecture
A future design may add:
- Volume classes.
- Node-local storage constraints.
- Shared storage.
- Snapshotting.
- Backup policies.
- Replicated storage.
The prototype uses simple named volumes and does not classify services by storage behaviour.
## Highly available control plane
A future `piqueld` architecture may add:
- Active-passive controllers.
- Replicated libSQL.
- Leader election.
- Shared encryption keys.
- Operation takeover.
The prototype has exactly one active daemon.
