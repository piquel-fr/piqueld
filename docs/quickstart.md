# Quickstart

This is the smallest complete development workflow for a local Docker Engine.
It assumes the Docker Unix socket is available to the current user and that
the engine can run a single-node Swarm.

## Build and start

```console
just build
just daemon --config examples/piqueld.toml
```

The example keeps its state in `/tmp/piqueld-dev` and its Unix API socket at
`/tmp/piqueld-dev/piqueld.sock`. The daemon creates the data directory with
mode `0700`; an existing directory must be private and owned by your user.
The daemon's production default is
`/etc/piqueld/config.toml`; use `--config` when running as a non-root
developer.

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
`--no-wait` returns immediately with its operation identifier. Apply the same
manifest to update an existing application by name. Identical manifests schedule no new work, but still wait for their existing
operation by default; failed attempts require explicit retry. Use the same development socket for repair, image refresh, and informational history:

```console
just run --socket /tmp/piqueld-dev/piqueld.sock reconcile notes --yes
just run --socket /tmp/piqueld-dev/piqueld.sock deploy notes --yes
just run --socket /tmp/piqueld-dev/piqueld.sock events --application <application-id>
```

## Dashboard and cleanup

The development toolchain serves the dashboard through the running
daemon, exactly like a deployment: run `just dev` instead of the two commands
above, give the first embedded build a moment to run Tailwind and Trunk, and
open `http://127.0.0.1:7845/dashboard/` in a browser to inspect the overview,
application list, and detail routes alongside `piquelctl`; refresh after Rust
or CSS changes. Any other daemon built with `--features embedded-ui` ships its
own dashboard bundle inside the binary, as described in
[`web-ui.md`](web-ui.md).

When finished, delete the application and note that its named volumes are
retained:

```console
just run --socket /tmp/piqueld-dev/piqueld.sock delete notes --yes
```

The retained named volumes are deliberate so deleting an application does not
silently destroy its data. Remove them separately only after confirming that
the data is no longer needed.
