# Application manifest contract

The prototype accepts strict TOML and JSON `piqueld.dev/v1alpha1` `Application`
documents. Unknown fields and unsupported source types are errors. Names are 1–63
lowercase ASCII letters, digits, or hyphens; they start with a letter and cannot end
with a hyphen. Route hosts are fully qualified DNS hostnames. Ports are 1–65535 and
replicas are 1–100.

Container mount targets are normalized absolute paths below `/`. Git context and
Dockerfile paths are relative and cannot contain a `..` component. Only HTTP(S) Git
repositories are supported; SSH, build secrets, bind/host mounts, placement, and raw
runtime/proxy options are rejected by the strict schema.

Defaults are replicas `1`, Git reference `main`, context `.`, Dockerfile
`Dockerfile`, secret mode `0400`, and HTTP health-check path `/health`, interval 10s,
timeout 3s. Services, volumes, routes, mounts, secrets, and ports are canonicalized;
environment maps are key ordered. Command and argument order is preserved.

The specification hash is SHA-256 over a versioned canonical JSON envelope after
validation, defaults, and normalization. Internal application IDs are assigned and
persisted separately from editable metadata. Renaming an application therefore does
not rename its owned resources. Docker and router names contain a truncated readable
part plus a 12-hex SHA-256 suffix and never exceed 63 bytes.

Manifests contain logical secret names and mount targets only. Secret existence is
intentionally not checked by the pure parser; callers can inspect
`logical_secret_references()` and validate those names against persistence later.
