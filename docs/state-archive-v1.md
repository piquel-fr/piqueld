# piqueld state archive v1

State exports are deterministic, uncompressed POSIX tar streams with media type
`application/vnd.piqueld.state-v1+tar`. The default `portable` mode contains
logical-secret metadata but no values or ciphertext. `encrypted` mode includes
authenticated ciphertext envelopes, but never the master key or plaintext.

`manifest.json` identifies the format, version, mode, source instance, schema
version, and SHA-256 digest of every other entry. `state.json` contains desired
application state, resolved immutable source metadata, status metadata, and
logical-secret metadata. Canonical application manifests are also stored under
`applications/<application-id>.toml` for independent inspection. Entries are
sorted, and tar ownership and timestamps are fixed for reproducible output.

The reader validates the complete archive before opening the replacement
transaction. It rejects links and devices, traversal or absolute paths,
case-colliding or duplicate entries, unknown fields or versions, archive bombs,
checksum mismatches, non-canonical manifests, invalid resource state, and
unauthenticated encrypted envelopes. Archives are bounded to 32 MiB, 2,048
entries, and 4 MiB per entry.

Import is destructive to control-plane state and requires a five-minute,
single-use confirmation token bound to the SHA-256 digest of the exact bytes.
The maintenance gate drains existing mutations and reconciliation scans, blocks
new ordinary mutations, replaces rows in one immediate SQLite transaction, and
then resumes normal reconciliation. A failed transaction leaves the prior rows
intact.

A same-instance restore preserves ownership compatibility. A new-instance
restore rebuilds resource ownership labels for the target instance and never
adopts runtime objects from the source instance. The result reports secret
values and key IDs, image/Git verification, runtime secret recreation, and
retained volumes requiring operator attention.

This is portable control-plane configuration, not a complete disaster-recovery
backup. It excludes volume contents, registry blobs, build/runtime logs, Git
worktrees, Cloudflare configuration, external credentials, and encryption master
keys.
