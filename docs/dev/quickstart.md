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

In a second terminal, open the first-account setup link and create an account
with a passkey, then sign in. The daemon runs as your user, so over its socket
the CLI also works without `login`, as the
[host operator](../user/accounts/recovery.md#the-host-operator); `piquelctl sign-in-link`
signs a browser in the same way, without a passkey. Over TCP, `login` is
required:

```console
just run --socket /tmp/piqueld-dev-run/piqueld.sock setup-link --open
just run --socket /tmp/piqueld-dev-run/piqueld.sock login
just run --socket /tmp/piqueld-dev-run/piqueld.sock status
just run --url http://localhost:7845 login
just run --url http://localhost:7845 status
just run --socket /tmp/piqueld-dev-run/piqueld.sock app plan \
  --file examples/basic-website.toml
just run --socket /tmp/piqueld-dev-run/piqueld.sock app apply \
  --file examples/basic-website.toml --deploy --yes
just run --socket /tmp/piqueld-dev-run/piqueld.sock app show basic-website
```

[`examples/basic-website.toml`](https://github.com/piquel-fr/piqueld/blob/main/examples/basic-website.toml) runs one
`nginx:alpine` replica with an HTTP health check.

`status` reports the daemon version and `--json` produces the same structured
result as the public API. `app apply` saves configuration; `app apply --deploy` also
creates a deployment and waits for its operation. Add `--no-wait` to return after
acceptance. Each explicit deployment prepares sources again and supersedes pending
work. Use the same development socket for repair, source refresh, and history:

```console
just run --socket /tmp/piqueld-dev-run/piqueld.sock app reconcile basic-website --yes
just run --socket /tmp/piqueld-dev-run/piqueld.sock app deploy basic-website --yes
just run --socket /tmp/piqueld-dev-run/piqueld.sock events --application <application-id>
```

The application got one environment, `production`, which shares its ID. Add
another with `piquelctl env create basic-website staging`; `app deploy`,
`app reconcile`, and `app logs` then need an environment, so use
`piquelctl env deploy basic-website staging`. A manifest can also
[declare its environments](../user/reference/manifest.md#declared-environments).
The [`piquelctl` reference](../user/reference/cli.md) covers field edits, secrets, previews, and
promotion.

## Dashboard and cleanup

The development toolchain serves the dashboard through the running
daemon, exactly like a deployment: run `just dev` instead of the commands
above. It runs an isolated instance for the worktree, with its own Docker
engine, on its own localhost port (see [development instances](development.md)). Give
the first embedded build a moment to compile the dashboard, open the URL that
`just dev wait` prints to inspect the overview, application list, and detail
routes, and use `just ctl` instead of `just run --socket ...` for `piquelctl`;
refresh after Rust or CSS changes. Any other daemon built with
`--features embedded-ui` ships its own dashboard bundle inside the binary, as
described in the [dashboard architecture](architecture/dashboard.md).

When finished, delete the application with its environments and note that
their named volumes, if any, are retained (with several environments, add
`--environments production,staging`):

```console
just run --socket /tmp/piqueld-dev-run/piqueld.sock app delete basic-website --yes
```

The retained named volumes are deliberate so deleting an application does not
silently destroy its data. Remove them separately only after confirming that
the data is no longer needed.
