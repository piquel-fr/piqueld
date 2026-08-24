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
spec: editing `metadata` does not change the hash and therefore does not redeploy
services.

The parser is pure. The Docker runtime resolves each image reference to an
immutable digest before the application is committed. Resolved runtime state is
internal persistence data and is not mixed into the public manifest DTOs.
