---
name: piqueld-e2e
description: >
  Exercise a piqueld change for real against this worktree's development instance,
  through all three surfaces: the manifest, `piquelctl`, and the dashboard in
  the T3 Code browser preview. Use after implementing a feature or fix that
  changes daemon, CLI, or dashboard behavior, when asked to "test it", "try it",
  "check it works", or to take screenshots for a pull request.
---

# Test a change end to end

Automated tests prove the contracts; this proves the feature works in the real
daemon, against real Docker, in a real browser. Every feature lives on three
surfaces (see `AGENTS.md`): check each one the change touches.

## 0. Prerequisites

This worktree's development instance must be running: use the `piqueld-dev`
skill. After each edit, run `just dev wait` before testing again.

The instance and its Docker engine belong to this worktree, but the user may
test with it too. Prefix every application you create with `agent-<topic>-`
(for example `agent-redirects-web`) and leave the user's applications alone.

## 1. CLI

`just ctl` builds and runs this worktree's `piquelctl` against the instance's
socket:

```sh
just ctl status
```

Credentials are saved per endpoint in `~/.config/piqueld/credentials.json`, so
each instance needs one login. On `401`/"not logged in", start a device login
in the background and keep its output:

```sh
setsid nohup just ctl login > target/piquelctl-login.log 2>&1 < /dev/null &
```

Give the user the printed URL and code and wait for them to approve it (open
the URL in the preview for them). Never print, copy, or commit tokens or
`credentials.json`. If the user hands you an automation token, use it only via
`PIQUELD_TOKEN`.

Prefer `--json` for assertions: stdout stays pure JSON, diagnostics go to
stderr. Useful commands: `app list|show|logs|plan|apply|deploy|reconcile|delete`,
`app service|volume|repository` (field edits, `--deploy` to deploy with the
change), `operation <id>`, `events --application <id>`, `builds list|logs`.
Mutations ask for confirmation: pass `--yes`. See `docs/piquelctl.md`.

## 2. Manifest

Write test manifests under `target/e2e/` (gitignored), starting from
`examples/basic-website.toml` or `crates/piqueld-core/tests/fixtures/manifests/`.
Validate offline first, then apply and deploy:

```sh
just ctl app validate --file target/e2e/app.toml
just ctl app plan --file target/e2e/app.toml
just ctl app apply --file target/e2e/app.toml --deploy --yes
just ctl app show agent-topic-web --json
```

Check failure paths too: an invalid manifest should name the field that failed.

## 3. Docker

Confirm what the daemon actually created in the instance's own engine. Point
the Docker CLI at it (the socket is in `just dev status`); without
`DOCKER_HOST`, you would be looking at the host engine and its production
piqueld:

```sh
export DOCKER_HOST=unix:///tmp/piqueld-dev/<instance>/docker/docker.sock
docker service ls --filter label=io.piqueld.application=<id>
docker service ps --no-trunc <service>
docker service inspect --format '{{json .Spec}}' <service>
docker network ls --filter label=io.piqueld.application=<id>
```

Daemon-side errors and reconciliation traces are in the instance's
`daemon.log` (JSON lines; path in `just dev status`).

## 4. Dashboard

Open the URL printed by `just dev wait` (passkeys are bound to that exact
origin):

1. `preview_open` with `url: "http://localhost:<port>/dashboard/"`.
2. `preview_snapshot` to read the page. If it shows the sign-in screen, ask the
   user to sign in with their passkey in the preview, then continue. Agents
   cannot complete passkey ceremonies. Sessions last 24 hours idle, 7 days total.
3. Drive it with role locators (`role=button[name='Deploy']`),
   `preview_type`, and `preview_wait_for` on text or URLs, never fixed sleeps.
4. After every action that matters, `preview_snapshot` and check the
   diagnostics section for console errors and failed requests.
5. After a rebuild, `just dev wait`, then `preview_navigate` to the same URL to
   load the new bundle.

Cover what the user would do: create or edit the setting, see validation
errors, save, deploy, and see the result in history and service views. Check
that unsaved edits survive navigation guards when the change touches editors.

For the pull request, save the key states with `preview_snapshot` and
`save: true` and embed the returned paths in your reply. For a multi-step flow,
wrap it in `preview_recording_start` / `preview_recording_stop`.

## 5. Clean up and report

```sh
just ctl app delete agent-topic-web --yes
```

Volumes are retained on delete by design. Remove the volumes your test created
(`docker volume ls --filter label=io.piqueld.application=<id>`, with the
instance's `DOCKER_HOST`).

Report what you exercised on each surface, what you could not (for example a
flow that needed the user's passkey), and any errors seen in the daemon log or
browser diagnostics, with screenshots.
