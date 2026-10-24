# Deploying on push

Keep an application's manifest in its Git repository, next to its code.
piqueld then reads the manifest from there each time it deploys, and can
deploy every push to a branch.

## Before you start

- You have [an account](../getting-started/first-account.md), and `piquelctl`
  is signed in. Run the commands on this page on the host, or on any machine
  where `piquelctl` is signed in. On another machine, keep `PIQUELD_URL` set,
  as when you
  [signed it in](../getting-started/first-account.md#sign-piquelctl-in).
- The host can read the repository. For a private one, give the daemon's user
  [an SSH key](git-builds.md#build-from-a-private-repository).
- Creating the application needs `apps:create`. Turning on deploying on push
  needs both `apps:write` and `apps:deploy` on the application; see
  [permissions](../accounts/authorization.md#grants).
- To deploy on push with a GitHub webhook, GitHub must reach the daemon. It
  needs [HTTP ingress with a webhook hostname](../guides/ingress.md#push-webhooks)
  whose DNS is public, and you need to be able to change the GitHub
  repository's settings.

## Keep the manifest in the repository

1. Commit the application's manifest to its repository, for example as
   `infra/piqueld.toml`, and push it to `main`:

   ```toml
   api_version = "piqueld.dev/v1alpha1"
   kind = "Application"

   [metadata]
   name = "notes"

   [[spec.services]]
   name = "web"

   [spec.services.source]
   type = "git"
   repository = "self"

   [spec.services.source.build]
   type = "docker"
   dockerfile = "Dockerfile"

   [spec.services.healthcheck]
   type = "http"
   port = 80
   path = "/"
   ```

   With `repository = "self"`, `web` builds the repository's own
   `Dockerfile`, at the commit the manifest was read from. See
   [building from the manifest's own repository](git-builds.md#build-from-the-manifests-own-repository).

2. Save a bootstrap manifest as `notes.toml`. It only says where the manifest
   is:

   ```toml
   api_version = "piqueld.dev/v1alpha1"
   kind = "Application"

   [metadata]
   name = "notes"

   [spec.manifest]
   path = "infra/piqueld.toml"

   [spec.manifest.repository]
   url = "https://github.com/example/notes.git"
   branch = "main"
   ```

3. Create the application from it.

   > **Warning:** apply this file only to create `notes`. Applying it again
   > replaces the saved manifest with one that has no services, and turns
   > [deploy on push](#deploy-every-push) off. The next deployment of `main`
   > then removes every service. Change the connection with
   > `piquelctl app repository url`, `path` or `sync`, or on the **Source**
   > tab.

   ```console
   piquelctl app apply --file notes.toml
   ```

4. Deploy it:

   ```console
   piquelctl --timeout 10m app deploy notes
   ```

The deployment reads `infra/piqueld.toml` from the latest commit of `main`,
builds `web` from that commit, and runs it:

```console
$ piquelctl app show notes
notes (app-01a16d9202157fe18b0580467224f247)

Configuration revision: 2
Sync: off
Environment: production (app-01a16d9202157fe18b0580467224f247): ready, deploys from main

Service: web
  Replicas: 1
  Source: git self (manifest repository)
```

The application is now *repository-backed*: every deployment reads the file
again. The `metadata.name` in `infra/piqueld.toml` must stay `notes`.

The repository and the path belong to the application, and the branch to
each environment. To deploy another branch to a staging environment, see
[environments](environments.md#add-an-environment).

### Connect an existing application

Point an application you already run at a manifest in a repository:

```console
piquelctl app repository connect hello https://github.com/example/infra.git hello/piqueld.toml --branch main
```

Its next deployment reads the manifest from there. The `metadata.name` in
`hello/piqueld.toml` must be `hello`. `--branch` is `main` by default.

In the dashboard, open the application's **Source** tab, check **Load
configuration from Git on deploy**, fill in **Repository**, **Branch** and
**Manifest path**, then select **Save changes**.

To stop reading the manifest from Git, run
`piquelctl app repository disconnect hello`. The application keeps its current
manifest. Its services with `repository = "self"` build from the repository's
URL and branch instead.

## Edit a repository-backed application

You can still edit the application in the dashboard or with `piquelctl`. For
example, to run two replicas of `web`:

```console
piquelctl app service replicas notes web 2
```

`piquelctl app show` then lists how the application differs from the
repository:

```text
Edited since read from the repository (reset with `app repository reset`):
  ~ services.web.replicas   1 → 2
```

Deployments of `main` deploy your edits, until a push changes
`infra/piqueld.toml`. That deployment deploys the file, which replaces your
edits. Previews, and environments that follow other branches, always deploy
the file of their own branch.

To discard your edits and save the repository's manifest again:

```console
piquelctl app repository reset notes
```

Add `--deploy` to also deploy it. In the dashboard, **Reset to repository** is
on the notice that lists the edits, at the top of each of the application's
settings tabs, such as **Services** and **Variables**.

## Deploy every push

Deploying on push is off by default. Once you turn it on, every
[preview](previews.md) follows its branch, and so does every environment that
you opt in.

1. Choose how piqueld learns about pushes. To check the repository every two
   minutes:

   ```console
   piquelctl app repository sync notes poll --interval 120
   ```

   `--interval` is in seconds, between 60 and 86,400. Without it, piqueld
   checks every 300 seconds. Polling works with any Git server. With GitHub,
   a [webhook](#deploy-on-push-with-a-github-webhook) reports each push
   instead.

2. Opt the environment in:

   ```console
   $ piquelctl env sync notes production on
   Environment production (app-01a16d9202157fe18b0580467224f247), deploys from branch main, opted into sync.
   ```

   An environment the manifest
   [declares](environments.md#declare-environments-in-the-manifest) opts in
   with `sync = true` in its block instead.

In the dashboard, the application's **Source** tab sets **Deploy on push**:
choose **Poll: check the branches on an interval**, fill in **Poll interval
(seconds)**, and select **Save changes**. Then, on the environment's
**Overview**, select **Deploy on push** on its **Source** card. **Stop
deploying on push** opts it out again.

Once on, each environment deploys every new commit of its branch once. It
starts following after its first deployment. A push during a deployment waits
for it to finish, then the latest commit deploys. A deployment that fails is
not retried until the next push. When the daemon starts, it checks the
repository once, in case it missed pushes while it was stopped.

The setting is part of the application's connection to the repository, like
`path`. Set it with `piquelctl app repository sync`, or on the **Source** tab.
When you create an application from a full manifest, as in
[environments](environments.md#set-up-staging-and-production), it is `sync`
in `[spec.manifest]`, such as
`sync = { mode = "poll", interval_seconds = 120 }`. piqueld ignores the
`[spec.manifest]` of the file it reads from the repository.

## Deploy on push with a GitHub webhook

With a webhook, GitHub tells piqueld about each push, so pushes deploy within
seconds. piqueld then lists the repository's branches itself: a delivery
can't choose what deploys.

1. Switch the application to webhooks:

   ```console
   piquelctl app repository sync notes webhook
   ```

2. Opt the environment in, if you haven't yet:

   ```console
   piquelctl env sync notes production on
   ```

3. Generate the webhook's secret:

   ```console
   $ piquelctl app repository webhook rotate notes
   Payload URL: https://hooks.example.com/hooks/github/app-01a16d9202157fe18b0580467224f247
   Secret: 3f9c…
   Configure this secret in GitHub now; piqueld never shows it again.
   ```

   The command asks you to confirm first; in a script, add `--yes`. Keep the
   secret for the next step: piqueld can't show it again.

4. In GitHub, open the repository's **Settings**, then **Webhooks**, and
   select **Add webhook**. Set:

   - **Payload URL** to the URL from the previous step;
   - **Content type** to `application/json`;
   - **Secret** to the secret from the previous step;
   - the events to **Just the push event**.

   Then select **Add webhook**.

`piquelctl app repository webhook show notes` prints the same settings later,
without the secret. Without a payload URL, it says that the daemon must set
`ingress.webhook_hostname`.

In the dashboard, choose **Webhook: when GitHub reports a push** for **Deploy
on push** and select **Save changes**. A **GitHub webhook** card then lists
the settings for GitHub. **Generate secret** shows the secret once, with a
**Copy** button, until you select **Done** or leave the **Source** tab.

![The Source tab of notes: the repository manifest is read from https://github.com/example/notes.git, Deploy on push is set to Webhook, and the GitHub webhook card shows the payload URL, the content type application/json, the push event, and a secret generated 5 minutes ago](../../images/applications/repository-webhook.png)

### Rotate the secret

Generating a new secret replaces the old one at once: GitHub's deliveries fail
until you update the secret there too.

```console
piquelctl app repository webhook rotate notes
```

The command asks you to confirm every time; in a script, add `--yes`. In the
dashboard, it is **Rotate secret** on the **GitHub webhook** card.

## Check that it works

Push a commit to `main`, then check the environment:

```console
$ piquelctl env show notes production
production of notes (app-01a16d9202157fe18b0580467224f247)

Intent: ready
Runtime: ready (observed 2 s ago)
Source: main
Sync: following, last synced 44928efc5d3229a7b6a918182df49a32afc18a3e
Release: rel-01a16d9438b27093811074404283fef8
Configuration revision: 6
Resolved revision: 6
```

`following` means pushes deploy it, and `last synced` is the commit its last
deployment found on `main`. `piquelctl app show notes` also prints
`Last sync check: succeeded`, or the error of the last check.

Each deployment that follows a push records a `branch_synced` event, such as
`branch main moved to 44928ef…`, requested by `sync:poll` or `sync:webhook`.
They are on the application's **Events** tab, and in `piquelctl events`.

With a webhook, GitHub's **Recent Deliveries**, on the webhook's settings,
shows each delivery and piqueld's response: `204` when it was accepted.

## Troubleshooting

- **A push deploys nothing.** `piquelctl env show` says why on its `Sync`
  line: `off` when the application doesn't deploy on push,
  `off (not opted in)` when the environment isn't opted in,
  `off (pinned)` when it deploys a pinned commit, and
  `off (waiting for a deployment)` until its first deployment, or the first
  one after its branch or the repository's URL changed. A deployment that
  failed waits for the next push: deploy by hand to retry it.
- **`Last sync check` reports an error.** piqueld couldn't list the
  repository's branches. Git's error is in the daemon's log
  (`journalctl -u piqueld`). piqueld tries again after a minute, then waits
  longer each time, up to an hour. For a private repository, see
  [building from a private repository](git-builds.md#build-from-a-private-repository).
- **The deployment fails with `manifest_fetch_failed`.** piqueld couldn't
  read the repository. Git's error is in the daemon's log
  (`journalctl -u piqueld`). For a private repository, see
  [building from a private repository](git-builds.md#build-from-a-private-repository).
- **GitHub's deliveries get `401`.** The secret doesn't match, or the payload
  URL names another application. Generate a new secret, and copy it and the
  payload URL into GitHub again.
- **The deployment fails with `manifest_not_found`.** There is no file at the
  manifest's path on that commit. Correct the path with
  `piquelctl app repository path notes infra/piqueld.toml`.
- **A deployment warns about `manifest_connection_ignored`.** The file's own
  `[spec.manifest]` names another repository or path. piqueld ignores it and
  keeps the application's connection. Change the connection with
  `piquelctl app repository url` or `piquelctl app repository path`.
- **Your edits are gone.** A push changed the manifest file, and its
  deployment replaced them. Make the change in the repository instead.
- **The application's services disappeared.** A manifest without services,
  such as the bootstrap manifest, was applied again. Run
  `piquelctl app repository reset notes --deploy` to deploy the repository's
  manifest again, then [turn deploy on push on](#deploy-every-push) again if
  you used it.
- **`piquelctl app rename` fails with `repository_managed`.** The name must
  match the repository's manifest. Disconnect the application, rename it,
  and change `metadata.name` in the repository. Then connect it again, with
  the deploy-on-push mode it had:

  ```console
  piquelctl app repository connect <new-name> https://github.com/example/notes.git infra/piqueld.toml --sync poll
  ```

  Finally, reset each environment's branch with `piquelctl env branch`, and
  deploy each one.
