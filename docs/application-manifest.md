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
digests. Mount targets are normalized absolute paths below `/`; environment
names and runtime strings reject invalid control data. Health-check paths are
absolute, and resource limits must specify CPU, memory, or both.

Defaults are replicas `1`, empty command and arguments, writable mounts, and the
documented health-check defaults. Services, mounts, volumes, and environment maps
are canonicalized before hashing. The specification hash is SHA-256 over a
versioned canonical JSON envelope.

The parser is pure. The Docker runtime resolves each image reference to an
immutable digest before the application is committed. Resolved runtime state is
internal persistence data and is not mixed into the public manifest DTOs.
