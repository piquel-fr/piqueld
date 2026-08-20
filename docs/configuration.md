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

The development example uses `/tmp/piqueld-dev` for the Unix socket and SQLite
database, while the production defaults are:

| Setting | Default |
| --- | --- |
| `server.unix_socket` | `/run/piqueld/piqueld.sock` |
| `server.http_listen` | `127.0.0.1:7845` |
| `server.ui_dir` | unset: `/usr/share/piqueld/ui`, or the packaged asset directory |
| `database.path` | `/var/lib/piqueld/piqueld.db` |
| `docker.socket` | `/var/run/docker.sock` |
| `docker.auto_initialize_swarm` | `true` |
| `reconciliation.scan_interval_seconds` | `60` |
| `reconciliation.max_parallel_operations` | `4` |

`server.ui_dir` is optional. An explicit absolute path always wins. When it is
omitted, the Nix package's small wrapper supplies its own installed asset
directory through `PIQUELD_UI_DIR`; an unpackaged binary uses
`/usr/share/piqueld/ui`.

Only the configured `database.path` is persistent daemon state. Its missing
parent directories are created safely; existing parents are not chmodded, and
symlinked path components are refused.
