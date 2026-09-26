# Configuration

`piqueld` reads `/etc/piqueld/config.toml` when no option is supplied. An
explicit file is selected with:

```console
piqueld --config /path/to/config.toml
```

An explicitly supplied file must exist and pass validation; a missing or
invalid file is an error with its path included in the diagnostic. If the
production default file is absent, the daemon uses its validated built-in
defaults and explains how to select the repository's complete development
example with `--config examples/piqueld.toml`. The development example
keeps its state in `/tmp/piqueld-dev`, with its Unix API socket at
`/tmp/piqueld-dev-run/piqueld.sock`.

The daemon keeps persistent state in a private data directory: the embedded
database (`piqueld.db`) and future user data. Missing data-directory components
are created with mode `0700`; existing components are never chmodded.

The Unix API socket is separate, at `<runtime_dir>/piqueld.sock`. It is always
`0660`, owned by the daemon's user and effective group. Group membership grants
full deployment/operator access; authentication is not yet implemented.

The service manager or installer must create the runtime directory before
startup. Use daemon ownership and mode `0750` for group access, or `0700` for
private development. Group-readable/traversable runtime directories must use
the daemon's effective group. Group write and all access by others are rejected.
Both paths reject symlinks and ancestors vulnerable to replacement by untrusted
users. Both final directories must grant their owner read, write, and execute
access. Existing directory permissions are never changed.

For the development example, run `mkdir -p -m 0700 /tmp/piqueld-dev-run` first;
`just dev` handles this automatically. The production defaults are:

| Setting | Default |
| --- | --- |
| `server.data_dir` | `/var/lib/piqueld` |
| `server.runtime_dir` | `/run/piqueld` (must already exist) |
| `server.listen_mode` | `"off"` |
| `server.port` | `7845` |
| derived socket path | `<runtime_dir>/piqueld.sock` |
| derived database path | `<data_dir>/piqueld.db` |
| `docker.socket` | `/var/run/docker.sock` |
| `docker.auto_initialize_swarm` | `true` |
| `reconciliation.scan_interval_seconds` | `60` |
| `reconciliation.prepare_timeout_seconds` | `300` |
| `reconciliation.convergence_timeout_seconds` | `120` |
| `retention.event_days` | `30` (`0` disables event pruning, independently of operations) |
| `retention.finished_operation_days` | `10` (`0` disables pruning; terminal operations older than the cutoff are pruned during each reconciliation cycle) |

Reconciliation intervals and timeouts are bounded to `1..=86400` seconds.
One async controller overlaps pending work. Internal global limits allow two
image resolutions, eight observations, and one resource mutation request. Timers
consume no I/O slot. These limits are not configurable.

The data directory is the only persistent daemon state. The daemon holds
exclusive OS locks on both directories for its lifetime. Separate instances
require separate data and runtime directories. A competing process fails before
opening the database or replacing a socket. Process exit (including a crash)
releases the locks; there are no lock files to remove.

The runtime directory is dedicated to piqueld. Its lock coordinates cooperating
daemon instances, and its permissions prevent operator-group members from
replacing entries. Processes running as the daemon user or root must also honor
the lock; an unrelated process with that identity can otherwise replace the
socket during recovery. The lock is not an isolation boundary between processes
sharing the daemon's identity.

Under the runtime lock, startup probes an existing socket. An active listener is
left untouched; a connection-refused socket is removed and rebound. Unexpected
files, symlinks, timeouts, and other probe errors stop startup without replacing
the path. Both listeners are bound before reconciliation starts.

The CLI now defaults to `/run/piqueld/piqueld.sock`; it does not fall back to the
old state-directory socket. Existing custom installations must prepare a runtime
directory and update their configuration. CLI socket overrides and profiles
remain available for custom locations.

The dashboard is not configurable at runtime: it is embedded when the daemon
is built with the `embedded-ui` cargo feature and absent otherwise. It is
served on the TCP listener only, so a TCP listen mode must be enabled to reach
it; the Unix API socket serves the API alone.

`[build_history]` bounds persisted build output: `log_max_bytes` defaults to
4194304 (maximum 64 MiB), and `log_retention_days` to 30 (1–3650). Build metadata
remains until the application is deleted. Expiration removes output chunks while
retaining the attempt and an explicit expired indicator.

## TCP listen modes and Tailscale

`server.listen_mode` selects `off` (Unix socket only), `localhost` (127.0.0.1
and ::1), `tailscale` (the host's Tailscale IPv4 and IPv6 addresses), or `both`.
Every selected TCP address uses `server.port`, which must be 1–65535. The Unix
socket remains available in every mode. The old `server.http_listen` setting is
rejected; replace it with `listen_mode` and `port`.

For remote access, install and connect Tailscale on the daemon host and client:

```toml
[server]
listen_mode = "tailscale"
port = 7845
```

At startup, piqueld runs `tailscale status --json --peers=false` with a five-second
timeout. If the command is missing, fails, or reports Tailscale is not running
or has no addresses, piqueld logs a warning and continues with the remaining
listeners. It never retries discovery: restart piqueld after bringing Tailscale
up or changing its addresses. Any actual bind failure aborts startup, including
a failure on just one address. Direct binding requires Tailscale's normal network
interface; userspace-only networking is not supported.

The tailnet is trusted like localhost: anyone who can reach the API has full
operator access. Use tailnet policy to control who can connect. HTTP traffic
between tailnet nodes is encrypted by Tailscale; piqueld does not manage HTTPS,
Tailscale Serve, enrollment, or certificates. Set `listen_mode = "localhost"`
to remove the Tailscale listener, or `"off"` for Unix-socket-only access, and
restart the daemon. Independently configured proxies can still expose localhost.

The first secret write creates `secrets.key` in `server.data_dir`, atomically and
with mode 0600. It holds a 32-byte master key. Back up this key together with the
database; losing it makes encrypted values unrecoverable. Once the database has
accepted a secret, a missing key is never automatically regenerated, even after all secrets are
deleted. New writes authenticate a persistent key
verifier; replacing the key with a different valid 32-byte file fails closed.
On upgrade, all existing ciphertext is authenticated before creating the verifier.
Restore the original key, owned by the daemon user with private permissions.
Secret metadata remains readable without it.

Secret values use authenticated XChaCha20-Poly1305 encryption, binding ciphertext
to application, logical name and version. The implementation uses
[RustCrypto's existing AEAD implementation](https://docs.rs/chacha20poly1305/0.10.1/chacha20poly1305/).
Docker receives values only when provisioning a service's immutable secret file
versions. File mounts use Docker's read-only 0444 permissions inside the container.

### Replacing the storage encryption key

`piquelctl secrets replace-key --yes` generates a new key and re-encrypts every
available retained version, including historical deployment pins. Values, names,
generations and running services stay unchanged; no Deploy is needed. The old
key must authenticate every retained value. A missing/wrong key or damaged
ciphertext fails the operation without discarding values.

If the original key cannot be recovered, use
`piquelctl secrets replace-key --discard-values --yes`. This deliberately discards
**all stored values for every application on the daemon**. Names, generations,
file references and deployment history remain; discarded versions are marked
unavailable. Docker secrets and running services are left in place. Supply new
values under the existing names, then explicitly Deploy. Old deployment pins
never switch to replacement values, and attempts to provision unavailable values
fail before changing running services. This replaces the storage key, not the
passwords or API tokens themselves.

Replacement waits for active deployments to finish before changing their secret
storage. For long builds or rollouts, increase the CLI timeout (for example
`--timeout 10m`). Writes serialize with replacement. New ciphertext and a durable
staged key are committed before `secrets.key` is atomically replaced; restart or
the next value operation completes an interrupted installation. Never delete
`secrets.key.<id>.pending` files during recovery; the daemon removes unused staged
keys after recovery. If a committed staged key is also lost, metadata remains
accessible and explicit `--discard-values` recovery remains available.

After success, take a new consistent backup of the database and `secrets.key`.
Stop the daemon while copying these files, or use a backup mechanism that captures
them consistently. During interrupted replacement, preserve the entire data
directory, including pending key files. An old database backup still needs its
matching old key. Discarding values is not credential revocation or a guarantee
of secure erasure from existing backups or filesystem history.
