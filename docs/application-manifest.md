# Application manifest

The supported document is a strict TOML or JSON
`piqueld.dev/v1alpha1` `Application`. Unknown fields and unsupported source
types are errors. The only service source is a prebuilt Docker/OCI image.

```toml
api_version = "piqueld.dev/v1alpha1"
kind = "Application"

[metadata]
name = "notes"

[[spec.services]]
name = "web"
replicas = 1

[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1.4.0"

[spec.services.environment]
RUST_LOG = "info"

[[spec.services.mounts]]
volume = "data"
target = "/var/lib/notes"

[[spec.volumes]]
name = "data"
```

Services support replicas, environment variables, command and argument arrays,
health checks, CPU/memory limits, and mounts of declared named volumes. Named
volumes are retained when an application is deleted. There are no manifest
fields for builds, source repositories, credentials, secrets, routes, or
published ports.

Names are 1–63 lowercase ASCII letters, digits, or hyphens; they start with a
letter and cannot end with a hyphen. Every application has at least one service.
Image references reject URL schemes, credentials, malformed tags, and malformed
digests; registry hostnames are validated case-insensitively and canonicalized
to lowercase (IPv6 literal hosts are not accepted). Mount targets are normalized
absolute paths below `/`; environment names and runtime strings reject invalid
control data. Health-check paths are absolute, and resource limits must specify
CPU, memory, or both.

Explicit budgets bound every manifest; exceeding one is a distinct validation
error whose message names the offending environment key where applicable:

| Budget | Limit |
| --- | --- |
| Services per application | 64 |
| Named volumes per application | 64 |
| Environment entries per service | 256 |
| Environment key size | 255 bytes |
| Environment value size | 65,536 bytes |
| Command / arguments elements | 128 each |
| Command / arguments element size | 4,096 bytes |
| Mounts per service | 32 |
| Health-check interval | 3,600 seconds |
| CPU limit | 1,048,576 millicores |

Defaults are replicas `1`, empty command and arguments, writable mounts, and
health-check values of path `/health`, interval `10` seconds, timeout `3`
seconds, and `3` retries. HTTP health checks run `wget` inside the container,
so the image must contain a `wget` binary; images without one (for example
distroless bases) must use command health checks instead. Services, mounts,
volumes, and environment maps are
canonicalized before hashing. The specification hash is SHA-256 over a versioned
canonical JSON envelope (`piqueld-spec-hash/v2`) covering only the canonical
spec. The application name selects which application an apply targets; changing
it targets a different application. Use the explicit rename action to retain
identity and resources, then update the manifest name.

The parser is pure. Apply compares complete normalized manifests. Identical intent
causes no image resolution or deployment, including after failure; use reconcile
to explicitly request another attempt. Changed intent is persisted before operation execution resolves
new/changed image references. Unchanged service image references reuse active
digests. Explicit refresh resolves unchanged image references again; reconciliation
reuses the prepared target. Resolved runtime state remains separate from portable
manifest DTOs. Generations advance only for changed manifests or deletion intent.

## Git build sources

A service may explicitly build from Git instead of pulling a prebuilt image:

```toml
[[spec.services]]
name = "web"
[spec.services.source]
type = "git"
[spec.services.source.repository]
url = "https://example.com/team/application.git"
branch = "main"
# commit = "0123456789012345678901234567890123456789"
[spec.services.source.build]
type = "docker"
dockerfile = "services/web/Dockerfile"
context = "."
```

The daemon requires Git and the Docker CLI in its PATH. Git inherits the host's
credentials; piqueld does not store credentials or prompt for them. Only trusted
repositories are supported: Dockerfiles execute build instructions on the host's
Docker Engine. Builds are serialized across applications, and the existing
`reconciliation.prepare_timeout_seconds` bounds preparation (default: 300 seconds).
Increase that budget for longer builds.

Each preparation gets an isolated checkout. A full configured commit hash is
used directly; otherwise the branch head is resolved once. Dockerfile and context
paths are relative to the repository root, must stay within it, and default build
context is `.`. There is no automatic build backend detection, submodule or LFS
setup, registry publishing, or automatic image cleanup. Docker's build cache is
reused and base images are refreshed with `--pull`.

The resolved source records the full Git commit and content-addressed local image
ID. All service sources are prepared before any application rollout. A failed
checkout or build preserves the existing running deployment. Normal reconciliation
reuses prepared images; explicit refresh resolves and builds sources again.
Local images are supported only on the existing single-node Swarm topology.
