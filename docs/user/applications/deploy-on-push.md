# Deploying on push

Keep an application's manifest in its Git repository, next to its code.
piqueld then reads the manifest from there each time it deploys, and can
deploy every push to a branch.

## Before you start

- You have [an account](../getting-started/first-account.md), and `piquelctl`
  is signed in. The commands on this page run on your machine.
- The host can read the repository. For a private one, give the daemon's user
  [an SSH key](git-builds.md#build-from-a-private-repository).
- Turning on deploying on push needs both `apps:write` and `apps:deploy` on
  the application; see [permissions](../accounts/authorization.md#grants).
- To deploy on push with a GitHub webhook, the daemon needs a webhook
  hostname, `ingress.webhook_hostname` in its
  [configuration](../reference/configuration.md#managed-application-ingress),
  and you need to be able to change the GitHub repository's settings.

## Keep the manifest in the repository

1. Commit the application's manifest to its repository, for example as
   `infra/piqueld.toml`:

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

2. On your machine, save a bootstrap manifest as `notes.toml`. It only says
   where the manifest is:

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

3. Create the application from it:

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
notes (app-01a16b4d52b6747382371486193802d5)

Configuration revision: 2
Sync: off
Environment: production (app-01a16b4d52b6747382371486193802d5): ready, deploys from main

Service: web
  Replicas: 1
  Source: git self (manifest repository)
```

The application is now *repository-backed*: every deployment reads the file
again. Its `metadata.name` must stay the application's name. Apply the bootstrap manifest only once, to create the
application: applying it again saves a manifest without services, and the
next deployment would remove them.

The repository and the path belong to the application, and the branch to
each environment. To deploy another branch to a staging environment, see
[environments](environments.md#add-an-environment).

### Connect an existing application

Point an application you already run at a manifest in a repository:

```console
piquelctl app repository connect hello https://github.com/example/infra.git hello/piqueld.toml --branch main
```

Its next deployment reads the manifest from there. `--branch` is `main` by
default.

In the dashboard, open the application's **Source** tab, check **Load
configuration from Git on deploy**, fill in **Repository**, **Branch** and
**Manifest path**, then select **Save changes**.

To stop reading the manifest from Git, run
`piquelctl app repository disconnect hello`. The application keeps its current
manifest.

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
on the notice that lists the edits, above the application's settings tabs,
such as **Services** and **Variables**.

## Deploy every push

Deploying on push is off by default. Once you turn it on, every
[preview](previews.md) follows its branch, and so does every environment that
you opt in.

1. Choose how piqueld learns about pushes. To check the repository every two
   minutes:

   ```console
   piquelctl app repository sync notes poll --interval 120
   ```

   piqueld then lists the repository's branches every `--interval` seconds,
   from 60 to 86,400, 300 by default. This works with any Git server. With
   GitHub, a [webhook](#deploy-on-push-with-a-github-webhook) reports each
   push instead.

2. Opt the environment in:

   ```console
   $ piquelctl env sync notes production on
   Environment production (app-01a16b4d52b6747382371486193802d5), deploys from branch main, opted into sync.
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
not retried until the next push.

The setting is part of the application's connection to the repository, like
`path`. In a manifest you apply, it is `sync` in `[spec.manifest]`, such as
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

2. Generate the webhook's secret:

   ```console
   $ piquelctl app repository webhook rotate notes
   Payload URL: https://hooks.example.com/hooks/github/app-01a16b4fa9437d239e8fcb831cc48256
   Secret: 3f9c…
   Configure this secret in GitHub now; piqueld never shows it again.
   ```

   Keep the secret for the next step: piqueld can't show it again.

3. In GitHub, open the repository's **Settings**, then **Webhooks**, and
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
**Copy** button, until you select **Done**.

![The Source tab of notes: the repository manifest is read from https://github.com/example/notes.git, Deploy on push is set to Webhook, and the GitHub webhook card shows the payload URL, the content type application/json, the push event, and a secret generated 5 minutes ago](../../images/applications/repository-webhook.png)

When the daemon starts, it checks the repository once, in case it missed
pushes while it was stopped.

### Rotate the secret

Generating a new secret replaces the old one at once: GitHub's deliveries fail
until you update the secret there too.

```console
piquelctl app repository webhook rotate notes
```

In the dashboard, it is **Rotate secret** on the **GitHub webhook** card.

## Check that it works

Push a commit to `main`, then check the environment:

```console
$ piquelctl env show notes production
production of notes (app-01a16b4d52b6747382371486193802d5)

Intent: ready
Runtime: ready (observed 14 s ago)
Source: main
Sync: following, last synced c3a28a2cf3c44030c5e5dea8439b519c6d8ae334
```

`following` means pushes deploy it, and `last synced` is the commit its last
deployment found on `main`. `piquelctl app show notes` also prints
`Last sync check: succeeded`, or the error of the last check.

Each deployment that follows a push records a `branch_synced` event, such as
`branch main moved to c3a28a2…`, requested by `sync:poll` or `sync:webhook`.
They are on the application's **Events** tab, and in `piquelctl events`.

With a webhook, GitHub's **Recent Deliveries**, on the webhook's settings,
shows each delivery and piqueld's response: `204` when it was accepted.

## Troubleshooting

- **A push deploys nothing.** `piquelctl env show` says why on its `Sync`
  line: `off (not opted in)` when the environment isn't opted in,
  `off (pinned)` when it deploys a pinned commit, and
  `off (waiting for a deployment)` until its first deployment, or the first
  one after its branch or the repository's URL changed. A deployment that
  failed waits for the next push: deploy by hand to retry it.
- **`Last sync check` reports an error.** piqueld couldn't list the
  repository's branches. It tries again after a minute, then waits longer
  each time, up to an hour.
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
  manifest again.
- **`piquelctl app rename` fails with `repository_managed`.** The name must
  match the repository's manifest. Disconnect the application, rename it,
  change `metadata.name` in the repository, then connect it again.
