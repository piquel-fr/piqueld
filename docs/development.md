# Development instances

`just dev` runs an isolated piqueld for the current worktree: its own
configuration, data, Unix socket, localhost port, and Docker engine. Any
number of worktrees can run one at once, and none of them touches the host's
Docker Engine or another instance's state.

## Start

```console
just dev
```

This watches the daemon, dashboard, and crate sources (hidden files aside),
rebuilds the daemon with the embedded dashboard, and restarts it on every save. Stopping it gives the
daemon its graceful shutdown period before terminating any remaining processes.
`just dev start` does the same in the background and returns once the instance
is ready, printing its dashboard URL.

On first use, `just dev` writes the gitignored `piqueld.local.toml`.
`just dev config` writes it ahead of time (T3 Code's worktree setup script
does), and `just dev config --force` regenerates it:

| Setting | Value |
| --- | --- |
| `server.data_dir` | `~/.local/state/piqueld-dev/<instance>`, outside `/tmp`, which systemd ages out |
| `server.runtime_dir` | `/tmp/piqueld-dev/<instance>`, holding the sockets and logs |
| `server.listen_mode`, `server.port` | `localhost` on the lowest free port from 7846 that no other worktree uses |
| `[tailscale]` | while the [tailnet node](#tailnet) is logged in, `https://piqueld-dev.<tailnet>.ts.net:<port>` |
| `auth.public_url` | the tailnet URL, or else `http://localhost:<port>`: the only origin where passkeys work |
| `docker.socket` | the instance's own engine (below) |

The instance is named after the worktree directory, or `main` for the main
checkout, followed by a hash of its path, so separate clones never share an
instance. Edit the file freely: `just dev` reads the directories, port, and
Docker socket from it.

## Browser

Open `<public_url>/dashboard/`, as printed by `just dev start` and
`just dev status`. Passkeys and browser sign-in only work on `public_url`.

On the tailnet, that is `https://piqueld-dev.<tailnet>.ts.net:<port>`, which
any device on the tailnet reaches, including T3 Code's browser preview.
Otherwise it is `http://localhost:<port>`: the preview runs on the machine
running the instance, so it reaches the port directly; elsewhere, forward the
port, for example with `ssh -L`. Use `localhost`, not `127.0.0.1`: browsers
allow passkeys on `localhost` over HTTP.

The daemon runs as your user, so `just ctl` acts as the
[host operator](authentication.md#the-host-operator) without signing in
(unless `PIQUELD_TOKEN` or a saved login selects an account), and
`just ctl sign-in-link` prints a one-time link that signs the browser in the
same way, even on a new instance. The link works once within 10 minutes; the
session lasts 12 hours and survives restarts. Only features that need an
account, such as passkeys, tokens, and invitations, need one: open the setup
link from `just ctl setup-link` and register its passkey.

## Tailnet

Instances serve over HTTPS on the tailnet through one shared node,
`piqueld-dev.<tailnet>.ts.net`, each on its own port, so any number of them
need a single node and certificate. Log the node in once:

```console
just dev tailnet up
```

This starts a `tailscaled` of its own, separate from the host's, in userspace
networking, and runs `tailscale up --hostname=piqueld-dev` against it; extra
arguments, such as `--auth-key=file:<path>` or `--advertise-tags=tag:dev`, are
passed on. Its state is in `~/.local/state/piqueld-dev/tailnet` and its socket
and log in `/tmp/piqueld-dev/tailnet`. The tailnet needs MagicDNS and HTTPS
certificates, and `tailscale` and `tailscaled` must be on `PATH`. Use tailnet
policy to limit who reaches the node.

From then on, `just dev config` sets `[tailscale]` in new configurations, and
`just dev config --force` moves an existing one to the node. The daemon only
adds its port to the node's Serve configuration (see
[sharing a tailscaled](configuration.md#sharing-a-tailscaled)), so the
dashboard, the API through `piquelctl --url`, and tailnet identities work as
in production. Session cookies are named after the port, so instances never
sign each other out, but passkeys are bound to the node's name: a browser
offers every instance's passkeys, and only the instance's own work.

The node runs in the background and outlives the instances. `just dev tailnet
stop` stops it, and the next instance that uses it, such as after a reboot,
starts it again.

## Docker

Each instance runs a private Docker-in-Docker engine in the privileged
container `piqueld-dev-<instance>`, listening on
`<runtime_dir>/docker/docker.sock`. Its images, Swarm, and services persist in
a volume of the same name across restarts. Inspect it with:

```console
DOCKER_HOST=unix:///tmp/piqueld-dev/<instance>/docker/docker.sock docker service ls
```

Set `docker.socket = "/var/run/docker.sock"` to use the host engine instead;
`just dev` then starts no engine.

## Commands

| Command | Effect |
| --- | --- |
| `just dev` | Watch, rebuild, and restart in the foreground |
| `just dev start [SECS]` | Start in the background and wait until ready |
| `just dev wait [SECS]` | Wait until the current sources are built and serving, and print the URL (default 900 s) |
| `just dev status` | Print the state, URL, socket, and log files |
| `just dev stop` | Stop the daemon and watcher; the engine keeps running |
| `just dev clean` | Stop, then delete the engine, its volume, and the instance's state |
| `just dev prune` | Stop and clean the instances of worktrees that no longer exist |
| `just dev tailnet up [ARGS]` | Start the shared tailnet node and log it in |
| `just dev tailnet stop` | Stop the shared tailnet node |
| `just ctl ARGS` | Run this worktree's `piquelctl` against the instance |

T3 Code runs `just dev config` when it creates a worktree and `just dev stop`
when its thread settles (see `t3.json`). An instance also stops itself once its
worktree is removed; `just dev prune` then deletes its engine, volume, and
directories.

`just dev wait` considers sources saved after the current build started as not
yet built, so it is safe to call right after an edit. It fails with the
compiler's output when the build fails and with the daemon's error when it
exits.

## Logs

The runtime directory keeps:

- `output.log`: the latest build's output, followed by the daemon's errors.
- `daemon.log`: the daemon's logs as JSON lines, from `piqueld --log-file`, e.g.
  `grep '"level":"ERROR"'`. It keeps every run; `just dev clean` removes it.
- `dev.log`: the terminal output of an instance started with `just dev start`.

The tailnet node logs to `/tmp/piqueld-dev/tailnet/tailscaled.log`.

The terminal keeps the daemon's usual human-readable output.

## Implementation

`just dev`, `just docker-test`, `just test-playwright`, and `just boundary` run
Rust tasks from `tools/xtask`: `cargo xtask --help` lists them. They talk to
Docker through its API, so they need no Docker CLI beyond resolving the current
context.
