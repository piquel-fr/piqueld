# Application manifest

The supported document is a strict TOML or JSON
`piqueld.dev/v1alpha1` `Application`. Unknown fields and unsupported source
types are errors. Services explicitly select a prebuilt Docker/OCI image or a Git build source.

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
fields for managed credentials, secrets, routes, or published ports.

Names are 1–63 lowercase ASCII letters, digits, or hyphens; they start with a
letter and cannot end with a hyphen. Applications may be empty. Deploying an empty application removes its services and network, retaining volume data.
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

The parser is pure. Apply saves the complete normalized configuration without
starting a deployment unless explicitly requested. Deploy captures that saved
revision and prepares every source again. Reconciliation and retries reuse the
captured deployment and its prepared target, so later saved edits cannot enter
an existing deployment. Deploy resolves and builds the saved configuration's sources again.
Resolved runtime state remains separate from portable manifests. Generations
advance on saves, changed names, and deletion intent.

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
reuses prepared images; explicit deploy resolves and builds sources again.
Local images are supported only on the existing single-node Swarm topology.

Git checkout permits file, Git, HTTP(S), and SSH transports. Executable remote
helpers such as `ext::` are disabled, including through host URL rewrites.

## Repository-backed manifests

Manifest retrieval is independent from service source selection. Add this to an
application to read its next configuration from Git when Deploy is requested:

```toml
[spec.manifest]
path = "infra/piqueld/application.toml"
[spec.manifest.repository]
url = "https://example.com/team/infrastructure.git"
branch = "main"
# commit = "0123456789012345678901234567890123456789"
```

Create the application manually with `piquelctl apply --file bootstrap.toml`.
A bootstrap manifest may contain only its header, metadata, and `spec.manifest`;
services can be supplied by the first fetched manifest. Then click **Deploy** in
the dashboard or run `piquelctl deploy NAME --yes`.

Deploy resolves the configured commit (or branch head), reads only the exact
configured TOML/JSON file, and checks that its name matches the existing
application. Empty manifests remove services and networks while retaining volume data. Other files in the
repository are ignored. A missing file fails with `manifest_not_found`; no
application is deleted. Invalid files and build failures preserve both accepted
configuration and the existing running target.

The fetched file must include `spec.manifest` to keep repository backing. Its
new repository, branch, commit, and path become the settings for subsequent
fetches after successful preparation, unless newer configuration was saved while
the deployment was preparing. Those intervening edits are preserved; the deployment
still uses its captured inputs. Omitting the section disconnects backing.
The fetched manifest and its commit are persisted for restart/retry; a new Deploy
fetches again. A manifest using a Git service source resolves that source's own
repository and revision independently. Image sources are explicitly refreshed,
even when the fetched manifest is unchanged.

Git owns runtime configuration while backing is enabled: direct apply cannot
change services or volumes, and rename is rejected with `repository_managed`.
Connection settings alone remain editable through apply so an incorrect path
can be repaired. Manifest connection settings do not change the runtime spec
hash. Source builds, deployment, and rollback retain the behavior described above.
Automatic synchronization and webhooks are not implemented.
