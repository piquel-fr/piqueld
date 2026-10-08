# Application manifest

The supported document is a strict TOML or JSON
`piqueld.dev/v1alpha1` `Application`. Unknown fields and unsupported source
types are errors. Services explicitly select a prebuilt Docker/OCI image or a Git build source.
Decode errors name the rejected field and its line and column, and may quote a
mistyped value, e.g.
``unknown field `routes`, expected ... at line 14 column 1``. For TOML requests,
the API returns them, like validation errors, as `details.errors` entries with a
path and message; JSON request bodies report only `json_malformed`.

The manifest is shared by the application's environments (for a
repository-backed application, each environment reads it from its own
branch), including services,
replica counts, limits, volumes, routes, jobs, repository settings and secret
file references. Manually set secret values live in one store per application,
shared by the environments their access lists allow; generated secret values
and the deployed snapshot belong to each environment. Values that differ between environments, such as hostnames or replica
counts, come from [variables](#variables).

```toml
api_version = "piqueld.dev/v1alpha1"
kind = "Application"

[metadata]
name = "notes"

[[spec.services]]
name = "web"
replicas = 1

[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1.4.0"

[spec.services.environment]
RUST_LOG = "info"

[[spec.services.mounts]]
volume = "data"
target = "/var/lib/notes"

[[spec.volumes]]
name = "data"
```

Services support replicas, environment variables, command and argument arrays,
health checks, startup dependencies, rollout settings, CPU/memory limits, mounts of declared
named volumes, and file references to application secrets. Named volumes are retained when an application
is deleted. Secret values are never manifest fields (manifests may only ask piqueld to
generate them), and there are no manifest
fields for directly published ports. Exact-host HTTP routes are declared in
`spec.routes`; see [managed ingress](ingress.md).

Rollouts start each replacement task before stopping the old one, except for
services that mount any volume without `read_only = true`. Those stop the old
task first so single-writer stores such as PostgreSQL or SQLite never share
their data directory, and are briefly unavailable during each rollout. A
service can choose its own order; see [Rollout](#rollout).

Services of one application share a private network on which each answers to
its manifest name, so `web` reaches a `postgres` service at `postgres:5432`.
Names resolve only within that application; identically named services in other
applications are unreachable.

Names are 1–63 lowercase ASCII letters, digits, or hyphens; they start with a
letter and cannot end with a hyphen. Applications may be empty. Deploying an empty application removes its services and network, retaining volume data.
Image references reject URL schemes, credentials, malformed tags, and malformed
digests; registry hostnames are validated case-insensitively and canonicalized
to lowercase (IPv6 literal hosts are not accepted). Mount targets are normalized
absolute paths below `/`; environment names and runtime strings reject invalid
control data. `PIQUELD_INGRESS_PROXIES` is reserved; piqueld injects it into
routed services (see [managed ingress](ingress.md#client-addresses)).
Health-check paths are absolute, and resource limits must specify CPU, memory,
or both.

Explicit budgets bound every manifest; exceeding one is a distinct validation
error whose message names the offending environment key where applicable:

| Budget | Limit |
| --- | --- |
| Services per application | 64 |
| Named volumes per application | 64 |
| Generated secrets per application | 64 |
| Environment entries per service | 256 |
| Environment key size | 255 bytes |
| Environment value size | 65,536 bytes |
| Command / arguments elements | 128 each |
| Command / arguments element size | 4,096 bytes |
| Mounts per service | 32 |
| Jobs per application | 16 |
| Job timeout | 86,400 seconds |
| Health-check interval | 3,600 seconds |
| Rollout monitor | 3,600 seconds |
| CPU limit | 1,048,576 millicores |

Defaults are replicas `1`, empty command and arguments, writable mounts, and
health-check values of path `/health`, interval `10` seconds, timeout `3`
seconds, and `3` retries. HTTP health checks run `wget` inside the container,
so the image must contain a `wget` binary; images without one (for example
distroless bases) must use command health checks instead. Services, mounts,
volumes, and environment maps are
canonicalized before hashing. The specification hash is SHA-256 over a versioned
canonical JSON envelope (`piqueld-spec-hash/v2`) covering only the canonical
spec. The application name selects which application an apply targets; changing
it targets a different application. Use the explicit rename action to retain
identity and resources, then update the manifest name.

Individual field endpoints edit the saved internal manifest atomically, validate
the result, and save without deployment unless requested. Git ownership and
configuration revision checks apply to field edits as well as manifest imports.
`piquelctl app repository disconnect NAME --yes` removes repository backing while
retaining the current saved services and volumes.

The parser is pure. Apply saves the complete normalized configuration without
starting a deployment unless explicitly requested. Deploy captures that saved
revision and prepares every source again. Reconciliation and retries reuse the
captured deployment and its prepared target, so later saved edits cannot enter
an existing deployment. Deploy resolves and builds the saved configuration's sources again.
Resolved runtime state remains separate from portable manifests. Generations
advance on saves, changed names, deletion intent, and environment creation,
renames and deletion, since environment names select configuration.

## Variables

Declare variables with a default in `[spec.variables]`, for one environment in
`[spec.environments.<name>.variables]`, or for every preview in
`[spec.previews.variables]` (see [Previews](#previews)), and reference them as
`${{ vars.<name> }}`:

```toml
[spec.variables]
web_replicas = 1

[spec.environments.staging.variables]
domain = "staging.piquel.fr"

[spec.environments.production.variables]
domain = "piquel.fr"
web_replicas = 3

[[spec.routes]]
hostname = "${{ vars.domain }}"
service = "web"
port = 3000
visibility = "public"

[[spec.services]]
name = "web"
replicas = "${{ vars.web_replicas }}"
```

An environment's own value overrides the default of the same name, as
`web_replicas` does in production. An environment may also declare a variable
that has no default, like `domain`. An environment uses its own value, else the
default. Every environment renders the same manifest (or its own branch's), so each environment that
deploys a reference needs a value for it: planning or deploying an environment
without one fails with `variable_value_missing`, naming the environment, the
field and the variable. Here staging and production each set `domain`, so they
serve different hostnames; two environments that render the same hostname
still conflict.

Syntax:

- `${{ namespace.name }}`, with optional whitespace inside the braces.
  `$${{` writes a literal `${{`. `${VAR}` and `$VAR` are ordinary text, so
  shell and application expansion keep working.
- References are lookups only: no functions, operators or conditionals.
  Substituted text is never evaluated again.

Variables are strings, integers or booleans. A string value may reference system
variables, but not other variables. System variables:

| Reference | Value |
| --- | --- |
| `app.name` | Application name. |
| `env.name` | Environment name; a preview's slug. |
| `env.slug` | DNS-safe environment identifier: the environment name, or a preview's slug. |
| `git.branch`, `git.sha` | Branch and commit the manifest was read from: the environment's branch, or a one-off `--branch`; only with `spec.manifest`. |
| `deployment.id` | ID of the deployment being captured. |

`secrets.*` is reserved for secret references.

References are allowed in values: service `environment` values, `command` and
`arguments`, health check settings, `replicas`, resource limits, rollout `order`
and `monitor_seconds`, route `hostname` and redirect `to`, job `command`, service
secret mount `name`, and the build inputs `image`, `dockerfile`, `context`, build argument values and
`target`. They are rejected elsewhere, including names, table keys, `type`
fields, mount and secret targets, `spec.secrets` declarations, Git repository
settings and `spec.manifest`.

The build inputs are baked into images, so each deployment's
[release](piquelctl.md#commands) fingerprints their rendered values. Rendering a
release for another environment reuses its images and requires every build
input to render the same there; a build argument such as
`VITE_ORIGIN = "https://${{ vars.domain }}"` makes the release incompatible
(`release_incompatible`) with environments whose `domain` differs. Keep
per-environment values in runtime fields, like `environment`, when the images
should be shared.

A field that is exactly one reference, like `replicas = "${{ vars.web_replicas }}"`,
takes the variable's own value, which must have the field's type: strings are
never parsed into numbers. Any other text containing a reference renders to a
string. Rendered values then pass the field's usual validation.

Validation (saving, `app validate`) rejects references to a variable declared
nowhere, unknown namespaces and system variables, and malformed references, with
the field path. Values that contain references are validated once rendered, when
an environment is planned or deployed. A `[spec.environments.<name>]` block
naming no environment of the application is allowed, so a manifest can be saved
before its environment exists; `app plan` warns about it with
`environment_block_unknown`.

A deployment renders the manifest for its environment when it is captured, or,
when repository-backed, once the manifest is fetched and its commit is known. The
deployment records the manifest as captured, the value of every variable in
scope, and the rendered manifest; retries reuse them and never read variables
again. Plans render with `deployment.id` set to `preview` and, for repository
manifests, the environment's branch and pinned commit (or 40 zeros), since
nothing is fetched. `deployment.id` changes with every deployment, so services that use
it are replaced on every deployment.

Renaming an environment would change which `[spec.environments.<name>]` block
applies, so piqueld refuses to rename an environment while the manifest it
deploys (for a repository-backed environment, the one last fetched from its
branch) has a block for its current or its new name (`environment_configured`). Remove
the block, rename the environment, then add the block back under the new name.

A rename never touches running services. When the configuration renders the
changed name through `${{ env.name }}`, `${{ env.slug }}` or `${{ app.name }}`,
the environment no longer reports the saved configuration as resolved: deploy
it to apply the new name.

## Validation and editor support

`piquelctl app validate --file application.toml` applies the same parser and
validation as the daemon, without contacting one, and lists every error with
its field path, including references to undeclared variables. Validation runs against the CLI's version of `piqueld-core`;
`app plan` checks the manifest against the connected daemon's version.

[`application-manifest.schema.json`](application-manifest.schema.json) is a
JSON Schema (draft 7) generated from the manifest types by `just generate`, and
it ships with each set of release binaries. It describes structure, field types, and
documentation. Names, budgets, and cross-references are left to
`app validate`. Editors that use [Taplo](https://taplo.tamasfe.dev/), such as
VS Code with Even Better TOML, give completion, hover documentation, and inline
errors when a manifest starts with a schema directive:

```toml
#:schema https://raw.githubusercontent.com/piquel-fr/piqueld/main/docs/application-manifest.schema.json
api_version = "piqueld.dev/v1alpha1"
```

Replace `main` with the commit your daemon runs, or point the directive at a
local copy of the schema. To apply the schema without directives, add a
`.taplo.toml` rule:

```toml
[[rule]]
include = ["deploy/**/*.toml"]
schema.path = "https://raw.githubusercontent.com/piquel-fr/piqueld/main/docs/application-manifest.schema.json"
```

## Git build sources

A service may explicitly build from Git instead of pulling a prebuilt image:

```toml
[[spec.services]]
name = "web"
[spec.services.source]
type = "git"
[spec.services.source.repository]
url = "https://example.com/team/application.git"
branch = "main"
# commit = "0123456789012345678901234567890123456789"
[spec.services.source.build]
type = "docker"
dockerfile = "services/web/Dockerfile"
context = "."
# Optional: values passed as --build-arg, and a multi-stage target.
args = { VITE_AUTH_ORIGIN = "https://auth.example.com" }
target = "runtime"
```

The daemon requires Git and the Docker CLI with the Buildx plugin in its PATH. Git inherits the host's
credentials; piqueld does not store credentials or prompt for them. Only trusted
repositories are supported: Dockerfiles execute build instructions on the host's
Docker Engine. Builds are serialized across applications, and the existing
`reconciliation.prepare_timeout_seconds` bounds preparation (default: 300 seconds).
Increase that budget for longer builds.

Each preparation gets an isolated checkout. A full configured commit hash is
used directly; otherwise the branch head is resolved once. Dockerfile and context
paths are relative to the repository root, must stay within it, and default build
context is `.`. Build argument names follow the environment variable rules, and
because they are passed on the `docker buildx build` command line, build arguments may
total at most 256 KiB. `target` must name a stage in the Dockerfile; without it Docker builds the final
stage. Build arguments are not secret: Docker records them in image metadata and
piqueld shows them in manifests and build history. Use service secrets for
sensitive values. There is no automatic build backend detection, submodule or LFS
setup, registry publishing, or automatic image cleanup. Docker's build cache is
reused and base images are refreshed with `--pull`. Builds record no provenance
attestations, which differ on every build, so a rebuild Docker serves from its
cache keeps the same image ID and shares its release.

The resolved source records the full Git commit and content-addressed local image
ID. Every repository and build field, including build arguments and the target, is
part of that source's identity, so changing any of them triggers a rebuild. All
service sources are prepared before any application rollout. A failed checkout or
build preserves the existing running deployment. Normal reconciliation
reuses prepared images; explicit deploy resolves and builds sources again.
Local images are supported only on the existing single-node Swarm topology.

A repository-backed application can build from its own manifest repository:

```toml
[spec.services.source]
type = "git"
repository = "self"
[spec.services.source.build]
type = "docker"
dockerfile = "infra/auth.Dockerfile"
```

`self` uses the repository the manifest was fetched from, at that exact commit,
so every such service in a deployment builds from one revision. Disconnecting
repository backing replaces `self` with the former manifest repository and
branch.

Git checkout permits file, Git, HTTP(S), and SSH transports. Executable remote
helpers such as `ext::` are disabled, including through host URL rewrites.

## Repository-backed manifests

Manifest retrieval is independent from service source selection. Add this to an
application to read its next configuration from Git when Deploy is requested:

```toml
[spec.manifest]
path = "infra/piqueld/application.toml"
[spec.manifest.repository]
url = "https://example.com/team/infrastructure.git"
branch = "main"
# commit = "0123456789012345678901234567890123456789"
```

Create the application manually with `piquelctl app apply --file bootstrap.toml`.
A bootstrap manifest may contain only its header, metadata, and `spec.manifest`;
services can be supplied by the first fetched manifest. Then click **Deploy** in
the dashboard or run `piquelctl app deploy NAME --yes` (`env deploy NAME ENV` when
the application has several environments).

The repository URL and manifest path belong to the application: every
environment reads the same file from the same repository. The branch belongs to
each environment. The `production` environment created with the application,
and environments created later without `--branch`, follow the branch and pinned
commit `spec.manifest` names; other environments follow their own:

```console
piquelctl env create notes staging --branch main
piquelctl env branch notes staging release
piquelctl env branch notes production main --commit 0123456789012345678901234567890123456789
```

Changing a branch redeploys nothing: the environment's next deployment fetches
it. Connecting a repository to an application points every environment at the
branch `spec.manifest` names; disconnecting returns every environment to the
saved manifest. Environments report their source as `saved` or as the branch they
follow.

Deploying an environment resolves its pinned commit (or branch head), reads only
the exact configured TOML/JSON file, and checks that its name matches the
existing application. It renders the file with that environment's variables and
records the environment, commit, manifest, and rendered manifest in the
deployment. Empty manifests remove services and networks while retaining volume
data. Other files in the repository are ignored. A missing file fails with
`manifest_not_found`; no application is deleted. Invalid files and build
failures preserve both accepted configuration and the existing running target.

Each environment keeps the manifest last fetched from its branch. Its hostname
reservations, the rename check, `app plan --env`, `env show`, and its dashboard
page read that manifest, so a fetch for one environment never changes what
another deploys or reserves. An environment that was never deployed has fetched
nothing and reserves no hostnames yet. The application's saved manifest is the
one last fetched by any of its environments, keeping the application's own
connection: the application page and `app show` display it, and environments
deploy it after disconnecting.

A fetched file's own `spec.manifest` is ignored: files on different branches
often differ, and one branch must not redirect every environment. It may be
omitted; when present it must still decode like any manifest's. When it names
another repository URL or manifest path than the application's connection, the
deployment records a `manifest_connection_ignored` warning, shown by
`app deploy`/`env deploy` and on the dashboard's deployment snapshot. Change the
connection with `piquelctl app repository url` or `path`. The fetched manifest
and its commit are persisted for restart/retry; a new Deploy fetches again. A
Git service source with an explicit repository resolves its own revision
independently; `repository = "self"` builds the fetched manifest commit. Image
sources are explicitly refreshed, even when the fetched manifest is unchanged.

Git owns runtime configuration while backing is enabled: direct apply cannot
change services or volumes, and rename is rejected with `repository_managed`.
Connection settings alone remain editable through apply so an incorrect path
can be repaired. Manifest connection settings do not change the runtime spec
hash. Source builds, deployment, and rollback retain the behavior described above.
Automatic synchronization and webhooks are not implemented.

`piquelctl env deploy NAME ENV --branch feature` (or `--commit SHA`, also on
`app deploy`) fetches the manifest from another revision for one deployment,
without changing the environment's branch. `self` sources build that revision
too. Subsequent deploys fetch the environment's own branch again.

Services mount secrets as files:

```toml
[[spec.services.secrets]]
name = "database-password"
target = "/run/secrets/database-password"
```

References contain names and paths, never values. A service supports up to 64
secret mounts, with unique normalized paths under `/run/secrets`. A name declared
in `spec.secrets` is generated for each environment (see below). Any other name
comes from the application's secret store, whose values are set with
`piquelctl app secret` or the dashboard and must exist when a deployment captures
its inputs. A name can't be both declared and stored: setting a stored value for
a declared name, or saving a declaration for a stored name, fails validation
with `secret_name_conflict`.

Each stored secret lists who may mount it: every environment, including ones
created later (the default), or only the listed ones, plus a flag for previews.
Previews may mount it only with that flag, which is off by default: "every
environment" never includes previews. Lists hold environment IDs, so a renamed
environment keeps its access and a deleted one drops out of every list. When a
deployment captures its inputs, or `app plan --env` previews them, an environment
that mounts a stored secret its list excludes fails with `secret_access_denied`,
naming the environment and the secret. Narrowing a list doesn't affect running
deployments, which keep their pinned versions.

Mount names accept variables, so each environment can mount its own stored
secret:

```toml
[[spec.services.secrets]]
name = "${{ vars.stripe_key }}"
target = "/run/secrets/stripe"

[spec.environments.production.variables]
stripe_key = "stripe-live"

[spec.environments.staging.variables]
stripe_key = "stripe-test"
```

Whether the name is generated or stored is decided on the rendered name.

An application can declare secrets whose values piqueld generates, so a new
application deploys in one `apply --deploy` step:

```toml
[[spec.secrets]]
name = "database-password"
generate = { type = "random", bytes = 32, encoding = "hex" }

[[spec.secrets]]
name = "signing-key"
generate = { type = "rsa", bits = 3072 }
```

Random secrets use 16–512 bytes from the operating system, encoded as lowercase
`hex` (the default) or unpadded `base64url`. RSA secrets are 2048-, 3072-, or
4096-bit private keys in PKCS#8 PEM.

A value is generated when a deployment prepares its inputs, a service mounts
the secret, and it has no stored value. Declarations that no service mounts are
not generated, so they never count against secret quotas. A value is never
changed afterwards: later applies, deploys, and edits to the declaration keep
it, so rotation stays explicit: `piquelctl env secret APP --env ENV regenerate NAME`
generates a new version for the next deployment. A value discarded by key
recovery has nothing left to keep, so the next deployment that mounts it
generates a new one. Removing a declaration retains the generated value.

## Previews

A preview is a disposable deployment of one branch of the application's
manifest repository, for testing a branch by hand or from an agent. It is an
environment of its own kind: it has its own deployments, generated secrets,
volumes, routes, logs and Docker network, but it never appears among the
application's environments, and environment commands never act on it.

```console
piquelctl preview create notes --branch feat/login [--slot agent-2]
piquelctl preview deploy notes feat/login      # redeploy the branch head
piquelctl preview list notes
piquelctl preview delete notes feat/login
piquelctl preview prune notes --branch-gone
```

Only repository-backed applications have previews; creating one for another
application fails with `preview_requires_repository`. A preview deploys the
manifest file from its own branch, rendered with `[spec.previews]` and never
with an `[spec.environments.<name>]` block:

```toml
[spec.previews]
# The strictest visibility preview routes get; private by default.
visibility = "private"

[spec.previews.variables]
domain = "${{ env.slug }}.dev.example.com"
web_replicas = 1
```

Each value in `[spec.previews.variables]` overrides the `[spec.variables]`
default of the same name, or declares a variable without one, for every
preview. `git.branch` and `git.sha` are the branch and commit the preview
fetched.

**Identity.** A preview is keyed by its branch and optional slot, so several
agents can work on one branch. Creating a preview again with the same branch
and slot returns the existing preview and its latest deployment without
redeploying it; `preview deploy` redeploys. Creation, deployment and deletion
need no application revision, so agents creating previews never conflict with
each other or with people saving the manifest.

**Slugs.** Each preview gets a DNS-safe slug derived once from
`<application>-<branch>[-<slot>]`: lowercase letters and digits, with every
other run of characters as one hyphen, truncated, then a hyphen and a short
hash of the exact application, branch and slot (`notes-feat-login-3fa2c1`).
Slugs are at most 40 characters, so a prefix still fits in a 63-character DNS
label. The slug is stored when the preview is created and never recomputed, so
renaming the application keeps it. It is `env.slug` (and `env.name`), and also
the preview's name, so no environment of the application can share it.

**Routes.** Previews render the same routes as environments. Use flat
hostnames, one label below a shared domain, so one wildcard certificate
(`*.dev.example.com`) covers every preview. The slug already names the
application, so a route needs a prefix only to tell several routes apart:

```toml
# notes-feat-login-3fa2c1.dev.example.com
[[spec.routes]]
hostname = "${{ env.slug }}.dev.example.com"
service = "web"
port = 3000

# api-notes-feat-login-3fa2c1.dev.example.com
[[spec.routes]]
hostname = "api-${{ env.slug }}.dev.example.com"
service = "api"
port = 8080
```

In practice these hostnames go through a variable, so environments keep their
own: `hostname = "${{ vars.domain }}"` with `domain` set per environment and in
`[spec.previews.variables]`.

Hostnames are reserved as for environments: a preview reserves the hostnames
its last fetched manifest renders, and two previews or environments rendering
the same hostname conflict with `hostname_conflict`. Preview routes are private
unless `[spec.previews] visibility = "public"`. Private hostnames need DNS
records pointing at the apps tailnet node (see
[public and private routes](ingress.md#public-and-private-routes)).

**Secrets.** Each preview generates its own `spec.secrets` values. It mounts a
stored secret only when the secret's access allows previews; otherwise its
deployment fails with `secret_access_denied`.

**Volumes.** A preview's volumes start empty. The preview keeps an inventory of
every volume its deployments created, including volumes a later manifest
dropped. Deleting the preview removes all of them, with their data, and checks
that none is left before the preview is gone; a deletion that cannot remove one
is retried. Environment volumes are retained, as always.

**Lifetime.** Previews are never deleted automatically. `preview list` and
`preview show` report each branch's state, read on demand with `git ls-remote`:
`exists` (at the deployed commit, or before the first fetch), `moved` (the
branch head is past the deployed commit), `gone`, or `unknown` when the
repository could not be read. `preview prune --branch-gone` deletes, after
confirmation, the previews whose branch is gone; the daemon checks again and
keeps any it can no longer confirm gone. Repository or authentication errors
never count as gone: pruning then deletes nothing. Previews cannot be promotion
sources and do not record releases.

## Startup dependencies

```toml
[[spec.services]]
name = "auth"
depends_on = ["postgres"]
```

A deployment rolls a service out only after every service in its `depends_on`
converges in that deployment: all replicas run and pass their health checks.
A dependency without a health check counts once its replicas are running, so
declare one on databases and other slow starters. Dependencies that are
already converged and unchanged do not delay the dependent. A failed or
timed-out dependency leaves its dependents unchanged. Each dependency gets the
full `reconciliation.convergence_timeout_seconds` to converge, since the
deadline restarts whenever a service converges.

Each entry names another service in the same application, at most once
(`service_dependency_missing`, `service_dependency_duplicate`); lists longer
than the 64-service limit are rejected (`service_dependency_count_excessive`). Cycles,
including a service depending on itself, are rejected with
`service_dependency_cycle` on every service in or behind the cycle.
Renaming a service through field edits updates references to it; removing one
drops it from other services' dependencies.

This is startup ordering only. It gives no runtime guarantee after rollout:
a dependency that later becomes unhealthy does not stop or restart its
dependents, and drift repair does not pass a dependency that is still
converging.

## Rollout

```toml
[[spec.services]]
name = "worker"

[spec.services.rollout]
order = "stop-first"   # or "start-first"
monitor_seconds = 30
```

Docker replaces one task at a time and pauses the rollout when a replacement
fails within `monitor_seconds` of starting. `stop-first` stops the old task
before starting its replacement: tasks never overlap, at the cost of a short
downtime. `start-first` starts the replacement first, so there is no downtime
but the two tasks briefly run side by side. Use `stop-first` for a singleton
worker that must not run twice, and `start-first` for a store that tolerates
two writers.

Both fields are optional, and an empty or missing block keeps the defaults:
the order is derived from the mounts (`stop-first` when any volume is mounted
writable, `start-first` otherwise) and the monitor window is 30 seconds. The
order must be one of the two values above, and `monitor_seconds` must be
between 1 and 3,600 (`rollout_monitor_invalid`). Docker only reports an update
as complete once the last replacement's monitor window has passed, so keep it
below `reconciliation.convergence_timeout_seconds`.

`app plan` lists each service's effective order, marked `derived` or
`explicit`, and its monitor window. It warns with
`rollout_start_first_writable_volume` when `start-first` is set on a service
that mounts a volume writable, since two tasks then write the same data
directory. The warning does not block the deployment. Changing either setting
updates the service once.

## Jobs

Jobs run a container to completion at a defined point of every deployment,
for example database migrations:

```toml
[[spec.jobs]]
name = "migrate"
service = "auth"             # reuse the new image, env, secrets and mounts
command = ["auth-service", "migrate"]
run = "before-rollout"
timeout_seconds = 300        # default
```

A job reuses the prepared image, environment, secret files, volume mounts,
resource limits, and private network of the referenced service in the same
deployment. Its `command` replaces the service's command and arguments. Health
checks do not apply, including an image's `HEALTHCHECK`, and a job does not
answer for the service's network name. `before-rollout` is currently the only
run point: jobs run in declared order after every source is prepared and before
the deployment is promoted. Missing networks and volumes are created first, and
a deployment whose plan is blocked runs no jobs.

Jobs inherit their referenced service's `depends_on`. Before each job, those
services and their transitive dependencies start or update to the prepared
configuration and converge, including their health checks. For example, set
`depends_on = ["postgres"]` on `auth` and give `postgres` a readiness health
check: even on the first deployment, the sequence is start Postgres, wait until
it is healthy, run the migration, then roll out auth. Without a health check,
a dependency counts as ready once its replicas are running. Each dependency
gets the normal service convergence timeout; the job's own timeout starts
after dependencies are ready.

Other services wait until all jobs succeed. If a dependency has its own jobs,
put all of them before the job that needs that dependency, or validation rejects
the order with `job_dependency_order_invalid`. This also applies to transitive
dependencies. Jobs cannot reach services that have not started and are not in
their inherited dependency list.

A non-zero exit, a rejected task, or exceeding `timeout_seconds` (1–86,400) fails
the deployment with `job_failed` or `job_timeout`. Services outside the job's
startup dependencies keep their previous configuration; dependencies may have
already started or updated and are not rolled back. Active-target repair does
not revert or remove those prerequisites while the deployment is unpromoted.
The application stays degraded, and failed jobs are not retried
automatically; deploy again after fixing the cause. Each job succeeds at most
once per deployment: a retried deployment skips jobs that already succeeded,
and after a daemon restart, or when Docker could not report a job's status, a
still-running job is resumed rather than started again, with a fresh timeout.
Retrying a promoted deployment or repairing drift never runs jobs. A deployment
that is superseded or cancelled stops its running job, and the next deployment
stops any job an earlier one left behind. Each run, its bounded output, and
Docker's explanation of a failed task are kept in the application's build
history; output past 1 MiB per run is dropped and the run is marked truncated.

Besides applying a manifest, jobs can be edited with `piquelctl app job`, the
dashboard's Jobs tab, or `PUT /api/v1/applications/{id}/jobs`. Renaming a
service repoints its jobs; removing a service removes them.

## Routes

```toml
[[spec.routes]]
hostname = "notes.example.com"
service = "web"
port = 3000
visibility = "public"
```

`visibility` is `public` (the internet) or `private` (only the tailnet, on the
same hostname), and defaults to `private`. `[spec.environments.<name>]
visibility` and `[spec.previews] visibility` cap the routes of an environment and
of previews: a route's effective visibility is the stricter of its own and the
ceiling. Environments default to `public`, which restricts nothing; previews
default to `private`.

```toml
[spec.environments.staging]
visibility = "private"
```

See [public and private routes](ingress.md#public-and-private-routes) for
listeners, the apps tailnet node and DNS.

Each route either references a service in the same application and its internal
HTTP port (1–65535), or sets `redirect` instead:

```toml
[[spec.routes]]
hostname = "www.notes.example.com"
redirect = { to = "https://notes.example.com", status = 308, preserve_path = true }
visibility = "public"
```

Redirects are answered by the gateway, so an application may consist of redirect
routes alone. `to` is an `http`/`https` URL with a lowercase public hostname, an
optional port and path, and no query, fragment, or `{}`; it must not target the
route's own hostname. `status` is 301, 302, 303, 307, or 308 (default 308).
`preserve_path` (default `true`) appends the request path and query to `to`.
Up to 64 routes are allowed. Hostnames are exact public ASCII
DNS names (use Punycode for internationalized domains), normalized to lowercase
without a trailing dot. Wildcards, paths, schemes, and hostname ports are rejected.
A hostname belongs to one application across saved, captured, and deployed state.
Conflicts fail atomically with `hostname_conflict`, including when ingress is off.

Save does not change live routes. Deploy activates additions/destination changes
once backends converge; explicit removals are withdrawn before backend cleanup.
Routes are still accepted and deployed while ingress is disabled, but are not
exposed until the daemon's global `ingress.enabled` setting is enabled.
