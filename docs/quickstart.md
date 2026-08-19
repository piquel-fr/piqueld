# Quickstart

This is the smallest complete development workflow for a local Docker Engine.
It assumes the Docker Unix socket is available to the current user and that
the engine can run a single-node Swarm.

## Build and start

```console
nix build .#default
./result/bin/piqueld --config ./result/share/piqueld/piqueld.example.toml
```

The example keeps the socket and database under a user-owned
`/run/user/1000/piqueld` data directory, so it does not require root-owned
`/run` or `/var/lib` paths; adjust `1000` to your UID. The daemon's production
default is `/etc/piqueld/config.toml`; use `--config` when running as a
non-root developer.

## Inspect and operate

In a second terminal:

```console
cargo run --package piquelctl -- --socket /run/user/1000/piqueld/piqueld.sock status
cargo run --package piquelctl -- --url http://127.0.0.1:7845 status
cargo run --package piquelctl -- --socket /run/user/1000/piqueld/piqueld.sock plan \
  --file crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml
cargo run --package piquelctl -- --socket /run/user/1000/piqueld/piqueld.sock apply \
  --file crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml --yes
cargo run --package piquelctl -- --socket /run/user/1000/piqueld/piqueld.sock show notes
```

`status` reports the daemon version and `--json` produces the same structured
result as the public API. `apply` waits for the durable operation by default;
`--no-wait` returns immediately with its operation identifier.

## Dashboard and cleanup

On this branch the daemon serves only the HTTP API; the read-only dashboard
arrives with the Plan 06C package, which will serve it at
`http://127.0.0.1:7845/` for inspecting the same state as `piquelctl`.

When finished, delete the application and note that its named volumes are
retained:

```console
cargo run --package piquelctl -- --socket /run/user/1000/piqueld/piqueld.sock delete notes --yes
```

The retained named volumes are deliberate so deleting an application does not
silently destroy its data. Remove them separately only after confirming that
the data is no longer needed.
