# Quickstart

This is the smallest complete development workflow for a local Docker Engine.
It assumes the Docker Unix socket is available to the current user and that
the engine can run a single-node Swarm.

## Build and start

```console
just build
just daemon --config config/piqueld.example.toml
```

The example keeps the socket and database under `/tmp/piqueld-dev`, so it does
not require root-owned `/run` or `/var/lib` directories. The daemon's production
default is `/etc/piqueld/config.toml`; use `--config` when running as a
non-root developer.

## Inspect and operate

In a second terminal:

```console
just run --socket /tmp/piqueld-dev/piqueld.sock status
just run --url http://127.0.0.1:7845 status
just run --socket /tmp/piqueld-dev/piqueld.sock plan \
  --file crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml
just run --socket /tmp/piqueld-dev/piqueld.sock apply \
  --file crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml --yes
just run --socket /tmp/piqueld-dev/piqueld.sock show notes
```

`status` reports the daemon version and `--json` produces the same structured
result as the public API. `apply` waits for the durable operation by default;
`--no-wait` returns immediately with its operation identifier.

## Dashboard and cleanup

The development toolchain serves the read-only dashboard through the running
daemon, exactly like a deployment: run `just dev` instead of the two commands
above, give Trunk a moment to produce the watched bundle, and open
`http://127.0.0.1:7845/dashboard/` in a browser to inspect the overview,
application list, and detail routes alongside `piquelctl`; refresh after Rust
or CSS changes. Serving the dashboard from any other daemon requires the UI
assets built by `just ui-build` and a configured `server.ui_dir`, as described
in [`web-ui.md`](web-ui.md).

When finished, delete the application and note that its named volumes are
retained:

```console
just run --socket /tmp/piqueld-dev/piqueld.sock delete notes --yes
```

The retained named volumes are deliberate so deleting an application does not
silently destroy its data. Remove them separately only after confirming that
the data is no longer needed.
