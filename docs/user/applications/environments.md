# Environments and promotion

An application can run several environments, such as `staging` and
`production`. Each has its own deployments, secrets, volumes and routes. This
page adds environments, gives each its own settings, and promotes what staging
runs to production without building it again.

## Before you start

- You have an application, such as the one from
  [deploying your first application](../getting-started/first-app.md), and
  `piquelctl` is signed in. The commands on this page run on your machine.
- For an environment to follow its own branch, or to deploy on push, the
  application reads its manifest from Git. See
  [deploying on push](deploy-on-push.md).
- Adding environments needs `apps:write` on the application, and deploying or
  promoting them `apps:deploy`; see
  [permissions](../accounts/authorization.md#grants).

## Add an environment

1. Add a `staging` environment to `hello`:

   ```console
   $ piquelctl env create hello staging
   Environment staging (env-01a16b4e87c97b93982e0d2c1754ee93), deploys from the saved manifest.
   ```

2. Deploy it:

   ```console
   piquelctl env deploy hello staging
   ```

Both environments now run the application's manifest:

```console
$ piquelctl env list hello
NAME  STATE  SOURCE  RESOLVED  ID
production  ready  saved  2  app-01a16b4e53f371f18fc637bee8e0a06d
staging  ready  saved  2  env-01a16b4e87c97b93982e0d2c1754ee93
```

`env` commands name the environment after the application. Commands that act
on an application's only environment, such as `piquelctl app deploy hello`,
now fail and list the environments to choose from.

When the application reads its manifest from Git, each environment follows
its own branch. `env create` takes `--branch`, by default the branch the
connection names, and `--commit` to pin one commit. `env branch` changes it
later, and the environment's next deployment reads the new branch:

```console
piquelctl env create notes qa --branch develop
piquelctl env branch notes qa release
```

In the dashboard, open the application's **Environments** tab and select
**New environment**. Fill in **Environment name**, and **Branch** for an
application that reads its manifest from Git, then select **Create
environment**. Each environment's row has its own **Deploy** button, and
selecting the row opens the environment's page.

### Delete an environment

Deleting an environment removes its services, secrets and deployment
history. Its named volumes are kept, with their data.

```console
piquelctl env delete hello staging
```

In the dashboard, it is **Delete environment**, at the bottom of the
environment's **Overview**.

## Declare environments in the manifest

A manifest can list the application's environments, each in its own block:

```toml
[spec.environments.staging]

[spec.environments.production]
```

The manifest then decides which environments exist. Saving it creates the
environments it declares that don't exist yet, without deploying them.
`piquelctl app plan` lists them before you save:

```text
Environments:
  + staging   created, deploys from branch main, follows pushes
  + production   created, deploys from releases promoted from staging
```

A declared environment can't be created, renamed or deleted with `piquelctl
env` or the dashboard: change the manifest instead. Removing a block doesn't
delete its environment, since it holds volumes and secrets. It keeps running,
and `piquelctl app show` lists it as no longer declared until you delete it
with `piquelctl env delete`.

A block also sets where its environment deploys from:

| Setting | Effect |
| --- | --- |
| `branch`, `commit` | The branch it follows, or the commit it deploys |
| `sync` | Whether pushes to its branch [deploy it](deploy-on-push.md#deploy-every-push) |
| `promote_from` | The environment it [receives releases from](#promote-releases), instead of building |
| `visibility` | `private` keeps all its routes on your tailnet |
| `variables` | Its [own values](#give-each-environment-its-own-values) |

The [manifest reference](../reference/manifest.md#declared-environments)
describes each one.

## Give each environment its own values

Values that differ between environments, such as a hostname or a number of
replicas, come from variables. Declare a default, override it for an
environment, and use it as `${{ vars.<name> }}`:

```toml
[spec.variables]
web_replicas = 1

[spec.environments.staging]

[spec.environments.production.variables]
web_replicas = 2

[[spec.services]]
name = "web"
replicas = "${{ vars.web_replicas }}"
```

Here production runs two replicas, and staging one. A
`[spec.environments.<name>]` block, even an empty one, declares its
environment, so the manifest lists both. An environment may also set a
variable that has no default, such as its hostname.

Add these lines to `hello.toml`, then check what production would run:

```console
piquelctl app plan --file hello.toml --env production
```

The plan shows the values production renders:

```text
Changes:
  ~ services.web.replicas   1 → 2

Variables:
  app.name = hello
  deployment.id = preview
  env.name = production
  env.slug = production
  vars.web_replicas = 2
```

piqueld sets system variables, such as `env.name` and `app.name`, itself. The
[manifest reference](../reference/manifest.md#variables) lists them, and the
settings that accept variables. Apply the manifest with
`piquelctl app apply --file hello.toml`, then deploy each environment.

With `piquelctl`, set the default, the production value, and the reference:

```console
piquelctl app variable set hello web_replicas 1
piquelctl app variable set hello web_replicas 2 --env production
piquelctl app service replicas hello web '${{ vars.web_replicas }}'
```

When the manifest doesn't declare environments yet, setting a value for one
adds a block for every existing environment, which declares them all.

In the dashboard, the application's **Variables** tab has one row per
variable, with its **Default** and a column for each environment. Select
**Add variable** to add one, then **Save changes**.

To keep an environment's routes on your tailnet, whatever each route says,
set its visibility to private:

```console
piquelctl env visibility hello staging private
```

On the environment's **Overview**, it is the **Visibility** card. Deploy the
environment to apply it.

## Promote releases

Once a deployment of an environment has built or pulled its images, piqueld
records a release: the manifest it deployed and its exact images. A promoted
environment never builds. It receives the release another environment runs,
so production runs exactly the images you tested on staging.

### Set up staging and production

This application builds on staging, from every push to `main`, and promotes
to production. It follows
[`examples/staging-production.toml`](https://github.com/piquel-fr/piqueld/blob/main/examples/staging-production.toml).

1. Commit the manifest to the application's repository as
   `infra/piqueld.toml`:

   ```toml
   api_version = "piqueld.dev/v1alpha1"
   kind = "Application"

   [metadata]
   name = "notes"

   [spec.manifest]
   path = "infra/piqueld.toml"
   sync = { mode = "poll", interval_seconds = 120 }

   [spec.manifest.repository]
   url = "https://github.com/example/notes.git"
   branch = "main"

   [spec.environments.staging]
   sync = true
   visibility = "private"

   [spec.environments.staging.variables]
   domain = "staging.notes.example.com"

   [spec.environments.production]
   promote_from = "staging"

   [spec.environments.production.variables]
   domain = "notes.example.com"

   [[spec.services]]
   name = "web"

   [spec.services.source]
   type = "git"
   repository = "self"

   [spec.services.source.build]
   type = "docker"
   dockerfile = "Dockerfile"

   # A runtime setting, not a build argument, so staging's image suits production.
   [spec.services.environment]
   ORIGIN = "https://${{ vars.domain }}"

   [spec.services.healthcheck]
   type = "http"
   port = 80
   path = "/"

   [[spec.routes]]
   name = "web"
   hostname = "${{ vars.domain }}"
   service = "web"
   port = 80
   visibility = "public"
   ```

   - `staging` follows `main` and deploys every push to it.
   - `production` only receives releases promoted from `staging`.
   - Each environment serves its own hostname, through the `domain`
     variable. Staging's route stays on your tailnet.

2. Apply the same file and deploy it:

   ```console
   piquelctl --timeout 10m app apply --file infra/piqueld.toml --deploy
   ```

   This creates both environments and deploys staging. Production waits for
   its first promotion:

   ```console
   $ piquelctl env list notes
   NAME  STATE  SOURCE  RESOLVED  ID
   production  not_deployed  promoted from staging  none  app-01a16b4fa9437d239e8fcb831cc48256
   staging  ready  main  1  env-01a16b4fa944788183a1f106f9644c08
   ```

### Promote staging to production

1. Test staging.

2. See what promoting would change:

   ```console
   piquelctl env promote notes production --plan
   ```

   The plan lists the release and where it comes from, how production would
   change, and the values it renders there:

   ```text
   Release:
   rel-01a16b4fb1637301891c0fdfc0fd59d7  commit d24b66dd0b37f343897ffc48d0b18e88d1c3bbd7  created 1792801747299
     Images: all present
     web: sha256:0e2b9e6e9271… (built from https://github.com/example/notes.git (d24b66dd…) at d24b66dd…)
     Origin: promoted from staging's deployment operation-01a16b4fa9467291867fdc328f9d9d4d

   Variables:
     …
     env.name = production
     vars.domain = notes.example.com
   ```

3. Promote it:

   ```console
   $ piquelctl --timeout 5m env promote notes production
   Promoting release rel-01a16b4fb1637301891c0fdfc0fd59d7 into environment production: promoted from staging's deployment operation-01a16b4fa9467291867fdc328f9d9d4d, deployment operation-01a16b50108b7e32852ff983fcb77e0a.
   ```

   After you confirm, production deploys the release staging runs, rendered
   with production's variables, and the command waits until it's done.

Promotion never happens on its own: pushes to `main` only redeploy staging.
Promote again whenever staging runs something you want in production.

In the dashboard, production's row on the **Environments** tab, and its page,
offer **Promote from staging**. Staging's page offers **Promote**. Both show
the plan first, then **Promote release** deploys it.

![The Environments tab of notes: production, promoted from staging, has a "Promote from staging" button, and staging, which follows branch main, has a "Deploy" button](../../images/applications/environments.png)

Only staging's current deployment can be promoted, once it succeeded and runs
healthy. To promote exactly the deployment you tested, even if staging
deploys again in the meantime, pass its ID with `--deployment`.

### Deploy an earlier release

`piquelctl app releases notes` lists the application's releases, newest
first, with the environments they were promoted to:

```text
rel-01a16b4fb1637301891c0fdfc0fd59d7  commit d24b66dd0b37f343897ffc48d0b18e88d1c3bbd7  created 1792801747299
  Images: all present
  web: sha256:0e2b9e6e9271… (built from https://github.com/example/notes.git (d24b66dd…) at d24b66dd…)
  Deployed to: production as operation-01a16b50108b7e32852ff983fcb77e0a, promoted from staging's deployment operation-01a16b4fa9467291867fdc328f9d9d4d
```

To go back to an earlier one, deploy it to the promoted environment by ID:

```console
piquelctl env promote notes production --release rel-01a16b4fb1637301891c0fdfc0fd59d7
```

In the dashboard, open the application's **Releases** tab, expand the release,
and select **Deploy to production**.

Images pulled from a registry are pulled again, but images piqueld built can
only be deployed while it keeps them: what each environment runs, and the
images of its last three successful deployments.
`images.keep_deployments`, in the
[daemon's configuration](../reference/configuration.md), changes that number.

### Keep builds the same in every environment

A release's images are built once, on staging. So every setting that goes
into a build must be the same in production: the image, the Dockerfile and
context, the build arguments and the target. A build argument that uses a
variable, such as `GREETING = "Hello from ${{ vars.domain }}"`, stops the
promotion:

```text
web.source.build.args.GREETING: renders differently in this environment than in the release, whose images were built from it (release_incompatible)
```

Pass such values to the running service instead, in `environment`, as
`ORIGIN` is above.

### Promote without declaring environments

For an environment the manifest doesn't declare, make it promoted with
`piquelctl`:

```console
piquelctl env create notes production --promote-from staging
piquelctl env source notes production --promote-from staging
piquelctl env source notes production --tracking
```

The first creates a promoted environment, and the second converts an
existing one. `--tracking` makes it build its own source again. None of them
deploys anything.

In the dashboard, choose the source in **Promote from** when you create the
environment. For an existing one, open its **Overview**, set **Deploys** on
its **Source** card to **Releases promoted from staging**, and select **Save
source**.

![The Source card of production: it deploys releases promoted from staging, and says that the manifest sets its source in spec.environments.production](../../images/applications/environment-source.png)

## Check that it works

```console
$ piquelctl env list notes
NAME  STATE  SOURCE  RESOLVED  ID
production  ready  promoted from staging  1  app-01a16b4fa9437d239e8fcb831cc48256
staging  ready  main  1  env-01a16b4fa944788183a1f106f9644c08
```

`piquelctl env show notes production` also prints the release the
environment runs, its variables and its URLs.

## Troubleshooting

- **A command says `environment required`.** The application has several
  environments. Use the `env` command and name one, such as
  `piquelctl env deploy hello staging`.
- **It fails with `environment_declared`.** The manifest declares that
  environment, so it decides whether it exists and where it deploys from.
  Change its `[spec.environments.<name>]` block instead.
- **It fails with `environment_unknown`.** No environment has that name, and
  editing the manifest never creates one. Check the name, or declare the
  environment and apply the manifest.
- **Planning or deploying fails with `variable_value_missing`.** The
  environment has no value for a variable the manifest uses. Set one for it,
  or a default.
- **Deploying fails with `environment_promoted`.** A promoted environment
  never builds: promote it with `piquelctl env promote`.
- **Promoting fails with `promotion_source_not_ready`.** The source's current
  deployment failed, or one of its services isn't healthy right now. Fix the
  source first.
- **Promoting fails with `promotion_source_changed`.** The source deployed
  again since you reviewed it. Plan the promotion again.
- **Promoting fails with `release_incompatible`.** A build setting renders
  differently in the target environment. See
  [keeping builds the same](#keep-builds-the-same-in-every-environment).
- **Promoting fails with `image_unavailable`.** piqueld removed the release's
  images. Promote a newer release.
- **Promoting fails with `secrets_unavailable`.** The release mounts secrets
  the target environment doesn't have or may not use. The error lists each
  one.
- **Deleting fails with `promotion_source_in_use`.** Other environments
  promote from it. Point them at another source first.
- **Deploying fails with `hostname_conflict`.** Two environments render the
  same route hostname. Give each its own value, through a variable.
