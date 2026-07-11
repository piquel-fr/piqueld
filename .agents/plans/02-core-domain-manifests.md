# Plan 02 — Core domain, manifests, validation, and identity

## Goal

Implement the complete pure application language in `piqueld-core`. A valid TOML or
JSON input must produce one canonical desired application and stable identity data;
invalid input must produce useful field-level errors.

## Deliverables

- Separate input/schema types, validated domain types, and normalized types for
  applications, services, image/Git sources, volumes, mounts, routes, health checks,
  environment, resource limits, and secret references.
- Strict TOML and JSON decoding (`deny_unknown_fields`) for API version
  `piqueld.dev/v1alpha1` and kind `Application`.
- Validation errors containing a stable machine code, field path, and safe message.
- Documented name/hostname/path/port/replica rules, safe defaults, canonical sorting,
  and duplicate/reference validation.
- Canonical JSON encoding and a versioned SHA-256 specification hash computed only
  after defaults and normalization.
- Stable internal IDs and deterministic Docker-safe resource/router names with
  length limits and collision-resistant suffixes. User rename behavior must be
  explicit: names are metadata; internal application ID owns resource identity.
- TOML export of the desired manifest with secret references only.
- JSON Schema generation through Schemars for public request/response types.

## Work

1. Make a service source an exhaustive tagged enum with exactly one of image or Git.
   Reject unsupported SSH/build-secret/host-mount features rather than preserving
   unknown data.
2. Validate duplicate services/volumes/routes, missing route services, missing mount
   volumes, duplicate mount targets and secret targets, path safety, health-check
   references, route host syntax, and conflicting public routes.
3. Do not check the database for logical-secret existence in the pure parser. Emit
   normalized references and provide a validation hook that Plan 04 can satisfy with
   repository data.
4. Define canonical ordering for all semantically unordered collections, including
   maps and labels. Preserve order only where it changes runtime meaning (command and
   arguments).
5. Prevent accidental secret-value types: manifests contain names/references only.
6. Include golden fixtures under `tests/fixtures/manifests/` for a prebuilt image,
   multi-service Git application, defaults, and representative invalid cases.

## Verification

- Unit tests for every validation rule and default.
- Property tests for normalization idempotence, parse/export/parse equivalence,
  stable hashing, stable resource names, and absence of panics on arbitrary inputs.
- Golden tests prove TOML and JSON normalize identically and reordered unordered
  inputs hash identically.
- Schema snapshots make accidental API shape changes visible.

## Done when

The core crate can accept either public format and return a fully validated,
canonical, hashable application without filesystem, network, database, or Docker
access. No deployment planning belongs in this plan.

