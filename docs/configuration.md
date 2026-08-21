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
example with `--config config/piqueld.example.toml`.

The development example keeps its state under a user-owned runtime directory
such as `/run/user/<uid>/piqueld-dev`; do not use a shared fixed path under
`/tmp`. The daemon creates missing socket parent directories with mode `0700`
and refuses to start when an existing parent is a symlink or grants any access
to group or other users, so only the owning user can reach the Unix socket.
The production defaults are:

| Setting | Default |
| --- | --- |
| `server.unix_socket` | `/run/piqueld/piqueld.sock` |
| `server.http_listen` | `127.0.0.1:7845` |
| `database.path` | `/var/lib/piqueld/piqueld.db` |
| `docker.socket` | `/var/run/docker.sock` |
| `docker.auto_initialize_swarm` | `true` |
| `reconciliation.scan_interval_seconds` | `60` |
| `reconciliation.max_parallel_operations` | `4` |

Only the configured `database.path` is persistent daemon state. Its missing
parent directories are created safely; existing parents are not chmodded, and
symlinked path components are refused.
