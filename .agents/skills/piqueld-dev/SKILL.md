---
name: piqueld-dev
description: >
  Start, wait for, inspect, or stop this worktree's isolated piqueld
  development instance (`just dev`), and find its localhost URL for the T3
  Code browser preview. Use before manually testing any daemon, CLI, or
  dashboard change, when asked to "run", "start", or "launch" piqueld, or after
  an edit to wait for the rebuilt daemon.
---

# Run the development instance

Every worktree has its own instance: configuration (`piqueld.local.toml`),
data, Unix socket, localhost port, and Docker-in-Docker engine, and serves on
a port of the shared tailnet node once it is logged in. It never
touches the host's Docker Engine, where a production piqueld runs, or another
worktree's instance. `docs/development.md` describes it in full.

## Start and wait

```sh
just dev start
```

It starts the instance in the background (unless the user already runs
`just dev` in this worktree), waits, and prints `ready: <url>/dashboard/`. The
first build compiles the daemon and the WASM dashboard and can take several
minutes: run it as a background command rather than polling. If it prints:

- Compiler output and `the build failed`: fix it; the watcher rebuilds on save.
- `the daemon exited`: a startup error, shown above the message.

## Iterate

Every save under `apps/piqueld`, `apps/piqueld-ui`, `crates`, or the Cargo
manifests rebuilds and restarts the daemon. Run `just dev wait` after editing
and before testing: it treats files saved after the current build started as
unbuilt, so it never returns the previous build. Then reload the preview with
`preview_navigate`. Data survives restarts; pending passkey ceremonies and CLI
device logins do not.

## Use it

- **Browser:** open the printed URL in the preview. It is `public_url`, the
  only origin where passkeys and sign-in work: either
  `https://piqueld-dev.<tailnet>.ts.net:<port>` when the instance serves on the
  shared tailnet node, which the user can also open from their own devices,
  or `http://localhost:<port>` (never `127.0.0.1`).
- **CLI:** `just ctl <args>` runs this worktree's `piquelctl` against the
  instance's socket.
- **Signing in:** `just ctl` needs no login: it acts as the host operator,
  with full access, unless `PIQUELD_TOKEN` or a saved login selects an
  account. For the browser, run `just ctl sign-in-link` and open the
  printed link in the preview; it works once within 10 minutes, and the
  session lasts 12 hours, across restarts. Only test account features
  (passkeys, tokens, invitations, enrollment links, CLI login approval) need
  an account: open the link from `just ctl setup-link` and ask the user to
  register a passkey; agents cannot create passkeys.
- **State and logs:** `just dev status` prints the instance's files.
  `output.log` has the latest build and the daemon's fatal errors;
  `daemon.log` has its logs as JSON lines.
- **Tailnet:** `just dev start` starts the shared node itself. Only if it
  reports the node is not logged in, ask the user to run
  `just dev tailnet up`, which needs their approval; report other startup
  errors as they are.
- **Docker:** prefix commands with
  `DOCKER_HOST=unix://<docker socket from status>`.

## Stop

`just dev stop` stops the daemon and watcher; the Docker engine keeps running
so images stay cached. Leave the instance running at the end of a task if the
user is likely to keep testing, and say so in your summary: T3 Code stops it
when the thread settles, and it stops itself if the worktree is removed.
`just dev clean` deletes the instance's data, engine, and images: only run it
when the user asks.
