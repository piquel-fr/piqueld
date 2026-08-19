# Quickstart

This is the smallest complete development workflow for a local Docker Engine.
It assumes the Docker Unix socket is available to the current user and that
the engine can run a single-node Swarm.

## Build and start

```console
nix build .#default
./result/bin/piqueld --config ./result/share/piqueld/piqueld.example.toml
```

The example keeps the socket and database under `/tmp/piqueld-dev`, so it does
not require root-owned `/run` or `/var/lib` directories. The daemon's production
default is `/etc/piqueld/config.toml`; use `--config` when running as a
non-root developer.

## Inspect and operate

In a second terminal:

```console
./result/bin/piquelctl --socket /tmp/piqueld-dev/piqueld.sock status
./result/bin/piquelctl --socket /tmp/piqueld-dev/piqueld.sock plan \
  --file crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml
./result/bin/piquelctl --socket /tmp/piqueld-dev/piqueld.sock apply \
  --file crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml --yes
./result/bin/piquelctl --socket /tmp/piqueld-dev/piqueld.sock show notes
```

`status` reports the daemon version and `--json` produces the same structured
result as the public API. `apply` waits for the durable operation by default;
`--no-wait` returns immediately with its operation identifier.

The Unix socket is the local administrative path. Loopback HTTP is
authenticated in the production stack; when a protected bearer credential is
configured, pass it through a private file:

```console
./result/bin/piquelctl --url http://127.0.0.1:7845 \
  --token-file ./protected-bearer-token status
```

## Dashboard and cleanup

The dashboard is served by the daemon in the package. Open
`http://127.0.0.1:7845/` in a browser after configuring the daemon's bearer
credential or trusted loopback proxy; it uses the same authenticated API as
`piquelctl`. The Plan 06C dashboard itself is read-only.

When finished, delete the application and note that its named volumes are
retained:

```console
./result/bin/piquelctl --socket /tmp/piqueld-dev/piqueld.sock delete notes --yes
```

The retained named volumes are deliberate so deleting an application does not
silently destroy its data. Remove them separately only after confirming that
the data is no longer needed.
