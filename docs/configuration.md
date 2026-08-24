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
example with `--config config/piqueld.example.toml`. The development example
keeps its state under a user-owned runtime directory such as
`/run/user/<uid>/piqueld`; do not use a shared fixed path under `/tmp`.

The daemon keeps all state in one private data directory: the Unix API socket
(`piqueld.sock`), the embedded database (`piqueld.db`), and future user data.
Missing data-directory components are created with mode `0700`; existing
components are never chmodded, symlinked components anywhere in the path are
refused, and the final directory must grant no access to group or other users,
so only the owning user can reach the socket and the database. The production
defaults are:

| Setting | Default |
| --- | --- |
| `server.data_dir` | `/var/lib/piqueld` |
| `server.http_listen` | `127.0.0.1:7845` (omit to disable TCP) |
| derived socket path | `<data_dir>/piqueld.sock` |
| derived database path | `<data_dir>/piqueld.db` |
| `docker.socket` | `/var/run/docker.sock` |
| `docker.auto_initialize_swarm` | `true` |
| `reconciliation.scan_interval_seconds` | `60` |
| `reconciliation.max_parallel_operations` | `4` |
| `reconciliation.prepare_timeout_seconds` | `300` |
| `reconciliation.convergence_timeout_seconds` | `120` |
| `retention.finished_operation_days` | `10` (`0` disables pruning; terminal operations older than the cutoff are pruned during each reconciliation cycle) |

The data directory is the only persistent daemon state.

The dashboard is not configurable at runtime: it is embedded when the daemon
is built with the `embedded-ui` cargo feature and absent otherwise.
