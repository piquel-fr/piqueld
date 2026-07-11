# Plan 10 — Application and control-plane import/export

## Goal

Provide portable application manifests and a checksummed, versioned control-plane
archive with explicit destructive import semantics and safe reconciliation pause.

## Deliverables

- Canonical TOML application export with desired source references and optional
  resolved commit/digest metadata, never secret values.
- A documented versioned archive format containing manifest/index, applications,
  desired/resolved state, relevant instance metadata, secret metadata, checksums,
  and optionally ciphertext—but never the master key.
- Streaming archive creation/validation with deterministic entries, size/count
  limits, path-traversal/link rejection, and checksum verification before mutation.
- State export/import API, OpenAPI, and typed-client support.
- Explicit replace-confirmation token/workflow; import maintenance gate pauses new
  ordinary mutations and reconciliation, drains or cancels safely, writes state
  transactionally, then resumes.
- Post-import dependency report for missing secret values/key compatibility, images,
  Git/external registries, runtime secrets, and retained volumes.

## Work

1. Reconcile the design's two export descriptions by supporting two explicit modes:
   portable (secret metadata only; values must be supplied) and encrypted (ciphertext
   included; same separately transferred key required). Default to portable.
2. Decide and document instance-ID semantics. A replace import into the same control
   plane preserves ownership compatibility; a restore to a new instance must not
   seize old unowned Docker resources without an explicit safe adoption design.
3. Fully parse, checksum, schema-check, and core-validate the archive into staging
   data before opening the replacement transaction.
4. Never include volumes, registry blobs, logs, Git worktrees, Cloudflare data, or
   master keys. State clearly that this is not a full disaster-recovery backup.
5. On any validation/write failure leave prior state intact and resume the prior
   reconciliation mode. Record the import operation and safe diagnostics.
6. Export from a consistent database snapshot without blocking longer than needed.

## Verification

- Application TOML and full state round trips preserve normalized state/hashes.
- Corrupt checksum, unknown schema version, malformed spec, duplicate entry, archive
  bomb, traversal, symlink, missing confirmation, and interrupted import tests.
- Portable exports contain neither ciphertext nor plaintext; encrypted exports
  contain no key and fail safely with the wrong key.
- Transaction fault injection proves failed import does not partially replace state.

## Done when

An operator can export/re-import control-plane state with an explicit, auditable
replacement operation, receive dependency warnings, and never implicitly delete
runtime volumes or disclose keys/plaintext.

