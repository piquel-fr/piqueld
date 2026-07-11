# Application manifest contract

The prototype accepts strict TOML and JSON `piqueld.dev/v1alpha1` `Application`
documents. Unknown fields and unsupported source types are errors. Names are 1–63
lowercase ASCII letters, digits, or hyphens; they start with a letter and cannot end
with a hyphen. Route hosts are fully qualified DNS hostnames. Ports are 1–65535 and
replicas are 1–100. Every application declares at least one service.

Container mount targets are normalized absolute paths below `/`; volume and secret
targets may not collide. Git context and Dockerfile paths are normalized relative
paths and cannot contain parent traversal or backslashes. Only HTTP(S) Git
repositories without embedded credentials are supported; SSH, build secrets,
bind/host mounts, placement, and raw runtime/proxy options are rejected by the
strict schema. Git repository query strings and fragments are rejected because they
commonly carry credentials. Image values use Docker/OCI registry-reference syntax;
URL schemes, user information, malformed tags, and malformed digests are rejected.
Git references follow safe Git ref syntax; source and container paths also enforce
conservative total and component length bounds.

Duplicate service ports normalize to one port. Secret modes use a leading zero and
three octal permission digits, grant at least one read bit, and grant no write bits.
Runtime strings and paths reject NUL or other control data where the downstream
runtime cannot represent it safely. HTTP health paths are absolute normalized paths,
and a resource-limit object must set CPU, memory, or both.

Defaults are replicas `1`, Git reference `main`, context `.`, Dockerfile
`Dockerfile`, secret mode `0400`, and HTTP health-check path `/health`, interval 10s,
timeout 3s. Services, volumes, routes, mounts, secrets, and ports are canonicalized;
environment maps are key ordered. Command and argument order is preserved.
Strict decode failures report the deepest known schema path without reflecting
unknown, user-controlled field names into errors.

The specification hash is SHA-256 over a versioned canonical JSON envelope after
validation, defaults, and normalization. Internal application IDs are assigned and
persisted separately from editable metadata. Renaming an application therefore does
not rename its owned resources. Docker and router names contain a truncated readable
part plus a 12-hex SHA-256 suffix and never exceed 63 bytes.
Canonical JSON, hashing, and TOML export defensively reapply canonical ordering.

Manifests contain logical secret names and mount targets only. Secret existence is
intentionally not checked by the pure parser; callers can inspect
`logical_secret_references()` and validate those names against persistence later.
Relative secret targets resolve below `/run/secrets`; collisions are checked against
that effective path.
