# Secrets and variables

Give your services passwords and API keys without writing them in the
manifest, and set values that differ between environments, such as how many
replicas a service runs.

## Before you start

- You have an application, such as the one from
  [Configuring services](services.md). The examples use `notes`, with a
  PostgreSQL service `db` and an nginx service `web`.
- Run the commands on this page on the host, or on any machine where
  `piquelctl` is [signed in](../getting-started/first-account.md#sign-piquelctl-in).
- Setting, regenerating and deleting secret values, and choosing who may mount
  them, needs the `secrets:write` [permission](../accounts/authorization.md)
  on the application. Editing the manifest needs `apps:write`, and deploying
  it `apps:deploy`.

## How secrets reach your services

piqueld puts each secret in a read-only file under `/run/secrets` in the
container. It never writes a secret value in the manifest or in an environment
variable, so point your service at the file. Many images read a variable
ending in `_FILE` for this, such as PostgreSQL's `POSTGRES_PASSWORD_FILE`.

A secret's value comes from one of two places:

- **Generated**: you declare the secret in the manifest, and piqueld generates
  a value for each environment. Use it for values nobody needs to type, such as
  a database password.
- **Stored**: you set the value yourself, in the application's secret store.
  Use it for values that come from elsewhere, such as an API key.

## Generate a secret

`notes.toml`, from [Configuring services](services.md), already generates the
database's password. These are the lines that do it:

```toml
[[spec.services]]
name = "db"
# ...

[spec.services.environment]
POSTGRES_PASSWORD_FILE = "/run/secrets/db-password"
# ...

[[spec.services.secrets]]
name = "db-password"
target = "/run/secrets/db-password"

# ...

[[spec.secrets]]
name = "db-password"
generate = { type = "random", bytes = 32 }
```

- `[[spec.secrets]]` declares the secret `db-password`, and how to generate
  it.
- `[[spec.services.secrets]]` mounts it in `db`, as the file
  `/run/secrets/db-password`.
- `POSTGRES_PASSWORD_FILE` tells PostgreSQL to read its password from that
  file.

To generate a secret for another service, add the same two tables to the
manifest and deploy it.

The first deployment that mounts `db-password` generates its value: 32 random
bytes, written as 64 hexadecimal characters. Each environment gets its own
value and keeps it: later deployments and changes to the declaration never
replace it.

`generate` can also write the bytes as `base64url`, with
`encoding = "base64url"`, or make an RSA private key, with
`{ type = "rsa", bits = 3072 }`. The
[manifest reference](../reference/manifest.md) lists every option.

Declare generated secrets in the manifest file and apply it: the dashboard and
`piquelctl`'s other commands can mount them, but not declare them.

## Store a secret

1. Put the value in a file, then store it:

   ```console
   piquelctl app secret notes set smtp-password --file ./smtp-password
   ```

   After you confirm, it prints:

   ```text
   Saved smtp-password generation 1, mountable by every environment. Deploy to use it.
   ```

   To pipe the value in instead, use `--stdin`. Values are 1 to 512,000 bytes
   long. Avoid typing them in a command, where your shell history keeps them.

2. Mount it like a generated secret. A name that `spec.secrets` doesn't declare
   comes from the store. Add to the existing `web` entry:

   ```diff
    [[spec.services]]
    name = "web"
    replicas = 2
    depends_on = ["db"]
   +
   +[spec.services.environment]
   +SMTP_PASSWORD_FILE = "/run/secrets/smtp-password"
   +
   +[[spec.services.secrets]]
   +name = "smtp-password"
   +target = "/run/secrets/smtp-password"

    [spec.services.source]
   ```

3. Deploy: `piquelctl app apply --file notes.toml --deploy`.

In the dashboard, open the application's **Secrets** tab. Under
**Save a secret**, enter the **Secret name** and the **Value**, then select
**Save secret**. The value can't be read back, and the dashboard only stores
text: store binary files with `piquelctl`.

![The application's Secrets tab: the secret store lists smtp-password, version 2, mountable by production, and smtp-test, version 1, mountable by staging, above the Save a secret form, then the secret files of db and web](../../images/applications/application-secrets.png)

Below the store, each service has a **secret files** card. Select
**Add secret file**, enter the **Secret name** and the **Container path**, then
select **Save changes**. `piquelctl` has no command for secret mounts: change
them in the manifest or in the dashboard.

Mount targets are files under `/run/secrets/`. [Jobs](jobs.md) mount secrets
the same way.

### Choose who may mount a stored secret

By default, every environment may mount a stored secret, including
environments created later, but previews may not. To allow only production:

```console
$ piquelctl app secret notes access smtp-password --environments production
Saved smtp-password generation 1, mountable by production. Deploy to use it.
```

`--environments` takes a comma-separated list of environment names, and
`--all-environments` allows every environment again. `--previews` lets
previews mount the secret too, and `--no-previews` stops them. `set` takes the
same options, to choose when you store a secret.
`piquelctl app secret notes list` shows who may mount each secret.

In the dashboard, select **Edit** next to the secret, change
**Who may mount it**, and select **Save access only**.

Changing who may mount a secret doesn't change running services. Deploying an
environment that mounts a secret it may not use fails at once, before changing
anything.

## Check that it works

```console
$ piquelctl env show notes
production of notes (app-01a16d95420677219cf7b65149b4f7e7)

Intent: ready
Runtime: ready (observed 0 s ago)
...
Secret: db-password: generated for this environment
Secret: smtp-password: application store, version 1
```

`env show` lists each secret the environment's manifest mounts, and where its
next deployment takes the value from. In the dashboard, open the environment,
then its **Secrets** tab: **Mounted secrets** shows the same, and
**Generated secrets** lists the values piqueld generated for it.

![The production environment's Secrets tab: Mounted secrets shows db-password generated for this environment and smtp-password from the application store, version 2; the secret store it may mount; and Generated secrets lists db-password, version 2, with Regenerate and Delete buttons](../../images/applications/environment-secrets.png)

To see the files themselves, [run a command](commands.md) in the container:

```console
$ piquelctl app exec notes db -- ls -l /run/secrets
total 4
-r--r--r--    1 root     root            64 Oct 24 11:08 db-password
```

## Use different values per environment

Manifest variables hold the values that differ between environments. Declare a
default in `[spec.variables]`, override it for one environment in
`[spec.environments.<name>.variables]`, and reference it as
`${{ vars.<name> }}`. For example, add at the end of `notes.toml`:

```toml
[spec.variables]
web_replicas = 1

[spec.environments.production.variables]
web_replicas = 2

[spec.environments.staging.variables]
```

Then use the variable in the `web` entry:

```diff
 [[spec.services]]
 name = "web"
-replicas = 2
+replicas = "${{ vars.web_replicas }}"
```

Production runs two replicas of `web`, and staging one. A variable needs no
default, but then every environment that deploys it needs its own value.

A `[spec.environments.<name>]` block also
[declares that environment](../reference/manifest.md#declared-environments):
saving this manifest creates `staging` if it doesn't exist yet.
`piquelctl app apply --file notes.toml --deploy` then deploys both
environments.

With `piquelctl`, quote the reference so that your shell leaves it alone:

```console
piquelctl app variable set notes web_replicas 1
piquelctl app variable set notes web_replicas 2 --env production
piquelctl app service replicas notes web '${{ vars.web_replicas }}'
```

`--env` names an environment that already exists.

In the dashboard, the application's **Variables** tab has one row per
variable, with its **Default** and one column for each environment. An empty
cell has no value there.

piqueld also sets a few variables itself, such as `${{ app.name }}` and
`${{ env.name }}`. See [variables](../reference/manifest.md#variables) for
them, and for the settings that accept references.

Variables are not secret: the manifest, `piquelctl app show` and the dashboard
show their values. Keep passwords in secrets.

### Mount a different secret in each environment

A secret's name can come from a variable, so each environment mounts its own
stored secret at the same path. Change the name of `web`'s mount, and give each
environment its value:

```diff
 [[spec.services.secrets]]
-name = "smtp-password"
+name = "${{ vars.smtp_secret }}"
 target = "/run/secrets/smtp-password"
```

```diff
 [spec.environments.production.variables]
 web_replicas = 2
+smtp_secret = "smtp-password"

 [spec.environments.staging.variables]
+smtp_secret = "smtp-test"
```

Store `smtp-test` as you stored `smtp-password`. Then [allow each secret](#choose-who-may-mount-a-stored-secret) only in its
environment. To check what an environment would mount before you deploy it,
plan it:

```console
$ piquelctl app plan --file notes.toml --env staging
...
  + services.web.secrets   name: smtp-test, target: /run/secrets/smtp-password
...
Variables:
  app.name = notes
  deployment.id = preview
  env.name = staging
  env.slug = staging
  vars.smtp_secret = smtp-test
  vars.web_replicas = 1
...
```

## Rotate a secret

To replace a stored value, set it again:

```console
$ piquelctl app secret notes set smtp-password --file ./new-smtp-password
Saved smtp-password generation 2, mountable by production. Deploy to use it.
```

Running services keep version 1 until you deploy them.

To replace a generated value, regenerate it in one environment:

```console
$ piquelctl env secret notes --env production regenerate db-password
Generated db-password generation 2. Deploy the environment to use it.
```

In the dashboard, open the environment's **Secrets** tab, and select
**Regenerate** next to the secret under **Generated secrets**.

A new value only replaces the file. A database that stored the old password
keeps it, so services that read the new file can't connect until you change it
there too. For PostgreSQL, deploy, then run:

```console
piquelctl app exec notes db --env production -i -- psql -U notes <<'EOF'
\set pw `cat /run/secrets/db-password`
ALTER USER notes PASSWORD :'pw';
EOF
```

## Delete a secret

Deleting a secret discards its value and every earlier version, and can't be
undone. You can only delete a secret that nothing uses. First, remove its
mounts from the manifest, and deploy every environment that mounted it. Then
delete it:

```console
piquelctl app secret notes delete smtp-password
```

In the dashboard, select **Delete** next to it on the **Secrets** tab.

A generated value is deleted in one environment, once nothing there uses it:

```console
piquelctl env secret notes --env staging delete db-password
```

If a deployment mounts that secret again, it generates a new value. Removing a
declaration from `spec.secrets` keeps the values already generated.

## If the secret key is lost

piqueld encrypts every value with the key in its data directory,
`secrets.key`. [Back it up](../guides/backups-and-upgrades.md) with the rest of
the data directory. Without it, piqueld can't read any secret value, and
deployments that need one fail with `secret_storage_unavailable`. Running
services keep their files. If no backup of the key exists, see
[recovering from a lost key](../reference/configuration.md#recovering-from-a-lost-key):
it discards every value, after which you store new ones and deploy.

## Troubleshooting

- **The deployment fails with `secret_missing`.** It mounts a stored secret
  with no value, often a misspelled name: `env show` reports it as
  `not set in the application store`. Store it, or fix the name, then deploy
  again. Running services are not changed.
- **The deployment fails with `secret_access_denied`.** The environment may not
  mount that stored secret, and `env show` reports it as
  `application store, not allowed here`.
  [Change who may mount it](#choose-who-may-mount-a-stored-secret).
- **Storing a secret fails with `secret_name_conflict`.** The manifest declares
  a generated secret of that name. Pick another name, or remove the
  declaration.
- **Deleting a secret fails with `secret_referenced`.** The saved manifest or a
  deployment still mounts it. Remove the mounts and deploy first.
- **Planning or deploying fails with `variable_value_missing`.** The
  environment has no value for a variable it uses, and the variable has no
  default. The error names the environment, the setting and the variable.
- **`--env` fails with `environment_unknown`.** No environment has that name.
  `piquelctl env list notes` lists them.
