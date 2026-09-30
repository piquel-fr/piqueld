# Quickstart

This is the smallest complete development workflow for a local Docker Engine.
It assumes the Docker Unix socket is available to the current user and that
the engine can run a single-node Swarm.

## Build and start

```console
just build
mkdir -p -m 0700 /tmp/piqueld-dev-run
just daemon-embedded --config examples/piqueld.toml
```

The example keeps its state in `/tmp/piqueld-dev` and its Unix API socket at
`/tmp/piqueld-dev-run/piqueld.sock`. The daemon creates the data directory with
mode `0700`; an existing directory must be private and owned by your user.
The daemon's production default is
`/etc/piqueld/config.toml`; use `--config` when running as a non-root
developer.

## Inspect and operate

Open the link in `/tmp/piqueld-dev/setup-link` in your browser and create an
account with a passkey. Then, in a second terminal:

```console
just run --socket /tmp/piqueld-dev-run/piqueld.sock login
just run --socket /tmp/piqueld-dev-run/piqueld.sock status
just run --url http://localhost:7845 login
just run --url http://localhost:7845 status
just run --socket /tmp/piqueld-dev-run/piqueld.sock app plan \
  --file crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml
just run --socket /tmp/piqueld-dev-run/piqueld.sock app apply \
  --file crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml --deploy --yes
just run --socket /tmp/piqueld-dev-run/piqueld.sock app show notes
```

`status` reports the daemon version and `--json` produces the same structured
result as the public API. `app apply` saves configuration; `app apply --deploy` also
creates a deployment and waits for its operation. Add `--no-wait` to return after
acceptance. Each explicit deployment prepares sources again and supersedes pending
work. Use the same development socket for repair, source refresh, and history:

```console
just run --socket /tmp/piqueld-dev-run/piqueld.sock app reconcile notes --yes
just run --socket /tmp/piqueld-dev-run/piqueld.sock app deploy notes --yes
just run --socket /tmp/piqueld-dev-run/piqueld.sock events --application <application-id>
```

## Dashboard and cleanup

The development toolchain serves the dashboard through the running
daemon, exactly like a deployment: run `just dev` instead of the commands
above (`just dev` also prepares the runtime directory), give the first embedded build a moment to run Tailwind and Trunk, and
open `http://localhost:7845/dashboard/` in a browser to inspect the overview,
application list, and detail routes alongside `piquelctl`; refresh after Rust
or CSS changes. Any other daemon built with `--features embedded-ui` ships its
own dashboard bundle inside the binary, as described in
[`web-ui.md`](web-ui.md).

When finished, delete the application and note that its named volumes are
retained:

```console
just run --socket /tmp/piqueld-dev-run/piqueld.sock app delete notes --yes
```

The retained named volumes are deliberate so deleting an application does not
silently destroy its data. Remove them separately only after confirming that
the data is no longer needed.
