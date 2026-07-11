# Plan 07 — Encrypted secrets and immutable Swarm delivery

## Goal

Implement the complete logical-secret lifecycle: safe ingestion, authenticated
encryption at rest, generation-based immutable Swarm secrets, rotation, reference
protection, reconciliation, and redaction.

## Deliverables

- A master-key provider loading a systemd credential/protected file outside the DB,
  config, Nix store, logs, and exports.
- XChaCha20-Poly1305 envelope records containing algorithm, key ID, random nonce,
  ciphertext, content hash, logical generation, Swarm name, and timestamps.
- Plaintext wrapper types without `Display`, ordinary `Debug`, cloning, or
  serialization, with zeroization on drop.
- Secret repository/service, API endpoints, OpenAPI/client methods, and metadata-only
  responses including references. Create/replace accepts raw request body or safe
  stdin/file client flow; no reveal endpoint exists.
- Deterministic reference mounts but randomized immutable Swarm generation names.
- Reconciliation for create, missing-secret recreation, service adoption, safe old-
  generation cleanup, and degraded state on decryption failure.
- Deletion refusal while any desired/deployed application references the secret.

## Work

1. Require an available valid master key before secret operations. Never substitute
   empty values or generate a new key automatically against an existing database.
2. Encrypt with a new cryptographic random nonce for every write and authenticate
   stable record context as associated data. Use constant-time comparisons where
   relevant.
3. During rotation, commit the encrypted new logical generation, create the new
   Docker secret, update all consuming services, wait for convergence, then remove
   an old generation only when Docker proves it is unused. Preserve old generations
   on failure.
4. Decrypt only immediately before Docker creation, minimize plaintext lifetime,
   zeroize buffers, and sanitize all upstream crypto/Docker errors.
5. Enforce target path/mode rules from core; grant a Swarm secret only to services
   declaring the reference.
6. Extend planner/executor/status and API events rather than adding an independent
   secret deployment path.

## Verification

- Encryption round-trip, nonce uniqueness, wrong-key/tamper failure, key-ID mismatch,
  zeroization-oriented, and migration tests.
- API/client snapshots prove values never appear in response, error, OpenAPI example,
  URL, tracing, or SSE data.
- Integration tests cover create/mount, rotation, rolling failure retention, missing
  Swarm-secret recreation, referenced deletion refusal, and eventual old cleanup.
- A repository-wide redaction test scans captured logs and serialized exports for a
  unique canary secret.

## Done when

Applications can safely consume and rotate file-mounted secrets, and loss of a
runtime secret is repaired from encrypted authoritative state without disclosure.

